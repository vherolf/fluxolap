//! Cells, batch-first. A cell is addressed by its path: one element per
//! cube dimension (id or name). A path containing a consolidated element
//! reads as the weighted sum of its base cells and, when written,
//! splashes the value down to them.
//!
//! - `PUT /cells` writes many cells, in order, with consecutive base-cell
//!   writes stored in one storage call.
//! - `POST /cells/query` reads many cells (explicit paths or an area).
//! - `GET /cells` pages through the stored base cells.
//! - `GET`/`PUT /cells/{path}` read/write one cell, path as `3,17,5`.

use crate::dto::{CellDto, CellExportQuery, CellKind, CellList, CellPage, CellQuery, CellValueWrite, CellWriteBatch, Value};
use crate::error::{ApiError, ApiResult, Json, Path, Query};
use crate::resolve::{self, cube_id, database, wire_path};
use axum::extract::State;
use axum::http::StatusCode;
use fluxolap_domain::ElementId;
use fluxolap_engine::{ops, AppState};
use fluxolap_storage::CellValue;
use fluxolap_error::FluxolapError;

const DEFAULT_EXPORT_LIMIT: usize = 1000;
const MAX_EXPORT_LIMIT: usize = 100_000;

/// Opens the cube `$cube` of database `$db` for the rest of the handler:
/// binds the loaded database, read guards on its dimensions and cubes,
/// and the resolved cube id and cube.
macro_rules! open_cube {
    ($state:expr, $db:expr, $cube:expr => $loaded:ident, $dimensions:ident, $cube_id:ident, $cube_ref:ident) => {
        let db = database(&$state, $db)?;
        let $loaded = db.loaded;
        let $dimensions = $loaded.dimensions.read().expect("dimensions lock poisoned");
        let cubes = $loaded.cubes.read().expect("cubes lock poisoned");
        let $cube_id = cube_id(&cubes, db.id, $cube)?;
        let $cube_ref = cubes.get($cube_id).expect("just found");
    };
}

fn cell_dto(wire: Vec<u32>, value: Option<CellValue>) -> CellDto {
    CellDto { path: wire, value: value.map(Value::from_cell) }
}

