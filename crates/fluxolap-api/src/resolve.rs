//! Id/name resolution shared by the handlers. Lock order everywhere:
//! the database registry, then a database's `dimensions`, then its
//! `cubes` -- the order `fluxolap_engine::provision` takes them in.

use crate::dto::{ChildRef, Ref};
use crate::error::{ApiError, ApiResult};
use fluxolap_domain::cube::{Cube, CubeTable};
use fluxolap_domain::dimension::{Dimension, DimensionTable};
use fluxolap_domain::{CubeId, DatabaseId, DimensionId, ElementId};
use fluxolap_engine::{AppState, LoadedDatabase};
use fluxolap_error::FluxolapError;
use std::sync::Arc;

/// A database resolved from its wire id, loaded.
pub struct Db {
    pub id: DatabaseId,
    pub loaded: Arc<LoadedDatabase>,
}

pub fn database(state: &AppState, wire_id: u32) -> ApiResult<Db> {
    let databases = state.databases.read().expect("databases lock poisoned");
    let id = databases.find_by_wire_id(wire_id).ok_or_else(FluxolapError::database_not_found)?;
    let loaded = state.resolve_loaded(&databases, id)?;
    Ok(Db { id, loaded })
}

pub fn dimension_id(dimensions: &DimensionTable, database_id: DatabaseId, wire_id: u32) -> ApiResult<DimensionId> {
    Ok(dimensions.find_by_wire_id(database_id, wire_id).ok_or_else(FluxolapError::dimension_not_found)?)
}

pub fn dimension_ref(dimensions: &DimensionTable, database_id: DatabaseId, r: &Ref) -> ApiResult<DimensionId> {
    let found = match r {
        Ref::Id(id) => dimensions.find_by_wire_id(database_id, *id),
        Ref::Name(name) => dimensions.find_by_name(database_id, name),
    };
    found.ok_or_else(|| FluxolapError::new(fluxolap_error::ErrorCode::DIMENSION_NOT_FOUND, "dimension not found", format!("dimension {r} not found")).into())
}

pub fn cube_id(cubes: &CubeTable, database_id: DatabaseId, wire_id: u32) -> ApiResult<CubeId> {
    Ok(cubes.find_by_wire_id(database_id, wire_id).ok_or_else(FluxolapError::cube_not_found)?)
}

pub fn element_id(dim: &Dimension, wire_id: u32) -> ApiResult<ElementId> {
    Ok(dim.find_element_by_wire_id(wire_id).ok_or_else(FluxolapError::element_not_found)?)
}

pub fn element_ref(dim: &Dimension, r: &Ref) -> ApiResult<ElementId> {
    let found = match r {
        Ref::Id(id) => dim.find_element_by_wire_id(*id),
        Ref::Name(name) => dim.find_element_by_name(name),
    };
    found.ok_or_else(|| FluxolapError::new(fluxolap_error::ErrorCode::ELEMENT_NOT_FOUND, "element not found", format!("element {r} not found")).into())
}

pub fn children(dim: &Dimension, children: &[ChildRef]) -> ApiResult<Vec<(ElementId, f64)>> {
    children.iter().map(|c| Ok((element_ref(dim, &c.element)?, c.weight))).collect()
}

fn coordinate(dimensions: &DimensionTable, cube: &Cube, d: usize, r: &Ref) -> ApiResult<ElementId> {
    let dim = dimensions.get(cube.dimensions[d]).expect("cube references a live dimension");
    element_ref(dim, r).map_err(|_| FluxolapError::invalid_coordinates(format!("element {r} not found in dimension '{}'", dim.name)).into())
}

fn check_len(cube: &Cube, len: usize, what: &str) -> ApiResult<()> {
    if len != cube.dimensions.len() {
        return Err(FluxolapError::invalid_coordinates(format!("{what} has {len} entries, the cube has {} dimensions", cube.dimensions.len())).into());
    }
    Ok(())
}

/// One element per cube dimension -> an engine cell path.
pub fn path(dimensions: &DimensionTable, cube: &Cube, refs: &[Ref]) -> ApiResult<Vec<ElementId>> {
    check_len(cube, refs.len(), "path")?;
    refs.iter().enumerate().map(|(d, r)| coordinate(dimensions, cube, d, r)).collect()
}

/// One element list per cube dimension -> per-dimension element ids.
pub fn area(dimensions: &DimensionTable, cube: &Cube, groups: &[Vec<Ref>]) -> ApiResult<Vec<Vec<ElementId>>> {
    check_len(cube, groups.len(), "area")?;
    groups.iter().enumerate().map(|(d, refs)| refs.iter().map(|r| coordinate(dimensions, cube, d, r)).collect()).collect()
}

/// A path segment like `3,17,5`: comma-separated element ids, or names
/// for entries that aren't numbers. A number is always taken as an id, so
/// an element named `2025` can only be addressed by name through JSON.
pub fn path_segment(segment: &str) -> Vec<Ref> {
    segment.split(',').map(|part| part.parse().map_or_else(|_| Ref::Name(part.to_string()), Ref::Id)).collect()
}

/// An engine cell path -> element wire ids. `None` if an element is gone
/// (cells left behind by an element deleted before deletes cleared them).
pub fn wire_path(dimensions: &DimensionTable, cube: &Cube, path: &[ElementId]) -> Option<Vec<u32>> {
    path.iter().zip(&cube.dimensions).map(|(&eid, &did)| dimensions.get(did)?.get_element(eid).map(|e| e.wire_id)).collect()
}

pub fn internal(e: impl std::fmt::Display) -> ApiError {
    FluxolapError::internal(e.to_string()).into()
}
