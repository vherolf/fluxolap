use fluxolap_engine::AppState;
use std::path::PathBuf;

/// Port defaults to 7777, the port OLAP clients traditionally expect.
/// Override with `FLUXOLAP_PORT`.
fn listen_port() -> u16 {
    std::env::var("FLUXOLAP_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(7777)
}

/// Persistence: if set, the server loads its database registry plus
/// every registered database from this directory on startup, and saves
/// every loaded database (plus the registry) back to it on clean shutdown
/// (SIGINT/SIGTERM). Unset means fully in-memory. Persistence is
/// per-database (one file per database plus a registry file, all under
/// this directory) -- see `fluxolap_engine::state`'s doc comments for the
/// full design.
///
/// Unlike an embedding application (which can load databases on demand
/// and evict them on an idle timeout), this
/// standalone server has no idle-unload concept at all, so it simply
/// loads every registered database eagerly at startup.
fn data_dir_path() -> Option<PathBuf> {
    std::env::var_os("FLUXOLAP_DATA_DIR").map(PathBuf::from)
}

/// The web app (fluxoscope) to serve at `/ui`: `FLUXOLAP_UI_DIR` if set
/// (set but empty turns it off), otherwise the `ui/` folder next to the
/// fluxolap binary -- where the release tarball puts fluxoscope.
fn ui_dir() -> Option<PathBuf> {
    match std::env::var_os("FLUXOLAP_UI_DIR") {
        Some(dir) if dir.is_empty() => None,
        Some(dir) => Some(PathBuf::from(dir)),
        None => {
            let beside = std::env::current_exe().ok()?.parent()?.join("ui");
            beside.join("index.html").is_file().then_some(beside)
        }
    }
}

/// Where the example databases the API offers to import (`GET
/// /api/v1/examples`) are looked for: `FLUXOLAP_EXAMPLES`, `examples/` next
/// to the fluxolap binary (the release tarball), the Docker image's
/// examples and `target/examples` (a source checkout's build).
fn examples_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("FLUXOLAP_EXAMPLES").filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        dirs.push(exe_dir.join("examples"));
    }
    dirs.push("/usr/share/fluxolap/examples".into());
    dirs.push("target/examples".into());
    dirs
}

/// `FLUXOLAP_EXAMPLES`: a directory of per-database files (the shipped
/// example databases, built by `cargo run -p xtask -- examples`) to import
/// at startup. Each file becomes a database under its own name unless one
/// with that name already exists, so importing on every start is harmless;
/// the directory itself is only read. Unset or empty: nothing is imported.
fn import_examples(state: &AppState) {
    let Some(dir) = std::env::var_os("FLUXOLAP_EXAMPLES").filter(|d| !d.is_empty()).map(PathBuf::from) else { return };
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(e) => {
            tracing::error!("can't read FLUXOLAP_EXAMPLES directory {}: {e}", dir.display());
            return;
        }
    };
    files.retain(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".bin.zst") && n != "registry.bin.zst"));
    files.sort();
    for file in files {
        match state.import_database_file(&file) {
            Ok(Some(_)) => tracing::info!("imported example database from {}", file.display()),
            Ok(None) => tracing::info!("skipped {}: a database with its name already exists", file.display()),
            Err(e) => tracing::error!("failed to import {}: {e}", file.display()),
        }
    }
}

/// What runs beside the REST API when the server is built with `--features
/// mesh` (docs/mesh.md): the mesh, if `FLUXOLAP_MESH_NODE` is set, and the
/// sensor ingest, if `FLUXOLAP_SENSOR_LISTEN` is. The handles have to stay
/// alive; the mesh's control goes into the API's options.
struct Extras {
    _running: Vec<Box<dyn std::any::Any>>,
    mesh: Option<std::sync::Arc<dyn fluxolap_api::MeshControl>>,
}

