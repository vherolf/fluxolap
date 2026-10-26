//! The REST API's request/response bodies, shared by the server
//! (`fluxolap-api`) and clients (`fluxoterm`), so both sides agree by
//! construction. Resources are identified by their numeric
//! `id`; wherever a request *refers* to an element or dimension, it may
//! give either the id or the name (`Ref`). Enums are lowercase strings.

#[cfg(feature = "convert")]
use fluxolap_domain::cube::{Cube, CUBE_TYPE_ATTRIBUTE, CUBE_TYPE_NORMAL, CUBE_TYPE_SYSTEM, CUBE_TYPE_USER_INFO};
#[cfg(feature = "convert")]
use fluxolap_domain::database::{Database, DB_TYPE_NORMAL, DB_TYPE_USER_INFO};
#[cfg(feature = "convert")]
use fluxolap_domain::dimension::{self, Dimension, DIM_TYPE_ATTRIBUTE, DIM_TYPE_NORMAL, DIM_TYPE_SYSTEM, DIM_TYPE_USER_INFO};
#[cfg(feature = "convert")]
use fluxolap_domain::ElementId;
#[cfg(feature = "convert")]
use fluxolap_storage::{CellValue, CubeLock};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

/// An element or dimension, by id or by name.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum Ref {
    Id(u32),
    Name(String),
}

impl std::fmt::Display for Ref {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ref::Id(id) => write!(f, "{id}"),
            Ref::Name(name) => write!(f, "'{name}'"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PageQuery {
    /// Maximum number of items to return.
    pub limit: Option<usize>,
    /// Number of items to skip.
    #[serde(default)]
    pub offset: usize,
}

impl PageQuery {
    /// Applies `offset`/`limit` to `items`, returning the page and the
    /// total before paging.
    pub fn apply<T>(&self, items: Vec<T>) -> (Vec<T>, usize) {
        let total = items.len();
        let items = items.into_iter().skip(self.offset).take(self.limit.unwrap_or(usize::MAX)).collect();
        (items, total)
    }
}

macro_rules! list_type {
    ($name:ident, $item:ty) => {
        /// One page of a collection.
        #[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
        pub struct $name {
            pub items: Vec<$item>,
            /// Number of matching items across all pages.
            pub total: usize,
        }
    };
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// RFC 9457 problem details, with three extension members: `code` (a
/// stable machine-readable error name), `error_code` (the engine's
/// numeric error code, when the engine raised it) and `index` (for batch
/// requests, the position of the item that failed).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Problem {
    /// Always `about:blank`: `title` is the HTTP status phrase.
    #[serde(rename = "type")]
    #[schema(example = "about:blank")]
    pub problem_type: String,
    #[schema(example = "Not Found")]
    pub title: String,
    #[schema(example = 404)]
    pub status: u16,
    #[schema(example = "database not found")]
    pub detail: String,
    #[schema(example = "database_not_found")]
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 2001)]
    pub error_code: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseType {
    #[default]
    Normal,
    UserInfo,
}

#[cfg(feature = "convert")]
impl DatabaseType {
    pub fn raw(self) -> u32 {
        match self {
            Self::Normal => DB_TYPE_NORMAL,
            Self::UserInfo => DB_TYPE_USER_INFO,
        }
    }

    pub fn from_raw(raw: u32) -> Self {
        if raw == DB_TYPE_USER_INFO { Self::UserInfo } else { Self::Normal }
    }
}

/// Type of a dimension or cube. `system` and `attribute` ones are
/// created automatically (see the `#_DIMENSION_` system dimension and
/// each normal dimension's `#_<name>_` attribute dimension and cube).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObjectType {
    #[default]
    Normal,
    System,
    Attribute,
    UserInfo,
}

#[cfg(feature = "convert")]
impl ObjectType {
    pub fn dimension_raw(self) -> u32 {
        match self {
            Self::Normal => DIM_TYPE_NORMAL,
            Self::System => DIM_TYPE_SYSTEM,
            Self::Attribute => DIM_TYPE_ATTRIBUTE,
            Self::UserInfo => DIM_TYPE_USER_INFO,
        }
    }

