//! The mesh (friends exchanging database catalogs, `docs/mesh.md`) as REST
//! resources. The mesh itself lives in `fluxolap-mesh`; this crate only knows
//! the [`MeshControl`] it plugs in through `Options::mesh`, so the API doesn't
//! depend on MQTT. Without a mesh both endpoints answer 404.

use crate::dto::{MeshDatabase, MeshDto};
use crate::error::{ApiError, ApiResult, Path};
use axum::extract::Extension;
use axum::http::StatusCode;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Why a mesh request failed.
#[derive(Debug)]
pub enum MeshError {
    /// No such friend, or the friend has no such database.
    NotFound(String),
    /// The friend can't be reached, or the transfer failed.
    Failed(String),
}

/// What the REST API needs from a running mesh.
pub trait MeshControl: std::fmt::Debug + Send + Sync {
    /// The friends and their databases as they stand now.
    fn status(&self) -> MeshDto;
    /// Asks `friend` for its database `database` and imports it, whatever the policy says.
    fn import<'a>(&'a self, friend: &'a str, database: &'a str) -> Pin<Box<dyn Future<Output = Result<MeshDatabase, MeshError>> + Send + 'a>>;
}

/// The mesh the router was given, if any.
#[derive(Clone)]
pub struct MeshExt(pub Option<Arc<dyn MeshControl>>);

fn mesh(ext: &MeshExt) -> ApiResult<&Arc<dyn MeshControl>> {
    ext.0.as_ref().ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "mesh_disabled", "this server runs without the mesh (see docs/mesh.md)"))
}

/// The mesh: this node's friends and the databases they offer, each with how it compares with the copy here.
#[utoipa::path(get, path = "/api/v1/mesh", tag = "mesh", responses(
    (status = 200, body = MeshDto),
    (status = 404, body = crate::error::Problem, content_type = "application/problem+json", description = "The mesh is not enabled"),
))]
pub async fn status(Extension(ext): Extension<MeshExt>) -> ApiResult<axum::Json<MeshDto>> {
    Ok(axum::Json(mesh(&ext)?.status()))
}

/// Imports a friend's database now: the friend is asked over the mesh, the database is fetched from its REST API and replaces the copy here (`<friend>/<name>`) or creates it.
#[utoipa::path(post, path = "/api/v1/mesh/friends/{friend}/databases/{database}/import", tag = "mesh",
    params(("friend" = String, Path, description = "Friend id"), ("database" = String, Path, description = "The database's name at the friend")),
    responses(
        (status = 200, body = MeshDatabase),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json", description = "The mesh is not enabled, or no such friend or database"),
        (status = 502, body = crate::error::Problem, content_type = "application/problem+json", description = "The friend can't be reached or the transfer failed"),
    ))]
pub async fn import(Extension(ext): Extension<MeshExt>, Path((friend, database)): Path<(String, String)>) -> ApiResult<axum::Json<MeshDatabase>> {
    match mesh(&ext)?.import(&friend, &database).await {
        Ok(db) => Ok(axum::Json(db)),
        Err(MeshError::NotFound(detail)) => Err(ApiError::new(StatusCode::NOT_FOUND, "mesh_not_found", detail)),
        Err(MeshError::Failed(detail)) => Err(ApiError::new(StatusCode::BAD_GATEWAY, "mesh_import_failed", detail)),
    }
}
