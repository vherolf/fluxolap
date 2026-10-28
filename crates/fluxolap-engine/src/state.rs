use fluxolap_domain::cube::CubeTable;
use fluxolap_domain::database::{Database, DatabaseStatus, DatabaseTable};
use fluxolap_domain::dimension::DimensionTable;
use fluxolap_domain::{unix_timestamp, CubeId, DatabaseId, DatabaseIdentity, DatabaseSnapshot, DimensionId};
use fluxolap_storage::{CubePartition, Storage};
use fluxolap_error::{ErrorCode, FluxolapError};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::Notify;

const REGISTRY_FILE: &str = "registry.bin.zst";

/// Filesystem-safe rendering of a database name for use inside the file
/// name a brand-new database is first saved under (see
/// `database_file_name`) -- purely a human convenience for identifying
/// files by eye in `data_dir` at the moment of creation. Never used to
/// *find* a file again afterwards (see `AppState.file_index`'s doc
/// comment) -- once written, a file keeps whatever name it was given,
/// even across a later rename, so this never needs to be reversible or
/// collision-free; two databases can never share a `wire_id`, so their
/// sanitized names colliding is harmless.
fn sanitize_name_for_filename(name: &str) -> String {
    let mut out: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' }).collect();
    out.truncate(80); // every char above is single-byte ASCII, so this never splits a multi-byte char
    let trimmed = out.trim_matches(|c: char| c == '.' || c == '_' || c == ' ');
    if trimmed.is_empty() { "db".to_string() } else { trimmed.to_string() }
}

/// The name a brand-new (never-yet-saved) database's file is first
/// written under. Cosmetic only -- see `sanitize_name_for_filename`.
fn database_file_name(wire_id: u32, name: &str) -> String {
    format!("db-{wire_id}-{}.bin.zst", sanitize_name_for_filename(name))
}

/// Removes every registry entry with no entry in `file_index` -- called
/// once, from `AppState::load_from_dir`, before anything is loaded.
/// Cannot distinguish "someone deleted the file" from "created but never
/// dirtied/saved before an unclean shutdown" (the registry carries no
/// such bit), but both cases share the same resolution: there is no data
/// to lose, so there is nothing wrong with removing an entry that points
/// at nothing. Returns the pruned `(wire_id, name)` pairs so the caller
/// can log them -- pruning is never silent.
fn prune_databases_missing_their_file(databases: &mut DatabaseTable, file_index: &HashMap<u32, PathBuf>) -> Vec<(u32, String)> {
    let missing_ids: Vec<DatabaseId> = databases
        .list_ids()
        .filter(|&id| {
            let db = databases.get(id).expect("id just came from this table's own list_ids");
            !file_index.contains_key(&db.wire_id)
        })
        .collect();

    let mut pruned = Vec::with_capacity(missing_ids.len());
    for id in missing_ids {
        if let Some(db) = databases.get(id) {
            pruned.push((db.wire_id, db.name.clone()));
        }
        let _ = databases.destroy(id); // just verified present via list_ids/get above
    }
    pruned
}

/// True for every file in `dir` that could possibly be a per-database
/// file -- everything except the registry itself and non-regular-file
/// entries (subdirectories, sockets, ...). Deliberately not filtered by
/// extension or any naming pattern: file *discovery* is content-driven
/// (see `DatabaseIdentity`), so a candidate is anything that isn't
/// already spoken for by a more specific role.
fn is_database_file_candidate(entry: &std::fs::DirEntry) -> bool {
    if entry.path().file_name().and_then(|n| n.to_str()) == Some(REGISTRY_FILE) {
        return false;
    }
    entry.file_type().map(|t| t.is_file()).unwrap_or(false)
}

/// Builds the `wire_id -> path` index by reading just the self-contained
/// `DatabaseIdentity` header (see its own doc comment) from every
/// candidate file in `dir` -- cheap even for a large cube file, since
/// bincode/zstd decoding naturally stops as soon as the much smaller
/// `DatabaseIdentity` value is fully read, never touching the
/// `DatabaseSnapshot`/cell data that follows it in the same stream. A
/// file that isn't a valid per-database file (foreign file, corrupt,
/// truncated) is skipped with a warning, never aborts the scan. Two
/// files claiming the same `wire_id` should never happen in normal
/// operation; if it does, the one encountered last wins and both paths
/// are named in the warning, rather than failing the whole scan.
fn scan_file_index(dir: &Path) -> HashMap<u32, PathBuf> {
    let mut index = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return index };
    for entry in entries.flatten() {
        if !is_database_file_candidate(&entry) {
            continue;
        }
        let path = entry.path();
        match load_compressed::<DatabaseIdentity>(&path) {
            Ok(identity) => {
                if let Some(previous) = index.insert(identity.wire_id, path.clone()) {
                    eprintln!(
                        "fluxolap: both {} and {} claim wire_id {} -- keeping {}",
                        previous.display(),
                        path.display(),
                        identity.wire_id,
                        path.display()
                    );
                }
            }
            Err(e) => {
                eprintln!("fluxolap: skipping {} while indexing {} -- not a valid per-database file: {e}", path.display(), dir.display());
            }
        }
    }
    index
}

