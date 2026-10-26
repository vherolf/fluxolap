use crate::dto::{CubeCreate, CubeDto, CubeList, CubeQuery, CubeUpdate, PageQuery};
use crate::error::{ApiResult, Json, Path, Query};
use crate::resolve::{cube_id, database, dimension_ref};
use crate::PREFIX;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use fluxolap_engine::{ops, AppState};

/// Lists a database's cubes.
#[utoipa::path(get, path = "/api/v1/databases/{db}/cubes", tag = "cubes",
    params(("db" = u32, Path, description = "Database id"), CubeQuery, PageQuery),
    responses((status = 200, body = CubeList), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn list(State(state): State<AppState>, Path(db): Path<u32>, Query(q): Query<CubeQuery>, Query(page): Query<PageQuery>) -> ApiResult<axum::Json<CubeList>> {
    let db = database(&state, db)?;
    let dimensions = db.loaded.dimensions.read().expect("dimensions lock poisoned");
    let cubes = db.loaded.cubes.read().expect("cubes lock poisoned");
    let dimension = q.dimension.map(|wire| dimensions.find_by_wire_id(db.id, wire));
    let mut items: Vec<CubeDto> = cubes
        .list_in_database(db.id)
        .filter(|(_, c)| q.name.as_ref().is_none_or(|n| *n == c.name))
        .filter(|(_, c)| q.cube_type.is_none_or(|t| t.cube_raw() == c.cube_type))
        .filter(|(_, c)| dimension.is_none_or(|d| d.is_some_and(|d| c.dimensions.contains(&d))))
        .map(|(id, c)| CubeDto::new(&dimensions, c, db.loaded.storage.filled_count(id), db.loaded.storage.modified(id)))
        .collect();
    items.sort_by_key(|c| c.id);
    let (items, total) = page.apply(items);
    Ok(axum::Json(CubeList { items, total }))
}

/// Creates a cube over the given dimensions.
#[utoipa::path(post, path = "/api/v1/databases/{db}/cubes", tag = "cubes",
    params(("db" = u32, Path, description = "Database id")), request_body = CubeCreate,
    responses(
        (status = 201, body = CubeDto, headers(("Location" = String))),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 409, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 422, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn create(State(state): State<AppState>, Path(db_wire): Path<u32>, Json(req): Json<CubeCreate>) -> ApiResult<impl IntoResponse> {
    let db = database(&state, db_wire)?;
    let dimensions = db.loaded.dimensions.read().expect("dimensions lock poisoned");
    let dim_ids = req.dimensions.iter().enumerate().map(|(i, r)| dimension_ref(&dimensions, db.id, r).map_err(|e| e.at(i))).collect::<ApiResult<Vec<_>>>()?;
    let n_dims = dim_ids.len();
    let mut cubes = db.loaded.cubes.write().expect("cubes lock poisoned");
    let id = cubes.create(db.id, &req.name, dim_ids, req.cube_type.cube_raw()).map_err(ops::map_cube_error)?;
    db.loaded.storage.cube_created(id, n_dims);
    let dto = CubeDto::new(&dimensions, cubes.get(id).expect("just created"), 0, 0);
    drop(cubes);
    db.loaded.mark_dirty();
    let location = format!("{PREFIX}/databases/{db_wire}/cubes/{}", dto.id);
    Ok((StatusCode::CREATED, [(header::LOCATION, location)], axum::Json(dto)))
}

/// Gets one cube.
#[utoipa::path(get, path = "/api/v1/databases/{db}/cubes/{cube}", tag = "cubes",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id")),
    responses((status = 200, body = CubeDto), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn get(State(state): State<AppState>, Path((db, cube)): Path<(u32, u32)>) -> ApiResult<axum::Json<CubeDto>> {
    let db = database(&state, db)?;
    let dimensions = db.loaded.dimensions.read().expect("dimensions lock poisoned");
    let cubes = db.loaded.cubes.read().expect("cubes lock poisoned");
    let id = cube_id(&cubes, db.id, cube)?;
    Ok(axum::Json(CubeDto::new(&dimensions, cubes.get(id).expect("just found"), db.loaded.storage.filled_count(id), db.loaded.storage.modified(id))))
}

/// Renames a cube.
#[utoipa::path(patch, path = "/api/v1/databases/{db}/cubes/{cube}", tag = "cubes",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id")), request_body = CubeUpdate,
    responses(
        (status = 200, body = CubeDto),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 409, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn update(State(state): State<AppState>, Path((db, cube)): Path<(u32, u32)>, Json(req): Json<CubeUpdate>) -> ApiResult<axum::Json<CubeDto>> {
    let db = database(&state, db)?;
    let dimensions = db.loaded.dimensions.read().expect("dimensions lock poisoned");
    let mut cubes = db.loaded.cubes.write().expect("cubes lock poisoned");
    let id = cube_id(&cubes, db.id, cube)?;
    if let Some(name) = &req.name {
        cubes.rename(id, name).map_err(ops::map_cube_error)?;
        db.loaded.mark_dirty();
    }
    Ok(axum::Json(CubeDto::new(&dimensions, cubes.get(id).expect("just found"), db.loaded.storage.filled_count(id), db.loaded.storage.modified(id))))
}

/// Deletes a cube and its cells.
#[utoipa::path(delete, path = "/api/v1/databases/{db}/cubes/{cube}", tag = "cubes",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id")),
    responses((status = 204), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn delete(State(state): State<AppState>, Path((db, cube)): Path<(u32, u32)>) -> ApiResult<StatusCode> {
    let db = database(&state, db)?;
    let mut cubes = db.loaded.cubes.write().expect("cubes lock poisoned");
    let id = cube_id(&cubes, db.id, cube)?;
    cubes.destroy(id).map_err(ops::map_cube_error)?;
    db.loaded.storage.cube_destroyed(id);
    drop(cubes);
    db.loaded.mark_dirty();
    Ok(StatusCode::NO_CONTENT)
}