    pub fn cube_raw(self) -> u32 {
        match self {
            Self::Normal => CUBE_TYPE_NORMAL,
            Self::System => CUBE_TYPE_SYSTEM,
            Self::Attribute => CUBE_TYPE_ATTRIBUTE,
            Self::UserInfo => CUBE_TYPE_USER_INFO,
        }
    }

    pub fn from_dimension_raw(raw: u32) -> Self {
        match raw {
            DIM_TYPE_SYSTEM => Self::System,
            DIM_TYPE_ATTRIBUTE => Self::Attribute,
            DIM_TYPE_USER_INFO => Self::UserInfo,
            _ => Self::Normal,
        }
    }

    pub fn from_cube_raw(raw: u32) -> Self {
        match raw {
            CUBE_TYPE_SYSTEM => Self::System,
            CUBE_TYPE_ATTRIBUTE => Self::Attribute,
            CUBE_TYPE_USER_INFO => Self::UserInfo,
            _ => Self::Normal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElementType {
    #[default]
    Numeric,
    String,
    /// Has children; its cells aggregate them. Set implicitly by giving
    /// an element children.
    Consolidated,
}

#[cfg(feature = "convert")]
impl ElementType {
    pub fn raw(self) -> u32 {
        match self {
            Self::Numeric => dimension::ElementType::Numeric as u32,
            Self::String => dimension::ElementType::String as u32,
            Self::Consolidated => dimension::ElementType::Consolidated as u32,
        }
    }
}

#[cfg(feature = "convert")]
impl From<dimension::ElementType> for ElementType {
    fn from(t: dimension::ElementType) -> Self {
        match t {
            dimension::ElementType::Numeric => Self::Numeric,
            dimension::ElementType::String => Self::String,
            dimension::ElementType::Consolidated => Self::Consolidated,
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ServerDto {
    #[schema(example = "fluxolap")]
    pub name: String,
    #[schema(example = "15.0.0")]
    pub version: String,
}

/// What the server is using and doing (`GET /stats`). Counters run since
/// the server started; a client that wants rates (requests per second,
/// average latency) takes two readings and divides the differences.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ServerStats {
    #[schema(example = "21.0.0")]
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    #[schema(example = "linux")]
    pub os: String,
    #[schema(example = "x86_64")]
    pub arch: String,
    /// CPU cores available to the server.
    pub cpus: usize,
    /// Where databases are saved (`FLUXOLAP_DATA_DIR`); none: in memory only.
    pub data_dir: Option<String>,
    pub memory: MemoryStats,
    pub requests: RequestStats,
    pub totals: TotalStats,
    /// Every database, loaded or not.
    pub databases: Vec<DatabaseStats>,
}

/// Memory, in bytes. A value the platform doesn't report is missing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct MemoryStats {
    /// The server process's resident memory (RSS).
    pub rss_bytes: Option<u64>,
    /// The most resident memory the process has had.
    pub peak_rss_bytes: Option<u64>,
    /// A limit on the process's memory, e.g. a container's (cgroup) limit.
    pub limit_bytes: Option<u64>,
    /// The machine's memory.
    pub system_total_bytes: Option<u64>,
    /// The machine's memory still available.
    pub system_available_bytes: Option<u64>,
    /// Estimated memory of all stored cells (the sum over the cubes'
    /// `hot_bytes` and `cold_bytes`); the rest of `rss_bytes` is the
    /// dimensions, the program and the allocator's overhead.
    pub data_bytes: u64,
}

/// API requests (under `/api/v1`, except `/stats` itself and the docs).
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct RequestStats {
    pub total: u64,
    /// Answered with 4xx.
    pub client_errors: u64,
    /// Answered with 5xx.
    pub server_errors: u64,
    /// Being answered right now.
    pub in_flight: u64,
    /// Time spent answering all requests, in microseconds.
    pub busy_micros: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct TotalStats {
    pub databases: usize,
    pub cubes: usize,
    pub dimensions: usize,
    pub elements: usize,
    pub cells: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DatabaseStats {
    pub id: u32,
    pub name: String,
    /// Whether the database is in memory; the other numbers are 0 if not.
    pub loaded: bool,
    pub dimensions: usize,
    pub elements: usize,
    pub cells: usize,
    /// Estimated memory of its cells.
    pub bytes: u64,
    pub cubes: Vec<CubeStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CubeStats {
    pub id: u32,
    pub name: String,
    #[serde(rename = "type")]
    pub cube_type: ObjectType,
    /// Number of dimensions.
    pub dimensions: usize,
    pub cells: usize,
    /// Cells in the write-optimized (hot) tier ...
    pub hot_cells: usize,
    /// ... and in the compressed (cold) one. A cell overwritten since the
    /// last compaction is in both.
    pub cold_cells: usize,
    /// Estimated memory of each tier.
    pub hot_bytes: u64,
    pub cold_bytes: u64,
    pub locks: usize,
}

// ---------------------------------------------------------------------------
// Databases
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DatabaseDto {
    pub id: u32,
    pub name: String,
    #[serde(rename = "type")]
    pub db_type: DatabaseType,
}

#[cfg(feature = "convert")]
impl DatabaseDto {
    pub fn new(db: &Database) -> Self {
        Self { id: db.wire_id, name: db.name.clone(), db_type: DatabaseType::from_raw(db.db_type) }
    }
}

list_type!(DatabaseList, DatabaseDto);

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DatabaseCreate {
    pub name: String,
    #[serde(default, rename = "type")]
    pub db_type: DatabaseType,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DatabaseUpdate {
    pub name: Option<String>,
}

/// A database file the server offers to import: one of its example
/// databases (`GET /examples`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ExampleDto {
    /// The file's name; `POST /examples/{file}/import` imports it.
    #[schema(example = "db-3-Climate.bin.zst")]
    pub file: String,
    /// The name of the database in the file.
    #[schema(example = "Climate")]
    pub name: String,
    /// The file's size in bytes.
    pub size: u64,
    /// Whether the server has a database with this name already (importing
    /// it again would be a conflict).
    pub loaded: bool,
}

list_type!(ExampleList, ExampleDto);

#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct NameQuery {
    /// Only the item with exactly this name.
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// Dimensions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DimensionDto {
    pub id: u32,
    pub name: String,
    #[serde(rename = "type")]
    pub dim_type: ObjectType,
    pub element_count: usize,
    /// When the dimension last changed (created, renamed, elements or
    /// consolidations changed), in seconds since the Unix epoch. `null` if
    /// unknown: last changed by a version that didn't record it.
    #[serde(default)]
    pub modified: Option<u64>,
}

#[cfg(feature = "convert")]
impl DimensionDto {
    pub fn new(dim: &Dimension) -> Self {
        Self {
            id: dim.wire_id,
            name: dim.name.clone(),
            dim_type: ObjectType::from_dimension_raw(dim.dim_type),
            element_count: dim.element_count(),
            modified: (dim.modified() != 0).then_some(dim.modified()),
        }
    }
}

list_type!(DimensionList, DimensionDto);

#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DimensionQuery {
    /// Only the dimension with exactly this name.
    pub name: Option<String>,
    /// Only dimensions of this type.
    #[serde(rename = "type")]
    #[param(inline)]
    pub dim_type: Option<ObjectType>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DimensionCreate {
    pub name: String,
    #[serde(default, rename = "type")]
    pub dim_type: ObjectType,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DimensionUpdate {
    /// New name; the dimension's attribute dimension/cube and system
    /// element are renamed along with it.
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// Elements
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChildDto {
    pub id: u32,
    pub weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ElementDto {
    pub id: u32,
    pub name: String,
    #[serde(rename = "type")]
    pub elem_type: ElementType,
    pub position: u32,
    /// Height above the leaves: 0 for a leaf, else 1 + the highest
    /// child's level.
    pub level: u32,
    pub parents: Vec<u32>,
    pub children: Vec<ChildDto>,
}

#[cfg(feature = "convert")]
impl ElementDto {
    pub fn new(dim: &Dimension, id: ElementId) -> Self {
        let el = dim.get_element(id).expect("element exists");
        let wire_id = |eid: ElementId| dim.get_element(eid).map_or(0, |e| e.wire_id);
        Self {
            id: el.wire_id,
            name: el.name.clone(),
            elem_type: el.elem_type.into(),
            position: el.position,
            level: dim.level(id),
            parents: dim.parents_of(id).iter().map(|&p| wire_id(p)).collect(),
            children: dim.children_of(id).iter().map(|&(c, weight)| ChildDto { id: wire_id(c), weight }).collect(),
        }
    }
}

list_type!(ElementList, ElementDto);

#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ElementQuery {
    /// Only the element with exactly this name.
    pub name: Option<String>,
    /// Only elements of this type.
    #[serde(rename = "type")]
    #[param(inline)]
    pub elem_type: Option<ElementType>,
    /// Only top-level elements (without a parent) -- e.g. a dimension's
    /// totals, to pick a default without loading every element.
    #[serde(default)]
    pub roots: bool,
    /// Maximum number of elements to return.
    pub limit: Option<usize>,
    /// Number of elements to skip (elements are ordered by position).
    #[serde(default)]
    pub offset: usize,
}

/// A child reference with its consolidation weight.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ChildRef {
    /// The child element, by id or name. Names may refer to elements
    /// created earlier in the same batch.
    pub element: Ref,
    #[serde(default = "one")]
    #[schema(default = 1.0)]
    pub weight: f64,
}

fn one() -> f64 {
    1.0
}

/// One element to create (`POST`) or create-or-update by name (`PUT`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementWrite {
    pub name: String,
    /// Ignored when `children` is non-empty: an element with children is
    /// always `consolidated`.
    #[serde(default, rename = "type")]
    pub elem_type: ElementType,
    /// The element's complete child list (replaces any existing one).
    pub children: Option<Vec<ChildRef>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementWriteBatch {
    pub items: Vec<ElementWrite>,
}

/// Changes to one element; every field is optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementPatch {
    pub name: Option<String>,
    /// `numeric` or `string`. A consolidated element only changes type by
    /// losing its children.
    #[serde(rename = "type")]
    pub elem_type: Option<ElementType>,
    /// New position; other elements shift to keep positions dense.
    pub position: Option<u32>,
    /// Replaces the complete child list (`[]` makes the element a leaf).
    pub children: Option<Vec<ChildRef>>,
    /// Adds children (or updates their weights), keeping existing ones.
    pub add_children: Option<Vec<ChildRef>>,
}

/// One element update in a batch: which element, and its changes.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementUpdate {
    /// The element to change, by id or (current) name.
    pub element: Ref,
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub elem_type: Option<ElementType>,
    pub position: Option<u32>,
    pub children: Option<Vec<ChildRef>>,
    pub add_children: Option<Vec<ChildRef>>,
}

impl ElementUpdate {
    pub fn split(self) -> (Ref, ElementPatch) {
        let patch = ElementPatch { name: self.name, elem_type: self.elem_type, position: self.position, children: self.children, add_children: self.add_children };
        (self.element, patch)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementUpdateBatch {
    pub items: Vec<ElementUpdate>,
}

/// Elements to delete, by id or name.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementDeleteBatch {
    pub items: Vec<Ref>,
}

#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ElementClearQuery {
    /// Must be `true`: deletes every element of the dimension and clears
    /// every cube that uses it. Required so a stray `DELETE` on the
    /// collection can't wipe a dimension.
    #[serde(default)]
    pub all: bool,
}

// ---------------------------------------------------------------------------
// Cubes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CubeDto {
    pub id: u32,
    pub name: String,
    #[serde(rename = "type")]
    pub cube_type: ObjectType,
    /// Dimension ids, in coordinate order.
    pub dimensions: Vec<u32>,
    /// Number of stored (non-empty base) cells.
    pub cell_count: usize,
    /// When the cube's cells last changed (a write, a clear, a rollback), in
    /// seconds since the Unix epoch. `null` if unknown: never written, or last
    /// written by a version that didn't record it.
    #[serde(default)]
    pub modified: Option<u64>,
}

#[cfg(feature = "convert")]
impl CubeDto {
    /// `modified` as the storage gives it (`Storage::modified`, 0 = unknown).
    pub fn new(dimensions: &dimension::DimensionTable, cube: &Cube, cell_count: usize, modified: u64) -> Self {
        let dims = cube.dimensions.iter().map(|&d| dimensions.get(d).map_or(0, |d| d.wire_id)).collect();
        Self {
            id: cube.wire_id,
            name: cube.name.clone(),
            cube_type: ObjectType::from_cube_raw(cube.cube_type),
            dimensions: dims,
            cell_count,
            modified: (modified != 0).then_some(modified),
        }
    }
}

list_type!(CubeList, CubeDto);

#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CubeQuery {
    /// Only the cube with exactly this name.
    pub name: Option<String>,
    /// Only cubes of this type.
    #[serde(rename = "type")]
    #[param(inline)]
    pub cube_type: Option<ObjectType>,
    /// Only cubes that use the dimension with this id.
    pub dimension: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CubeCreate {
    pub name: String,
    /// The cube's dimensions, by id or name, in coordinate order.
    pub dimensions: Vec<Ref>,
    #[serde(default, rename = "type")]
    pub cube_type: ObjectType,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CubeUpdate {
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// Cells
// ---------------------------------------------------------------------------

/// A cell value: a number or a string. `null` means empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum Value {
    Number(f64),
    Text(String),
}

#[cfg(feature = "convert")]
impl Value {
    pub fn from_cell(v: CellValue) -> Self {
        match v {
            CellValue::Numeric(n) => Self::Number(n),
            CellValue::String(s) => Self::Text(s),
        }
    }

    /// `None` (empty) becomes numeric 0, which the engine stores as "no
    /// cell" -- a leaf is cleared, a consolidated coordinate clears its
    /// whole base area.
    pub fn to_cell(v: Option<Self>) -> CellValue {
        match v {
            Some(Self::Number(n)) => CellValue::Numeric(n),
            Some(Self::Text(s)) => CellValue::String(s),
            None => CellValue::Numeric(0.0),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CellDto {
    /// Element ids, one per cube dimension.
    pub path: Vec<u32>,
    /// `null` when the cell is empty.
    pub value: Option<Value>,
}

list_type!(CellList, CellDto);

/// One page of stored cells.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CellPage {
    pub items: Vec<CellDto>,
    /// `offset` for the next page; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CellKind {
    Numeric,
    String,
}

#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CellExportQuery {
    /// Maximum number of cells to return (default 1000, at most 100000).
    pub limit: Option<usize>,
    /// Number of stored cells to skip. The order is stable while the
    /// cube isn't written to.
    #[serde(default)]
    pub offset: usize,
    /// Only numeric or only string cells.
    #[serde(rename = "type")]
    #[param(inline)]
    pub kind: Option<CellKind>,
}

/// A read of many cells: either explicit `paths`, or an `area` (one list
/// of elements per dimension, read as their cartesian product).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CellQuery {
    pub paths: Option<Vec<Vec<Ref>>>,
    pub area: Option<Vec<Vec<Ref>>>,
    /// Leave out cells without a value: a sparse area (most combinations
    /// empty) then comes back small. `total` counts the cells returned.
    /// Sent only when true, so a query without it also works against older
    /// servers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skip_empty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CellWrite {
    /// One element (id or name) per cube dimension. A consolidated
    /// element splashes the value down to its base cells.
    pub path: Vec<Ref>,
    /// `null` clears the cell (or, at a consolidated coordinate, every
    /// base cell under it).
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CellWriteBatch {
    /// Applied in order.
    pub items: Vec<CellWrite>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CellValueWrite {
    /// `null` clears the cell.
    pub value: Option<Value>,
}

// ---------------------------------------------------------------------------
// Locks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LockDto {
    pub id: u32,
    /// Locked elements per dimension; `null` for a whole-cube lock.
    pub area: Option<Vec<Vec<u32>>>,
    /// Writes journaled under this lock (what a rollback can undo).
    pub steps: usize,
}

#[cfg(feature = "convert")]
impl LockDto {
    pub fn new(dimensions: &dimension::DimensionTable, cube: &Cube, lock: &CubeLock) -> Self {
        let area = (!lock.whole).then(|| {
            lock.raw_area
                .iter()
                .zip(&cube.dimensions)
                .map(|(ids, &did)| {
                    let dim = dimensions.get(did);
                    ids.iter().filter_map(|&eid| dim.and_then(|d| d.get_element(eid)).map(|e| e.wire_id)).collect()
                })
                .collect()
        });
        Self { id: lock.wire_id, area, steps: lock.undo_log.len() }
    }
}

list_type!(LockList, LockDto);

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LockCreate {
    /// Elements per dimension to lock; omit to lock the whole cube.
    pub area: Option<Vec<Vec<Ref>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RollbackQuery {
    /// Number of most recent writes to undo; omit to undo all of them
    /// and release the lock.
    pub steps: Option<u32>,
}
