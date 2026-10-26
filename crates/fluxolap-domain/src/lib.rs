pub mod cube;
pub mod database;
pub mod dimension;
pub mod error;
pub mod ids;

use cube::CubeTable;
use dimension::DimensionTable;
use std::time::{SystemTime, UNIX_EPOCH};

pub use error::DomainError;
pub use ids::{CubeId, DatabaseId, DimensionId, ElementId};

/// One loaded database's plain, directly-`Serialize`/`Deserialize`-able
/// snapshot -- **not** part of the live request path. With
/// per-database load/unload, `AppState` holds one `DatabaseTable`
/// registry (always resident, every known database whether loaded or
/// not) plus a `LoadedDatabase` (its own `DimensionTable`/`CubeTable`/
/// `Storage`) per currently-*loaded* database (`fluxolap-engine`'s
/// `state.rs`) -- this struct is the bincode-(de)serialized shape one
/// loaded database's dimensions/cubes round-trip through when its file
/// is saved/loaded, paired alongside its own `Storage` snapshot
/// (`HashMap<CubeId, CubePartition>`, kept separate since `fluxolap-domain`
/// doesn't depend on `fluxolap-storage`) in the same file.
///
/// Deliberately does **not** include `DatabaseTable`/the registry --
/// unlike a single whole-process state
/// (which would bundle `databases`/`dimensions`/`cubes` together for one
/// whole-process save), the registry is now a separate, always-resident,
/// top-level structure saved on its own (`registry.bin.zst`) independent
/// of any individual database's load/unload state.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct DatabaseSnapshot {
    pub dimensions: DimensionTable,
    pub cubes: CubeTable,
}

impl DatabaseSnapshot {
    pub fn new() -> Self {
        Self::default()
    }
}

/// A per-database file's self-contained identity -- written as the first
/// value in the file, ahead of the (potentially large) `DatabaseSnapshot`
/// and cell storage that follow it, specifically so a reader can decode
/// just this and stop, without paying to decompress/deserialize the rest
/// (`fluxolap-engine::state`'s `load_compressed_prefix`). This is
/// what makes file *discovery* independent of file *naming*: which
/// `wire_id`/name a file holds is determined by reading the file, never
/// by parsing or trusting whatever it happens to be called on disk --
/// `data_dir` can be scanned cold (registry lost, or just at startup) and
/// every file identifies itself. Deliberately excludes `Database`'s
/// `status`/`token` fields (`database::Database` has those too) -- both
/// are registry-managed, transient, and never meaningful to reconstruct
/// from a single file in isolation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DatabaseIdentity {
    pub wire_id: u32,
    pub name: String,
    pub db_type: u32,
}

/// Seconds since the Unix epoch (when a database was last used). Panics
/// only if the system clock is set before 1970, which is not a case worth
/// a `Result` for.
pub fn unix_timestamp() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock before Unix epoch").as_secs()
}
