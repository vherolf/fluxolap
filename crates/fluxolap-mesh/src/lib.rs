//! The fluxolap mesh: nodes that are friends tell each other which databases
//! and cubes they have, over MQTT, and fetch what a friend has calculated.
//!
//! No central broker: every node runs its own (`broker`) and connects, as a
//! client, to the brokers of its friends. On the topics of a node `n`:
//!
//! - `fluxolap/n/catalog`, retained: `n`'s [`catalog::Catalog`], published
//!   whenever it changes. A friend subscribes to it and keeps the last one
//!   ([`Mesh::peers`]).
//! - `fluxolap/n/req/<f>`: friend `f` asks `n` about a database.
//! - `fluxolap/n/resp/<f>`: `n`'s answer, with the database's id and
//!   modification time.
//!
//! The database itself is not sent over MQTT: the friend that wants it
//! fetches it from the REST API (`GET /databases/{id}/export`) and imports
//! it as `<friend>/<name>`. That happens by itself for the databases the
//! import rules (`policy`) say `auto`, and on request (`POST
//! /api/v1/mesh/friends/{friend}/databases/{database}/import`) for the rest.
//!
//! Who the friends are comes from an external service ([`friends`]). Nothing
//! received over the mesh changes a cube except such an import. Sensors
//! publish to a separate local broker ([`sensor`]). See `docs/mesh.md`.

pub mod broker;
pub mod catalog;
pub mod friends;
pub mod imports;
pub mod policy;
pub mod sensor;

use catalog::{Catalog, CatalogDatabase};
use fluxolap_api::{MeshControl, MeshError};
use fluxolap_api_types::{MeshCube, MeshDatabase, MeshDto, MeshFriend, MeshState};
use fluxolap_engine::AppState;
use friends::Friend;
use imports::{ImportLog, Record};
use policy::{Mode, Rules};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// How often the node looks whether its catalog changed.
const CATALOG_CHECK: Duration = Duration::from_secs(2);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// How long a friend has to answer a request.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct MeshConfig {
    /// This node's id: what the friends service and the friends call it.
    pub node: String,
    /// Where the node's own broker listens for friends.
    pub listen: SocketAddr,
    /// Base URL of this node's REST API as friends reach it.
    pub api: String,
    /// The friends service (`GET <url>?node=<id>`).
    pub friends_url: String,
    pub friends_token: Option<String>,
    /// How often the friends service is asked again.
    pub poll: Duration,
    /// Which friends' databases are imported by themselves.
    pub import: Rules,
    /// Where the node remembers what it imported; `None`: only in memory.
    pub state_file: Option<PathBuf>,
}

impl MeshConfig {
    /// The mesh settings from the environment; `Ok(None)` if
    /// `FLUXOLAP_MESH_NODE` isn't set (the mesh is off).
    /// `FLUXOLAP_MESH_FRIENDS_URL` and `FLUXOLAP_MESH_API` are then required;
    /// `FLUXOLAP_MESH_LISTEN` (default `0.0.0.0:1883`),
    /// `FLUXOLAP_MESH_FRIENDS_TOKEN`, `FLUXOLAP_MESH_POLL_SECS` (default 10)
    /// and `FLUXOLAP_MESH_IMPORT` (see [`Rules::parse`], default: nothing is
    /// imported by itself) are optional. `data_dir` is where the node keeps
    /// its import log.
    pub fn from_env(data_dir: Option<&std::path::Path>) -> Result<Option<Self>, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let Some(node) = var("FLUXOLAP_MESH_NODE") else { return Ok(None) };
        let required = |name: &str| var(name).ok_or_else(|| format!("{name} is required when FLUXOLAP_MESH_NODE is set"));
        let listen = var("FLUXOLAP_MESH_LISTEN").unwrap_or_else(|| "0.0.0.0:1883".into());
        Ok(Some(Self {
            node,
            listen: listen.parse().map_err(|e| format!("FLUXOLAP_MESH_LISTEN={listen}: {e}"))?,
            api: required("FLUXOLAP_MESH_API")?,
            friends_url: required("FLUXOLAP_MESH_FRIENDS_URL")?,
            friends_token: var("FLUXOLAP_MESH_FRIENDS_TOKEN"),
            poll: Duration::from_secs(var("FLUXOLAP_MESH_POLL_SECS").and_then(|s| s.parse().ok()).unwrap_or(10)),
            import: Rules::parse(&var("FLUXOLAP_MESH_IMPORT").unwrap_or_default()).map_err(|e| format!("FLUXOLAP_MESH_IMPORT: {e}"))?,
            state_file: data_dir.map(|d| d.join("mesh-imports.json")),
        }))
    }
}

