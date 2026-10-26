//! System/attribute auto-provisioning (`fluxolap_engine::provision`): like
//! the embedded connector's client-side Java equivalent
//! (`FluxolapDatabase.provisionSystemDimension`/`provisionAttributeBacking`),
//! the engine auto-creates a database-wide system dimension and, per normal
//! dimension, an attribute dimension + cube.

use fluxolap_domain::cube::CUBE_TYPE_ATTRIBUTE;
use fluxolap_domain::database::DB_TYPE_NORMAL;
use fluxolap_domain::dimension::{DIM_TYPE_ATTRIBUTE, DIM_TYPE_NORMAL, DIM_TYPE_SYSTEM};
use fluxolap_domain::DatabaseId;
use fluxolap_engine::provision::{create_database, create_dimension, destroy_dimension, rename_dimension, SYSTEM_DIMENSION_NAME};
use fluxolap_engine::{AppState, LoadedDatabase};
use std::sync::Arc;

fn new_database(state: &AppState) -> (DatabaseId, Arc<LoadedDatabase>) {
    let id = create_database(state, "Sales", DB_TYPE_NORMAL).expect("create database");
    let databases = state.databases.read().unwrap();
    let loaded = state.resolve_loaded(&databases, id).expect("resolve loaded");
    (id, loaded)
}

/// Sorted names of every dimension in `database_id` of type `dim_type`.
fn dimension_names(loaded: &LoadedDatabase, database_id: DatabaseId, dim_type: u32) -> Vec<String> {
    let dimensions = loaded.dimensions.read().unwrap();
    let mut names: Vec<String> = dimensions.list_in_database(database_id).filter(|d| d.dim_type == dim_type).map(|d| d.name.clone()).collect();
    names.sort();
    names
}

fn system_dimension_has_element(loaded: &LoadedDatabase, database_id: DatabaseId, name: &str) -> bool {
    let dimensions = loaded.dimensions.read().unwrap();
    let system_id = dimensions.find_by_name(database_id, SYSTEM_DIMENSION_NAME).expect("system dimension exists");
    dimensions.get(system_id).unwrap().find_element_by_name(name).is_some()
}

#[test]
fn create_database_provisions_the_system_dimension_with_its_own_attribute_backing() {
    let state = AppState::new();
    let (id, loaded) = new_database(&state);

    assert_eq!(dimension_names(&loaded, id, DIM_TYPE_SYSTEM), vec![SYSTEM_DIMENSION_NAME.to_string()]);
    // The system dimension gets its own attribute backing too (the
    // "#_<name>_" convention applied to "#_DIMENSION_" itself).
    assert_eq!(dimension_names(&loaded, id, DIM_TYPE_ATTRIBUTE), vec!["#_#_DIMENSION__".to_string()]);
    let cubes = loaded.cubes.read().unwrap();
    let attr_cube = cubes.find_by_name(id, "#_#_DIMENSION__").expect("system dimension's attribute cube");
    assert_eq!(cubes.get(attr_cube).unwrap().cube_type, CUBE_TYPE_ATTRIBUTE);
}

#[test]
fn create_dimension_registers_the_dimension_in_system_and_provisions_its_own_attribute_backing() {
    let state = AppState::new();
    let (id, loaded) = new_database(&state);
    let products = create_dimension(&loaded, id, "Products", DIM_TYPE_NORMAL).expect("create dimension");

    assert!(system_dimension_has_element(&loaded, id, "Products"));
    assert_eq!(dimension_names(&loaded, id, DIM_TYPE_ATTRIBUTE), vec!["#_#_DIMENSION__".to_string(), "#_Products_".to_string()]);

    // The attribute cube's dimension order is [attribute, target].
    let attr_dim = loaded.dimensions.read().unwrap().find_by_name(id, "#_Products_").unwrap();
    let cubes = loaded.cubes.read().unwrap();
    let attr_cube = cubes.get(cubes.find_by_name(id, "#_Products_").expect("attribute cube")).unwrap();
    assert_eq!(attr_cube.dimensions, vec![attr_dim, products]);
    assert_eq!(attr_cube.cube_type, CUBE_TYPE_ATTRIBUTE);
}

#[test]
fn a_directly_created_attribute_dimension_gets_no_further_auto_provisioning() {
    let state = AppState::new();
    let (id, loaded) = new_database(&state);
    create_dimension(&loaded, id, "#_Weird_", DIM_TYPE_ATTRIBUTE).expect("create dimension");

    // "#_Weird_" itself is attribute-typed, so it's listed -- but no
    // nested "#_#_Weird__" attribute-of-an-attribute dimension exists.
    assert_eq!(dimension_names(&loaded, id, DIM_TYPE_ATTRIBUTE), vec!["#_#_DIMENSION__".to_string(), "#_Weird_".to_string()]);
    assert!(!system_dimension_has_element(&loaded, id, "#_Weird_"));
}

#[test]
fn destroy_dimension_removes_its_attribute_backing_and_system_element() {
    let state = AppState::new();
    let (id, loaded) = new_database(&state);
    let products = create_dimension(&loaded, id, "Products", DIM_TYPE_NORMAL).unwrap();

    destroy_dimension(&loaded, id, products).unwrap();

    assert_eq!(dimension_names(&loaded, id, DIM_TYPE_NORMAL), Vec::<String>::new());
    assert_eq!(dimension_names(&loaded, id, DIM_TYPE_ATTRIBUTE), vec!["#_#_DIMENSION__".to_string()]);
    assert!(loaded.cubes.read().unwrap().find_by_name(id, "#_Products_").is_none());
    assert!(!system_dimension_has_element(&loaded, id, "Products"));
}

#[test]
fn destroy_dimension_is_refused_while_a_cube_uses_it() {
    let state = AppState::new();
    let (id, loaded) = new_database(&state);
    let products = create_dimension(&loaded, id, "Products", DIM_TYPE_NORMAL).unwrap();
    loaded.cubes.write().unwrap().create(id, "Sales", vec![products], fluxolap_domain::cube::CUBE_TYPE_NORMAL).unwrap();

    let err = destroy_dimension(&loaded, id, products).unwrap_err();
    assert_eq!(err.code, fluxolap_error::ErrorCode::DIMENSION_IN_USE);
    assert_eq!(dimension_names(&loaded, id, DIM_TYPE_NORMAL), vec!["Products".to_string()]);
}

#[test]
fn rename_dimension_renames_its_attribute_backing_and_system_element() {
    let state = AppState::new();
    let (id, loaded) = new_database(&state);
    let products = create_dimension(&loaded, id, "Products", DIM_TYPE_NORMAL).unwrap();

    rename_dimension(&loaded, id, products, "Items").unwrap();

    assert_eq!(dimension_names(&loaded, id, DIM_TYPE_NORMAL), vec!["Items".to_string()]);
    assert_eq!(dimension_names(&loaded, id, DIM_TYPE_ATTRIBUTE), vec!["#_#_DIMENSION__".to_string(), "#_Items_".to_string()]);
    assert!(loaded.cubes.read().unwrap().find_by_name(id, "#_Items_").is_some());
    assert!(system_dimension_has_element(&loaded, id, "Items"));
    assert!(!system_dimension_has_element(&loaded, id, "Products"));
}
