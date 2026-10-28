//! fluxolap's REST API: JSON resources under `/api/v1`, errors as RFC 9457
//! problem details, an OpenAPI 3.1 spec at `/api/v1/openapi.json` and
//! Swagger UI at `/api/v1/docs` (served from the binary, works offline).
//!
//! Resources nest as databases -> dimensions -> elements and databases ->
//! cubes -> cells/locks, each addressed by its numeric id. Collections
//! use `GET` to list (`{"items": [...], "total": n}`), `POST` to create
//! (201 + `Location`), and single resources `GET`/`PATCH`/`DELETE` (204).
//! Elements and cells are batch-first -- see `handlers::elements` and
//! `handlers::cells`. All engine logic lives in `fluxolap-engine`; this
//! crate only maps HTTP onto it.

mod docs;
pub use docs::SWAGGER_UI_VERSION;
pub use fluxolap_api_types as dto;
pub mod error;
mod handlers;
/// The statistics of `GET /api/v1/stats` without HTTP, for a host that embeds the engine.
pub use handlers::stats::collect_stats;
mod resolve;

use axum::extract::DefaultBodyLimit;
use axum::response::Redirect;
use axum::routing::{get, post};
use axum::Router;
use fluxolap_engine::AppState;
use handlers::{cells, cubes, databases, dimensions, elements, examples, locks, mesh, stats};
pub use handlers::mesh::{MeshControl, MeshError};
use utoipa::OpenApi;

pub const PREFIX: &str = "/api/v1";

/// Largest accepted request body: batch writes of hundreds of thousands
/// of cells or elements are the normal case, far past axum's 2 MB default.
pub const MAX_BODY_BYTES: usize = 512 * 1024 * 1024;

#[derive(OpenApi)]
#[openapi(
    info(title = "fluxolap", description = "REST API for the fluxolap OLAP engine."),
    paths(
        databases::server, stats::stats,
        databases::list, databases::create, databases::get, databases::update, databases::delete, databases::import, databases::export,
        dimensions::list, dimensions::create, dimensions::get, dimensions::update, dimensions::delete,
        elements::list, elements::create, elements::upsert, elements::update_many, elements::delete_many, elements::clear,
        elements::get, elements::update, elements::delete,
        cubes::list, cubes::create, cubes::get, cubes::update, cubes::delete,
        cells::export, cells::query, cells::write_many, cells::clear, cells::get, cells::put,
        locks::list, locks::create, locks::get, locks::commit, locks::rollback,
        examples::list, examples::import,
        mesh::status, mesh::import,
    ),
    components(schemas(error::Problem)),
    tags(
        (name = "server"),
        (name = "databases"),
        (name = "dimensions", description = "A normal dimension comes with an attribute dimension and cube (`#_<name>_`) and an element in the `#_DIMENSION_` system dimension; they are renamed and deleted with it."),
        (name = "elements", description = "Batch-first: `POST` (create), `PUT` (upsert by name), `PATCH` (update) on the collection and `POST .../elements/delete` work on many elements at once, all-or-nothing."),
        (name = "cubes"),
        (name = "cells", description = "Batch-first: `PUT /cells` writes and `POST /cells/query` reads many cells per request."),
        (name = "locks"),
        (name = "mesh", description = "Friends exchanging database catalogs over MQTT, see docs/mesh.md. 404 unless the server runs with the mesh."),
        (name = "examples", description = "The example database files the server offers to import (see `FLUXOLAP_EXAMPLES`), for clients that can't upload a file from the server's disk."),
    )
)]
pub struct ApiDoc;

/// Optional server behavior, all off by default.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// A running mesh (`fluxolap-mesh`) for `/api/v1/mesh`; without one those endpoints answer 404.
    pub mesh: Option<std::sync::Arc<dyn MeshControl>>,
    /// Which browser origins may call the API from another site (CORS).
    pub cors: Cors,
    /// A directory with a static web app (e.g. fluxoscope's build) to
    /// serve at `/ui`; `/` then redirects there instead of to the docs.
    pub ui_dir: Option<std::path::PathBuf>,
    /// Directories whose database files `GET /examples` offers to import.
    pub examples_dirs: Vec<std::path::PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Cors {
    /// No CORS headers: only same-origin pages can call the API.
    #[default]
    Off,
    /// Any origin (`*`).
    Any,
    /// These origins, e.g. `http://localhost:8080`.
    Origins(Vec<String>),
}

impl Cors {
    /// From `FLUXOLAP_CORS_ORIGINS`'s format: empty is off, `*` any origin,
    /// otherwise a comma-separated list of origins.
    pub fn parse(value: &str) -> Self {
        let value = value.trim();
        if value.is_empty() {
            Cors::Off
        } else if value == "*" {
            Cors::Any
        } else {
            Cors::Origins(value.split(',').map(|o| o.trim().trim_end_matches('/').to_string()).filter(|o| !o.is_empty()).collect())
        }
    }
}