/// A friend as this node currently sees it.
#[derive(Debug, Clone)]
pub struct Peer {
    pub friend: Friend,
    /// Whether the connection to the friend's broker is up. The catalog is
    /// the last one received, also while the friend is away.
    pub connected: bool,
    pub catalog: Option<Catalog>,
}

/// A question to a friend: do you have this database?
#[derive(Debug, Serialize, Deserialize)]
struct Request {
    id: u64,
    database: String,
}

/// The friend's answer, as it is now (not as the last catalog said).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Reply {
    id: u64,
    found: bool,
    /// The database's id in the friend's REST API.
    db_id: Option<u32>,
    modified: Option<u64>,
}

struct Conn {
    task: JoinHandle<()>,
    client: AsyncClient,
}

/// Why the last import of a database failed, and for which state of it, so a
/// failed one isn't retried until the friend's database changes.
struct Failure {
    remote_modified: Option<u64>,
    error: String,
}

type Key = (String, String);

struct Inner {
    config: MeshConfig,
    state: AppState,
    http: reqwest::Client,
    peers: Mutex<HashMap<String, Peer>>,
    conns: Mutex<HashMap<String, Conn>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    next_request: AtomicU64,
    imports: Mutex<ImportLog>,
    /// Transfers running, as (friend, database).
    busy: Mutex<HashSet<Key>>,
    failed: Mutex<HashMap<Key, Failure>>,
}

/// A running mesh node. Dropping it stops its tasks (the broker keeps running).
pub struct Mesh {
    inner: Arc<Inner>,
    tasks: Vec<JoinHandle<()>>,
}

impl Mesh {
    /// Starts the broker and the mesh tasks. Must be called inside a Tokio runtime.
    pub fn start(state: AppState, config: MeshConfig) -> std::io::Result<Mesh> {
        if !valid_id(&config.node) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("node id {:?} must be non-empty without / + # or spaces", config.node)));
        }
        broker::start("mesh", config.listen)?;
        let (client, eventloop) = AsyncClient::new(options(&config.node, "127.0.0.1", config.listen.port()), 16);
        let inner = Arc::new(Inner {
            http: reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build().expect("http client"),
            imports: Mutex::new(ImportLog::load(config.state_file.clone())),
            config,
            state,
            peers: Default::default(),
            conns: Default::default(),
            pending: Default::default(),
            next_request: AtomicU64::new(1),
            busy: Default::default(),
            failed: Default::default(),
        });
        let tasks = vec![
            tokio::spawn(serve_requests(inner.clone(), client.clone(), eventloop)),
            tokio::spawn(publish_catalog(inner.clone(), client)),
            tokio::spawn(sync_friends(inner.clone())),
        ];
        Ok(Mesh { inner, tasks })
    }

    /// The friends the friends service named, with the last catalog of each.
    pub fn peers(&self) -> Vec<Peer> {
        let mut peers: Vec<Peer> = self.inner.peers.lock().expect("peers lock poisoned").values().cloned().collect();
        peers.sort_by(|a, b| a.friend.id.cmp(&b.friend.id));
        peers
    }

    /// This node for the REST API (`fluxolap_api::Options::mesh`).
    pub fn control(&self) -> Arc<dyn MeshControl> {
        Arc::new(Control(self.inner.clone()))
    }
}

impl Drop for Mesh {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        for conn in self.inner.conns.lock().expect("conns lock poisoned").values() {
            conn.task.abort();
        }
    }
}

/// Ids end up in topics, so they can't contain topic syntax.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && !id.contains(['/', '+', '#']) && !id.chars().any(char::is_whitespace)
}

