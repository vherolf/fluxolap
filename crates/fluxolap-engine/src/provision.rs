//! Database/dimension creation with system/attribute
//! auto-provisioning. An API layer creates databases and dimensions
//! through `create_database` / `create_dimension` rather than the bare
//! domain tables, so the system and attribute dimensions and cubes are
//! always set up.

use crate::ops::{map_cube_error, map_database_error, map_dimension_error, map_element_error};
use crate::state::{AppState, LoadedDatabase};
use fluxolap_domain::cube::{CubeTable, CUBE_TYPE_ATTRIBUTE};
use fluxolap_domain::dimension::{DimensionTable, ElementType, DIM_TYPE_ATTRIBUTE, DIM_TYPE_NORMAL, DIM_TYPE_SYSTEM, DIM_TYPE_USER_INFO};
use fluxolap_domain::{CubeId, DatabaseId, DimensionId};
use fluxolap_error::FluxolapError;

/// Name of the database-wide system dimension the engine (and,
/// client-side, the embedded connector's `FluxolapDatabase`)
/// auto-provisions on database creation. Its elements are one per real
/// dimension in the database; metadata utilities (`DimensionMetadataUtil`
/// on the Java side) attach TYPE/INFO/MASTER attribute values directly
/// onto them, exactly like any other dimension's attributes.
pub const SYSTEM_DIMENSION_NAME: &str = "#_DIMENSION_";

/// Auto-provisions `dim_id`'s (named `dim_name`) attribute dimension +
/// cube -- the automatic attribute backing every normal dimension gets,
/// matching the embedded connector's client-side Java equivalent
/// (`FluxolapDatabase.provisionAttributeBacking`).
/// Both entities share the identical name `"#_<dim_name>_"` (distinct
/// namespaces: dimensions vs. cubes, matching the Java client's own
/// naming), and the attribute cube's dimension order is
/// `[attribute, target]` -- the same cell-path convention the Java
/// client's attribute read/write helpers use
/// (`[attributeElementWire, targetElementWire]`).
///
/// A genuine name collision (a dimension or cube already named
/// `"#_<dim_name>_"`) is reported as a normal error via the usual
/// `DuplicateName` mapping, not swallowed -- matches what the Java
/// client would do too.
pub fn provision_attribute_backing(
    dimensions: &mut DimensionTable,
    cubes: &mut CubeTable,
    database_id: DatabaseId,
    dim_id: DimensionId,
    dim_name: &str,
) -> Result<(), FluxolapError> {
    let attr_name = format!("#_{dim_name}_");
    let attr_dim_id = dimensions.create(database_id, &attr_name, DIM_TYPE_ATTRIBUTE).map_err(map_dimension_error)?;
    cubes.create(database_id, &attr_name, vec![attr_dim_id, dim_id], CUBE_TYPE_ATTRIBUTE).map_err(map_cube_error)?;
    Ok(())
}

/// Adds `dimension_name` as a numeric base element into the
/// always-present system dimension (`SYSTEM_DIMENSION_NAME`) -- mirrors
/// the real server's automatic behavior on dimension creation, and the
/// Java client's `FluxolapDatabase.registerInSystemDimension`. A no-op
/// if the system dimension doesn't exist yet in `dimensions` (can't
/// happen via `create_database`, which always provisions it first, but
/// kept defensive rather than asserting -- same posture as the Java side).
pub fn register_in_system_dimension(dimensions: &mut DimensionTable, database_id: DatabaseId, dimension_name: &str) {
    let Some(system_dim_id) = dimensions.find_by_name(database_id, SYSTEM_DIMENSION_NAME) else { return };
    let Some(system_dim) = dimensions.get_mut(system_dim_id) else { return };
    let _ = system_dim.create_element(dimension_name, ElementType::Numeric as u32);
}

