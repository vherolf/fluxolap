//! What a node tells its friends it has: its databases and their normal
//! cubes, each with the time it last changed. A friend compares those times
//! with the ones it stored at its last import to decide whether to fetch
//! again (never with its own clock, so clock skew between nodes is harmless).

use fluxolap_domain::cube::CUBE_TYPE_NORMAL;
use fluxolap_engine::AppState;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// The node's id, as the friends service knows it.
    pub node: String,
    /// Base URL of the node's REST API, where a friend fetches databases
    /// (`GET {api}/api/v1/databases/{id}/export`).
    pub api: String,
    pub databases: Vec<CatalogDatabase>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogDatabase {
    /// The id in the node's REST API (the `{db}` of its URLs).
    pub id: u32,
    pub name: String,
    /// Unix seconds of the last change to any cube's cells or any
    /// dimension; `None` while the database isn't loaded (unknown).
    pub modified: Option<u64>,
    pub cubes: Vec<CatalogCube>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogCube {
    pub name: String,
    pub modified: Option<u64>,
    pub cells: usize,
}

/// The catalog of `state`. Reads only what is already in memory: a database
/// that isn't loaded is listed without cubes and stays unloaded. Databases
/// named in `skip` are left out: the copies of friends' databases, which are
/// not offered on (each node gets a database from where it was calculated).
pub fn build(state: &AppState, node: &str, api: &str, skip: &HashSet<String>) -> Catalog {
    let registry = state.databases.read().expect("databases lock poisoned");
    let loaded = state.loaded.read().expect("loaded lock poisoned");
    let mut databases = Vec::new();
    for id in registry.list_ids() {
        let Some(db) = registry.get(id).filter(|db| !skip.contains(&db.name)) else { continue };
        let mut entry = CatalogDatabase { id: db.wire_id, name: db.name.clone(), modified: None, cubes: Vec::new() };
        if let Some(ld) = loaded.get(&id) {
            let dimensions = ld.dimensions.read().expect("dimensions lock poisoned");
            let cubes = ld.cubes.read().expect("cubes lock poisoned");
            let mut modified = dimensions.list_in_database(id).map(|d| d.modified()).max().unwrap_or(0);
            for (cube_id, cube) in cubes.list_in_database(id).filter(|(_, c)| c.cube_type == CUBE_TYPE_NORMAL) {
                let cube_modified = ld.storage.modified(cube_id);
                modified = modified.max(cube_modified);
                entry.cubes.push(CatalogCube { name: cube.name.clone(), modified: (cube_modified != 0).then_some(cube_modified), cells: ld.storage.filled_count(cube_id) });
            }
            entry.cubes.sort_by(|a, b| a.name.cmp(&b.name));
            entry.modified = (modified != 0).then_some(modified);
        }
        databases.push(entry);
    }
    databases.sort_by_key(|d| d.id);
    Catalog { node: node.to_string(), api: api.to_string(), databases }
}
