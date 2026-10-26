//! Locks: journal every write to an area so it can be rolled back. There
//! is no authentication, so a lock doesn't keep other clients from
//! writing -- it only records what they wrote.

use crate::dto::{LockCreate, LockDto, LockList, RollbackQuery};
use crate::error::{ApiResult, Json, Path, Query};
use crate::resolve::{self, cube_id, database};
use crate::PREFIX;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use fluxolap_engine::{ops, AppState};
use fluxolap_storage::areas_overlap;
use fluxolap_error::FluxolapError;

/// Lists the cube's locks.
#[utoipa::path(get, path = "/api/v1/databases/{db}/cubes/{cube}/locks", tag = "locks",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id")),
    responses((status = 200, body = LockList), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn list(State(state): State<AppState>, Path((db, cube)): Path<(u32, u32)>) -> ApiResult<axum::Json<LockList>> {
    let db = database(&state, db)?;
    let dimensions = db.loaded.dimensions.read().expect("dimensions lock poisoned");
    let cubes = db.loaded.cubes.read().expect("cubes lock poisoned");
    let id = cube_id(&cubes, db.id, cube)?;
    let cube = cubes.get(id).expect("just found");
    let items: Vec<LockDto> = db.loaded.storage.locks_for(id).iter().map(|l| LockDto::new(&dimensions, cube, l)).collect();
    Ok(axum::Json(LockList { total: items.len(), items }))
}

/// Locks an area (or, without `area`, the whole cube). Refused (409) if
/// it overlaps an existing lock.
#[utoipa::path(post, path = "/api/v1/databases/{db}/cubes/{cube}/locks", tag = "locks",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id")), request_body = LockCreate,
    responses(
        (status = 201, body = LockDto, headers(("Location" = String))),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 409, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn create(State(state): State<AppState>, Path((db_wire, cube_wire)): Path<(u32, u32)>, Json(req): Json<LockCreate>) -> ApiResult<impl IntoResponse> {
    let db = database(&state, db_wire)?;
    let dimensions = db.loaded.dimensions.read().expect("dimensions lock poisoned");
    let cubes = db.loaded.cubes.read().expect("cubes lock poisoned");
    let id = cube_id(&cubes, db.id, cube_wire)?;
    let cube = cubes.get(id).expect("just found");
    let (whole, raw_area, leaves) = match &req.area {
        None => (true, Vec::new(), Vec::new()),
        Some(area) => {
            let per_dim = resolve::area(&dimensions, cube, area)?;
            let leaves = ops::lock_overlap_leaves(&dimensions, cube, &per_dim);
            (false, per_dim, leaves)
        }
    };
    if db.loaded.storage.locks_for(id).iter().any(|l| areas_overlap(l.whole, &l.overlap_leaves, whole, &leaves)) {
        return Err(FluxolapError::cube_wrong_lock().into());
    }
    let lock_id = db.loaded.storage.create_lock(id, whole, raw_area, leaves);
    let dto = LockDto::new(&dimensions, cube, &db.loaded.storage.find_lock(id, lock_id).expect("just created"));
    db.loaded.mark_dirty();
    let location = format!("{PREFIX}/databases/{db_wire}/cubes/{cube_wire}/locks/{lock_id}");
    Ok((StatusCode::CREATED, [(header::LOCATION, location)], axum::Json(dto)))
}

/// Gets one lock.
#[utoipa::path(get, path = "/api/v1/databases/{db}/cubes/{cube}/locks/{lock}", tag = "locks",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id"), ("lock" = u32, Path, description = "Lock id")),
    responses((status = 200, body = LockDto), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn get(State(state): State<AppState>, Path((db, cube, lock)): Path<(u32, u32, u32)>) -> ApiResult<axum::Json<LockDto>> {
    let db = database(&state, db)?;
    let dimensions = db.loaded.dimensions.read().expect("dimensions lock poisoned");
    let cubes = db.loaded.cubes.read().expect("cubes lock poisoned");
    let id = cube_id(&cubes, db.id, cube)?;
    let found = db.loaded.storage.find_lock(id, lock).ok_or_else(FluxolapError::cube_lock_not_found)?;
    Ok(axum::Json(LockDto::new(&dimensions, cubes.get(id).expect("just found"), &found)))
}

/// Keeps the writes made under the lock and releases it.
#[utoipa::path(post, path = "/api/v1/databases/{db}/cubes/{cube}/locks/{lock}/commit", tag = "locks",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id"), ("lock" = u32, Path, description = "Lock id")),
    responses((status = 204), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn commit(State(state): State<AppState>, Path((db, cube, lock)): Path<(u32, u32, u32)>) -> ApiResult<StatusCode> {
    let db = database(&state, db)?;
    let id = cube_id(&db.loaded.cubes.read().expect("cubes lock poisoned"), db.id, cube)?;
    db.loaded.storage.commit_lock(id, lock).map_err(|_| FluxolapError::cube_lock_not_found())?;
    db.loaded.mark_dirty();
    Ok(StatusCode::NO_CONTENT)
}

/// Undoes the last `steps` writes made under the lock, or all of them
/// (releasing the lock) without `steps`. A whole-cube lock can only be
/// committed (409).
#[utoipa::path(post, path = "/api/v1/databases/{db}/cubes/{cube}/locks/{lock}/rollback", tag = "locks",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id"), ("lock" = u32, Path, description = "Lock id"), RollbackQuery),
    responses(
        (status = 204),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 409, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn rollback(State(state): State<AppState>, Path((db, cube, lock)): Path<(u32, u32, u32)>, Query(q): Query<RollbackQuery>) -> ApiResult<StatusCode> {
    let db = database(&state, db)?;
    let id = cube_id(&db.loaded.cubes.read().expect("cubes lock poisoned"), db.id, cube)?;
    let found = db.loaded.storage.find_lock(id, lock).ok_or_else(FluxolapError::cube_lock_not_found)?;
    if found.whole {
        return Err(FluxolapError::cube_blocked_by_lock("a whole-cube lock can't be rolled back, only committed").into());
    }
    db.loaded.storage.rollback_lock(id, lock, q.steps).map_err(|_| FluxolapError::cube_lock_not_found())?;
    db.loaded.mark_dirty();
    Ok(StatusCode::NO_CONTENT)
}