/// Creates the database-wide system dimension (`SYSTEM_DIMENSION_NAME`)
/// -- called once, right after a database is created (see
/// `create_database`). Matches the Java client's
/// `FluxolapDatabase.provisionSystemDimension`. Unlike a user-created
/// normal dimension, this is never itself routed through
/// `register_in_system_dimension` (that would try to add an element for
/// it into itself, which doesn't exist yet) -- it *does* get attribute
/// backing of its own, though, matching the Java side exactly.
///
/// Can only fail if the system dimension (or its own attribute backing)
/// is somehow already present, which cannot happen on a database that
/// was truly just created -- propagated as a real error rather than
/// asserted, purely as defense in depth.
pub fn provision_system_dimension(loaded: &LoadedDatabase, database_id: DatabaseId) -> Result<(), FluxolapError> {
    let mut dimensions = loaded.dimensions.write().expect("dimensions lock poisoned");
    let mut cubes = loaded.cubes.write().expect("cubes lock poisoned");
    let system_dim_id = dimensions.create(database_id, SYSTEM_DIMENSION_NAME, DIM_TYPE_SYSTEM).map_err(map_dimension_error)?;
    provision_attribute_backing(&mut dimensions, &mut cubes, database_id, system_dim_id, SYSTEM_DIMENSION_NAME)
}

/// Creates and registers a new database, loads it, provisions its system
/// dimension (unconditionally, regardless of `db_type`, matching
/// `FluxolapConnection.addDatabase`), and persists the registry.
pub fn create_database(state: &AppState, name: &str, db_type: u32) -> Result<DatabaseId, FluxolapError> {
    let mut databases = state.databases.write().expect("databases lock poisoned");
    let id = databases.create(name, db_type).map_err(map_database_error)?;
    state.insert_new_loaded(id);
    let loaded = state.resolve_loaded(&databases, id)?;
    drop(databases);
    provision_system_dimension(&loaded, id)?;
    loaded.mark_dirty();
    state.save_registry().map_err(|e| FluxolapError::internal(e.to_string()))?;
    Ok(id)
}

/// Creates a dimension in an already-loaded database, auto-provisioned
/// like the Java client's `FluxolapDatabase.addDimension` does it: a `DIM_TYPE_NORMAL` or
/// `DIM_TYPE_USER_INFO` dimension is registered as an element of the
/// system dimension, plus -- `DIM_TYPE_NORMAL` only -- gets its own
/// attribute dimension/cube. A `DIM_TYPE_SYSTEM`/`DIM_TYPE_ATTRIBUTE`
/// dimension created directly gets neither, avoiding
/// attribute-on-attribute regress.
pub fn create_dimension(loaded: &LoadedDatabase, database_id: DatabaseId, name: &str, dim_type: u32) -> Result<DimensionId, FluxolapError> {
    let mut dimensions = loaded.dimensions.write().expect("dimensions lock poisoned");
    let id = dimensions.create(database_id, name, dim_type).map_err(map_dimension_error)?;
    if dim_type == DIM_TYPE_NORMAL || dim_type == DIM_TYPE_USER_INFO {
        if dim_type == DIM_TYPE_NORMAL {
            let mut cubes = loaded.cubes.write().expect("cubes lock poisoned");
            provision_attribute_backing(&mut dimensions, &mut cubes, database_id, id, name)?;
        }
        register_in_system_dimension(&mut dimensions, database_id, name);
    }
    drop(dimensions);
    loaded.mark_dirty();
    Ok(id)
}

/// The attribute dimension/cube pair `provision_attribute_backing` created
/// for `dim_id`, if it's still there: an attribute dimension named
/// `"#_<name>_"` and an attribute cube of the same name over
/// `[attribute, dim_id]`.
fn attribute_backing(dimensions: &DimensionTable, cubes: &CubeTable, database_id: DatabaseId, dim_id: DimensionId, dim_name: &str) -> (Option<DimensionId>, Option<CubeId>) {
    let attr_name = format!("#_{dim_name}_");
    let attr_dim = dimensions.find_by_name(database_id, &attr_name).filter(|&id| dimensions.get(id).is_some_and(|d| d.dim_type == DIM_TYPE_ATTRIBUTE));
    let attr_cube = cubes.find_by_name(database_id, &attr_name).filter(|&id| {
        cubes.get(id).is_some_and(|c| c.cube_type == CUBE_TYPE_ATTRIBUTE && c.dimensions.len() == 2 && c.dimensions[1] == dim_id && Some(c.dimensions[0]) == attr_dim)
    });
    (attr_dim, attr_cube)
}