fn catalog_topic(node: &str) -> String {
    format!("fluxolap/{node}/catalog")
}

/// `from` asking `node` ...
fn request_topic(node: &str, from: &str) -> String {
    format!("fluxolap/{node}/req/{from}")
}

/// ... and `node` answering `to`.
fn reply_topic(node: &str, to: &str) -> String {
    format!("fluxolap/{node}/resp/{to}")
}

fn options(node: &str, host: &str, port: u16) -> MqttOptions {
    let mut options = MqttOptions::new(format!("fluxolap-{node}"), host, port);
    options.set_keep_alive(Duration::from_secs(15));
    options.set_max_packet_size(broker::MAX_PACKET_BYTES, broker::MAX_PACKET_BYTES);
    options
}

fn now() -> u64 {
    fluxolap_domain::unix_timestamp()
}

/// Publishes the catalog, retained, on the node's own broker whenever it differs from the last one.
async fn publish_catalog(inner: Arc<Inner>, client: AsyncClient) {
    let topic = catalog_topic(&inner.config.node);
    let mut last = String::new();
    loop {
        // The copies of friends' databases aren't offered on.
        let copies = inner.imports.lock().expect("imports lock poisoned").local_names();
        let json = serde_json::to_string(&catalog::build(&inner.state, &inner.config.node, &inner.config.api, &copies)).expect("catalog serializes");
        if json != last {
            if client.publish(&topic, QoS::AtLeastOnce, true, json.clone()).await.is_err() {
                return;
            }
            last = json;
        }
        tokio::time::sleep(CATALOG_CHECK).await;
    }
}

/// Keeps the node's own connection going and answers the friends' requests.
async fn serve_requests(inner: Arc<Inner>, client: AsyncClient, mut eventloop: rumqttc::EventLoop) {
    let prefix = format!("fluxolap/{}/req/", inner.config.node);
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                let _ = client.try_subscribe(format!("{prefix}+"), QoS::AtLeastOnce);
            }
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                if let Some(from) = publish.topic.strip_prefix(&prefix) {
                    answer(&inner, &client, from, &publish.payload);
                }
            }
            Ok(_) => {}
            Err(_) => tokio::time::sleep(RECONNECT_DELAY).await,
        }
    }
}

/// Answers a friend's question about a database. Only friends are answered.
fn answer(inner: &Inner, client: &AsyncClient, from: &str, payload: &[u8]) {
    if !inner.peers.lock().expect("peers lock poisoned").contains_key(from) {
        tracing::warn!("mesh: request from {from:?}, who isn't a friend; ignored");
        return;
    }
    let Ok(request) = serde_json::from_slice::<Request>(payload) else {
        tracing::warn!("mesh: request from {from} doesn't parse; ignored");
        return;
    };
    let catalog = catalog::build(&inner.state, &inner.config.node, &inner.config.api, &inner.imports.lock().expect("imports lock poisoned").local_names());
    let found = catalog.databases.iter().find(|d| d.name == request.database);
    let reply = Reply { id: request.id, found: found.is_some(), db_id: found.map(|d| d.id), modified: found.and_then(|d| d.modified) };
    let json = serde_json::to_vec(&reply).expect("reply serializes");
    if let Err(e) = client.try_publish(reply_topic(&inner.config.node, from), QoS::AtLeastOnce, false, json) {
        tracing::warn!("mesh: can't answer {from}: {e}");
    }
}

/// Asks the friends service for the friends, and starts, restarts and stops the connections to match.
async fn sync_friends(inner: Arc<Inner>) {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("http client");
    let config = &inner.config;
    let mut failing = false;
    loop {
        match friends::fetch(&http, &config.friends_url, &config.node, config.friends_token.as_deref()).await {
            Ok(list) => {
                failing = false;
                apply_friends(&inner, list);
            }
            Err(e) => {
                // Keep the friends we have; say it once per outage.
                if !failing {
                    tracing::warn!("friends service {} unreachable: {e}", config.friends_url);
                }
                failing = true;
            }
        }
        tokio::time::sleep(config.poll).await;
    }
}