/// Reconstructs the registry from every per-database file's own embedded
/// `DatabaseIdentity`, for when `registry.bin.zst` is missing or
/// unreadable. Unlike `scan_file_index`, each file is *fully* decoded --
/// identity, dimensions/cubes, and cell storage -- not just its header:
/// "valid" here means the whole file parses cleanly end to end, matching
/// what actually matters before trusting a file to survive a rebuild,
/// not just that its first few bytes look right. A file that fails to
/// fully decode (foreign file, truncated, genuinely corrupt) is skipped
/// with a warning; the rest of the rebuild proceeds regardless.
fn rebuild_registry_from_files(dir: &Path) -> (DatabaseTable, HashMap<u32, PathBuf>) {
    let mut table = DatabaseTable::new();
    let mut index = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return (table, index) };
    let mut rebuilt = 0u32;
    for entry in entries.flatten() {
        if !is_database_file_candidate(&entry) {
            continue;
        }
        let path = entry.path();
        match load_compressed::<(DatabaseIdentity, DatabaseSnapshot, HashMap<CubeId, CubePartition>)>(&path) {
            Ok((identity, _snapshot, _storage)) => {
                let DatabaseIdentity { wire_id, name, db_type } = identity;
                table.insert_with_wire_id(Database { wire_id, name: name.clone(), db_type, status: DatabaseStatus::Unloaded, token: 1, read_only: false });
                if let Some(previous) = index.insert(wire_id, path.clone()) {
                    eprintln!("fluxolap: both {} and {} claim wire_id {wire_id} -- keeping {}", previous.display(), path.display(), path.display());
                }
                eprintln!("fluxolap: rebuilt registry entry for '{name}' (wire_id {wire_id}) from {}", path.display());
                rebuilt += 1;
            }
            Err(e) => {
                eprintln!("fluxolap: skipping {} while rebuilding registry -- not a valid per-database file: {e}", path.display());
            }
        }
    }
    eprintln!(
        "fluxolap: rebuilt {rebuilt} database registry entr{} from {}",
        if rebuilt == 1 { "y" } else { "ies" },
        dir.display()
    );
    (table, index)
}

/// One loaded database's live, lock-guarded tables (per-database
/// load/unload). See `DatabaseSnapshot` (`fluxolap-domain`) for the
/// persisted shape these round-trip through.
///
/// Deliberately a *wholesale-owned* trio per loaded database, not a
/// `database_id`-filtered view over shared global tables: a `Cube` holds
/// `Vec<DimensionId>` and `Storage` keys cells by `CubeId`, both SlotMap
/// keys that a naive "extract entries, reinsert elsewhere" approach would
/// silently invalidate (SlotMap's public API always assigns a *new* key on
/// insert). Because each loaded database gets its own private
/// `DimensionTable`/`CubeTable`/`Storage`, its own bincode round trip
/// preserves its own SlotMap keys exactly -- by construction, the same
/// property the old whole-process `save_to_file`/`load_from_file` already
/// relied on, just scoped smaller.
pub struct LoadedDatabase {
    pub dimensions: RwLock<DimensionTable>,
    pub cubes: RwLock<CubeTable>,
    pub storage: Storage,
    /// Set by callers after any mutation (dimension/cube edits, cell
    /// writes); cleared once actually saved. Eviction (`AppState::unload`/
    /// `check_unload`) only writes to disk when this is true -- read-heavy
    /// databases (the exact case this feature exists for) never pay a
    /// needless re-save/re-compress on every idle-timeout cycle.
    pub dirty: AtomicBool,
    /// Unix seconds, bumped by `resolve_loaded` on every real data access.
    /// A pure `is_loaded` status check does **not** bump this (see
    /// `AppState::is_loaded`'s doc comment) -- deliberately, so merely
    /// checking whether a database is loaded can never itself keep it
    /// pinned in memory.
    last_accessed: AtomicU64,
}

impl LoadedDatabase {
    fn empty() -> Self {
        Self {
            dimensions: RwLock::new(DimensionTable::new()),
            cubes: RwLock::new(CubeTable::new()),
            storage: Storage::new(),
            dirty: AtomicBool::new(false),
            last_accessed: AtomicU64::new(unix_timestamp()),
        }
    }

    fn from_snapshot(mut snapshot: DatabaseSnapshot, storage_data: HashMap<CubeId, CubePartition>) -> Self {
        // Mirrors Storage::from_snapshot's own hot::HotTier::rebuild_index
        // call just below -- DimensionTable's by_wire_id index is
        // #[serde(skip)]'d the same way, for the same reason.
        snapshot.dimensions.rebuild_wire_id_indices();
        Self {
            dimensions: RwLock::new(snapshot.dimensions),
            cubes: RwLock::new(snapshot.cubes),
            storage: Storage::from_snapshot(storage_data),
            dirty: AtomicBool::new(false),
            last_accessed: AtomicU64::new(unix_timestamp()),
        }
    }

