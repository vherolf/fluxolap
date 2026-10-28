//! The node's MQTT brokers: the one its friends connect to (`mesh`) and the
//! local one its sensors publish to (`sensor`). Embedded
//! `rumqttd`, which has login-only auth and no per-topic ACLs: the mesh is
//! read-only because nothing here ever turns a received message into a
//! change to a cube, not because the broker forbids publishing. Friends are
//! trusted not to publish to topics that aren't theirs (see `docs/mesh.md`).

use rumqttd::{Broker, Config, ConnectionSettings, RouterConfig, ServerSettings};
use std::collections::HashMap;
use std::net::SocketAddr;

/// Largest MQTT message the mesh sends: a catalog.
pub const MAX_PACKET_BYTES: usize = 4 * 1024 * 1024;

/// Starts a broker called `name` on `listen` in background threads. It can't be stopped
/// again; it lives until the process ends.
pub fn start(name: &str, listen: SocketAddr) -> std::io::Result<()> {
    // The broker reports a port it can't bind only from its own threads.
    drop(std::net::TcpListener::bind(listen)?);
    let server = ServerSettings {
        name: name.into(),
        listen,
        tls: None,
        next_connection_delay_ms: 1,
        connections: ConnectionSettings {
            connection_timeout_ms: 60_000,
            max_payload_size: MAX_PACKET_BYTES,
            max_inflight_count: 100,
            auth: None,
            external_auth: None,
            dynamic_filters: true,
        },
    };
    let config = Config {
        id: 0,
        router: RouterConfig {
            max_connections: 100,
            max_outgoing_packet_count: 200,
            max_segment_size: 2 * MAX_PACKET_BYTES,
            max_segment_count: 4,
            custom_segment: None,
            initialized_filters: None,
            shared_subscriptions_strategy: Default::default(),
        },
        v4: Some(HashMap::from([(name.to_string(), server)])),
        ..Default::default()
    };
    let mut broker = Broker::new(config);
    let name = name.to_string();
    std::thread::Builder::new().name(format!("fluxolap-{name}-broker")).spawn(move || {
        if let Err(e) = broker.start() {
            tracing::error!("{name} broker stopped: {e}");
        }
    })?;
    Ok(())
}