fn apply_friends(inner: &Arc<Inner>, list: Vec<Friend>) {
    let list: Vec<Friend> = list
        .into_iter()
        .filter(|f| {
            let ok = valid_id(&f.id) && f.id != inner.config.node;
            if !ok && f.id != inner.config.node {
                tracing::warn!("ignoring friend with invalid id {:?}", f.id);
            }
            ok
        })
        .collect();
    let mut peers = inner.peers.lock().expect("peers lock poisoned");
    let mut conns = inner.conns.lock().expect("conns lock poisoned");
    let gone: Vec<String> = peers.keys().filter(|id| !list.iter().any(|f| &f.id == *id)).cloned().collect();
    for id in gone {
        peers.remove(&id);
        if let Some(conn) = conns.remove(&id) {
            conn.task.abort();
        }
        tracing::info!("friend {id} is gone");
    }
    for friend in list {
        if peers.get(&friend.id).is_some_and(|p| p.friend == friend) {
            continue;
        }
        if let Some(conn) = conns.remove(&friend.id) {
            conn.task.abort();
        }
        let Some((host, port)) = friend.mqtt.rsplit_once(':').and_then(|(h, p)| Some((h, p.parse::<u16>().ok()?))) else {
            tracing::warn!("friend {}: mqtt address {:?} is not host:port", friend.id, friend.mqtt);
            continue;
        };
        tracing::info!("following friend {} at {}", friend.id, friend.mqtt);
        let catalog = peers.get(&friend.id).and_then(|p| p.catalog.clone());
        peers.insert(friend.id.clone(), Peer { friend: friend.clone(), connected: false, catalog });
        let (client, eventloop) = AsyncClient::new(options(&inner.config.node, host, port), 16);
        let task = tokio::spawn(follow(inner.clone(), friend.clone(), client.clone(), eventloop));
        conns.insert(friend.id, Conn { task, client });
    }
}

