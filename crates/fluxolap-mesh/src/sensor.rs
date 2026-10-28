//! Sensor data in: a broker of its own, on the local machine, that devices
//! publish to. It is a different broker from the one friends connect to, so a
//! friend can't inject readings.
//!
//! A message on `fluxolap/sensor/<database>/<cube>/cells` with the body of
//! `PUT /api/v1/databases/{db}/cubes/{cube}/cells` (`{"items": [{"path": [...],
//! "value": 1.5}]}`) is written to that cube. It goes through the node's own
//! REST API, so a sensor write behaves exactly like any other write. Database
//! and cube are addressed by name, which therefore can't contain `/`, `+` or
//! `#`.

use crate::broker;
use fluxolap_api_types::{CubeList, DatabaseList};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const FILTER: &str = "fluxolap/sensor/+/+/cells";

#[derive(Debug, Clone)]
pub struct SensorConfig {
    /// Where the sensor broker listens. Keep it on `127.0.0.1` unless the
    /// network in front of it decides who may publish.
    pub listen: SocketAddr,
    /// Base URL of this node's own REST API.
    pub api: String,
}

impl SensorConfig {
    /// From the environment; `Ok(None)` if `FLUXOLAP_SENSOR_LISTEN` isn't set.
    /// `FLUXOLAP_SENSOR_API` defaults to this server's own port.
    pub fn from_env() -> Result<Option<Self>, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let Some(listen) = var("FLUXOLAP_SENSOR_LISTEN") else { return Ok(None) };
        let port = var("FLUXOLAP_PORT").unwrap_or_else(|| "7777".into());
        Ok(Some(Self {
            listen: listen.parse().map_err(|e| format!("FLUXOLAP_SENSOR_LISTEN={listen}: {e}"))?,
            api: var("FLUXOLAP_SENSOR_API").unwrap_or_else(|| format!("http://127.0.0.1:{port}")),
        }))
    }
}

/// A running sensor ingest. Dropping it stops the ingest (the broker keeps running).
pub struct Sensor {
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Sensor {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Starts the sensor broker and the ingest. Must be called inside a Tokio runtime.
pub fn start(config: SensorConfig) -> std::io::Result<Sensor> {
    broker::start("sensor", config.listen)?;
    let mut options = MqttOptions::new("fluxolap-sensor-ingest", "127.0.0.1", config.listen.port());
    options.set_keep_alive(Duration::from_secs(15));
    options.set_max_packet_size(broker::MAX_PACKET_BYTES, broker::MAX_PACKET_BYTES);
    let (client, mut eventloop) = AsyncClient::new(options, 16);
    // The eventloop only queues; a worker writes in the order the messages came.
    let (queue, messages) = mpsc::channel::<(String, Vec<u8>)>(1024);
    let listen = tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    let _ = client.try_subscribe(FILTER, QoS::AtLeastOnce);
                }
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    if queue.try_send((p.topic, p.payload.to_vec())).is_err() {
                        tracing::warn!("sensor: more messages than can be written, dropping one");
                    }
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
    });
    let write = tokio::spawn(write_all(messages, config.api));
    Ok(Sensor { tasks: vec![listen, write] })
}

async fn write_all(mut messages: mpsc::Receiver<(String, Vec<u8>)>, api: String) {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().expect("http client");
    let mut ids: HashMap<(String, String), (u32, u32)> = HashMap::new();
    while let Some((topic, body)) = messages.recv().await {
        let parts: Vec<&str> = topic.split('/').collect();
        let [_, _, database, cube, _] = parts[..] else { continue };
        if let Err(e) = write(&http, &api, &mut ids, database, cube, body).await {
            tracing::warn!("sensor: {database}/{cube}: {e}");
        }
    }
}

/// Writes one message to the cube, looking up the ids by name (and again if the cached ones turned out stale).
async fn write(http: &reqwest::Client, api: &str, ids: &mut HashMap<(String, String), (u32, u32)>, database: &str, cube: &str, body: Vec<u8>) -> Result<(), String> {
    for attempt in 0..2 {
        let key = (database.to_string(), cube.to_string());
        let (db_id, cube_id) = match ids.get(&key) {
            Some(found) => *found,
            None => {
                let found = resolve(http, api, database, cube).await?;
                ids.insert(key.clone(), found);
                found
            }
        };
        let response = http
            .put(format!("{api}/api/v1/databases/{db_id}/cubes/{cube_id}/cells"))
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND && attempt == 0 {
            ids.remove(&key);
            continue;
        }
        return Err(format!("{status}: {}", response.text().await.unwrap_or_default()));
    }
    unreachable!("the second attempt returns")
}

async fn resolve(http: &reqwest::Client, api: &str, database: &str, cube: &str) -> Result<(u32, u32), String> {
    let get = |url: String, name: String| {
        let request = http.get(url).query(&[("name", name)]);
        async move { request.send().await?.error_for_status() }
    };
    let databases: DatabaseList = get(format!("{api}/api/v1/databases"), database.into()).await.map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())?;
    let db_id = databases.items.first().ok_or_else(|| format!("no database {database:?}"))?.id;
    let cubes: CubeList = get(format!("{api}/api/v1/databases/{db_id}/cubes"), cube.into()).await.map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())?;
    let cube_id = cubes.items.first().ok_or_else(|| format!("no cube {cube:?} in {database:?}"))?.id;
    Ok((db_id, cube_id))
}
