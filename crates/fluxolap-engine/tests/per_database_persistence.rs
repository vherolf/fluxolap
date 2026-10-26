//! Per-database load/unload: the single most important property
//! this redesign depends on -- that a *loaded* database's own bincode
//! round trip (through its own file, its own freshly-deserialized
//! `DimensionTable`/`CubeTable`/`Storage`) preserves internal SlotMap
//! cross-references (a `Cube`'s `Vec<DimensionId>`, `Storage`'s
//! `CubeId`-keyed cells) exactly -- was previously unverified by any
//! test in this project (the one prior round-trip test,
//! `fluxolap-storage`'s `snapshot_round_trips_through_from_snapshot`,
//! never actually went through serde/bincode at all). See
//! `fluxolap-engine::state`'s doc comments for the full design this
//! is grounded in.

use fluxolap_domain::cube::CUBE_TYPE_NORMAL;
use fluxolap_domain::database::DB_TYPE_NORMAL;
use fluxolap_domain::dimension::DIM_TYPE_NORMAL;
use fluxolap_engine::AppState;
use fluxolap_storage::CellValue;

fn temp_dir(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("fluxolap_engine_test_{label}_{}_{nanos}", std::process::id()))
}

/// Builds a database with two dimensions and a cube referencing both (so
/// `Cube.dimensions: Vec<DimensionId>` is genuinely non-trivial -- a
/// single-dimension cube would never exercise cross-reference ordering),
/// writes a couple of real cells, and marks the database dirty (mirrors
/// what a real handler does after a mutation -- `AppState` itself has no
/// automatic dirty-tracking, callers own that).
fn build_and_populate(state: &AppState) -> (fluxolap_domain::DatabaseId, u32) {
    let mut databases = state.databases.write().unwrap();
    let database_id = databases.create("Test", DB_TYPE_NORMAL).unwrap();
    let wire_id = databases.get(database_id).unwrap().wire_id;
    drop(databases);
    state.insert_new_loaded(database_id);

    let databases = state.databases.read().unwrap();
    let loaded = state.resolve_loaded(&databases, database_id).unwrap();
    drop(databases);

    let (product_leaf, customer_leaf, cube_id) = {
        let mut dimensions = loaded.dimensions.write().unwrap();
        let products_id = dimensions.create(database_id, "Products", DIM_TYPE_NORMAL).unwrap();
        let customers_id = dimensions.create(database_id, "Customers", DIM_TYPE_NORMAL).unwrap();
        let product_leaf = dimensions.get_mut(products_id).unwrap().create_element("Widget", 1).unwrap();
        let customer_leaf = dimensions.get_mut(customers_id).unwrap().create_element("Acme", 1).unwrap();

        let mut cubes = loaded.cubes.write().unwrap();
        let cube_id = cubes.create(database_id, "Sales", vec![products_id, customers_id], CUBE_TYPE_NORMAL).unwrap();
        loaded.storage.cube_created(cube_id, 2);
        (product_leaf, customer_leaf, cube_id)
    };

    loaded.storage.record_and_set(cube_id, vec![product_leaf, customer_leaf], CellValue::Numeric(42.5));
    loaded.mark_dirty();

    (database_id, wire_id)
}

/// Reads back "Sales" cell at (Widget, Acme) through a **freshly loaded**
/// `LoadedDatabase` -- resolving dimensions/cube/elements by name/wire_id
/// each time (never reusing ids from a previous load, since those are
/// only ever meaningful within one load's own fresh SlotMap instances).
fn read_back(state: &AppState, database_id: fluxolap_domain::DatabaseId) -> Option<CellValue> {
    let databases = state.databases.read().unwrap();
    let loaded = state.resolve_loaded(&databases, database_id).unwrap();
    drop(databases);

    let dimensions = loaded.dimensions.read().unwrap();
    let cubes = loaded.cubes.read().unwrap();
    let cube_id = cubes.find_by_name(database_id, "Sales").expect("cube survived the round trip");
    let cube = cubes.get(cube_id).unwrap();
    assert_eq!(cube.dimensions.len(), 2, "cube must still reference exactly 2 dimensions after reload");

    let products_id = cube.dimensions[0];
    let customers_id = cube.dimensions[1];
    let products = dimensions.get(products_id).expect("cube's first DimensionId must resolve in the reloaded DimensionTable");
    let customers = dimensions.get(customers_id).expect("cube's second DimensionId must resolve in the reloaded DimensionTable");
    assert_eq!(products.name, "Products", "cube.dimensions[0] must still be Products after a fresh SlotMap reload");
    assert_eq!(customers.name, "Customers", "cube.dimensions[1] must still be Customers after a fresh SlotMap reload");

    let product_leaf = products.find_element_by_name("Widget").expect("Widget element survived the round trip");
    let customer_leaf = customers.find_element_by_name("Acme").expect("Acme element survived the round trip");
    loaded.storage.get(cube_id, &[product_leaf, customer_leaf])
}