/// Connects to a friend's broker, keeps its catalog up to date and collects its answers.
async fn follow(inner: Arc<Inner>, friend: Friend, client: AsyncClient, mut eventloop: rumqttc::EventLoop) {
    let node = &inner.config.node;
    let catalog_topic = catalog_topic(&friend.id);
    let reply_topic = reply_topic(&friend.id, node);
    let update = |f: &dyn Fn(&mut Peer)| {
        if let Some(peer) = inner.peers.lock().expect("peers lock poisoned").get_mut(&friend.id) {
            f(peer);
        }
    };
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                update(&|p| p.connected = true);
                let _ = client.try_subscribe(&catalog_topic, QoS::AtLeastOnce);
                let _ = client.try_subscribe(&reply_topic, QoS::AtLeastOnce);
            }
            Ok(Event::Incoming(Packet::Publish(publish))) if publish.topic == catalog_topic => match serde_json::from_slice::<Catalog>(&publish.payload) {
                Ok(catalog) if catalog.node == friend.id => {
                    update(&|p| p.catalog = Some(catalog.clone()));
                    import_automatically(&inner, &friend.id);
                }
                Ok(catalog) => tracing::warn!("friend {} sent the catalog of {}; ignored", friend.id, catalog.node),
                Err(e) => tracing::warn!("friend {} sent a catalog that doesn't parse: {e}", friend.id),
            },
            Ok(Event::Incoming(Packet::Publish(publish))) if publish.topic == reply_topic => {
                if let Ok(reply) = serde_json::from_slice::<Reply>(&publish.payload) {
                    if let Some(waiting) = inner.pending.lock().expect("pending lock poisoned").remove(&reply.id) {
                        let _ = waiting.send(reply);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                update(&|p| p.connected = false);
                tracing::debug!("friend {}: {e}", friend.id);
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
}

/// Asks a friend about one of its databases, over the mesh.
async fn ask(inner: &Inner, friend: &str, database: &str) -> Result<Reply, String> {
    let client = {
        let connected = inner.peers.lock().expect("peers lock poisoned").get(friend).is_some_and(|p| p.connected);
        let conns = inner.conns.lock().expect("conns lock poisoned");
        match conns.get(friend) {
            Some(conn) if connected => conn.client.clone(),
            _ => return Err(format!("friend {friend} is not connected")),
        }
    };
    let id = inner.next_request.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    inner.pending.lock().expect("pending lock poisoned").insert(id, tx);
    let json = serde_json::to_vec(&Request { id, database: database.to_string() }).expect("request serializes");
    let sent = client.publish(request_topic(friend, &inner.config.node), QoS::AtLeastOnce, false, json).await;
    let reply = match sent {
        Ok(()) => tokio::time::timeout(REPLY_TIMEOUT, rx).await,
        Err(e) => {
            inner.pending.lock().expect("pending lock poisoned").remove(&id);
            return Err(e.to_string());
        }
    };
    inner.pending.lock().expect("pending lock poisoned").remove(&id);
    match reply {
        Ok(Ok(reply)) => Ok(reply),
        _ => Err(format!("friend {friend} didn't answer")),
    }
}

/// Where this database stands against the copy here.
fn state_of(inner: &Inner, friend: &str, db: &CatalogDatabase) -> MeshState {
    let key = (friend.to_string(), db.name.clone());
    if inner.busy.lock().expect("busy lock poisoned").contains(&key) {
        return MeshState::Importing;
    }
    let imports = inner.imports.lock().expect("imports lock poisoned");
    match (imports.get(friend, &db.name), db.modified) {
        (None, _) => MeshState::New,
        (Some(_), None) => MeshState::Unknown,
        (Some(record), Some(modified)) if record.remote_modified.is_none_or(|stored| modified > stored) => MeshState::Update,
        (Some(_), Some(_)) => MeshState::Current,
    }
}

fn describe(inner: &Inner, friend: &str, db: &CatalogDatabase) -> MeshDatabase {
    let record = inner.imports.lock().expect("imports lock poisoned").get(friend, &db.name).cloned();
    let error = inner.failed.lock().expect("failed lock poisoned").get(&(friend.to_string(), db.name.clone())).filter(|f| f.remote_modified == db.modified).map(|f| f.error.clone());
    MeshDatabase {
        name: db.name.clone(),
        id: db.id,
        modified: db.modified,
        cubes: db.cubes.iter().map(|c| MeshCube { name: c.name.clone(), modified: c.modified, cells: c.cells }).collect(),
        local_name: local_name(friend, &db.name),
        policy: inner.config.import.mode_for(friend, &db.name),
        state: state_of(inner, friend, db),
        imported_modified: record.as_ref().and_then(|r| r.remote_modified),
        imported_at: record.as_ref().map(|r| r.imported_at),
        error,
    }
}

fn local_name(friend: &str, database: &str) -> String {
    format!("{friend}/{database}")
}

/// Starts the import of every database of `friend` that the rules import by
/// themselves and that is new or newer than its copy.
fn import_automatically(inner: &Arc<Inner>, friend: &str) {
    let Some((friend, catalog)) = inner.peers.lock().expect("peers lock poisoned").get(friend).and_then(|p| Some((p.friend.clone(), p.catalog.clone()?))) else { return };
    for db in catalog.databases {
        if inner.config.import.mode_for(&friend.id, &db.name) != Mode::Auto || !matches!(state_of(inner, &friend.id, &db), MeshState::New | MeshState::Update) {
            continue;
        }
        let failed_already = inner.failed.lock().expect("failed lock poisoned").get(&(friend.id.clone(), db.name.clone())).is_some_and(|f| f.remote_modified == db.modified);
        if failed_already {
            continue;
        }
        let (inner, friend) = (inner.clone(), friend.clone());
        tokio::spawn(async move {
            match transfer(&inner, &friend, &db.name, db.id, db.modified).await {
                Ok(()) => tracing::info!("mesh: imported {} from {}", db.name, friend.id),
                Err(e) => tracing::warn!("mesh: importing {} from {} failed: {e}", db.name, friend.id),
            }
        });
    }
}

/// Fetches a friend's database from its REST API and imports it as
/// `<friend>/<name>`, replacing the copy from an earlier import. Remembers
/// the `modified` the friend announced, so the copy counts as current until
/// the friend announces a higher one.
async fn transfer(inner: &Arc<Inner>, friend: &Friend, name: &str, remote_id: u32, remote_modified: Option<u64>) -> Result<(), String> {
    let key = (friend.id.clone(), name.to_string());
    if !inner.busy.lock().expect("busy lock poisoned").insert(key.clone()) {
        return Err("a transfer of this database is already running".into());
    }
    let result = fetch_and_import(inner, friend, name, remote_id, remote_modified).await;
    inner.busy.lock().expect("busy lock poisoned").remove(&key);
    let mut failed = inner.failed.lock().expect("failed lock poisoned");
    match &result {
        Ok(()) => {
            failed.remove(&key);
        }
        Err(error) => {
            failed.insert(key, Failure { remote_modified, error: error.clone() });
        }
    }
    result
}

async fn fetch_and_import(inner: &Arc<Inner>, friend: &Friend, name: &str, remote_id: u32, remote_modified: Option<u64>) -> Result<(), String> {
    let local = local_name(&friend.id, name);
    let is_copy = inner.imports.lock().expect("imports lock poisoned").get(&friend.id, name).is_some();
    if !is_copy && inner.state.databases.read().expect("databases lock poisoned").find_by_name(&local).is_some() {
        return Err(format!("a database named {local:?} exists here and isn't a copy made by the mesh"));
    }
    let url = format!("{}/api/v1/databases/{remote_id}/export", friend.api.trim_end_matches('/'));
    let response = inner.http.get(&url).send().await.and_then(|r| r.error_for_status()).map_err(|e| format!("fetching {url}: {e}"))?;
    let bytes = response.bytes().await.map_err(|e| format!("fetching {url}: {e}"))?;
    let state = inner.state.clone();
    let imported = tokio::task::spawn_blocking(move || state.import_database_bytes_as(&bytes, &local, true)).await.map_err(|e| e.to_string())?;
    imported.map_err(|e| format!("importing: {}", e.message))?;
    inner.imports.lock().expect("imports lock poisoned").put(Record {
        friend: friend.id.clone(),
        database: name.to_string(),
        local_name: local_name(&friend.id, name),
        remote_modified,
        imported_at: now(),
    });
    Ok(())
}

/// The mesh as the REST API sees it.
struct Control(Arc<Inner>);

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Mesh({})", self.0.config.node)
    }
}