/// Pages through the cube's stored base cells.
#[utoipa::path(get, path = "/api/v1/databases/{db}/cubes/{cube}/cells", tag = "cells",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id"), CellExportQuery),
    responses((status = 200, body = CellPage), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn export(State(state): State<AppState>, Path((db, cube)): Path<(u32, u32)>, Query(q): Query<CellExportQuery>) -> ApiResult<axum::Json<CellPage>> {
    open_cube!(state, db, cube => loaded, dimensions, cube_id, cube);
    let limit = q.limit.unwrap_or(DEFAULT_EXPORT_LIMIT).clamp(1, MAX_EXPORT_LIMIT);
    let mut items = Vec::with_capacity(limit.min(1024));
    let mut skipped = 0;
    let mut more = false;
    loaded.storage.scan_in_order(cube_id, &[], |path, value| {
        match (q.kind, value) {
            (Some(CellKind::Numeric), CellValue::String(_)) | (Some(CellKind::String), CellValue::Numeric(_)) => return true,
            _ => {}
        }
        let Some(wire) = wire_path(&dimensions, cube, path) else { return true };
        if skipped < q.offset {
            skipped += 1;
            return true;
        }
        if items.len() == limit {
            more = true;
            return false;
        }
        items.push(cell_dto(wire, Some(value.clone())));
        true
    });
    Ok(axum::Json(CellPage { items, next_offset: more.then_some(q.offset + limit) }))
}

/// Reads many cells: either `paths` (one element list per cell) or an
/// `area` (one element list per dimension, read as their cartesian
/// product). Results are in request order; with `skip_empty`, cells
/// without a value are left out.
#[utoipa::path(post, path = "/api/v1/databases/{db}/cubes/{cube}/cells/query", tag = "cells",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id")), request_body = CellQuery,
    responses(
        (status = 200, body = CellList),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 422, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn query(State(state): State<AppState>, Path((db, cube)): Path<(u32, u32)>, Json(q): Json<CellQuery>) -> ApiResult<axum::Json<CellList>> {
    open_cube!(state, db, cube => loaded, dimensions, cube_id, cube);
    let paths: Vec<Vec<ElementId>> = match (q.paths, q.area) {
        (Some(paths), None) => {
            if paths.len() > ops::MAX_AREA_CELLS {
                return Err(FluxolapError::max_cell_reached().into());
            }
            paths.iter().enumerate().map(|(i, p)| resolve::path(&dimensions, cube, p).map_err(|e| e.at(i))).collect::<ApiResult<_>>()?
        }
        (None, Some(area)) => {
            let per_dim = resolve::area(&dimensions, cube, &area)?;
            if ops::product_size(&per_dim).is_none_or(|n| n > ops::MAX_AREA_CELLS) {
                return Err(FluxolapError::max_cell_reached().into());
            }
            ops::cartesian_product(&per_dim)
        }
        _ => return Err(ApiError::unprocessable("give either `paths` or `area`")),
    };
    let values = ops::read_cells(&dimensions, &loaded.storage, cube_id, cube, &paths);
    let items: Vec<CellDto> = paths
        .iter()
        .zip(values)
        .filter(|(_, v)| !(q.skip_empty && v.is_none()))
        .map(|(p, v)| cell_dto(wire_path(&dimensions, cube, p).expect("path elements were just resolved"), v))
        .collect();
    Ok(axum::Json(CellList { total: items.len(), items }))
}

/// Writes many cells, in order. `null` clears a cell; at a consolidated
/// path a number splashes down to the base cells and `null` clears them
/// all. Every path is checked before anything is written; a splash
/// refused for its size (422, with `index`) leaves the items before it
/// written.
#[utoipa::path(put, path = "/api/v1/databases/{db}/cubes/{cube}/cells", tag = "cells",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id")), request_body = CellWriteBatch,
    responses(
        (status = 204),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 422, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn write_many(State(state): State<AppState>, Path((db, cube)): Path<(u32, u32)>, Json(batch): Json<CellWriteBatch>) -> ApiResult<StatusCode> {
    open_cube!(state, db, cube => loaded, dimensions, cube_id, cube);
    let entries = batch
        .items
        .into_iter()
        .enumerate()
        .map(|(i, w)| Ok((resolve::path(&dimensions, cube, &w.path).map_err(|e| e.at(i))?, Value::to_cell(w.value))))
        .collect::<ApiResult<Vec<_>>>()?;
    let result = ops::write_cells(&dimensions, &loaded.storage, cube_id, cube, entries);
    loaded.mark_dirty();
    result.map_err(|(i, e)| ApiError::from(e).at(i))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Clears every cell of the cube.
#[utoipa::path(delete, path = "/api/v1/databases/{db}/cubes/{cube}/cells", tag = "cells",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id")),
    responses((status = 204), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn clear(State(state): State<AppState>, Path((db, cube)): Path<(u32, u32)>) -> ApiResult<StatusCode> {
    open_cube!(state, db, cube => loaded, _dimensions, cube_id, _cube);
    loaded.storage.clear_cube(cube_id);
    loaded.mark_dirty();
    Ok(StatusCode::NO_CONTENT)
}

/// Reads one cell. `path` is comma-separated element ids (or names; a
/// number is always an id).
#[utoipa::path(get, path = "/api/v1/databases/{db}/cubes/{cube}/cells/{path}", tag = "cells",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id"), ("path" = String, Path, description = "Element per dimension, comma-separated, e.g. `3,17,5` or `North,Jan`. A number is always an element id; to address an element whose name is a number (a year, say), use `POST .../cells/query`, where `\"2025\"` is a name and `2025` an id.")),
    responses(
        (status = 200, body = CellDto),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 422, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn get(State(state): State<AppState>, Path((db, cube, path)): Path<(u32, u32, String)>) -> ApiResult<axum::Json<CellDto>> {
    open_cube!(state, db, cube => loaded, dimensions, cube_id, cube);
    let path = resolve::path(&dimensions, cube, &resolve::path_segment(&path))?;
    let value = ops::read_cell(&dimensions, &loaded.storage, cube_id, cube, &path);
    Ok(axum::Json(cell_dto(wire_path(&dimensions, cube, &path).expect("path elements were just resolved"), value)))
}

/// Writes one cell and returns its new value.
#[utoipa::path(put, path = "/api/v1/databases/{db}/cubes/{cube}/cells/{path}", tag = "cells",
    params(("db" = u32, Path, description = "Database id"), ("cube" = u32, Path, description = "Cube id"), ("path" = String, Path, description = "Element per dimension, comma-separated, e.g. `3,17,5` or `North,Jan`. A number is always an element id; to address an element whose name is a number (a year, say), use `POST .../cells/query`, where `\"2025\"` is a name and `2025` an id.")),
    request_body = CellValueWrite,
    responses(
        (status = 200, body = CellDto),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 422, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn put(State(state): State<AppState>, Path((db, cube, path)): Path<(u32, u32, String)>, Json(req): Json<CellValueWrite>) -> ApiResult<axum::Json<CellDto>> {
    open_cube!(state, db, cube => loaded, dimensions, cube_id, cube);
    let path = resolve::path(&dimensions, cube, &resolve::path_segment(&path))?;
    ops::write_cells(&dimensions, &loaded.storage, cube_id, cube, vec![(path.clone(), Value::to_cell(req.value))]).map_err(|(_, e)| ApiError::from(e))?;
    loaded.mark_dirty();
    let value = ops::read_cell(&dimensions, &loaded.storage, cube_id, cube, &path);
    Ok(axum::Json(cell_dto(wire_path(&dimensions, cube, &path).expect("path elements were just resolved"), value)))
}