    fn touch(&self) {
        self.last_accessed.store(unix_timestamp(), Ordering::Relaxed);
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn idle_seconds(&self) -> u64 {
        unix_timestamp().saturating_sub(self.last_accessed.load(Ordering::Relaxed))
    }
}

/// The server state: sharded locks (see PLAN.md's "Concurrency"), with
/// per-database load/unload.
///
/// `databases` is the always-resident *registry*: every known database's
/// wire_id/name/type, whether currently loaded or not -- so
/// database listing and id/name resolution never force a
/// load. `loaded` holds one `LoadedDatabase` per currently-*loaded*
/// database; a caller resolves a specific database's live tables via
/// `resolve_loaded` (which transparently loads it from disk on first
/// access, or on re-access after an idle-timeout eviction) rather than
/// reaching into a shared global table directly.
///
/// Fixed lock-acquisition order everywhere a handler needs more than one:
/// **databases → (resolve_loaded) → dimensions → cubes → storage**. No
/// handler ever needs to go backwards; this is the only thing that
/// matters for avoiding lock-order deadlocks.
#[derive(Clone)]
pub struct AppState {
    pub databases: Arc<RwLock<DatabaseTable>>,
    pub loaded: Arc<RwLock<HashMap<DatabaseId, Arc<LoadedDatabase>>>>,
    /// `wire_id -> path` for every database with a file on disk --
    /// built once at `load_from_dir` by reading each file's own embedded
    /// `DatabaseIdentity` (never by trusting its name), and kept current
    /// on every save/`forget`. This is what makes file *discovery*
    /// independent of file *naming*: a database's file is found by what
    /// the file says about itself, never by recomputing a name from
    /// `wire_id`/the registry's current name and hoping it matches
    /// what's actually on disk -- a file can be renamed to anything by
    /// hand and it is still found, still by the same `wire_id`. Empty in
    /// pure in-memory mode (`data_dir: None`).
    file_index: Arc<RwLock<HashMap<u32, PathBuf>>>,
    /// `None` means fully in-memory, no persistence at all (matches
    /// `AppState::new()`'s historical behavior). `Some` is the directory
    /// `registry.bin.zst` and every per-database file live under.
    pub data_dir: Option<PathBuf>,
    /// Notifying this triggers the exact same graceful-shutdown path
    /// SIGINT/SIGTERM already drive (see `fluxolap-server::shutdown_signal`),
    /// so an API-triggered shutdown goes through the same save-if-configured
    /// sequence as a signal-triggered one, not a separate ad hoc code path.
    pub shutdown: Arc<Notify>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            databases: Arc::new(RwLock::new(DatabaseTable::new())),
            loaded: Arc::new(RwLock::new(HashMap::new())),
            file_index: Arc::new(RwLock::new(HashMap::new())),
            data_dir: None,
            shutdown: Arc::new(Notify::new()),
        }
    }

    /// Whether `dir`'s registry file exists and parses cleanly --
    /// `load_from_dir` itself never needs this (a missing/corrupt
    /// registry is recovered transparently either way, see its own doc
    /// comment), it's for a caller that wants to report the distinction
    /// -- e.g. an embedding application logging "resumed" only when a real, valid registry backs it, not merely
    /// because *a* registry file happened to be present.
    pub fn registry_loads_cleanly(dir: &Path) -> bool {
        let path = dir.join(REGISTRY_FILE);
        path.exists() && load_compressed::<DatabaseTable>(&path).is_ok()
    }

    /// Points this `AppState` at `dir` for persistence (creating it if
    /// missing), builds `file_index` by scanning `dir` (see
    /// `scan_file_index`), and loads the registry (`registry.bin.zst`).
    /// Never loads any individual database's data itself; that's
    /// `resolve_loaded`'s job, on demand (or a caller can eagerly loop
    /// every registry entry through it, which is what `fluxolap-server`'s
    /// `main.rs` does, since the standalone server has no idle-unload
    /// concept at all).
    ///
    /// If the registry is missing *or* fails to parse, it is rebuilt from
    /// every per-database file's own embedded identity instead of just
    /// starting empty (see `rebuild_registry_from_files`) -- the files
    /// themselves are the actual data; losing only the registry should
    /// never mean losing visibility into what they are. Also sanitizes
    /// whatever registry results (loaded or rebuilt): a database with no
    /// entry in `file_index` is removed (see
    /// `prune_databases_missing_their_file`), since a stale registry
    /// entry backed by nothing on disk would otherwise silently
    /// masquerade as a real, merely-empty database forever -- exactly the
    /// kind of gap this project's design otherwise goes out of its way to
    /// surface, not hide. Either kind of registry fix-up is saved back to
    /// disk immediately, not left to the next clean shutdown.
    pub fn load_from_dir(dir: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        std::fs::create_dir_all(dir)?;
        let registry_path = dir.join(REGISTRY_FILE);
        let (mut databases, file_index, rebuilt) = if registry_path.exists() {
            match load_compressed::<DatabaseTable>(&registry_path) {
                Ok(table) => (table, scan_file_index(dir), false),
                Err(e) => {
                    eprintln!("fluxolap: registry at {} is corrupt/unreadable ({e}) -- rebuilding it from per-database files", registry_path.display());
                    let (table, index) = rebuild_registry_from_files(dir);
                    (table, index, true)
                }
            }
        } else {
            let (table, index) = rebuild_registry_from_files(dir);
            (table, index, true)
        };
        let pruned = prune_databases_missing_their_file(&mut databases, &file_index);

        let state = Self {
            databases: Arc::new(RwLock::new(databases)),
            loaded: Arc::new(RwLock::new(HashMap::new())),
            file_index: Arc::new(RwLock::new(file_index)),
            data_dir: Some(dir.to_path_buf()),
            shutdown: Arc::new(Notify::new()),
        };
        for (wire_id, name) in &pruned {
            // eprintln!, not a logging facade: matches the FFI-boundary
            // convention of an embedding application, so this is actually
            // visible when fluxolap runs embedded (e.g. in a JVM),
            // not just the standalone fluxolap-server binary -- this crate
            // has no tracing-subscriber (or any subscriber) installed when
            // embedded, so tracing::warn! would be silently dropped there.
            eprintln!(
                "fluxolap: startup sanitization removed database '{name}' (wire_id {wire_id}) from the registry at {} -- \
                 its data file is missing (deleted by hand, or never saved before an unclean shutdown)",
                dir.display()
            );
        }
        if rebuilt || !pruned.is_empty() {
            state.save_registry()?;
        }
        Ok(state)
    }

    /// Saves just the registry -- cheap (wire_id/name/type metadata only,
    /// no dimension/cube/cell data), so callers write it eagerly on every
    /// database create/destroy/rename, not just at shutdown, so the
    /// registry itself survives a hard kill even though a given loaded
    /// database's in-memory changes don't (consistent with this project's
    /// "no autosave/WAL for real OLAP data" decision -- this is cheap
    /// metadata, not the data that decision was about). No-op if this
    /// `AppState` has no `data_dir` (pure in-memory mode).
    pub fn save_registry(&self) -> Result<(), Box<dyn std::error::Error>> {
        let Some(dir) = &self.data_dir else { return Ok(()) };
        let databases = self.databases.read().expect("databases lock poisoned");
        save_compressed(&*databases, &dir.join(REGISTRY_FILE))
    }

    /// Where `wire_id`'s file currently is, if it has ever been saved --
    /// resolved from `file_index`, never recomputed from `wire_id`/a
    /// name. This is the read path: if it isn't indexed, there is
    /// genuinely nothing on disk for it yet, full stop -- no fallback
    /// filename to try instead.
    fn indexed_path(&self, wire_id: u32) -> Option<PathBuf> {
        self.file_index.read().expect("file_index lock poisoned").get(&wire_id).cloned()
    }

    /// Where `wire_id` should be *written* to: its already-indexed path
    /// if it has one (so every subsequent save reuses the exact same
    /// file forever, regardless of any later rename -- a file's name is
    /// decided once, at its first save, and never revisited), or a fresh
    /// cosmetic name under `data_dir` if this is its first save ever.
    fn path_for_write(&self, wire_id: u32, name: &str) -> Option<PathBuf> {
        if let Some(path) = self.indexed_path(wire_id) {
            return Some(path);
        }
        self.data_dir.as_ref().map(|dir| dir.join(database_file_name(wire_id, name)))
    }

    /// Ensures `database_id` is loaded (from its file if one exists and
    /// it isn't already resident) and bumps its idle-access clock. Every
    /// handler that needs a specific database's dimensions/cubes/cells
    /// calls this first instead of reaching into a shared global table.
    ///
    /// `Err(FluxolapError::database_not_found())` if the registry doesn't
    /// know this id at all. `Err(FluxolapError::internal(..))` only for a
    /// genuine load failure (corrupt file, IO error) -- a *missing* file
    /// is not an error, it just means "never saved yet" and yields an
    /// empty `LoadedDatabase`, matching a freshly created database's
    /// existing immediate-usability behavior.
    ///
    /// Takes the caller's already-acquired `databases` read guard rather
    /// than locking `self.databases` itself -- every call site resolves
    /// `database_id` via that same guard just beforehand anyway, so this
    /// costs callers nothing extra, and avoids a same-thread read-lock
    /// reentrancy hazard: `std::sync::RwLock` (backed by `pthread_rwlock`
    /// on Linux) can deadlock if a thread holding a read guard tries to
    /// acquire a second read guard on the *same* lock while a writer is
    /// concurrently queued (glibc's rwlock is writer-preferring) -- a real
    /// risk here since `databases` is a genuinely hot lock (every
    /// database-scoped handler reads it).
    pub fn resolve_loaded(&self, databases: &DatabaseTable, database_id: DatabaseId) -> Result<Arc<LoadedDatabase>, FluxolapError> {
        if let Some(ld) = self.loaded.read().expect("loaded lock poisoned").get(&database_id) {
            ld.touch();
            return Ok(ld.clone());
        }
        let wire_id = databases.get(database_id).ok_or_else(FluxolapError::database_not_found)?.wire_id;
        let loaded = Arc::new(self.load_database_data(wire_id).map_err(|e| FluxolapError::internal(e.to_string()))?);
        loaded.touch();
        self.loaded.write().expect("loaded lock poisoned").insert(database_id, loaded.clone());
        Ok(loaded)
    }

    /// `Ok(LoadedDatabase::empty())`, not an error, when `wire_id` has no
    /// entry in `file_index` -- "never saved yet" (a freshly created
    /// database) and "has no file" are indistinguishable at this point
    /// and both mean the same thing: nothing to load.
    fn load_database_data(&self, wire_id: u32) -> Result<LoadedDatabase, Box<dyn std::error::Error>> {
        let Some(path) = self.indexed_path(wire_id) else { return Ok(LoadedDatabase::empty()) };
        let (_identity, snapshot, storage_data) = load_database_file(std::fs::File::open(&path)?)?;
        Ok(LoadedDatabase::from_snapshot(snapshot, storage_data))
    }

    /// Registers a brand-new database as immediately loaded -- nothing on
    /// disk for it yet, so there's nothing to load. Call right after
    /// `DatabaseTable::create` succeeds; starts `dirty = false` (an empty,
    /// never-modified database has nothing worth saving until its first
    /// real mutation marks it dirty).
    pub fn insert_new_loaded(&self, database_id: DatabaseId) {
        self.loaded.write().expect("loaded lock poisoned").insert(database_id, Arc::new(LoadedDatabase::empty()));
    }

    /// Saves `database_id`'s current data to its file if loaded and dirty.
    /// No-op if unloaded, not dirty, or this `AppState` has no `data_dir`.
    /// Writes `DatabaseIdentity` as the file's own leading, self-
    /// contained record of what it is (`path_for_write`'s doc comment) --
    /// not derived from or checked against the path being written to.
    pub fn save_if_dirty(&self, database_id: DatabaseId) -> Result<(), Box<dyn std::error::Error>> {
        let Some(loaded) = self.loaded.read().expect("loaded lock poisoned").get(&database_id).cloned() else { return Ok(()) };
        if !loaded.dirty.load(Ordering::Relaxed) {
            return Ok(());
        }
        let identity = {
            let databases = self.databases.read().expect("databases lock poisoned");
            let db = databases.get(database_id).ok_or("database not found in registry")?;
            DatabaseIdentity { wire_id: db.wire_id, name: db.name.clone(), db_type: db.db_type }
        };
        let Some(path) = self.path_for_write(identity.wire_id, &identity.name) else { return Ok(()) };
        let snapshot = DatabaseSnapshot {
            dimensions: loaded.dimensions.read().expect("dimensions lock poisoned").clone(),
            cubes: loaded.cubes.read().expect("cubes lock poisoned").clone(),
        };
        let storage_data = loaded.storage.snapshot();
        save_database_file(&(identity.clone(), snapshot, storage_data), std::fs::File::create(&path)?)?;
        self.file_index.write().expect("file_index lock poisoned").insert(identity.wire_id, path);
        loaded.dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Unloads `database_id` (saving first if dirty), freeing its
    /// `DimensionTable`/`CubeTable`/`Storage` from memory. No-op if not
    /// currently loaded.
    pub fn unload(&self, database_id: DatabaseId) -> Result<(), Box<dyn std::error::Error>> {
        self.save_if_dirty(database_id)?;
        self.loaded.write().expect("loaded lock poisoned").remove(&database_id);
        Ok(())
    }

    /// Idle-timeout check: if `database_id` is loaded and has been
    /// idle at least `expiry_minutes`, unloads it (saving first if dirty)
    /// and returns `true`. Returns `false` without touching anything if
    /// not loaded, or loaded but not yet idle long enough. A negative
    /// `expiry_minutes` never evicts (treated as "no timeout").
    pub fn check_unload(&self, database_id: DatabaseId, expiry_minutes: i64) -> Result<bool, Box<dyn std::error::Error>> {
        if expiry_minutes < 0 {
            return Ok(false);
        }
        let idle_secs = {
            let guard = self.loaded.read().expect("loaded lock poisoned");
            match guard.get(&database_id) {
                Some(loaded) => loaded.idle_seconds(),
                None => return Ok(false),
            }
        };
        if idle_secs < (expiry_minutes as u64).saturating_mul(60) {
            return Ok(false);
        }
        self.unload(database_id)?;
        Ok(true)
    }

    /// Whether `database_id` is currently loaded. A pure status check --
    /// does not bump the idle-access clock (see `LoadedDatabase::last_accessed`'s
    /// doc comment).
    pub fn is_loaded(&self, database_id: DatabaseId) -> bool {
        self.loaded.read().expect("loaded lock poisoned").contains_key(&database_id)
    }

    /// Removes `database_id`'s load state and on-disk file entirely -- no
    /// save (the whole point is deletion). Does **not** touch the
    /// registry itself; callers remove the `DatabaseTable` entry
    /// separately. This is what makes `database_destroy` cascade
    /// correctly (destroying a database must not leave its
    /// dimensions/cubes/cells orphaned). Removes `wire_id` from
    /// `file_index` too, so it doesn't linger claiming a file that no
    /// longer has a registry entry behind it.
    pub fn forget(&self, database_id: DatabaseId, wire_id: u32) {
        self.loaded.write().expect("loaded lock poisoned").remove(&database_id);
        if let Some(path) = self.file_index.write().expect("file_index lock poisoned").remove(&wire_id) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Saves the registry plus every currently-loaded dirty database.
    /// Used at clean shutdown (`fluxolap_save_all`) -- does not touch
    /// unloaded databases (nothing to save: by construction, nothing
    /// changed since their file was last written).
    pub fn save_all(&self) -> Result<(), Box<dyn std::error::Error>> {
        let ids: Vec<DatabaseId> = self.loaded.read().expect("loaded lock poisoned").keys().copied().collect();
        for id in ids {
            self.save_if_dirty(id)?;
        }
        self.save_registry()
    }

    /// Writes `database_id`'s *current* data to an arbitrary file `path`,
    /// outside `data_dir` and regardless of its dirty flag -- backs
    /// `IDatabase.backup(filename)`. Requires `database_id` to already be
    /// loaded (callers resolve it via `resolve_loaded` first, which loads
    /// on demand); unlike `save_if_dirty`, this never skips writing just
    /// because nothing changed, since an explicit backup request means
    /// "write it now" regardless. Leads with the source database's own
    /// `DatabaseIdentity`, the exact same self-contained shape as the
    /// internal per-database file, so a backup produced here can later be
    /// read back by [`Self::load_snapshot_from_path`] (`IConnection
    /// .restoreDatabase`) or dropped into `data_dir` under any name at
    /// all and picked up transparently on next load -- naming was never
    /// what made a file findable; its own embedded identity is.
    pub fn export_loaded_to_path(&self, database_id: DatabaseId, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        save_database_file(&self.database_file(database_id)?, std::fs::File::create(path)?)?;
        Ok(())
    }

    /// `database_id`'s current data as a per-database file's bytes -- the
    /// in-memory twin of [`Self::export_loaded_to_path`] (loading the
    /// database first if needed), for export over the network. The result
    /// round-trips through [`Self::import_database_bytes`].
    pub fn export_database_bytes(&self, database_id: DatabaseId) -> Result<Vec<u8>, FluxolapError> {
        {
            let databases = self.databases.read().expect("databases lock poisoned");
            self.resolve_loaded(&databases, database_id)?;
        }
        let file = self.database_file(database_id).map_err(|e| FluxolapError::internal(e.to_string()))?;
        save_database_file(&file, Vec::new()).map_err(|e| FluxolapError::internal(e.to_string()))
    }

    /// A loaded database as `(identity, snapshot, cells)` -- exactly what a
    /// per-database file holds.
    fn database_file(&self, database_id: DatabaseId) -> Result<DatabaseFile, Box<dyn std::error::Error>> {
        let loaded = self.loaded.read().expect("loaded lock poisoned").get(&database_id).cloned().ok_or("database is not loaded")?;
        let identity = {
            let databases = self.databases.read().expect("databases lock poisoned");
            let db = databases.get(database_id).ok_or("database not found in registry")?;
            DatabaseIdentity { wire_id: db.wire_id, name: db.name.clone(), db_type: db.db_type }
        };
        let snapshot = DatabaseSnapshot {
            dimensions: loaded.dimensions.read().expect("dimensions lock poisoned").clone(),
            cubes: loaded.cubes.read().expect("cubes lock poisoned").clone(),
        };
        Ok((identity, snapshot, loaded.storage.snapshot()))
    }

    /// Reads the `(DatabaseSnapshot, HashMap<CubeId, CubePartition>)` pair
    /// previously written by [`Self::export_loaded_to_path`] (or copied
    /// out of `data_dir`) from an arbitrary file `path` -- backs
    /// `IConnection.restoreDatabase(name, type, filename)`. The file's
    /// leading `DatabaseIdentity` is read and discarded here: the
    /// destination database's identity is whatever the caller already
    /// created it with (a deliberate restore into a possibly-differently-
    /// named database), not whatever the source happened to be called.
    /// Does not touch `self` at all (no registry entry, no `loaded`
    /// insertion); pair with [`Self::insert_loaded_from_snapshot`] once
    /// the caller has created the destination database.
    pub fn load_snapshot_from_path(path: &Path) -> Result<(DatabaseSnapshot, HashMap<CubeId, CubePartition>), Box<dyn std::error::Error>> {
        let (_identity, snapshot, storage_data) = load_database_file(std::fs::File::open(path)?)?;
        Ok((snapshot, storage_data))
    }

    /// Installs `snapshot`/`storage_data` (from [`Self::load_snapshot_from_path`])
    /// as `database_id`'s loaded state, replacing whatever (if anything) was
    /// there -- the second half of `IConnection.restoreDatabase`, called
    /// right after creating the destination database's registry entry.
    /// Marks the result dirty so a later `save_all`/idle-unload actually
    /// persists it under `data_dir`, exactly like any other freshly-
    /// mutated database (an imported database has never been written to
    /// its own `db-<wire_id>-<name>.bin.zst` file yet).
    pub fn insert_loaded_from_snapshot(&self, database_id: DatabaseId, mut snapshot: DatabaseSnapshot, storage_data: HashMap<CubeId, CubePartition>) {
        // The snapshot's dimensions/cubes still carry whatever DatabaseId
        // they were exported under -- always different from the fresh id
        // the destination database was just created with. Every lookup
        // (find_by_name/find_by_wire_id/list_in_database) filters by
        // database_id, so without this every one of them would find
        // nothing in the restored database. See retarget_database_id's
        // doc comment.
        snapshot.dimensions.retarget_database_id(database_id);
        snapshot.cubes.retarget_database_id(database_id);
        let loaded = LoadedDatabase::from_snapshot(snapshot, storage_data);
        loaded.mark_dirty();
        self.loaded.write().expect("loaded lock poisoned").insert(database_id, Arc::new(loaded));
    }

    /// Imports a per-database file (from `data_dir` elsewhere, a backup, or
    /// the shipped example databases) as a new database under the name the
    /// file carries and a fresh id, so it can't collide with an existing
    /// database's id. Returns `None`, importing nothing, if a database
    /// with that name already exists -- which makes importing the same
    /// files on every start idempotent. Never writes to `path`; with a
    /// `data_dir`, the imported database is saved there like any other.
    pub fn import_database_file(&self, path: &Path) -> Result<Option<DatabaseId>, Box<dyn std::error::Error>> {
        let file = load_database_file(std::fs::File::open(path)?)?;
        match self.import_parts(file) {
            Ok(id) => Ok(Some(id)),
            Err(e) if e.code == ErrorCode::DATABASE_NAME_IN_USE => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Imports a per-database file's bytes (e.g. uploaded over the network)
    /// like [`Self::import_database_file`], except that an existing
    /// database with the file's name is an error (`DATABASE_NAME_IN_USE`)
    /// rather than a silent skip. Bytes that aren't a database file are
    /// `ILLEGAL_PARAMETER_VALUE`.
    pub fn import_database_bytes(&self, bytes: &[u8]) -> Result<DatabaseId, FluxolapError> {
        let file = load_database_file(bytes)
            .map_err(|e| FluxolapError::new(ErrorCode::ILLEGAL_PARAMETER_VALUE, "invalid database file", format!("not a fluxolap database file: {e}")))?;
        self.import_parts(file)
    }

    /// Imports a per-database file's bytes under `name` instead of the name
    /// the file carries. If a database called `name` exists, it is an error
    /// (`DATABASE_NAME_IN_USE`) unless `replace` is set, which swaps that
    /// database's contents in place: it keeps its id, and requests that are
    /// already running keep reading the old contents until they finish.
    pub fn import_database_bytes_as(&self, bytes: &[u8], name: &str, replace: bool) -> Result<DatabaseId, FluxolapError> {
        let (mut identity, snapshot, storage_data) = load_database_file(bytes)
            .map_err(|e| FluxolapError::new(ErrorCode::ILLEGAL_PARAMETER_VALUE, "invalid database file", format!("not a fluxolap database file: {e}")))?;
        identity.name = name.to_string();
        let existing = self.databases.read().expect("databases lock poisoned").find_by_name(name);
        match existing {
            Some(id) if replace => {
                self.insert_loaded_from_snapshot(id, snapshot, storage_data);
                Ok(id)
            }
            _ => self.import_parts((identity, snapshot, storage_data)),
        }
    }

    fn import_parts(&self, (identity, snapshot, storage_data): DatabaseFile) -> Result<DatabaseId, FluxolapError> {
        let mut databases = self.databases.write().expect("databases lock poisoned");
        let id = databases.create(&identity.name, identity.db_type).map_err(crate::ops::map_database_error)?;
        drop(databases);
        self.insert_loaded_from_snapshot(id, snapshot, storage_data);
        self.save_registry().map_err(|e| FluxolapError::internal(e.to_string()))?;
        Ok(id)
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// The name of the database in a per-database file (`.bin.zst`), from
/// the file's leading `DatabaseIdentity` alone -- a fraction of the file is
/// decompressed. `None` if it isn't a database file.
pub fn read_database_name(path: &Path) -> Option<String> {
    load_compressed::<DatabaseIdentity>(path).ok().map(|identity| identity.name)
}

/// What a per-database file holds, in order. After it the file has two more
/// bincode values, the cubes' and the dimensions' modification times
/// (`save_database_file`).
type DatabaseFile = (DatabaseIdentity, DatabaseSnapshot, HashMap<CubeId, CubePartition>);

/// Writes a per-database file: the `DatabaseFile` tuple, then a
/// `HashMap<CubeId, u64>` of the cubes' modification times
/// (`CubePartition::modified`, unix seconds; cubes without one are left out),
/// then a `HashMap<DimensionId, u64>` of the dimensions'
/// (`Dimension::modified`, likewise). The times are sections of their own
/// behind the tuple, not fields of `CubePartition`/`Dimension`: bincode has no
/// optional fields, so a new field would make every file written before it
/// unreadable, while a reader that stops after the tuple or after the cubes'
/// times (an older version, `rebuild_registry_from_files`) never sees a
/// trailing value. A further section goes behind the last one, never between.
fn save_database_file<W: std::io::Write>(file: &DatabaseFile, writer: W) -> Result<W, Box<dyn std::error::Error>> {
    let modified: HashMap<CubeId, u64> = file.2.iter().filter(|(_, partition)| partition.modified() != 0).map(|(&id, partition)| (id, partition.modified())).collect();
    let mut bytes = bincode::serialize(file)?;
    bincode::serialize_into(&mut bytes, &modified)?;
    let dimensions_modified: HashMap<DimensionId, u64> = file.1.dimensions.iter().filter(|(_, dim)| dim.modified() != 0).map(|(id, dim)| (id, dim.modified())).collect();
    bincode::serialize_into(&mut bytes, &dimensions_modified)?;
    let mut encoder = zstd::Encoder::new(writer, 0)?;
    encoder.write_all(&bytes)?;
    Ok(encoder.finish()?)
}

/// Reads a per-database file written by `save_database_file` and puts the
/// modification times back into the partitions and dimensions. A file that
/// ends after the tuple (written before the times existed) loads with every
/// time unknown; one that ends after the cubes' times (written before the
/// dimensions' existed) with the dimensions' times unknown.
fn load_database_file<R: std::io::Read>(reader: R) -> Result<DatabaseFile, Box<dyn std::error::Error>> {
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, zstd::Decoder::new(reader)?);
    let mut file: DatabaseFile = bincode::deserialize_from(&mut reader)?;
    if let Ok(modified) = bincode::deserialize_from::<_, HashMap<CubeId, u64>>(&mut reader) {
        for (id, unix_seconds) in modified {
            if let Some(partition) = file.2.get_mut(&id) {
                partition.set_modified(unix_seconds);
            }
        }
        if let Ok(modified) = bincode::deserialize_from::<_, HashMap<DimensionId, u64>>(&mut reader) {
            for (id, unix_seconds) in modified {
                if let Some(dim) = file.1.dimensions.get_mut(id) {
                    dim.set_modified(unix_seconds);
                }
            }
        }
    }
    Ok(file)
}

fn save_compressed<T: serde::Serialize>(value: &T, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    save_compressed_to(value, std::fs::File::create(path)?)?;
    Ok(())
}

/// Writes one bincode value, zstd-compressed, to `writer`, and hands the writer back.
fn save_compressed_to<T: serde::Serialize, W: std::io::Write>(value: &T, writer: W) -> Result<W, Box<dyn std::error::Error>> {
    let bytes = bincode::serialize(value)?;
    let mut encoder = zstd::Encoder::new(writer, 0)?;
    encoder.write_all(&bytes)?;
    Ok(encoder.finish()?)
}

/// Reads one bincode value from a zstd file. The `BufReader` is load-bearing: bincode pulls a
/// struct field at a time (an integer, a length prefix, ...) through `Read`, and a `zstd::Decoder`
/// has a fixed cost per `read` call, so feeding it unbuffered turned a database with tens of
/// millions of small fields into tens of millions of decoder calls -- loading a real 115 MB
/// database took 17 s unbuffered versus 1.7 s buffered, identical bytes. The buffer
/// is small enough that reading just a file's leading `DatabaseIdentity` (`scan_file_index`)
/// still decompresses only a fraction of a block.
fn load_compressed<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Box<dyn std::error::Error>> {
    load_compressed_from(std::fs::File::open(path)?)
}

/// `load_compressed` from any reader (a file, or bytes already in memory).
fn load_compressed_from<T: serde::de::DeserializeOwned, R: std::io::Read>(reader: R) -> Result<T, Box<dyn std::error::Error>> {
    let decoder = zstd::Decoder::new(reader)?;
    Ok(bincode::deserialize_from(std::io::BufReader::with_capacity(256 * 1024, decoder))?)
}