/// The API with default options (no CORS, no web app).
pub fn router(state: AppState) -> Router {
    router_with(state, Options::default())
}

pub fn router_with(state: AppState, options: Options) -> Router {
    let db = "/api/v1/databases/:db";
    let api = Router::new()
        .route("/api/v1", get(databases::server))
        .route("/api/v1/openapi.json", get(|| async { axum::Json(ApiDoc::openapi()) }))
        .route("/api/v1/docs", get(docs::swagger_ui))
        .route("/api/v1/docs/:version/:file", get(docs::asset))
        .route("/api/v1/databases", get(databases::list).post(databases::create))
        .route("/api/v1/databases/import", post(databases::import))
        .route(db, get(databases::get).patch(databases::update).delete(databases::delete))
        .route(&format!("{db}/export"), get(databases::export))
        .route(&format!("{db}/dimensions"), get(dimensions::list).post(dimensions::create))
        .route(&format!("{db}/dimensions/:dim"), get(dimensions::get).patch(dimensions::update).delete(dimensions::delete))
        .route(
            &format!("{db}/dimensions/:dim/elements"),
            get(elements::list).post(elements::create).put(elements::upsert).patch(elements::update_many).delete(elements::clear),
        )
        .route(&format!("{db}/dimensions/:dim/elements/delete"), post(elements::delete_many))
        .route(&format!("{db}/dimensions/:dim/elements/:el"), get(elements::get).patch(elements::update).delete(elements::delete))
        .route(&format!("{db}/cubes"), get(cubes::list).post(cubes::create))
        .route(&format!("{db}/cubes/:cube"), get(cubes::get).patch(cubes::update).delete(cubes::delete))
        .route(&format!("{db}/cubes/:cube/cells"), get(cells::export).put(cells::write_many).delete(cells::clear))
        .route(&format!("{db}/cubes/:cube/cells/query"), post(cells::query))
        .route(&format!("{db}/cubes/:cube/cells/:path"), get(cells::get).put(cells::put))
        .route(&format!("{db}/cubes/:cube/locks"), get(locks::list).post(locks::create))
        .route(&format!("{db}/cubes/:cube/locks/:lock"), get(locks::get))
        .route(&format!("{db}/cubes/:cube/locks/:lock/commit"), post(locks::commit))
        .route(&format!("{db}/cubes/:cube/locks/:lock/rollback"), post(locks::rollback))
        .route("/api/v1/stats", get(stats::stats))
        .route("/api/v1/examples", get(examples::list))
        .route("/api/v1/examples/:file/import", post(examples::import))
        .route("/api/v1/mesh", get(mesh::status))
        .route("/api/v1/mesh/friends/:friend/databases/:database/import", post(mesh::import));

    let home = if options.ui_dir.is_some() { "/ui/" } else { "/api/v1/docs" };
    let mut app = api.route("/", get(move || async move { Redirect::temporary(home) }));
    if let Some(dir) = options.ui_dir {
        app = app
            .route("/ui", get(|| async { Redirect::permanent("/ui/") }))
            .nest_service("/ui/", tower_http::services::ServeDir::new(dir).append_index_html_on_directories(true));
    }
    let metrics = std::sync::Arc::new(stats::Metrics::default());
    let tracked = metrics.clone();
    let app = app
        .fallback(|| async { error::ApiError::route_not_found() })
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::Extension(examples::ExampleDirs(std::sync::Arc::new(options.examples_dirs))))
        .layer(axum::Extension(mesh::MeshExt(options.mesh)))
        .layer(axum::Extension(metrics))
        .layer(axum::middleware::from_fn(move |request, next| stats::track(tracked.clone(), request, next)))
        .with_state(state);
    match cors_layer(&options.cors) {
        Some(layer) => app.layer(layer),
        None => app,
    }
}

fn cors_layer(cors: &Cors) -> Option<tower_http::cors::CorsLayer> {
    use axum::http::{header, HeaderValue};
    use tower_http::cors::{AllowOrigin, Any, CorsLayer};
    let origin = match cors {
        Cors::Off => return None,
        Cors::Any => AllowOrigin::any(),
        Cors::Origins(list) => AllowOrigin::list(list.iter().filter_map(|o| HeaderValue::from_str(o).ok())),
    };
    // Location (after creates) and Content-Disposition (exports) must be
    // readable by the calling page too.
    Some(CorsLayer::new().allow_origin(origin).allow_methods(Any).allow_headers(Any).expose_headers([header::LOCATION, header::CONTENT_DISPOSITION]))
}