#[cfg(feature = "mesh")]
fn start_extras(state: &AppState, data_dir: Option<&std::path::Path>) -> Extras {
    let mut running: Vec<Box<dyn std::any::Any>> = Vec::new();
    let mut control = None;
    match fluxolap_mesh::MeshConfig::from_env(data_dir) {
        Ok(Some(config)) => {
            let node = config.node.clone();
            let mesh = fluxolap_mesh::Mesh::start(state.clone(), config).unwrap_or_else(|e| panic!("mesh: {e}"));
            tracing::info!("mesh node {node} started");
            control = Some(mesh.control());
            running.push(Box::new(mesh));
        }
        Ok(None) => {}
        Err(e) => panic!("mesh: {e}"),
    }
    match fluxolap_mesh::sensor::SensorConfig::from_env() {
        Ok(Some(config)) => {
            tracing::info!("sensor ingest: MQTT on {}, writing through {}", config.listen, config.api);
            running.push(Box::new(fluxolap_mesh::sensor::start(config).unwrap_or_else(|e| panic!("sensor: {e}"))));
        }
        Ok(None) => {}
        Err(e) => panic!("sensor: {e}"),
    }
    Extras { _running: running, mesh: control }
}

#[cfg(not(feature = "mesh"))]
fn start_extras(_: &AppState, _: Option<&std::path::Path>) -> Extras {
    for var in ["FLUXOLAP_MESH_NODE", "FLUXOLAP_SENSOR_LISTEN"] {
        if std::env::var_os(var).is_some() {
            tracing::warn!("{var} is set, but this build has no mesh or sensor ingest (build with --features mesh)");
        }
    }
    Extras { _running: Vec::new(), mesh: None }
}

/// Resolves on Ctrl-C, SIGTERM, or `AppState::shutdown` being notified,
/// whichever comes first -- drives `axum::serve`'s graceful shutdown, so
/// the same save-state-if-configured sequence runs no matter which of the
/// three triggered it. Unix-only is fine -- Windows is an explicit
/// non-goal (see PLAN.md).
async fn shutdown_signal(state: AppState) {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("failed to install Ctrl-C handler");
    };

    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    let requested = state.shutdown.notified();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
        _ = requested => {},
    }
}

#[tokio::main]
async fn main() {
    // An explicit level rather than `fmt::init()`: the mesh's broker enables
    // tracing-subscriber's `env-filter`, which would make `init()` log errors only.
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

    let data_dir = data_dir_path();

    let state = match &data_dir {
        Some(dir) => match AppState::load_from_dir(dir) {
            Ok(state) => {
                let databases = state.databases.read().expect("databases lock poisoned");
                let ids: Vec<_> = databases.list_ids().collect();
                let count = ids.len();
                for id in ids {
                    if let Err(e) = state.resolve_loaded(&databases, id) {
                        tracing::error!("failed to load database {id:?} from {}: {e} -- leaving it empty", dir.display());
                    }
                }
                drop(databases);
                tracing::info!("loaded {count} database(s) from {}", dir.display());
                state
            }
            Err(e) => {
                tracing::error!("failed to load registry from {}: {e} -- starting fresh", dir.display());
                AppState::new()
            }
        },
        None => AppState::new(),
    };
    import_examples(&state);
    let extras = start_extras(&state, data_dir.as_deref());

    let port = listen_port();
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| panic!("failed to bind 0.0.0.0:{port}: {e}"));
    tracing::info!("fluxolap listening on 0.0.0.0:{port} (REST API at /api/v1, docs at /api/v1/docs)");
    let options = fluxolap_api::Options {
        cors: fluxolap_api::Cors::parse(&std::env::var("FLUXOLAP_CORS_ORIGINS").unwrap_or_default()),
        ui_dir: ui_dir(),
        examples_dirs: examples_dirs(),
        mesh: extras.mesh.clone(),
    };
    if options.cors != fluxolap_api::Cors::Off {
        tracing::info!("CORS enabled for {:?}", options.cors);
    }
    if let Some(dir) = &options.ui_dir {
        tracing::info!("serving the web app in {} at /ui", dir.display());
    }
    axum::serve(listener, fluxolap_api::router_with(state.clone(), options))
        .with_graceful_shutdown(shutdown_signal(state.clone()))
        .await
        .expect("server error");

    if data_dir.is_some() {
        match state.save_all() {
            Ok(()) => tracing::info!("saved state"),
            Err(e) => tracing::error!("failed to save state: {e}"),
        }
    }
}
