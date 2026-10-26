use crate::dto::{DatabaseCreate, DatabaseDto, DatabaseList, DatabaseUpdate, NameQuery, PageQuery, ServerDto};
use crate::error::{ApiResult, Json, Path, Query};
use crate::resolve::internal;
use crate::PREFIX;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use fluxolap_engine::{ops, provision, AppState};
use fluxolap_error::FluxolapError;

/// Server name and version.
#[utoipa::path(get, path = "/api/v1", tag = "server", responses((status = 200, body = ServerDto)))]
pub async fn server() -> axum::Json<ServerDto> {
    axum::Json(ServerDto { name: "fluxolap".to_string(), version: env!("CARGO_PKG_VERSION").to_string() })
}

/// Lists databases.
#[utoipa::path(get, path = "/api/v1/databases", tag = "databases", params(NameQuery, PageQuery), responses((status = 200, body = DatabaseList)))]
pub async fn list(State(state): State<AppState>, Query(name): Query<NameQuery>, Query(page): Query<PageQuery>) -> axum::Json<DatabaseList> {
    let databases = state.databases.read().expect("databases lock poisoned");
    let mut items: Vec<DatabaseDto> = databases.list().filter(|db| name.name.as_ref().is_none_or(|n| *n == db.name)).map(DatabaseDto::new).collect();
    items.sort_by_key(|d| d.id);
    let (items, total) = page.apply(items);
    axum::Json(DatabaseList { items, total })
}

/// Creates a database, with its `#_DIMENSION_` system dimension.
#[utoipa::path(post, path = "/api/v1/databases", tag = "databases", request_body = DatabaseCreate, responses(
    (status = 201, body = DatabaseDto, headers(("Location" = String))),
    (status = 409, body = crate::error::Problem, content_type = "application/problem+json"),
    (status = 422, body = crate::error::Problem, content_type = "application/problem+json"),
))]
pub async fn create(State(state): State<AppState>, Json(req): Json<DatabaseCreate>) -> ApiResult<impl IntoResponse> {
    let id = provision::create_database(&state, &req.name, req.db_type.raw())?;
    let databases = state.databases.read().expect("databases lock poisoned");
    let dto = DatabaseDto::new(databases.get(id).expect("just created"));
    let location = format!("{PREFIX}/databases/{}", dto.id);
    Ok((StatusCode::CREATED, [(header::LOCATION, location)], axum::Json(dto)))
}

/// Gets one database.
#[utoipa::path(get, path = "/api/v1/databases/{db}", tag = "databases", params(("db" = u32, Path, description = "Database id")), responses(
    (status = 200, body = DatabaseDto),
    (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
))]
pub async fn get(State(state): State<AppState>, Path(db): Path<u32>) -> ApiResult<axum::Json<DatabaseDto>> {
    let databases = state.databases.read().expect("databases lock poisoned");
    let id = databases.find_by_wire_id(db).ok_or_else(FluxolapError::database_not_found)?;
    Ok(axum::Json(DatabaseDto::new(databases.get(id).expect("just found"))))
}

/// Renames a database.
#[utoipa::path(patch, path = "/api/v1/databases/{db}", tag = "databases", params(("db" = u32, Path, description = "Database id")), request_body = DatabaseUpdate, responses(
    (status = 200, body = DatabaseDto),
    (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
    (status = 409, body = crate::error::Problem, content_type = "application/problem+json"),
))]
pub async fn update(State(state): State<AppState>, Path(db): Path<u32>, Json(req): Json<DatabaseUpdate>) -> ApiResult<axum::Json<DatabaseDto>> {
    let mut databases = state.databases.write().expect("databases lock poisoned");
    let id = databases.find_by_wire_id(db).ok_or_else(FluxolapError::database_not_found)?;
    if let Some(name) = &req.name {
        databases.rename(id, name).map_err(ops::map_database_error)?;
    }
    let dto = DatabaseDto::new(databases.get(id).expect("just found"));
    drop(databases);
    if req.name.is_some() {
        state.save_registry().map_err(internal)?;
    }
    Ok(axum::Json(dto))
}

/// Deletes a database and its data file.
#[utoipa::path(delete, path = "/api/v1/databases/{db}", tag = "databases", params(("db" = u32, Path, description = "Database id")), responses(
    (status = 204),
    (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
))]
pub async fn delete(State(state): State<AppState>, Path(db): Path<u32>) -> ApiResult<StatusCode> {
    let mut databases = state.databases.write().expect("databases lock poisoned");
    let id = databases.find_by_wire_id(db).ok_or_else(FluxolapError::database_not_found)?;
    databases.destroy(id).map_err(ops::map_database_error)?;
    drop(databases);
    state.forget(id, db);
    state.save_registry().map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

// Only documents the import/export bodies as a binary file in the OpenAPI spec.
/// A fluxolap database file (`.bin.zst`), as raw bytes.
#[derive(utoipa::ToSchema)]
#[schema(value_type = String, format = Binary)]
pub struct DatabaseFile(#[allow(dead_code)] Vec<u8>);

/// Imports a database file (as written by `GET .../export`, or a shipped
/// example) under the name it carries. The request body is the raw file.
#[utoipa::path(post, path = "/api/v1/databases/import", tag = "databases",
    request_body(content = DatabaseFile, content_type = "application/octet-stream", description = "A fluxolap database file (`.bin.zst`)"),
    responses(
        (status = 201, body = DatabaseDto, headers(("Location" = String))),
        (status = 409, body = crate::error::Problem, content_type = "application/problem+json", description = "A database with the file's name exists"),
        (status = 422, body = crate::error::Problem, content_type = "application/problem+json", description = "Not a fluxolap database file"),
    ))]
pub async fn import(State(state): State<AppState>, body: axum::body::Bytes) -> ApiResult<impl IntoResponse> {
    let id = state.import_database_bytes(&body)?;
    let databases = state.databases.read().expect("databases lock poisoned");
    let dto = DatabaseDto::new(databases.get(id).expect("just imported"));
    let location = format!("{PREFIX}/databases/{}", dto.id);
    Ok((StatusCode::CREATED, [(header::LOCATION, location)], axum::Json(dto)))
}

/// Downloads a database as a file that `POST /databases/import` (or the
/// server's `FLUXOLAP_EXAMPLES` / data directory) can load again.
#[utoipa::path(get, path = "/api/v1/databases/{db}/export", tag = "databases", params(("db" = u32, Path, description = "Database id")),
    responses(
        (status = 200, content_type = "application/octet-stream", body = DatabaseFile, description = "The database file (`.bin.zst`)"),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn export(State(state): State<AppState>, Path(db): Path<u32>) -> ApiResult<impl IntoResponse> {
    let (id, name) = {
        let databases = state.databases.read().expect("databases lock poisoned");
        let id = databases.find_by_wire_id(db).ok_or_else(FluxolapError::database_not_found)?;
        (id, databases.get(id).expect("just found").name.clone())
    };
    let bytes = state.export_database_bytes(id)?;
    let file_name: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{file_name}.bin.zst\"")),
        ],
        bytes,
    ))
}
