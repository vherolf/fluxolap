//! `AppState::import_database_bytes_as`: a database file under another name, or in place of a copy.

use fluxolap_domain::database::DB_TYPE_NORMAL;
use fluxolap_domain::dimension::DIM_TYPE_NORMAL;
use fluxolap_engine::provision::{create_database, create_dimension};
use fluxolap_engine::AppState;

fn database_with_dimension(state: &AppState, name: &str, dimension: &str) -> Vec<u8> {
    let id = create_database(state, name, DB_TYPE_NORMAL).unwrap();
    let databases = state.databases.read().unwrap();
    let loaded = state.resolve_loaded(&databases, id).unwrap();
    create_dimension(&loaded, id, dimension, DIM_TYPE_NORMAL).unwrap();
    drop(databases);
    state.export_database_bytes(id).unwrap()
}

fn has_dimension(state: &AppState, database: &str, dimension: &str) -> bool {
    let id = state.databases.read().unwrap().find_by_name(database).unwrap();
    let loaded = state.loaded.read().unwrap().get(&id).cloned().unwrap();
    let found = loaded.dimensions.read().unwrap().find_by_name(id, dimension).is_some();
    found
}

#[test]
fn imports_under_the_given_name_and_replaces_in_place_only_when_asked() {
    let source = AppState::new();
    let first = database_with_dimension(&source, "Sales", "Region");

    let target = AppState::new();
    let id = target.import_database_bytes_as(&first, "berlin/Sales", false).unwrap();
    assert!(has_dimension(&target, "berlin/Sales", "Region"));
    assert!(target.databases.read().unwrap().find_by_name("Sales").is_none(), "only the new name exists");

    // Without `replace` the name is taken.
    assert_eq!(target.import_database_bytes_as(&first, "berlin/Sales", false).unwrap_err().code, fluxolap_error::ErrorCode::DATABASE_NAME_IN_USE);

    // With it the contents are swapped and the id stays.
    let other = AppState::new();
    let second = database_with_dimension(&other, "Sales", "Month");
    assert_eq!(target.import_database_bytes_as(&second, "berlin/Sales", true).unwrap(), id);
    assert!(has_dimension(&target, "berlin/Sales", "Month"));
    assert!(!has_dimension(&target, "berlin/Sales", "Region"));
    assert_eq!(target.databases.read().unwrap().list().count(), 1);

    // Replacing a name that doesn't exist just creates it; garbage is refused.
    target.import_database_bytes_as(&second, "wien/Sales", true).unwrap();
    assert_eq!(target.databases.read().unwrap().list().count(), 2);
    assert!(target.import_database_bytes_as(b"nope", "x", true).is_err());
}