/// Destroys a dimension together with what `create_dimension` provisioned
/// for it: its attribute cube (and that cube's cells) and attribute
/// dimension, and its element in the system dimension. Refused with
/// `DIMENSION_IN_USE` while any other cube still uses the dimension --
/// dropping it anyway would leave that cube pointing at nothing.
pub fn destroy_dimension(loaded: &LoadedDatabase, database_id: DatabaseId, dim_id: DimensionId) -> Result<(), FluxolapError> {
    let mut dimensions = loaded.dimensions.write().expect("dimensions lock poisoned");
    let mut cubes = loaded.cubes.write().expect("cubes lock poisoned");
    let name = dimensions.get(dim_id).ok_or_else(FluxolapError::dimension_not_found)?.name.clone();
    let (attr_dim, attr_cube) = attribute_backing(&dimensions, &cubes, database_id, dim_id, &name);

    if let Some((_, cube)) = cubes.list_in_database(database_id).find(|&(id, c)| Some(id) != attr_cube && c.dimensions.contains(&dim_id)) {
        return Err(FluxolapError::dimension_in_use(&cube.name));
    }
    if let Some(cube_id) = attr_cube {
        cubes.destroy(cube_id).map_err(map_cube_error)?;
        loaded.storage.cube_destroyed(cube_id);
    }
    if let Some(attr_dim_id) = attr_dim.filter(|&id| !cubes.list_in_database(database_id).any(|(_, c)| c.dimensions.contains(&id))) {
        dimensions.destroy(attr_dim_id).map_err(map_dimension_error)?;
    }
    dimensions.destroy(dim_id).map_err(map_dimension_error)?;
    if let Some(system_dim) = dimensions.find_by_name(database_id, SYSTEM_DIMENSION_NAME).and_then(|id| dimensions.get_mut(id)) {
        if let Some(element) = system_dim.find_element_by_name(&name) {
            let _ = system_dim.destroy_element(element);
        }
    }
    drop(cubes);
    drop(dimensions);
    loaded.mark_dirty();
    Ok(())
}

/// Renames a dimension and keeps what `create_dimension` provisioned for
/// it in step: the attribute dimension and cube become `"#_<new_name>_"`,
/// and its element in the system dimension is renamed too.
pub fn rename_dimension(loaded: &LoadedDatabase, database_id: DatabaseId, dim_id: DimensionId, new_name: &str) -> Result<(), FluxolapError> {
    let mut dimensions = loaded.dimensions.write().expect("dimensions lock poisoned");
    let mut cubes = loaded.cubes.write().expect("cubes lock poisoned");
    let old_name = dimensions.get(dim_id).ok_or_else(FluxolapError::dimension_not_found)?.name.clone();
    if old_name == new_name {
        return Ok(());
    }
    let (attr_dim, attr_cube) = attribute_backing(&dimensions, &cubes, database_id, dim_id, &old_name);

    dimensions.rename(dim_id, new_name).map_err(map_dimension_error)?;
    let new_attr_name = format!("#_{new_name}_");
    if let Some(id) = attr_dim {
        dimensions.rename(id, &new_attr_name).map_err(map_dimension_error)?;
    }
    if let Some(id) = attr_cube {
        cubes.rename(id, &new_attr_name).map_err(map_cube_error)?;
    }
    if let Some(system_dim) = dimensions.find_by_name(database_id, SYSTEM_DIMENSION_NAME).and_then(|id| dimensions.get_mut(id)) {
        if let Some(element) = system_dim.find_element_by_name(&old_name) {
            system_dim.rename_element(element, new_name).map_err(map_element_error)?;
        }
    }
    drop(cubes);
    drop(dimensions);
    loaded.mark_dirty();
    Ok(())
}
