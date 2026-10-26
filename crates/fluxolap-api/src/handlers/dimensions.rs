use crate::dto::{DimensionCreate, DimensionDto, DimensionList, DimensionQuery, DimensionUpdate, PageQuery};
use crate::error::{ApiResult, Json, Path, Query};
use crate::resolve::{database, dimension_id};
use crate::PREFIX;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use fluxolap_engine::{provision, AppState};

/// Lists a database's dimensions, including the automatic `system` and
/// `attribute` ones unless filtered by `type`.
#[utoipa::path(get, path = "/api/v1/databases/{db}/dimensions", tag = "dimensions",
    params(("db" = u32, Path, description = "Database id"), DimensionQuery, PageQuery),
    responses((status = 200, body = DimensionList), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn list(State(state): State<AppState>, Path(db): Path<u32>, Query(q): Query<DimensionQuery>, Query(page): Query<PageQuery>) -> ApiResult<axum::Json<DimensionList>> {
    let db = database(&state, db)?;
    let dimensions = db.loaded.dimensions.read().expect("dimensions lock poisoned");
    let mut items: Vec<DimensionDto> = dimensions
        .list_in_database(db.id)
        .filter(|d| q.name.as_ref().is_none_or(|n| *n == d.name))
        .filter(|d| q.dim_type.is_none_or(|t| t.dimension_raw() == d.dim_type))
        .map(DimensionDto::new)
        .collect();
    items.sort_by_key(|d| d.id);
    let (items, total) = page.apply(items);
    Ok(axum::Json(DimensionList { items, total }))
}

/// Creates a dimension. A `normal` dimension gets an attribute dimension
/// and cube (`#_<name>_`) and an element in the `#_DIMENSION_` system
/// dimension.
#[utoipa::path(post, path = "/api/v1/databases/{db}/dimensions", tag = "dimensions",
    params(("db" = u32, Path, description = "Database id")), request_body = DimensionCreate,
    responses(
        (status = 201, body = DimensionDto, headers(("Location" = String))),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 409, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn create(State(state): State<AppState>, Path(db_wire): Path<u32>, Json(req): Json<DimensionCreate>) -> ApiResult<impl IntoResponse> {
    let db = database(&state, db_wire)?;
    let id = provision::create_dimension(&db.loaded, db.id, &req.name, req.dim_type.dimension_raw())?;
    let dimensions = db.loaded.dimensions.read().expect("dimensions lock poisoned");
    let dto = DimensionDto::new(dimensions.get(id).expect("just created"));
    let location = format!("{PREFIX}/databases/{db_wire}/dimensions/{}", dto.id);
    Ok((StatusCode::CREATED, [(header::LOCATION, location)], axum::Json(dto)))
}

/// Gets one dimension.
#[utoipa::path(get, path = "/api/v1/databases/{db}/dimensions/{dim}", tag = "dimensions",
    params(("db" = u32, Path, description = "Database id"), ("dim" = u32, Path, description = "Dimension id")),
    responses((status = 200, body = DimensionDto), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn get(State(state): State<AppState>, Path((db, dim)): Path<(u32, u32)>) -> ApiResult<axum::Json<DimensionDto>> {
    let db = database(&state, db)?;
    let dimensions = db.loaded.dimensions.read().expect("dimensions lock poisoned");
    let id = dimension_id(&dimensions, db.id, dim)?;
    Ok(axum::Json(DimensionDto::new(dimensions.get(id).expect("just found"))))
}

/// Renames a dimension, along with its attribute dimension/cube and its
/// system element.
#[utoipa::path(patch, path = "/api/v1/databases/{db}/dimensions/{dim}", tag = "dimensions",
    params(("db" = u32, Path, description = "Database id"), ("dim" = u32, Path, description = "Dimension id")), request_body = DimensionUpdate,
    responses(
        (status = 200, body = DimensionDto),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 409, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn update(State(state): State<AppState>, Path((db, dim)): Path<(u32, u32)>, Json(req): Json<DimensionUpdate>) -> ApiResult<axum::Json<DimensionDto>> {
    let db = database(&state, db)?;
    let id = dimension_id(&db.loaded.dimensions.read().expect("dimensions lock poisoned"), db.id, dim)?;
    if let Some(name) = &req.name {
        provision::rename_dimension(&db.loaded, db.id, id, name)?;
    }
    let dimensions = db.loaded.dimensions.read().expect("dimensions lock poisoned");
    Ok(axum::Json(DimensionDto::new(dimensions.get(id).expect("just found"))))
}

/// Deletes a dimension with its attribute dimension/cube and system
/// element. Refused (409) while another cube uses it.
#[utoipa::path(delete, path = "/api/v1/databases/{db}/dimensions/{dim}", tag = "dimensions",
    params(("db" = u32, Path, description = "Database id"), ("dim" = u32, Path, description = "Dimension id")),
    responses(
        (status = 204),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 409, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn delete(State(state): State<AppState>, Path((db, dim)): Path<(u32, u32)>) -> ApiResult<StatusCode> {
    let db = database(&state, db)?;
    let id = dimension_id(&db.loaded.dimensions.read().expect("dimensions lock poisoned"), db.id, dim)?;
    provision::destroy_dimension(&db.loaded, db.id, id)?;
    Ok(StatusCode::NO_CONTENT)
}
