//! Nodes with real brokers and real REST APIs on localhost, and a friends service.

use fluxolap_api::Options;
use fluxolap_api_types::{MeshDto, MeshState};
use fluxolap_domain::cube::CUBE_TYPE_NORMAL;
use fluxolap_domain::database::DB_TYPE_NORMAL;
use fluxolap_domain::dimension::DIM_TYPE_NORMAL;
use fluxolap_engine::provision::{create_database, create_dimension};
use fluxolap_engine::AppState;
use fluxolap_mesh::friends::Friend;
use fluxolap_mesh::policy::Rules;
use fluxolap_mesh::{Mesh, MeshConfig, Peer};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A database with one dimension and one normal cube over it.
fn add_database(state: &AppState, name: &str, cube: &str) {
    let id = create_database(state, name, DB_TYPE_NORMAL).unwrap();
    let databases = state.databases.read().unwrap();
    let loaded = state.resolve_loaded(&databases, id).unwrap();
    let dim = create_dimension(&loaded, id, "Region", DIM_TYPE_NORMAL).unwrap();
    let mut cubes = loaded.cubes.write().unwrap();
    let cube_id = cubes.create(id, cube, vec![dim], CUBE_TYPE_NORMAL).unwrap();
    loaded.storage.cube_created(cube_id, 1);
}

fn add_dimension(state: &AppState, database: &str, name: &str) {
    let databases = state.databases.read().unwrap();
    let id = databases.find_by_name(database).unwrap();
    let loaded = state.resolve_loaded(&databases, id).unwrap();
    create_dimension(&loaded, id, name, DIM_TYPE_NORMAL).unwrap();
}

/// The names of a database's dimensions, `None` while it doesn't exist (or isn't loaded) here.
fn dimension_names(state: &AppState, database: &str) -> Option<Vec<String>> {
    let id = state.databases.read().unwrap().find_by_name(database)?;
    let loaded = state.loaded.read().unwrap().get(&id).cloned()?;
    let dimensions = loaded.dimensions.read().unwrap();
    let mut names: Vec<String> = dimensions.list_in_database(id).map(|d| d.name.clone()).collect();
    names.sort();
    Some(names)
}

/// A node that has its ports but isn't running yet: the friends service has to know all of them first.
struct Prepared {
    friend: Friend,
    api: TcpListener,
}

async fn prepare(id: &str) -> Prepared {
    let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let friend = Friend { id: id.into(), mqtt: format!("127.0.0.1:{}", free_port()), api: format!("http://{}", api.local_addr().unwrap()) };
    Prepared { friend, api }
}

async fn friends_service(nodes: &[&Prepared]) -> String {
    let service = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/friends", service.local_addr().unwrap());
    let nodes: Vec<Friend> = nodes.iter().map(|n| n.friend.clone()).collect();
    tokio::spawn(async move { axum::serve(service, fluxolap_mesh::friends::router(nodes)).await.unwrap() });
    url
}

/// Starts the node's mesh and serves its REST API, with the mesh plugged in.
fn launch(node: Prepared, state: &AppState, import: &str, friends_url: &str) -> Mesh {
    let mesh = Mesh::start(
        state.clone(),
        MeshConfig {
            node: node.friend.id.clone(),
            listen: node.friend.mqtt.parse().unwrap(),
            api: node.friend.api.clone(),
            friends_url: friends_url.into(),
            friends_token: None,
            poll: Duration::from_secs(1),
            import: Rules::parse(import).unwrap(),
            state_file: None,
        },
    )
    .unwrap();
    let router = fluxolap_api::router_with(state.clone(), Options { mesh: Some(mesh.control()), ..Default::default() });
    tokio::spawn(async move { axum::serve(node.api, router).await.unwrap() });
    mesh
}

async fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn database_names(peer: &Peer) -> Vec<String> {
    peer.catalog.as_ref().map(|c| c.databases.iter().map(|d| d.name.clone()).collect()).unwrap_or_default()
}

fn friend_state(mesh: &Mesh, friend: &str, database: &str) -> Option<MeshState> {
    let status = status(mesh);
    status.friends.iter().find(|f| f.id == friend)?.databases.iter().find(|d| d.name == database).map(|d| d.state)
}

fn status(mesh: &Mesh) -> MeshDto {
    mesh.control().status()
}