impl MeshControl for Control {
    fn status(&self) -> MeshDto {
        let inner = &self.0;
        let mut peers: Vec<Peer> = inner.peers.lock().expect("peers lock poisoned").values().cloned().collect();
        peers.sort_by(|a, b| a.friend.id.cmp(&b.friend.id));
        let friends = peers
            .into_iter()
            .map(|p| MeshFriend {
                databases: p.catalog.iter().flat_map(|c| &c.databases).map(|db| describe(inner, &p.friend.id, db)).collect(),
                id: p.friend.id,
                mqtt: p.friend.mqtt,
                api: p.friend.api,
                connected: p.connected,
            })
            .collect();
        MeshDto { node: inner.config.node.clone(), friends }
    }

    fn import<'a>(&'a self, friend: &'a str, database: &'a str) -> Pin<Box<dyn Future<Output = Result<MeshDatabase, MeshError>> + Send + 'a>> {
        Box::pin(async move {
            let inner = &self.0;
            let peer = inner.peers.lock().expect("peers lock poisoned").get(friend).cloned().ok_or_else(|| MeshError::NotFound(format!("no friend {friend:?}")))?;
            let reply = ask(inner, friend, database).await.map_err(MeshError::Failed)?;
            let (Some(remote_id), true) = (reply.db_id, reply.found) else {
                return Err(MeshError::NotFound(format!("{friend} has no database {database:?}")));
            };
            transfer(inner, &peer.friend, database, remote_id, reply.modified).await.map_err(MeshError::Failed)?;
            // What the friend answered is newer than its last catalog; show that.
            let announced = CatalogDatabase { id: remote_id, name: database.to_string(), modified: reply.modified, cubes: Vec::new() };
            let known = peer.catalog.and_then(|c| c.databases.into_iter().find(|d| d.name == database)).map_or(announced, |d| CatalogDatabase { modified: reply.modified, ..d });
            Ok(describe(inner, friend, &known))
        })
    }
}