#[test]
fn unload_then_reload_preserves_cube_dimension_references_and_cell_values() {
    let dir = temp_dir("round_trip");
    let state = AppState::load_from_dir(&dir).unwrap();
    let (database_id, _wire_id) = build_and_populate(&state);

    assert!(state.is_loaded(database_id));
    state.unload(database_id).unwrap();
    assert!(!state.is_loaded(database_id), "unload must actually remove the database from the loaded set");

    let value = read_back(&state, database_id);
    assert_eq!(value, Some(CellValue::Numeric(42.5)), "cell value must survive a real unload -> save -> reload -> deserialize cycle");
    assert!(state.is_loaded(database_id), "resolve_loaded (called by read_back) must have reloaded it");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Same property, but a **second** unload/reload cycle -- rules out the
/// first cycle succeeding only by coincidence (e.g. matching SlotMap
/// generation counters that happen to line up once but wouldn't reliably
/// on repeated use).
#[test]
fn a_second_unload_reload_cycle_still_preserves_everything() {
    let dir = temp_dir("round_trip_twice");
    let state = AppState::load_from_dir(&dir).unwrap();
    let (database_id, _wire_id) = build_and_populate(&state);

    state.unload(database_id).unwrap();
    assert_eq!(read_back(&state, database_id), Some(CellValue::Numeric(42.5)));

    // Second cycle: the database is loaded again from read_back above --
    // mark it dirty again (a real handler would, on any further write)
    // and unload/reload once more.
    let databases = state.databases.read().unwrap();
    let loaded = state.resolve_loaded(&databases, database_id).unwrap();
    drop(databases);
    loaded.mark_dirty();
    drop(loaded);

    state.unload(database_id).unwrap();
    assert_eq!(read_back(&state, database_id), Some(CellValue::Numeric(42.5)), "a second independent unload/reload cycle must also round-trip correctly");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Idle-timeout eviction (`check_unload`) actually frees the loaded
/// entry, and a subsequent access transparently reloads it with
/// identical data -- proving proactive eviction is safe to do
/// aggressively, since a caller touching an evicted database just pays
/// a one-time reload cost.
#[test]
fn check_unload_evicts_when_idle_past_expiry_and_reload_is_transparent() {
    let dir = temp_dir("idle_unload");
    let state = AppState::load_from_dir(&dir).unwrap();
    let (database_id, _wire_id) = build_and_populate(&state);

    // expiry_minutes = 0 means "idle for at least 0 minutes", which any
    // already-created database satisfies immediately -- no time-mocking
    // needed.
    let evicted = state.check_unload(database_id, 0).unwrap();
    assert!(evicted, "check_unload with expiry_minutes=0 must evict an idle-since-creation database");
    assert!(!state.is_loaded(database_id));

    assert_eq!(read_back(&state, database_id), Some(CellValue::Numeric(42.5)), "a normal read after eviction must transparently reload with the exact pre-eviction data");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_unload_does_nothing_when_not_yet_idle_long_enough() {
    let dir = temp_dir("idle_not_yet");
    let state = AppState::load_from_dir(&dir).unwrap();
    let (database_id, _wire_id) = build_and_populate(&state);

    // Just accessed (build_and_populate), so it's not idle at all --
    // any positive expiry must not evict it yet.
    let evicted = state.check_unload(database_id, 60).unwrap();
    assert!(!evicted, "a database accessed moments ago must not be evicted by a 60-minute expiry");
    assert!(state.is_loaded(database_id));

    let _ = std::fs::remove_dir_all(&dir);
}

/// `database_destroy`'s cascade: destroying a database removes
/// its on-disk file too, not just its registry entry -- a pre-existing
/// bug before this redesign (orphaned dimensions/cubes/cells survived a
/// database's permanent destruction).
#[test]
fn forget_removes_both_the_loaded_entry_and_the_on_disk_file() {
    let dir = temp_dir("destroy_cascade");
    let state = AppState::load_from_dir(&dir).unwrap();
    let (database_id, wire_id) = build_and_populate(&state);

    state.unload(database_id).unwrap(); // force a real file to exist on disk
    // build_and_populate names the database "Test" -- the persisted file name
    // embeds it (purely for human identification browsing data_dir by eye;
    // wire_id alone is still the authoritative key, see state.rs).
    let db_file = dir.join(format!("db-{wire_id}-Test.bin.zst"));
    assert!(db_file.exists(), "sanity check: unload must have actually written a per-database file");

    state.forget(database_id, wire_id);
    assert!(!state.is_loaded(database_id));
    assert!(!db_file.exists(), "forget must delete the per-database file, not just drop the in-memory entry");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A registry entry whose file is missing entirely -- most commonly
/// because someone deleted it by hand outside the server -- must not
/// keep masquerading as a real (merely empty) database forever. Startup
/// sanitization (`AppState::load_from_dir`) prunes it, and that pruning
/// must itself be persisted (checked here via a *second* independent
/// reload), not just fixed up in the freshly-returned in-memory copy.
#[test]
fn startup_sanitization_removes_a_registry_entry_whose_file_is_missing() {
    let dir = temp_dir("sanitize_missing_file");
    let state = AppState::load_from_dir(&dir).unwrap();
    let (database_id, _wire_id) = build_and_populate(&state);
    // Registered in the registry, but never unloaded/saved -- no
    // per-database file exists for it at all, exactly like a manually
    // deleted file would look from the registry's point of view.
    state.save_registry().unwrap();
    drop(state);

    let reloaded = AppState::load_from_dir(&dir).unwrap();
    let databases = reloaded.databases.read().unwrap();
    assert!(databases.get(database_id).is_none(), "a registry entry with no file at all must be pruned on load_from_dir");
    drop(databases);
    drop(reloaded);

    // Reload again, independently -- proves the prune was written back to
    // registry.bin.zst, not only fixed up in the first reload's in-memory copy.
    let reloaded_again = AppState::load_from_dir(&dir).unwrap();
    assert!(reloaded_again.databases.read().unwrap().get(database_id).is_none(), "the pruned entry must not reappear on a later reload");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The same startup sanitization must never touch a database that
/// genuinely has a file on disk -- pruning is strictly for entries
/// backed by nothing.
#[test]
fn startup_sanitization_leaves_a_database_with_a_real_file_alone() {
    let dir = temp_dir("sanitize_keeps_real_file");
    let state = AppState::load_from_dir(&dir).unwrap();
    let (database_id, _wire_id) = build_and_populate(&state);
    state.unload(database_id).unwrap(); // writes the real per-database file
    state.save_registry().unwrap();
    drop(state);

    let reloaded = AppState::load_from_dir(&dir).unwrap();
    assert!(reloaded.databases.read().unwrap().get(database_id).is_some(), "a database with a real file must survive startup sanitization");
    assert_eq!(read_back(&reloaded, database_id), Some(CellValue::Numeric(42.5)), "and its data must still be intact");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A per-database file can be renamed to literally anything by hand and
/// still be found and loaded correctly -- discovery is driven entirely
/// by each file's own embedded `DatabaseIdentity` (see `fluxolap-domain`),
/// never by parsing or trusting the file's name. Direct test of "internal
/// logic independent of file naming."
#[test]
fn a_database_file_renamed_to_something_unrelated_still_loads() {
    let dir = temp_dir("rename_arbitrary");
    let state = AppState::load_from_dir(&dir).unwrap();
    let (database_id, wire_id) = build_and_populate(&state);
    state.unload(database_id).unwrap(); // writes db-{wire_id}-Test.bin.zst
    state.save_registry().unwrap();
    drop(state);

    let original = dir.join(format!("db-{wire_id}-Test.bin.zst"));
    let renamed = dir.join("banana.bin.zst"); // no wire_id, no "db-" prefix, nothing
    assert!(original.exists(), "sanity check: unload must have written the file under its original name");
    std::fs::rename(&original, &renamed).unwrap();

    let reloaded = AppState::load_from_dir(&dir).unwrap();
    assert_eq!(
        read_back(&reloaded, database_id),
        Some(CellValue::Numeric(42.5)),
        "a database whose file was renamed to something with no relationship to its wire_id/name must still load correctly"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// If the registry is lost or corrupted outright, every per-database
/// file's own embedded identity is enough to fully reconstruct it -- not
/// just avoid a crash, but recover the real wire_id/name and the actual
/// data behind it.
#[test]
fn a_corrupt_registry_is_rebuilt_from_the_surviving_per_database_file() {
    let dir = temp_dir("rebuild_from_corrupt_registry");
    let state = AppState::load_from_dir(&dir).unwrap();
    let (database_id, wire_id) = build_and_populate(&state);
    state.unload(database_id).unwrap();
    state.save_registry().unwrap();
    drop(state);

    // "assume it is corrupted" -- overwrite the registry with garbage.
    std::fs::write(dir.join("registry.bin.zst"), b"not a valid zstd+bincode registry").unwrap();

    let rebuilt = AppState::load_from_dir(&dir).unwrap();
    let recovered_id = {
        let databases = rebuilt.databases.read().unwrap();
        let id = databases.find_by_wire_id(wire_id).expect("rebuild must recreate a registry entry for the surviving file's wire_id");
        assert_eq!(databases.get(id).unwrap().name, "Test", "the rebuilt entry's name must come from the file's own embedded identity");
        id
    };
    assert_eq!(
        read_back(&rebuilt, recovered_id),
        Some(CellValue::Numeric(42.5)),
        "the rebuilt database's actual data must be intact, not just its registry entry"
    );
    drop(rebuilt);

    // The rebuild must itself be persisted -- an independent second reload
    // must find a real, valid registry, not need to rebuild again.
    assert!(AppState::registry_loads_cleanly(&dir), "the rebuilt registry must have been saved back as a real, parseable registry.bin.zst");
    let reloaded_again = AppState::load_from_dir(&dir).unwrap();
    assert!(reloaded_again.databases.read().unwrap().find_by_wire_id(wire_id).is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

/// `IDatabase.backup(filename)` / `IConnection.restoreDatabase(name, type,
/// filename)`: exporting a loaded database to an arbitrary path (outside
/// `data_dir`, unaffected by the dirty flag) and importing it back into a
/// **different** destination database must reproduce the exact same
/// dimension/cube/cell state -- same cross-reference-preservation property
/// as the unload/reload cycles above, just through an explicitly
/// user-chosen file instead of the automatic per-database one.
#[test]
fn export_then_import_into_a_new_database_preserves_everything() {
    let state_dir = temp_dir("export_import_state");
    let state = AppState::load_from_dir(&state_dir).unwrap();
    let (source_id, _source_wire) = build_and_populate(&state);

    let backup_path = temp_dir("export_import_backup").with_extension("bin.zst");
    let databases = state.databases.read().unwrap();
    let loaded = state.resolve_loaded(&databases, source_id).unwrap();
    drop(databases);
    state.export_loaded_to_path(source_id, &backup_path).unwrap();
    drop(loaded);
    assert!(backup_path.exists(), "export must actually write a file at the given path");

    let (snapshot, storage_data) = fluxolap_engine::AppState::load_snapshot_from_path(&backup_path).unwrap();

    let mut databases = state.databases.write().unwrap();
    let dest_id = databases.create("Restored", DB_TYPE_NORMAL).unwrap();
    drop(databases);
    state.insert_loaded_from_snapshot(dest_id, snapshot, storage_data);

    assert_eq!(read_back(&state, dest_id), Some(CellValue::Numeric(42.5)), "restored database must have the exact same cell value as the exported source");
    // Source database must be completely untouched by the export.
    assert_eq!(read_back(&state, source_id), Some(CellValue::Numeric(42.5)));

    let _ = std::fs::remove_file(&backup_path);
    let _ = std::fs::remove_dir_all(&state_dir);
}

/// The modification time of "Sales", through a freshly resolved database.
fn modified_of_sales(state: &AppState, database_id: fluxolap_domain::DatabaseId) -> u64 {
    let databases = state.databases.read().unwrap();
    let loaded = state.resolve_loaded(&databases, database_id).unwrap();
    drop(databases);
    let cube_id = loaded.cubes.read().unwrap().find_by_name(database_id, "Sales").unwrap();
    loaded.storage.modified(cube_id)
}

#[test]
fn a_cubes_modification_time_survives_unload_and_reload() {
    let dir = temp_dir("modified");
    let state = AppState::load_from_dir(&dir).unwrap();
    let before = fluxolap_domain::unix_timestamp();
    let (database_id, _wire_id) = build_and_populate(&state);

    let written = modified_of_sales(&state, database_id);
    assert!(written >= before, "a cell write must set the cube's modification time");

    state.unload(database_id).unwrap();
    assert_eq!(modified_of_sales(&state, database_id), written, "the time is the write's, not the reload's");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The modification time of dimension `name`, through a freshly resolved
/// database.
fn modified_of_dimension(state: &AppState, database_id: fluxolap_domain::DatabaseId, name: &str) -> u64 {
    let databases = state.databases.read().unwrap();
    let loaded = state.resolve_loaded(&databases, database_id).unwrap();
    drop(databases);
    let dimensions = loaded.dimensions.read().unwrap();
    dimensions.get(dimensions.find_by_name(database_id, name).unwrap()).unwrap().modified()
}

#[test]
fn a_dimensions_modification_time_survives_unload_and_reload() {
    let dir = temp_dir("dimension_modified");
    let state = AppState::load_from_dir(&dir).unwrap();
    let before = fluxolap_domain::unix_timestamp();
    let (database_id, _wire_id) = build_and_populate(&state);
    assert!(modified_of_dimension(&state, database_id, "Products") >= before, "creating a dimension and its elements sets its modification time");

    // A time in the past, so a reload that stamped "now" would show.
    {
        let databases = state.databases.read().unwrap();
        let loaded = state.resolve_loaded(&databases, database_id).unwrap();
        drop(databases);
        let mut dimensions = loaded.dimensions.write().unwrap();
        let products = dimensions.find_by_name(database_id, "Products").unwrap();
        dimensions.get_mut(products).unwrap().set_modified(1_000_000);
        loaded.mark_dirty();
    }
    state.unload(database_id).unwrap();
    assert_eq!(modified_of_dimension(&state, database_id, "Products"), 1_000_000, "the time is the change's, not the reload's");
    assert!(modified_of_dimension(&state, database_id, "Customers") >= before);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Rewrites the per-database file in `dir` to end after the cells tuple and
/// `keep` of the trailing sections (0: before any modification time existed,
/// 1: with the cubes' times, before the dimensions' existed) -- what an older
/// version wrote.
fn truncate_database_file(dir: &std::path::Path, keep: usize) {
    use std::io::{Read, Write};
    use fluxolap_domain::{CubeId, DatabaseIdentity, DatabaseSnapshot, DimensionId};
    use std::collections::HashMap;

    let path = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).find(|p| p.file_name().unwrap().to_string_lossy().starts_with("db-")).unwrap();
    let mut bytes = Vec::new();
    zstd::Decoder::new(std::fs::File::open(&path).unwrap()).unwrap().read_to_end(&mut bytes).unwrap();
    let mut cursor = std::io::Cursor::new(&bytes[..]);
    let _: (DatabaseIdentity, DatabaseSnapshot, HashMap<CubeId, fluxolap_storage::CubePartition>) = bincode::deserialize_from(&mut cursor).unwrap();
    let mut ends = vec![cursor.position() as usize];
    let _: HashMap<CubeId, u64> = bincode::deserialize_from(&mut cursor).unwrap();
    ends.push(cursor.position() as usize);
    let dimensions: HashMap<DimensionId, u64> = bincode::deserialize_from(&mut cursor).unwrap();
    assert_eq!(dimensions.len(), 2, "both dimensions have a time");
    assert_eq!(cursor.position() as usize, bytes.len(), "the dimensions' times are the file's last value");

    bytes.truncate(ends[keep]);
    let mut encoder = zstd::Encoder::new(std::fs::File::create(&path).unwrap(), 0).unwrap();
    encoder.write_all(&bytes).unwrap();
    encoder.finish().unwrap();
}

/// A file from before the modification times existed ends after the cells:
/// it must still load, with every time unknown.
#[test]
fn a_database_file_without_modification_times_still_loads() {
    let dir = temp_dir("modified_old_file");
    let state = AppState::load_from_dir(&dir).unwrap();
    let (database_id, _wire_id) = build_and_populate(&state);
    state.unload(database_id).unwrap();
    truncate_database_file(&dir, 0);

    assert_eq!(read_back(&state, database_id), Some(CellValue::Numeric(42.5)));
    assert_eq!(modified_of_sales(&state, database_id), 0);
    assert_eq!(modified_of_dimension(&state, database_id, "Products"), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A file from before the dimensions' times existed ends after the cubes':
/// it must still load, with the cubes' times and the dimensions' unknown.
#[test]
fn a_database_file_without_dimension_modification_times_still_loads() {
    let dir = temp_dir("dimension_modified_old_file");
    let state = AppState::load_from_dir(&dir).unwrap();
    let (database_id, _wire_id) = build_and_populate(&state);
    let written = modified_of_sales(&state, database_id);
    state.unload(database_id).unwrap();
    truncate_database_file(&dir, 1);

    assert_eq!(read_back(&state, database_id), Some(CellValue::Numeric(42.5)));
    assert_eq!(modified_of_sales(&state, database_id), written);
    assert_eq!(modified_of_dimension(&state, database_id, "Products"), 0);
    assert_eq!(modified_of_dimension(&state, database_id, "Customers"), 0);

    let _ = std::fs::remove_dir_all(&dir);
}