#[tokio::test(flavor = "multi_thread")]
async fn friends_see_each_others_catalogs_and_changes() {
    let (a, b) = (prepare("a").await, prepare("b").await);
    let friends_url = friends_service(&[&a, &b]).await;
    let b_friend = b.friend.clone();

    let (state_a, state_b) = (AppState::new(), AppState::new());
    add_database(&state_b, "Sales", "Revenue");
    let mesh_a = launch(a, &state_a, "", &friends_url);
    let mesh_b = launch(b, &state_b, "", &friends_url);

    // A sees B's database and its cube, with its API address for the later fetch.
    wait_for("a to see b's Sales", || mesh_a.peers().first().is_some_and(|p| p.connected && database_names(p) == ["Sales"])).await;
    let peers = mesh_a.peers();
    assert_eq!(peers.len(), 1, "a is not its own friend");
    assert_eq!(peers[0].friend, b_friend);
    let catalog = peers[0].catalog.clone().unwrap();
    assert_eq!((catalog.node.as_str(), catalog.api.as_str()), ("b", b_friend.api.as_str()));
    let cubes: Vec<_> = catalog.databases[0].cubes.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cubes, ["Revenue"], "only normal cubes are listed, not the attribute cubes");

    // B sees A, which has nothing yet.
    wait_for("b to see a", || mesh_b.peers().first().is_some_and(|p| p.connected && p.catalog.is_some())).await;
    assert!(database_names(&mesh_b.peers()[0]).is_empty());

    // A change after start reaches the friend without a restart, both ways.
    add_database(&state_b, "Costs", "Spend");
    add_database(&state_a, "Weather", "Readings");
    wait_for("a to see b's new database", || mesh_a.peers().first().is_some_and(|p| database_names(p) == ["Sales", "Costs"])).await;
    wait_for("b to see a's new database", || mesh_b.peers().first().is_some_and(|p| database_names(p) == ["Weather"])).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_friend_is_listed_but_not_connected() {
    let (a, ghost) = (prepare("a").await, prepare("ghost").await);
    let friends_url = friends_service(&[&a, &ghost]).await;
    let ghost_friend = ghost.friend.clone();
    drop(ghost);

    let mesh = launch(a, &AppState::new(), "", &friends_url);
    wait_for("the ghost to be listed", || !mesh.peers().is_empty()).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let peers = mesh.peers();
    assert_eq!(peers[0].friend, ghost_friend);
    assert!(!peers[0].connected && peers[0].catalog.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_auto_import_copies_a_friends_database_and_refreshes_it_only_when_newer() {
    let (a, b) = (prepare("a").await, prepare("b").await);
    let friends_url = friends_service(&[&a, &b]).await;
    let (state_a, state_b) = (AppState::new(), AppState::new());
    add_database(&state_b, "Sales", "Revenue");
    let mesh_a = launch(a, &state_a, "b/*=auto", &friends_url);
    let mesh_b = launch(b, &state_b, "", &friends_url);

    wait_for("the copy", || dimension_names(&state_a, "b/Sales").is_some_and(|d| d.contains(&"Region".to_string()))).await;
    wait_for("the state to be current", || friend_state(&mesh_a, "b", "Sales") == Some(MeshState::Current)).await;
    let copy = state_a.databases.read().unwrap().find_by_name("b/Sales").unwrap();
    let imported = status(&mesh_a).friends[0].databases[0].clone();
    assert_eq!((imported.local_name.as_str(), imported.imported_modified), ("b/Sales", imported.modified));

    // Not changed at the friend: not fetched again (the copy keeps its own contents).
    add_dimension(&state_a, "b/Sales", "LocalOnly");
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(dimension_names(&state_a, "b/Sales").unwrap().contains(&"LocalOnly".to_string()), "an unchanged friend database is not imported again");

    // Changed at the friend (times are whole seconds, so let one pass): the copy is replaced, under the same id.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    add_dimension(&state_b, "Sales", "Extra");
    wait_for("the refresh", || dimension_names(&state_a, "b/Sales").is_some_and(|d| d.contains(&"Extra".to_string()))).await;
    let names = dimension_names(&state_a, "b/Sales").unwrap();
    assert!(!names.contains(&"LocalOnly".to_string()), "the copy is the friend's database again: {names:?}");
    assert_eq!(state_a.databases.read().unwrap().find_by_name("b/Sales"), Some(copy), "the copy keeps its id");

    // Copies are not offered on: b doesn't see "b/Sales" in a's catalog, and a offers nothing else.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(database_names(&mesh_b.peers()[0]).is_empty(), "{:?}", database_names(&mesh_b.peers()[0]));
}

#[tokio::test(flavor = "multi_thread")]
async fn notify_only_shows_what_is_new_and_a_manual_import_fetches_it() {
    let (a, b) = (prepare("a").await, prepare("b").await);
    let a_api = a.friend.api.clone();
    let friends_url = friends_service(&[&a, &b]).await;
    let (state_a, state_b) = (AppState::new(), AppState::new());
    add_database(&state_b, "Sales", "Revenue");
    let mesh_a = launch(a, &state_a, "", &friends_url);
    let _mesh_b = launch(b, &state_b, "", &friends_url);

    wait_for("the announcement", || friend_state(&mesh_a, "b", "Sales") == Some(MeshState::New)).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(dimension_names(&state_a, "b/Sales").is_none(), "notify doesn't import");

    // Through the REST API of a.
    let http = reqwest::Client::new();
    let shown: MeshDto = http.get(format!("{a_api}/api/v1/mesh")).send().await.unwrap().json().await.unwrap();
    assert_eq!((shown.node.as_str(), shown.friends[0].id.as_str(), shown.friends[0].connected), ("a", "b", true));
    assert_eq!(shown.friends[0].databases[0].cubes[0].name, "Revenue");
    let imported = http.post(format!("{a_api}/api/v1/mesh/friends/b/databases/Sales/import")).send().await.unwrap();
    assert_eq!(imported.status(), 200, "{}", imported.text().await.unwrap());
    assert!(dimension_names(&state_a, "b/Sales").unwrap().contains(&"Region".to_string()));
    assert_eq!(friend_state(&mesh_a, "b", "Sales"), Some(MeshState::Current));

    // A change at the friend shows as an update, and is imported on request.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    add_dimension(&state_b, "Sales", "Extra");
    wait_for("the update to show", || friend_state(&mesh_a, "b", "Sales") == Some(MeshState::Update)).await;
    assert!(!dimension_names(&state_a, "b/Sales").unwrap().contains(&"Extra".to_string()));
    let again = http.post(format!("{a_api}/api/v1/mesh/friends/b/databases/Sales/import")).send().await.unwrap();
    assert_eq!(again.status(), 200);
    assert!(dimension_names(&state_a, "b/Sales").unwrap().contains(&"Extra".to_string()));
    assert_eq!(friend_state(&mesh_a, "b", "Sales"), Some(MeshState::Current));

    // What isn't there is a 404 with a problem.
    for path in ["friends/b/databases/Nope/import", "friends/nobody/databases/Sales/import"] {
        let r = http.post(format!("{a_api}/api/v1/mesh/{path}")).send().await.unwrap();
        assert_eq!(r.status(), 404, "{path}");
        assert_eq!(r.headers()["content-type"], "application/problem+json");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_database_of_the_same_name_that_is_not_a_copy_is_never_overwritten() {
    let (a, b) = (prepare("a").await, prepare("b").await);
    let friends_url = friends_service(&[&a, &b]).await;
    let (state_a, state_b) = (AppState::new(), AppState::new());
    add_database(&state_b, "Sales", "Revenue");
    add_database(&state_a, "b/Sales", "Mine");
    let mesh_a = launch(a, &state_a, "auto", &friends_url);
    let _mesh_b = launch(b, &state_b, "", &friends_url);

    wait_for("the failure to show", || status(&mesh_a).friends.first().and_then(|f| f.databases.first()).is_some_and(|d| d.error.is_some())).await;
    let error = status(&mesh_a).friends[0].databases[0].error.clone().unwrap();
    assert!(error.contains("isn't a copy"), "{error}");
    let id = state_a.databases.read().unwrap().find_by_name("b/Sales").unwrap();
    let loaded = state_a.loaded.read().unwrap().get(&id).cloned().unwrap();
    assert!(loaded.cubes.read().unwrap().find_by_name(id, "Mine").is_some(), "the local database is untouched");
}

#[tokio::test(flavor = "multi_thread")]
async fn sensor_messages_are_written_to_the_cube_through_the_rest_api() {
    let state = AppState::new();
    let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", api.local_addr().unwrap());
    let router = fluxolap_api::router(state.clone());
    tokio::spawn(async move { axum::serve(api, router).await.unwrap() });
    let http = reqwest::Client::new();
    let rest = |path: &str| format!("{api_url}/api/v1{path}");
    http.post(rest("/databases")).json(&json!({"name": "Plant"})).send().await.unwrap();
    let dim: Value = http.post(rest("/databases/0/dimensions")).json(&json!({"name": "Sensor"})).send().await.unwrap().json().await.unwrap();
    http.post(rest(&format!("/databases/0/dimensions/{}/elements", dim["id"]))).json(&json!({"items": [{"name": "t1"}]})).send().await.unwrap();
    // Not cube 0: the dimension came with an attribute cube.
    let cube: Value = http.post(rest("/databases/0/cubes")).json(&json!({"name": "Readings", "dimensions": ["Sensor"]})).send().await.unwrap().json().await.unwrap();
    let cube_id = cube["id"].clone();

    let listen = format!("127.0.0.1:{}", free_port());
    let _sensor = fluxolap_mesh::sensor::start(fluxolap_mesh::sensor::SensorConfig { listen: listen.parse().unwrap(), api: api_url.clone() }).unwrap();

    let mut options = rumqttc::MqttOptions::new("a-sensor", "127.0.0.1", listen.rsplit_once(':').unwrap().1.parse::<u16>().unwrap());
    options.set_keep_alive(Duration::from_secs(5));
    let (client, mut eventloop) = rumqttc::AsyncClient::new(options, 16);
    tokio::spawn(async move {
        loop {
            if eventloop.poll().await.is_err() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    });
    // The ingest may not be subscribed yet; the same reading, sent again, is harmless.
    let read = || async {
        let cells: Value = http.post(rest(&format!("/databases/0/cubes/{cube_id}/cells/query"))).json(&json!({"area": [["t1"]]})).send().await.unwrap().json().await.unwrap();
        cells["items"][0]["value"].clone()
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while read().await != json!(21.5) {
        assert!(Instant::now() < deadline, "the reading never arrived");
        client.publish("fluxolap/sensor/Plant/Readings/cells", rumqttc::QoS::AtLeastOnce, false, r#"{"items": [{"path": ["t1"], "value": 21.5}]}"#).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
