//! Cube cell storage.
//! Sparse per-cube cell maps, sharded so that different cubes' reads and
//! writes never contend with each other (see PLAN.md's
//! "Concurrency" section). `CubePartition` stores its cells as a dense
//! row `Vec` plus a point-lookup index (see `CubePartition`'s own doc
//! comment) rather than a bare `HashMap` -- a paged/columnar layout whose
//! core win is that a full-cube scan
//! (the single operation every bulk read ultimately funnels through --
//! see `iter_cube`) walks sequential memory instead of chasing hash
//! buckets. Landed once a real large-query performance investigation
//! confirmed this was worth the added complexity.
//!
//! Kept as a separate crate from `fluxolap-domain` (which owns `Cube`'s
//! metadata: name, dimension list, type, token) so the hot-path cell
//! storage can be profiled, fuzzed, or swapped for a different
//! representation independently of the CRUD-heavy domain model -- see
//! PLAN.md's Architecture section for the full rationale.

mod cold;
mod hot;

use fluxolap_domain::{unix_timestamp, CubeId, ElementId};
use hot::{CellRef, HotTier};
use smallvec::SmallVec;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

/// A stored cell's path, keyed by `ElementId` (an 8-byte `Copy` slotmap
/// key) per cube dimension. `Vec<ElementId>` was the original
/// representation, but every `Storage::cells` key is one heap allocation
/// (and every `iter_cube` scan -- the single pass every bulk read/
/// aggregation ultimately funnels through -- clones every key it
/// touches), and real cube dimension counts are almost always well under
/// 8. `SmallVec` keeps that common case entirely on the stack: no
/// allocation to build a path, no allocation to clone one out of
/// `iter_cube`. Above 8 dimensions it transparently spills to the heap,
/// same as `Vec` always did -- purely a performance change, `CellPath`'s
/// own `Deref<Target = [ElementId]>`/`Borrow<[ElementId]>` keep every
/// existing `&[ElementId]`-based call site (`Storage::get`/`get_many`,
/// `HashMap::remove`) working unchanged. Found to matter via a real
/// large-query performance investigation: this allocation churn, not raw
/// CPU work, was the best explanation for CPU utilization capping well
/// short of 100% on a large scan (allocator/memory-bandwidth bound, not
/// compute bound).
pub type CellPath = SmallVec<[ElementId; 8]>;

/// Below this many stored cells, `iter_cube`'s clone-collect just isn't
/// worth handing to rayon: spinning up/coordinating worker threads costs
/// more than a plain sequential clone of a few hundred small entries.
/// Large enough that any
/// cube actually worth parallelizing (the "very large data ops" this
/// exists for) clears it easily, small enough that ordinary interactive
/// queries never pay rayon's coordination cost for no benefit.
const PARALLEL_SCAN_THRESHOLD: usize = 1000;

/// A cell's stored value: numeric or string, like the two leaf element
/// types.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CellValue {
    Numeric(f64),
    String(String),
}

impl CellValue {
    /// The numeric type code of a cell value: 1 for numeric, 2 for string
    /// (the same codes as element types).
    pub fn wire_type(&self) -> u32 {
        match self {
            CellValue::Numeric(_) => 1,
            CellValue::String(_) => 2,
        }
    }

    /// A cell holding its type's zero value (it "exists" only if a numeric
    /// value is non-zero or a string value is non-empty).
    /// `Storage::set` treats setting a cell to its default as clearing it,
    /// so the filled-cell count only counts genuinely non-default
    /// cells.
    fn is_default(&self) -> bool {
        match self {
            CellValue::Numeric(v) => *v == 0.0,
            CellValue::String(s) => s.is_empty(),
        }
    }
}

/// The lock asked for doesn't exist on this cube (`rollback_lock`,
/// `commit_lock`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockNotFound;

/// A pessimistic lock over a cube area, with an undo log for rollback.
/// fluxolap has no authentication at all (a permanent decision, see
/// PLAN.md's non-goals), so there is no "wrong user tried to touch a
/// locked area" to enforce, and a lock has no owner.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CubeLock {
    pub wire_id: u32,
    /// `true` for a whole-cube lock -- covers every
    /// coordinate unconditionally, and (per `Cube::rollbackCube`'s
    /// "Exclusive lock can't be rolled back" check) can only be released
    /// by committing it, never by rolling it back.
    pub whole: bool,
    /// The originally requested per-dimension element lists, kept
    /// verbatim (not expanded) only so the lock endpoints'
    /// `area` result field can echo back what the client asked to lock.
    /// Empty when `whole` is true.
    pub raw_area: Vec<Vec<ElementId>>,
    /// Per dimension, the *leaf* elements this lock covers -- `raw_area`
    /// expanded through `Dimension::expand_to_base` one dimension at a
    /// time. fluxolap's storage model never holds anything but leaf cell
    /// paths in the first place, so restricting overlap/coverage
    /// checks to leaves is sufficient here; tracking ancestor coordinates
    /// too (so a write to a shared consolidated ancestor gets caught) isn't
    /// needed, since no write ever targets anything but a leaf path (`write_cell`
    /// already expands any consolidated target down to its leaves before
    /// touching `Storage`). Empty when `whole` is true.
    pub overlap_leaves: Vec<HashSet<ElementId>>,
    /// One entry per cell write this lock has journaled, oldest first:
    /// the leaf path and its value immediately before the write (`None`
    /// if the cell didn't exist yet). Rollback pops from the end
    /// and restores each value; commit just discards the log.
    pub undo_log: Vec<(Vec<ElementId>, Option<CellValue>)>,
}

impl CubeLock {
    /// Whether a leaf cell path falls within this lock's coverage --
    /// used both to decide whether a write needs journaling into this
    /// lock's undo log, and (via `areas_overlap`) whether a new lock
    /// request would conflict with this one.
    fn covers(&self, path: &[ElementId]) -> bool {
        self.whole || path.iter().zip(self.overlap_leaves.iter()).all(|(eid, set)| set.contains(eid))
    }
}

/// Whether two lock areas overlap. Two areas overlap iff **every**
/// dimension has at least one leaf element in common; a `whole` lock on
/// either side always overlaps. The classic mistake here is comparing an
/// area against itself instead of against the other lock (every new lock
/// then looks "overlapping" once any lock exists) -- the property test
/// below checks against a brute-force reference for exactly that.
pub fn areas_overlap(whole_a: bool, leaves_a: &[HashSet<ElementId>], whole_b: bool, leaves_b: &[HashSet<ElementId>]) -> bool {
    if whole_a || whole_b {
        return true;
    }
    leaves_a.iter().zip(leaves_b.iter()).all(|(a, b)| a.intersection(b).next().is_some())
}

/// A columnar (struct-of-arrays) dense row store used for both
/// `CubePartition::numeric`/`::string`: one `Vec<ElementId>` per
/// dimension -- sized to the cube's *real* dimension count, not a fixed
/// inline capacity the way the old per-row `SmallVec<[ElementId;8]>` was
/// -- plus one `Vec<T>` of values, index-aligned across all of them. A
/// 4-dimension cube's path now costs 4x8=32 bytes instead of the old
/// SmallVec's fixed 72 bytes regardless of dimension count (confirmed via
/// `membench`: bytes/cell was flat across 2/4/8-dimension cubes before
/// this change, direct proof the old inline capacity was paid for
/// unconditionally). `dim_columns` starts empty and is sized from the
/// first row ever pushed if never told explicitly (`with_dims`) -- a
/// cube's dimension count is fixed for its whole lifetime, so every
/// later push's path length is expected to match whichever established
/// it first.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ColumnarRows<T> {
    pub(crate) dim_columns: Vec<Vec<ElementId>>,
    pub(crate) values: Vec<T>,
}

impl<T> ColumnarRows<T> {
    pub(crate) fn with_dims(n_dims: usize) -> Self {
        Self { dim_columns: (0..n_dims).map(|_| Vec::new()).collect(), values: Vec::new() }
    }

    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.values.capacity()
    }

    /// Heap bytes these columns hold, by capacity (what the process has
    /// allocated, not just what is used); memory a value owns itself (a
    /// `String`'s text) is the caller's to add.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.dim_columns.iter().map(|c| c.capacity() * std::mem::size_of::<ElementId>()).sum::<usize>() + self.values.capacity() * std::mem::size_of::<T>()
    }

    pub(crate) fn path_at(&self, i: usize) -> CellPath {
        self.dim_columns.iter().map(|col| col[i]).collect()
    }

    /// Appends a new row, returning its index. If this is the very first
    /// row ever pushed (no `with_dims` call established `dim_columns`
    /// up front), sizes `dim_columns` from `path`'s length.
    pub(crate) fn push(&mut self, path: &[ElementId], value: T) -> u32 {
        if self.dim_columns.is_empty() && !path.is_empty() {
            self.dim_columns = (0..path.len()).map(|_| Vec::new()).collect();
        }
        debug_assert_eq!(self.dim_columns.len(), path.len(), "a cube's dimension count must be constant across all its cells");
        for (col, &e) in self.dim_columns.iter_mut().zip(path) {
            col.push(e);
        }
        let i = self.values.len() as u32;
        self.values.push(value);
        i
    }

    /// swap_removes row `i` (moving every column's last row into the
    /// freed slot, keeping all columns in lockstep), returning the
    /// removed value plus -- if another row got swapped into slot `i`
    /// (i.e. `i` wasn't already the last row) -- that row's *old* index
    /// (`last`, before the swap) and its path, so the caller can locate
    /// and fix up its stale `index` entry itself (its new index is simply
    /// `i`, the same slot the caller already passed in).
    pub(crate) fn swap_remove(&mut self, i: usize) -> (T, Option<(usize, CellPath)>) {
        let last = self.values.len() - 1;
        for col in &mut self.dim_columns {
            col.swap(i, last);
            col.pop();
        }
        self.values.swap(i, last);
        let value = self.values.pop().expect("just swapped a row into range, can't be empty");
        let swapped = if i < self.values.len() { Some((last, self.path_at(i))) } else { None };
        (value, swapped)
    }

    pub(crate) fn clear(&mut self) {
        for col in &mut self.dim_columns {
            col.clear();
        }
        self.values.clear();
    }

    pub(crate) fn reserve(&mut self, additional: usize) {
        for col in &mut self.dim_columns {
            col.reserve(additional);
        }
        self.values.reserve(additional);
    }

    pub(crate) fn reserve_exact(&mut self, additional: usize) {
        for col in &mut self.dim_columns {
            col.reserve_exact(additional);
        }
        self.values.reserve_exact(additional);
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        for col in &mut self.dim_columns {
            col.shrink_to_fit();
        }
        self.values.shrink_to_fit();
    }
}

/// A path's hash, used as `hot::HotTier::index`'s key instead of the
/// path itself -- see `index`'s own doc comment. `DefaultHasher` (SipHash
/// with fixed, hardcoded keys) is used deliberately instead of
/// `std::collections::HashMap`'s own `RandomState` (randomized per
/// process/instance for DoS-hardening): this hash needs to be
/// *reproducible* across separate calls within the same process (once to
/// insert a path, again later to look it up), which a randomized hasher
/// would break.
pub(crate) fn hash_path(path: &[ElementId]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

/// Above this many total (live + tombstone) hot-tier rows, a write
/// triggers a synchronous compaction (`CubePartition::compact`) folding
/// hot into cold. Bounds one cube's *peak* hot-tier memory to roughly
/// `HOT_COMPACT_TRIGGER` cells at old-format cost (~260 bytes/cell) --
/// about 520MB -- however many cells the whole cube eventually holds,
/// while also bounding total compaction work for an N-cell bulk load to
/// roughly `O(N log(N / HOT_COMPACT_TRIGGER))` (each compaction rewrites
/// the whole cold run, but doubling-ish cold-run sizes between
/// compactions keeps that from degrading to `O(N^2 / HOT_COMPACT_TRIGGER)`
/// the way compacting after every single small batch would). 2,000,000 is
/// a starting point, not yet tuned.
const HOT_COMPACT_TRIGGER: usize = 2_000_000;

/// One cube's full data: its stored cells and any active locks --
/// everything that needs to live behind the same lock so a write and its
/// lock-journaling stay atomic. `Clone` is only for
/// `Storage::snapshot()`'s benefit (persistence) -- never cloned on
/// any live request path.
///
/// Two tiers: `hot` (the mutable columnar+hash-index
/// tier, `hot::HotTier`) holds every live/recent write; `cold` (built only
/// by `compact`, never mutated in place) holds the bulk of a cube's cells,
/// sorted and front-coded/delta-compressed (`cold::ColdRun`) -- see both
/// modules' doc comments for why (classic trie-store compression techniques,
/// implemented as a simpler restart-point columnar format instead of a
/// trie, validated against real data with a prototype before this was
/// built). A path's authoritative state is
/// whichever tier answers first: hot's index (a `CellRef::Tombstone` there
/// means "definitely absent," never falls through to cold, since cold is
/// immutable and can't itself record a delete) or, failing that, cold.
/// `live_count` is maintained explicitly across both tiers so
/// `cell_count` stays O(1) rather than needing a tier-spanning scan.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct CubePartition {
    hot: HotTier,
    /// `None` until the first compaction. No `Arc` needed -- compaction is
    /// synchronous (on the same write-lock-holding thread that triggered
    /// it), so a cold run is never shared across threads the way the
    /// per-cube `Arc<RwLock<CubePartition>>` sharding shares a whole
    /// partition.
    cold: Option<cold::ColdRun>,
    /// Number of live cells across both tiers -- exact whenever `count_dirty`
    /// is false. A write that lands on a path hot doesn't know yet can't
    /// tell whether that path already exists in cold without a cold lookup
    /// (a linear decode from the nearest restart, orders of magnitude
    /// slower than the hot tier's hash lookup), so such writes only set
    /// `count_dirty` and `ensure_count` settles the number later, in one
    /// sorted batch, when something actually asks for it (or at compaction,
    /// which knows it exactly for free). Never persisted as dirty:
    /// `Storage::snapshot` settles it before a partition is saved.
    live_count: usize,
    #[serde(skip)]
    count_dirty: bool,
    /// Unix seconds of the last change to this cube's cells (a write, a clear, a
    /// rollback), `0` if none is known. Set once per `Storage` call that changes
    /// cells, not per cell. Not part of the partition's serialized form, so that
    /// files written before it existed still load: a per-database file carries
    /// these times in a trailing section of its own (`fluxolap-engine`'s
    /// `state.rs`).
    #[serde(skip)]
    modified: u64,
    pub locks: Vec<CubeLock>,
    /// Lock wire ids only need to be unique *within* their cube -- every
    /// API reference to a lock (rollback,
    /// commit) always carries `cube` alongside `lock`. Scoping
    /// the counter per-cube (rather than one Storage-wide counter) means
    /// no shared mutable state between cubes at all, matching the rest of
    /// this sharded design.
    pub next_lock_wire_id: u32,
}

impl CubePartition {
    /// Sizes the hot tier's columns from the cube's real dimension count
    /// up front -- the normal case, since a cube's dimension count is
    /// known the moment it's created (`Storage::cube_created`). Without
    /// this, a caller that reserves capacity (`Storage::reserve_cube`)
    /// before any cell has ever been written would have no columns yet to
    /// reserve into -- `ColumnarRows::push`'s lazy first-row sizing exists
    /// only as a fallback for a partition that skipped `cube_created`
    /// entirely (some tests do, relying on `Storage::partition_or_create`).
    fn with_dims(n_dims: usize) -> Self {
        Self { hot: HotTier::with_dims(n_dims), cold: None, live_count: 0, count_dirty: false, modified: 0, locks: Vec::new(), next_lock_wire_id: 0 }
    }

    /// Falls back to hot's own dimension count when this partition was
    /// never given one via `with_dims` (some tests skip `cube_created`) --
    /// still correct once any cell has been pushed, via `ColumnarRows`'s
    /// lazy first-row sizing.
    fn n_dims(&self) -> usize {
        self.hot.n_dims()
    }

    /// See the field. Public for persistence, which stores the time beside the
    /// partition.
    pub fn modified(&self) -> u64 {
        self.modified
    }

    pub fn set_modified(&mut self, unix_seconds: u64) {
        self.modified = unix_seconds;
    }

    fn touch(&mut self) {
        self.modified = unix_timestamp();
    }

    fn get_cell(&self, path: &[ElementId]) -> Option<CellValue> {
        match self.hot.index_get(path) {
            // Hot answers definitively either way -- `None` from `value_at`
            // means a tombstone, which must NOT fall through to cold (that
            // would resurrect a value hot has explicitly recorded as
            // deleted).
            Some(cell_ref) => self.hot.value_at(cell_ref),
            None => self.cold.as_ref().and_then(|c| c.get(path)),
        }
    }

    /// Batched `get_cell`: hot answers what it knows; everything else goes to
    /// cold in one sorted pass (`ColdRun::get_many_into`) rather than one
    /// restart-to-target scan per path.
    fn get_cells_many(&self, paths: &[&[ElementId]]) -> Vec<Option<CellValue>> {
        let mut out: Vec<Option<CellValue>> = vec![None; paths.len()];
        let mut for_cold = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            match self.hot.index_get(path) {
                Some(cell_ref) => out[i] = self.hot.value_at(cell_ref),
                None if self.cold.is_some() => for_cold.push(i),
                None => {}
            }
        }
        if let Some(cold) = &self.cold {
            cold.get_many_into(paths, &for_cold, &mut out);
        }
        out
    }

    /// Makes `live_count` exact if a write left it unsettled. Every hot
    /// entry (live or tombstone) is one path, so the live total is
    /// `cold cells - (hot paths that also exist in cold) + hot live cells`;
    /// the overlap is one sorted batch lookup against cold.
    fn ensure_count(&mut self) {
        if !self.count_dirty {
            return;
        }
        let hot_paths = self.hot.all_paths();
        let overlap = match &self.cold {
            Some(cold) => {
                let slices: Vec<&[ElementId]> = hot_paths.iter().map(|p| p.as_slice()).collect();
                let which: Vec<usize> = (0..slices.len()).collect();
                let mut found: Vec<Option<CellValue>> = vec![None; slices.len()];
                cold.get_many_into(&slices, &which, &mut found);
                found.iter().filter(|v| v.is_some()).count()
            }
            None => 0,
        };
        let cold_len = self.cold.as_ref().map_or(0, |c| c.len());
        self.live_count = cold_len - overlap + self.hot.live_len();
        self.count_dirty = false;
    }

    fn clear(&mut self) {
        self.hot.clear();
        self.cold = None;
        self.live_count = 0;
        self.count_dirty = false;
    }

    /// Reserves capacity for `additional` more cells in the hot tier, for
    /// a caller that doesn't know the final total ahead of time and may
    /// call this many times across a series of batches
    /// (`record_and_set_many`'s per-batch use).
    fn reserve_additional(&mut self, additional: usize) {
        self.hot.reserve(additional);
    }

    /// Like `reserve_additional`, but for a caller that knows the *true
    /// final total* up front (e.g. a migration from another server, from
    /// the source cube's filled-cell count) and calls this exactly
    /// once before any batches, rather than per-batch. `Vec::reserve_exact`
    /// avoids `reserve`'s amortized-doubling growth entirely.
    fn reserve_exact_additional(&mut self, additional: usize) {
        self.hot.reserve_exact(additional);
    }

    /// Inserts a new cell or overwrites/retypes an existing one, then
    /// checks whether this write pushed the hot tier over
    /// `HOT_COMPACT_TRIGGER`, compacting synchronously if so.
    ///
    /// Deliberately never looks in cold: whether a brand-new hot entry also
    /// replaces a cold cell only matters for `live_count`, which is settled
    /// lazily instead (see `count_dirty`). Checking here cost a cold point
    /// lookup per written cell -- ~20us each, versus ~0.5us for the write
    /// itself -- which made every incremental load into a compacted cube
    /// ~40x slower.
    fn set_cell(&mut self, path: CellPath, value: CellValue) {
        match self.hot.index_get(path.as_slice()) {
            // A live hot entry being overwritten/retyped: count unchanged.
            Some(CellRef::Numeric(_)) | Some(CellRef::Str(_)) => {}
            // Deleted earlier (so cold, or the cell before it was removed, had it): live again.
            Some(CellRef::Tombstone(_)) => self.live_count += 1,
            None if self.cold.is_none() => self.live_count += 1,
            // Unknown without a cold lookup: either a new cell or a replacement of a cold one.
            None => self.count_dirty = true,
        }
        self.hot.set_cell(path, value);
        self.maybe_compact();
    }

    /// Removes a cell if present. A cold-resident cell can't be physically
    /// deleted (cold pages are immutable) -- it's shadowed by a tombstone
    /// instead, resolved away for real the next time this cube compacts
    /// (see `cold::merge`'s `tombstoned` set). Like `set_cell`, never
    /// looks in cold: when hot has no entry for `path` and a cold tier
    /// exists, a tombstone is recorded unconditionally (harmless if cold
    /// never had the path -- compaction just drops it) and the live count
    /// is left to be settled lazily.
    fn remove_cell(&mut self, path: &[ElementId]) {
        match self.hot.index_get(path) {
            Some(CellRef::Tombstone(_)) => {}
            Some(cell_ref) => {
                // One live cell (new, or shadowing a cold one -- either way a single
                // counted cell) becomes none: exactly -1. A live hot entry can be
                // shadowing a stale cold entry for the same path, and physically removing
                // hot's copy alone would let that value resurface on the next read, so
                // with a cold tier a tombstone replaces it.
                self.hot.remove_live(path, cell_ref);
                if self.cold.is_some() {
                    self.hot.insert_tombstone(path);
                }
                // An unsettled count isn't maintained (`ensure_count` recomputes it) and may
                // already be below this cell: a hot cell set over a cold tier never counted it.
                if !self.count_dirty {
                    self.live_count -= 1;
                }
            }
            None => {
                if self.cold.is_some() {
                    self.hot.insert_tombstone(path);
                    self.count_dirty = true;
                    // Tombstones count toward the trigger like live cells: a long run of deletes
                    // of absent paths (zero-writes over mostly empty base cells, e.g. a CubeLoad
                    // DELETE fed by a generateBases CubeSlice) would otherwise grow hot forever.
                    // Compaction drops the tombstones cold never had.
                    self.maybe_compact();
                }
            }
        }
    }

    fn maybe_compact(&mut self) {
        if self.hot.total_len() >= HOT_COMPACT_TRIGGER {
            self.compact();
        }
    }

    /// Folds every hot-tier entry (live and tombstoned) into a fresh cold
    /// run merged with whatever cold run already existed, then empties hot
    /// -- the only place a `ColdRun` is ever built or replaced (never
    /// mutated in place, see `cold`'s module doc). Called opportunistically
    /// by `maybe_compact` once hot crosses `HOT_COMPACT_TRIGGER`, and
    /// unconditionally by `Storage::shrink_cube` (now a real compaction,
    /// not just capacity trimming -- see that method's doc comment) --
    /// exposed to Java callers as `ICube.compact()`/`IDimension.compact()`,
    /// which callers are expected to call liberally at natural "done for
    /// now" points (end of a bulk load, before a save) rather than sparingly,
    /// so a no-op hot tier must be a cheap no-op here too: without this
    /// early return, calling `compact` on an already-compacted cube would
    /// still fully re-encode the entire existing cold run for no logical
    /// change, an O(cold run size) waste that would make "just call compact
    /// defensively" an expensive habit instead of a safe one.
    pub(crate) fn compact(&mut self) {
        if self.hot.total_len() == 0 {
            // Nothing pending: the live count is exactly what cold holds.
            self.live_count = self.cold.as_ref().map_or(0, |c| c.len());
            self.count_dirty = false;
            return;
        }
        let n_dims = self.n_dims();
        let (mut entries, tombstoned) = self.hot.live_entries_and_tombstoned();
        entries.sort_by(|(a, _), (b, _)| cold::path_ffi_cmp(a, b));
        let merged = cold::merge(self.cold.as_ref(), &entries, &tombstoned, n_dims);
        // Compaction resolves every shadow and tombstone, so the count is now exact for free.
        self.live_count = merged.len();
        self.count_dirty = false;
        self.cold = Some(merged);
        self.hot.clear();
    }
}

/// All cube cell data (sharded per cube -- see PLAN.md's
/// "Concurrency" section for the full design rationale). The outer lock
/// is only ever held briefly: to look up + clone the `Arc` for an
/// existing cube (a plain `HashMap` lookup), or -- rarely -- to insert or
/// remove a cube's entire partition on cube create/destroy. All actual
/// cell/lock work happens under a specific cube's *own* inner lock, so
/// two different cubes' operations never contend on anything.
///
/// Every method here takes `&self`, not `&mut self` -- mutation happens
/// through interior mutability via the nested locks, which is what lets
/// `Storage` be shared as a plain `Arc<Storage>` (no outer `RwLock`
/// needed at the `AppState` level at all).
#[derive(Debug, Default)]
pub struct Storage {
    cubes: RwLock<HashMap<CubeId, Arc<RwLock<CubePartition>>>>,
}

/// See `Storage::capacity`. The `hot_*` fields are about the hot
/// tier only, which is what capacity-management tooling actually cares about --
/// cold is immutable and rebuilt wholesale by compaction, so it has no
/// "unused capacity" concept of its own); `cold_len`/`cold_stream_bytes`
/// report the cold run's logical cell count and encoded byte size, the
/// two numbers needed to compute real bytes/cell for the compressed tier.
/// See `Storage::memory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CubeMemory {
    /// Live cells in the hot (write-optimized) tier.
    pub hot_cells: usize,
    /// Cells in the cold (compressed) tier.
    pub cold_cells: usize,
    /// Estimated heap bytes of the hot tier: its columns and index.
    pub hot_bytes: usize,
    /// Heap bytes of the cold tier: its compressed stream and restart index.
    pub cold_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionCapacity {
    pub hot_numeric_len: usize,
    pub hot_numeric_capacity: usize,
    pub hot_string_len: usize,
    pub hot_string_capacity: usize,
    pub hot_tombstones_len: usize,
    pub hot_index_len: usize,
    pub hot_index_capacity: usize,
    pub cold_len: usize,
    pub cold_stream_bytes: usize,
}

impl Storage {
    pub fn new() -> Self {
        Self::default()
    }

    /// The shared internal lookup every per-cube operation goes through:
    /// briefly read-locks the outer map, clones the partition's `Arc`,
    /// drops the outer guard immediately. Returns `None` for a cube with
    /// no partition yet (never created, or destroyed).
    fn partition(&self, cube: CubeId) -> Option<Arc<RwLock<CubePartition>>> {
        self.cubes.read().expect("storage outer lock poisoned").get(&cube).cloned()
    }

    /// Like `partition`, but auto-creates an empty partition if the cube
    /// doesn't have one yet -- matches the original single-lock
    /// implementation's `self.cubes.entry(cube).or_default()` behavior,
    /// preserved here for `set`/`record_and_set`/`create_lock`. Fast path
    /// (cube already exists, true for every cell write once `cube_create`
    /// has run) only ever takes the outer *read* lock; the outer *write*
    /// lock is only taken on the rare miss, so this doesn't reintroduce
    /// cross-cube contention on the common path.
    fn partition_or_create(&self, cube: CubeId) -> Arc<RwLock<CubePartition>> {
        if let Some(p) = self.partition(cube) {
            return p;
        }
        self.cubes.write().expect("storage outer lock poisoned").entry(cube).or_default().clone()
    }

    /// Must be called when a cube is created, so `filled_count`/`get`
    /// have a (possibly empty) map to look up rather than treating a
    /// never-initialized cube the same as one with no cells filled yet --
    /// keeps the two "no data" reasons distinguishable if that ever
    /// matters later, and matches the arena-per-object pattern used by
    /// `DimensionTable`'s per-dimension element arenas.
    /// `n_dims` is the cube's real dimension count, known at creation time
    /// -- sizes `CubePartition`'s columnar tables up front (see
    /// `CubePartition::with_dims`'s doc comment for why this matters
    /// beyond just avoiding a later resize).
    pub fn cube_created(&self, id: CubeId, n_dims: usize) {
        self.cubes.write().expect("storage outer lock poisoned").entry(id).or_insert_with(|| Arc::new(RwLock::new(CubePartition::with_dims(n_dims))));
    }

    pub fn cube_destroyed(&self, id: CubeId) {
        self.cubes.write().expect("storage outer lock poisoned").remove(&id);
    }

    /// One-shot capacity reservation for a caller that knows the true
    /// final cell count of a bulk load before writing a single batch --
    /// e.g. a migration from another server, from the source cube's
    /// filled-cell count. Call this once, before any
    /// `record_and_set_many` batches for the same load; see
    /// `CubePartition::reserve_exact_additional` for why this avoids
    /// `record_and_set_many`'s own per-batch reservation overshooting.
    pub fn reserve_cube(&self, cube: CubeId, additional: usize) {
        let partition_arc = self.partition_or_create(cube);
        partition_arc.write().expect("cube partition lock poisoned").reserve_exact_additional(additional);
    }

    /// Removes every stored cell for a cube without removing the cube's
    /// (possibly-empty) entry itself -- unlike `cube_destroyed`, the cube
    /// keeps existing, just with no data. Used for clearing a whole cube
    /// and for clearing a dimension (which clears every cube using the
    /// dimension being cleared, since the cleared elements' ids are no
    /// longer valid cell-path coordinates for any of them).
    pub fn clear_cube(&self, id: CubeId) {
        if let Some(partition) = self.partition(id) {
            let mut partition = partition.write().expect("cube partition lock poisoned");
            partition.clear();
            partition.touch();
        }
    }

    /// Removes every stored cell of `cube` whose path lies inside `leaves` (`leaves[d]` = the
    /// leaf elements of dimension `d` to clear, `None` = the whole dimension); see
    /// `rewrite_area`. Returns the number of cells removed.
    pub fn clear_area(&self, cube: CubeId, leaves: &[Option<HashSet<ElementId>>]) -> usize {
        self.rewrite_area(cube, leaves, |_, _| None)
    }

    /// Replaces every stored cell of `cube` whose path lies inside `leaves` with
    /// `f(path, value)` (`None` removes the cell), journaling each change into every covering
    /// lock exactly like `record_and_set_many`. Returns the number of cells visited. The cost
    /// follows the cells actually stored, never the size of the area: `write_cell`'s splash
    /// used to enumerate every base path of a consolidated coordinate -- the full leaf cross
    /// product, tens of billions of paths for a root coordinate of a real cube, which aborted
    /// the process on allocation. Scan and rewrite happen under one partition write lock, so
    /// no concurrent write can land in between.
    pub fn rewrite_area<F>(&self, cube: CubeId, leaves: &[Option<HashSet<ElementId>>], f: F) -> usize
    where
        F: Fn(&[ElementId], &CellValue) -> Option<CellValue>,
    {
        if leaves.iter().any(|set| set.as_ref().is_some_and(|s| s.is_empty())) {
            return 0;
        }
        let Some(p) = self.partition(cube) else { return 0 };
        let mut partition = p.write().expect("cube partition lock poisoned");
        // Pruning hint for cold blocks: the leading dimensions that are actually restricted.
        let prefix_ffi: Vec<Vec<u64>> = leaves
            .iter()
            .map_while(|set| set.as_ref())
            .map(|set| {
                let mut v: Vec<u64> = set.iter().map(|e| slotmap::Key::data(e).as_ffi()).collect();
                v.sort_unstable();
                v
            })
            .collect();
        let inside = |path: &[ElementId]| path.iter().zip(leaves).all(|(eid, set)| set.as_ref().is_none_or(|s| s.contains(eid)));

        let (hot_entries, shadowed) = if partition.cold.is_some() {
            partition.hot.live_entries_and_shadowed()
        } else {
            (partition.hot.live_entries(), HashSet::new())
        };
        let mut matched: Vec<(CellPath, CellValue)> = match &partition.cold {
            Some(cold) => cold.par_fold(
                &prefix_ffi,
                &Vec::new,
                &|acc: &mut Vec<(CellPath, CellValue)>, path: &[ElementId], value: &CellValue| {
                    if inside(path) && !shadowed.contains(path) {
                        acc.push((CellPath::from_slice(path), value.clone()));
                    }
                },
                &|mut a, mut b| {
                    a.append(&mut b);
                    a
                },
            ),
            None => Vec::new(),
        };
        matched.extend(hot_entries.into_iter().filter(|(path, _)| inside(path)));
        if !matched.is_empty() {
            partition.touch();
        }

        for (path, old) in &matched {
            let new = f(path, old).filter(|v| !v.is_default());
            if !partition.locks.is_empty() {
                for lock in partition.locks.iter_mut().filter(|l| l.covers(path)) {
                    lock.undo_log.push((path.to_vec(), Some(old.clone())));
                }
            }
            match new {
                Some(value) => partition.set_cell(path.clone(), value),
                None => partition.remove_cell(path),
            }
        }
        matched.len()
    }

    /// Folds this cube's entire hot tier into cold (`CubePartition::compact`)
    /// -- a no-op on a cube with no partition yet. A real compaction,
    /// which both reclaims hot's capacity *and* moves its
    /// live cells into the compressed cold tier, so this is now genuinely
    /// worth calling mid-migration on a single very large cube's load (caps
    /// *peak* memory during the load itself), not just as post-load
    /// hygiene. Meant to be called once a bulk load into this cube is known
    /// to be finished, or periodically during a very large single-cube
    /// load -- not on every write (`maybe_compact` already handles that via
    /// `HOT_COMPACT_TRIGGER`).
    pub fn shrink_cube(&self, id: CubeId) {
        if let Some(partition) = self.partition(id) {
            partition.write().expect("cube partition lock poisoned").compact();
        }
    }

    /// Diagnostic-only capacity vs. logical-size snapshot for one cube's
    /// partition -- never read on any request-serving path. Exists so
    /// capacity-management tooling (`xtask membench`) can tell "grew more
    /// than it needed to" apart from "the per-cell format itself is the
    /// cost," rather than inferring it indirectly from RSS deltas alone.
    pub fn capacity(&self, cube: CubeId) -> Option<PartitionCapacity> {
        self.partition(cube).map(|p| {
            let partition = p.read().expect("cube partition lock poisoned");
            PartitionCapacity {
                hot_numeric_len: partition.hot.numeric_len(),
                hot_numeric_capacity: partition.hot.numeric_capacity(),
                hot_string_len: partition.hot.string_len(),
                hot_string_capacity: partition.hot.string_capacity(),
                hot_tombstones_len: partition.hot.tombstones_len(),
                hot_index_len: partition.hot.index_len(),
                hot_index_capacity: partition.hot.index_capacity(),
                cold_len: partition.cold.as_ref().map_or(0, |c| c.len()),
                cold_stream_bytes: partition.cold.as_ref().map_or(0, |c| c.stream_bytes()),
            }
        })
    }

    /// An estimate of the memory one cube's cells take, for statistics
    /// (`GET /api/v1/stats`): the tiers' allocations by capacity, from the
    /// sizes of their parts rather than from the allocator, so it leaves out
    /// allocator overhead. Cheap except for string cells, whose texts are
    /// summed.
    pub fn memory(&self, cube: CubeId) -> Option<CubeMemory> {
        self.partition(cube).map(|p| {
            let partition = p.read().expect("cube partition lock poisoned");
            CubeMemory {
                hot_cells: partition.hot.live_len(),
                cold_cells: partition.cold.as_ref().map_or(0, |c| c.len()),
                hot_bytes: partition.hot.heap_bytes(),
                cold_bytes: partition.cold.as_ref().map_or(0, |c| c.heap_bytes()),
            }
        })
    }

    pub fn get(&self, cube: CubeId, path: &[ElementId]) -> Option<CellValue> {
        self.partition(cube)?.read().expect("cube partition lock poisoned").get_cell(path)
    }

    /// Batched form of `get`: reads many paths from the same cube under a
    /// single partition-lock acquisition, in input order. Exists for
    /// consolidation reads (`read_cell` in `fluxolap-engine`), which sum
    /// `child_value * weight` across a base-path expansion that can be
    /// tens of thousands of leaves for a large consolidated element --
    /// looping `get` there paid one lock acquire/release per leaf for no
    /// reason, since all of them target the same cube (per-call locking was
    /// measurably slow on large same-cube operations).
    pub fn get_many(&self, cube: CubeId, paths: &[Vec<ElementId>]) -> Vec<Option<CellValue>> {
        match self.partition(cube) {
            Some(p) => {
                let partition = p.read().expect("cube partition lock poisoned");
                let slices: Vec<&[ElementId]> = paths.iter().map(|path| path.as_slice()).collect();
                partition.get_cells_many(&slices)
            }
            None => vec![None; paths.len()],
        }
    }

    /// Setting a cell to its type's default value (numeric 0.0, empty
    /// string) clears it rather than storing an explicit entry -- a sparse
    /// representation, where "default value" and "not present" are the
    /// same observable state on read, and `filled_count` counts real
    /// values only.
    pub fn set(&self, cube: CubeId, path: Vec<ElementId>, value: CellValue) {
        let partition_arc = self.partition_or_create(cube);
        let mut partition = partition_arc.write().expect("cube partition lock poisoned");
        partition.touch();
        if value.is_default() {
            partition.remove_cell(path.as_slice());
        } else {
            partition.set_cell(path.into(), value);
        }
    }

    /// Unix seconds of the last change to `cube`'s cells, `0` if none is known
    /// (never written, or last written by a version that didn't record it).
    pub fn modified(&self, cube: CubeId) -> u64 {
        self.partition(cube).map_or(0, |p| p.read().expect("cube partition lock poisoned").modified)
    }

    pub fn filled_count(&self, cube: CubeId) -> usize {
        let Some(p) = self.partition(cube) else { return 0 };
        {
            let partition = p.read().expect("cube partition lock poisoned");
            if !partition.count_dirty {
                return partition.live_count;
            }
        }
        // A write left the count unsettled (see `CubePartition::count_dirty`): settle it once under
        // the write lock; later calls hit the read-only fast path above until the next such write.
        let mut partition = p.write().expect("cube partition lock poisoned");
        partition.ensure_count();
        partition.live_count
    }

    /// Every stored (non-default) cell for a cube, in no particular order
    /// -- callers that need a deterministic order (a cell export's
    /// `blocksize`/`path` pagination) sort the result themselves. Returns
    /// owned data (not a borrowing iterator) since the underlying data
    /// lives behind a lock guard scoped to this call, not to `&self`.
    ///
    /// This is the single scan every bulk read (`ops::read_cell`'s
    /// aggregation, cell export, an embedding application's
    /// `fluxolap_cube_cells_read_all`/`_read_area`) ultimately funnels
    /// through, so it's the highest-leverage place to add parallelism:
    /// for a cube with enough stored cells to matter, cloning each
    /// `(path, value)` pair is split across rayon's work-stealing thread
    /// pool (sized to the machine's core count) instead of running on one
    /// core while 63 others sit idle. Still just one partition read-lock
    /// acquisition -- `RwLockReadGuard` is `Sync`, so many worker threads
    /// reading through the same guard concurrently is exactly what a read
    /// lock is for; this doesn't change the concurrency-correctness
    /// model at all, only how the read itself is carried out.
    pub fn iter_cube(&self, cube: CubeId) -> Vec<(CellPath, CellValue)> {
        match self.partition(cube) {
            Some(p) => {
                let partition = p.read().expect("cube partition lock poisoned");
                let (hot_entries, shadowed) = if partition.cold.is_some() {
                    partition.hot.live_entries_and_shadowed()
                } else {
                    (partition.hot.live_entries(), std::collections::HashSet::new())
                };
                let mut out: Vec<(CellPath, CellValue)> = match &partition.cold {
                    Some(cold) if cold.len() >= PARALLEL_SCAN_THRESHOLD => {
                        cold.iter_parallel().into_iter().filter(|(p, _)| !shadowed.contains(p)).collect()
                    }
                    Some(cold) => cold.iter().filter(|(p, _)| !shadowed.contains(p)).collect(),
                    None => Vec::new(),
                };
                out.extend(hot_entries);
                out
            }
            None => Vec::new(),
        }
    }

    /// Streams every stored cell of a cube through `fold` without
    /// materializing a `Vec` of them (see `iter_cube` for that): the cold
    /// tier is decoded block-parallel straight into per-block accumulators
    /// (`cold::ColdRun::par_fold`), hot cells are folded afterward, and the
    /// accumulators are combined with `reduce`. For callers that aggregate
    /// over the whole cube and keep only a small result -- `read_cell`'s
    /// consolidated sum, the embedded connector's area read. `identity`
    /// makes an empty accumulator, `fold` adds one cell to one, `reduce`
    /// merges two; all must be usable from several threads at once.
    /// Cells shadowed by a newer hot entry or tombstone are skipped, same
    /// as `iter_cube`. The visit order is unspecified.
    pub fn par_fold_cells<A, I, F, R>(&self, cube: CubeId, identity: I, fold: F, reduce: R) -> A
    where
        A: Send,
        I: Fn() -> A + Sync + Send,
        F: Fn(&mut A, &[ElementId], &CellValue) + Sync + Send,
        R: Fn(A, A) -> A + Sync + Send,
    {
        self.par_fold_cells_in(cube, &[], identity, fold, reduce)
    }

    /// `par_fold_cells`, told that `fold` only cares about cells whose leading dimensions are
    /// inside `prefix` (`prefix[d]` = the elements of dimension `d` that can matter, for `d`
    /// from 0 up to `prefix.len()`): cold blocks that cannot hold such a cell are skipped without
    /// being decoded. Only worth passing for *selective* leading dimensions -- cold rows are
    /// sorted by dimension 0 first, so a drilldown into one year of a date dimension that comes
    /// first visits that year's slice of the cube instead of all of it (see
    /// `cold::ColdRun::relevant_blocks`). `fold` must still apply its own filter: pruning is a
    /// hint that skips work, never the filter itself (hot cells are always all visited).
    pub fn par_fold_cells_in<A, I, F, R>(&self, cube: CubeId, prefix: &[Vec<ElementId>], identity: I, fold: F, reduce: R) -> A
    where
        A: Send,
        I: Fn() -> A + Sync + Send,
        F: Fn(&mut A, &[ElementId], &CellValue) + Sync + Send,
        R: Fn(A, A) -> A + Sync + Send,
    {
        let prefix_ffi: Vec<Vec<u64>> = prefix
            .iter()
            .map(|ids| {
                let mut v: Vec<u64> = ids.iter().map(|e| slotmap::Key::data(e).as_ffi()).collect();
                v.sort_unstable();
                v.dedup();
                v
            })
            .collect();
        use rayon::prelude::*;
        let Some(p) = self.partition(cube) else { return identity() };
        let partition = p.read().expect("cube partition lock poisoned");
        let (hot_entries, shadowed) = if partition.cold.is_some() {
            partition.hot.live_entries_and_shadowed()
        } else {
            (partition.hot.live_entries(), std::collections::HashSet::new())
        };
        let cold_acc = match &partition.cold {
            Some(cold) if shadowed.is_empty() => cold.par_fold(&prefix_ffi, &identity, &fold, &reduce),
            Some(cold) => cold.par_fold(&prefix_ffi, &identity, &|acc: &mut A, path: &[ElementId], value: &CellValue| {
                if !shadowed.contains(path) {
                    fold(acc, path, value);
                }
            }, &reduce),
            None => identity(),
        };
        let hot_acc = if hot_entries.len() >= PARALLEL_SCAN_THRESHOLD {
            hot_entries
                .par_iter()
                .fold(&identity, |mut acc, (path, value)| {
                    fold(&mut acc, path, value);
                    acc
                })
                .reduce(&identity, &reduce)
        } else {
            let mut acc = identity();
            for (path, value) in &hot_entries {
                fold(&mut acc, path, value);
            }
            acc
        };
        reduce(cold_acc, hot_acc)
    }

    /// Visits stored cells one at a time, in a repeatable order (cold rows in sorted path order, then the hot tier), stopping as soon
    /// as `f` returns false. `prefix` is the same pruning hint as `par_fold_cells_in`'s (only skips work; `f` still filters). For
    /// callers that want just the first N results of a large cube -- a 1000-line preview of a 26M-cell cube -- where materializing
    /// everything (`iter_cube`) or scanning everything in parallel (`par_fold_cells`) costs gigabytes and seconds for nothing.
    /// Cells shadowed by a newer hot entry or tombstone are skipped, like everywhere else.
    pub fn scan_in_order<F: FnMut(&[ElementId], &CellValue) -> bool>(&self, cube: CubeId, prefix: &[Vec<ElementId>], mut f: F) {
        let Some(p) = self.partition(cube) else { return };
        let partition = p.read().expect("cube partition lock poisoned");
        let (hot_entries, shadowed) = if partition.cold.is_some() {
            partition.hot.live_entries_and_shadowed()
        } else {
            (partition.hot.live_entries(), std::collections::HashSet::new())
        };
        if let Some(cold) = &partition.cold {
            let prefix_ffi: Vec<Vec<u64>> = prefix
                .iter()
                .map(|ids| {
                    let mut v: Vec<u64> = ids.iter().map(|e| slotmap::Key::data(e).as_ffi()).collect();
                    v.sort_unstable();
                    v.dedup();
                    v
                })
                .collect();
            let mut visit_unshadowed = |path: &[ElementId], value: &CellValue| {
                if !shadowed.is_empty() && shadowed.contains(path) {
                    true // superseded by a newer hot entry (or deleted): skip, keep going
                } else {
                    f(path, value)
                }
            };
            if !cold.scan_in_order(&prefix_ffi, &mut visit_unshadowed) {
                return;
            }
        }
        for (path, value) in &hot_entries {
            if !f(path, value) {
                return;
            }
        }
    }

    /// Like `set`, but first journals the path's pre-write value into
    /// every active lock on this cube that covers it -- the
    /// mechanism rollback replays. Journaling and the write
    /// itself happen under one single partition-lock acquisition (not
    /// two), so there's no window where a concurrent write to the same
    /// cube could land between "read the old value to journal" and
    /// "apply the new value." If a path happens to be covered by more
    /// than one lock (locks are only checked for overlap against *each
    /// other* at creation time, via `areas_overlap`; two locks can still
    /// each cover cells the other doesn't), the write is journaled into
    /// every covering lock, so rolling back any one of them correctly
    /// restores this cell. `write_cell` in `fluxolap-engine` is the sole
    /// caller -- every cell-mutating operation (cell writes, single
    /// and in batches, and clearing an area) already routes through
    /// it, so this one function is enough to make all of them lock-aware.
    pub fn record_and_set(&self, cube: CubeId, path: Vec<ElementId>, value: CellValue) {
        let partition_arc = self.partition_or_create(cube);
        let mut partition = partition_arc.write().expect("cube partition lock poisoned");
        partition.touch();
        if !partition.locks.is_empty() {
            let old = partition.get_cell(path.as_slice());
            for lock in partition.locks.iter_mut().filter(|l| l.covers(&path)) {
                lock.undo_log.push((path.clone(), old.clone()));
            }
        }
        if value.is_default() {
            partition.remove_cell(path.as_slice());
        } else {
            partition.set_cell(path.into(), value);
        }
    }

    /// Batched form of `record_and_set`: writes many cells to the same
    /// cube under a single partition-lock acquisition instead of one per
    /// cell. Exists for splash writes (`write_cell` in `fluxolap-engine`),
    /// which can touch tens of thousands of leaves in one request -- a
    /// large splash's held-lock window is exactly as long either way
    /// (same total work, same cube), so batching gives up nothing on the
    /// "don't block unrelated cubes" property the sharding exists for, while
    /// avoiding tens of thousands of separate lock acquire/release calls
    /// for one logical write. As a side effect, the whole batch is now
    /// atomic with respect to any other read or write on this cube -- a
    /// concurrent `get` on this cube during the batch sees either every
    /// entry's old value or every entry's new value, never a mix (proven
    /// by `record_and_set_many_is_atomic_relative_to_other_writes_on_the_same_cube`
    /// below); with per-cell locking a reader could previously observe a
    /// splash half-applied.
    pub fn record_and_set_many<I>(&self, cube: CubeId, entries: I)
    where
        I: IntoIterator<Item = (Vec<ElementId>, CellValue)>,
        I::IntoIter: ExactSizeIterator,
    {
        let entries = entries.into_iter();
        let partition_arc = self.partition_or_create(cube);
        let mut partition = partition_arc.write().expect("cube partition lock poisoned");
        // Not every entry is necessarily a genuine insert (some may
        // overwrite an existing path, or clear one back to default), so
        // this can over-reserve slightly -- harmless, and far better than
        // the repeated-doubling growth this replaces (confirmed via
        // `membench` to otherwise leave ~20-30% capacity unused at steady
        // state).
        partition.reserve_additional(entries.len());
        if entries.len() > 0 {
            partition.touch();
        }
        for (path, value) in entries {
            if !partition.locks.is_empty() {
                let old = partition.get_cell(path.as_slice());
                for lock in partition.locks.iter_mut().filter(|l| l.covers(&path)) {
                    lock.undo_log.push((path.clone(), old.clone()));
                }
            }
            if value.is_default() {
                partition.remove_cell(path.as_slice());
            } else {
                partition.set_cell(path.into(), value);
            }
        }
    }

    /// Atomically reads a leaf cell's current numeric value (a missing or
    /// non-numeric cell counts as `0.0`, the usual add-mode semantics),
    /// adds `delta`, and writes the result back -- all under
    /// one partition-lock acquisition, so a concurrent write to the same
    /// cell can never land between the read and the write the way it
    /// could if a caller did `get` then `record_and_set` as two separate
    /// calls. Journals into covering locks exactly like `record_and_set`.
    /// Returns the new value. Numeric-only and leaf-only by design -- a
    /// consolidated coordinate
    /// has no single stored value to add to in the first place.
    pub fn add_and_record(&self, cube: CubeId, path: Vec<ElementId>, delta: f64) -> f64 {
        let partition_arc = self.partition_or_create(cube);
        let mut partition = partition_arc.write().expect("cube partition lock poisoned");
        partition.touch();
        let old = partition.get_cell(path.as_slice());
        let old_numeric = match &old {
            Some(CellValue::Numeric(v)) => *v,
            _ => 0.0,
        };
        let new_value = old_numeric + delta;
        if !partition.locks.is_empty() {
            for lock in partition.locks.iter_mut().filter(|l| l.covers(&path)) {
                lock.undo_log.push((path.clone(), old.clone()));
            }
        }
        if CellValue::Numeric(new_value).is_default() {
            partition.remove_cell(path.as_slice());
        } else {
            partition.set_cell(path.into(), CellValue::Numeric(new_value));
        }
        new_value
    }

    /// Every active lock on a cube, in creation order.
    pub fn locks_for(&self, cube: CubeId) -> Vec<CubeLock> {
        match self.partition(cube) {
            Some(p) => p.read().expect("cube partition lock poisoned").locks.clone(),
            None => Vec::new(),
        }
    }

    /// Creates a new lock after checking it doesn't overlap any existing
    /// lock on this cube (`ERROR_CUBE_WRONG_LOCK` if it does -- checked
    /// by the caller via `areas_overlap` before calling this, since the
    /// error needs to carry `fluxolap-error` types this crate doesn't depend
    /// on). Returns the new lock's wire id.
    pub fn create_lock(&self, cube: CubeId, whole: bool, raw_area: Vec<Vec<ElementId>>, overlap_leaves: Vec<HashSet<ElementId>>) -> u32 {
        let partition_arc = self.partition_or_create(cube);
        let mut partition = partition_arc.write().expect("cube partition lock poisoned");
        let wire_id = partition.next_lock_wire_id;
        partition.next_lock_wire_id += 1;
        partition.locks.push(CubeLock { wire_id, whole, raw_area, overlap_leaves, undo_log: Vec::new() });
        wire_id
    }

    pub fn find_lock(&self, cube: CubeId, lock_wire_id: u32) -> Option<CubeLock> {
        self.partition(cube)?.read().expect("cube partition lock poisoned").locks.iter().find(|l| l.wire_id == lock_wire_id).cloned()
    }

    /// Undoes `steps` most-recent writes journaled under a lock (all of
    /// them, and removes the lock, if `steps` is `None` or `Some(0)`). A
    /// partial rollback
    /// (`Some(n)` with `n > 0`) shrinks the log by up to `n` entries and
    /// keeps the lock, even if that happens to fully drain it -- only an
    /// omitted (or zero) `steps` removes the lock. Returns `Err(LockNotFound)` if no such lock exists
    /// on this cube (caller maps to `ERROR_CUBE_LOCK_NOT_FOUND`).
    pub fn rollback_lock(&self, cube: CubeId, lock_wire_id: u32, steps: Option<u32>) -> Result<(), LockNotFound> {
        let partition_arc = self.partition(cube).ok_or(LockNotFound)?;
        let mut partition = partition_arc.write().expect("cube partition lock poisoned");
        let idx = partition.locks.iter().position(|l| l.wire_id == lock_wire_id).ok_or(LockNotFound)?;

        let full = steps.unwrap_or(0) == 0;
        let n = if full { partition.locks[idx].undo_log.len() } else { (steps.unwrap() as usize).min(partition.locks[idx].undo_log.len()) };
        let mut to_restore = Vec::with_capacity(n);
        for _ in 0..n {
            if let Some(entry) = partition.locks[idx].undo_log.pop() {
                to_restore.push(entry);
            }
        }
        if full {
            partition.locks.remove(idx);
        }

        // Inlined `set`'s logic (rather than calling `self.set(...)`)
        // since we're already holding this partition's write guard --
        // std's RwLock isn't reentrant, so calling back through a method
        // that re-acquires it would deadlock.
        if !to_restore.is_empty() {
            partition.touch();
        }
        for (path, old) in to_restore {
            match old {
                Some(v) if !v.is_default() => {
                    partition.set_cell(path.into(), v);
                }
                _ => {
                    partition.remove_cell(path.as_slice());
                }
            }
        }
        Ok(())
    }

    /// Discards a lock's undo log (keeping every value it journaled) and
    /// removes the lock. Returns `Err(LockNotFound)` if no such lock exists on this
    /// cube (caller maps to `ERROR_CUBE_LOCK_NOT_FOUND`).
    pub fn commit_lock(&self, cube: CubeId, lock_wire_id: u32) -> Result<(), LockNotFound> {
        let partition_arc = self.partition(cube).ok_or(LockNotFound)?;
        let mut partition = partition_arc.write().expect("cube partition lock poisoned");
        let idx = partition.locks.iter().position(|l| l.wire_id == lock_wire_id).ok_or(LockNotFound)?;
        partition.locks.remove(idx);
        Ok(())
    }

    /// Persistence: a fully-owned, plain-data snapshot of every cube's
    /// partition, for `bincode` to serialize directly -- `Storage` itself
    /// can no longer derive `Serialize`/`Deserialize` now that its fields
    /// are lock-wrapped (serde has no impl for `std::sync::RwLock`, for
    /// good reason: serializing *through* a lock isn't well-defined).
    /// Only ever called once, at clean shutdown -- briefly locks the
    /// outer map and every partition in turn, which is fine for a
    /// rare, one-shot operation.
    pub fn snapshot(&self) -> HashMap<CubeId, CubePartition> {
        let outer = self.cubes.read().expect("storage outer lock poisoned");
        outer
            .iter()
            .map(|(&id, partition)| {
                let mut copy = partition.read().expect("cube partition lock poisoned").clone();
                copy.ensure_count(); // never persist an unsettled count
                (id, copy)
            })
            .collect()
    }

    /// The inverse of `snapshot` -- rebuilds a live, lock-sharded
    /// `Storage` from a loaded snapshot. Rebuilds each partition's hot
    /// tier's `index` (`hot::HotTier::rebuild_index`) -- `#[serde(skip)]`'d
    /// and so comes back empty via `Default` on deserialization -- from
    /// its (real, serialized) `numeric`/`string`/`tombstones` tables.
    /// `cold`'s data needs no such repair: it carries no separate index at
    /// all, only the encoded stream plus the sparse `restarts` list, both
    /// real serialized data.
    pub fn from_snapshot(data: HashMap<CubeId, CubePartition>) -> Self {
        let cubes = data
            .into_iter()
            .map(|(id, mut partition)| {
                partition.hot.rebuild_index();
                (id, Arc::new(RwLock::new(partition)))
            })
            .collect();
        Self { cubes: RwLock::new(cubes) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slotmap::SlotMap;

    fn fake_ids<K: slotmap::Key, const N: usize>() -> [K; N] {
        let mut sm: SlotMap<K, ()> = SlotMap::default();
        std::array::from_fn(|_| sm.insert(()))
    }

    /// Everything the lazy live count, batched reads and streaming fold must
    /// agree on, checked against a plain `HashMap` model through a long random
    /// mix of writes, deletes, compactions and reads -- over a universe large
    /// enough (512 paths) that the cold run spans several restart blocks.
    #[test]
    fn storage_matches_a_model_through_random_writes_deletes_and_compactions() {
        let mut ekeys: SlotMap<ElementId, ()> = SlotMap::default();
        let dims: Vec<Vec<ElementId>> = (0..3).map(|_| (0..8).map(|_| ekeys.insert(())).collect()).collect();
        let [cube] = fake_ids::<CubeId, 1>();
        let mut all_paths: Vec<Vec<ElementId>> = Vec::new();
        for &a in &dims[0] {
            for &b in &dims[1] {
                for &c in &dims[2] {
                    all_paths.push(vec![a, b, c]);
                }
            }
        }

        for seed in 1..=6u64 {
            let storage = Storage::new();
            storage.cube_created(cube, 3);
            let mut model: HashMap<Vec<ElementId>, f64> = HashMap::new();
            let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
            let mut next = move || {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x
            };
            for step in 0..3000 {
                let path = all_paths[next() as usize % all_paths.len()].clone();
                match next() % 10 {
                    0..=5 => {
                        let v = (next() % 50) as f64 + 1.0;
                        storage.set(cube, path.clone(), CellValue::Numeric(v));
                        model.insert(path, v);
                    }
                    6 | 7 => {
                        storage.set(cube, path.clone(), CellValue::Numeric(0.0)); // default value == delete
                        model.remove(&path);
                    }
                    8 => {
                        if step % 3 == 0 {
                            storage.shrink_cube(cube); // compaction: moves everything to cold
                        }
                    }
                    _ => {
                        assert_eq!(storage.get(cube, &path), model.get(&path).map(|&v| CellValue::Numeric(v)), "seed {seed} step {step}: get");
                        if step % 5 == 0 {
                            assert_eq!(storage.filled_count(cube), model.len(), "seed {seed} step {step}: filled_count");
                        }
                    }
                }
                if step % 250 == 0 {
                    let got = storage.get_many(cube, &all_paths);
                    for (p, g) in all_paths.iter().zip(&got) {
                        assert_eq!(g, &model.get(p).map(|&v| CellValue::Numeric(v)), "seed {seed} step {step}: get_many {p:?}");
                    }
                    assert_eq!(storage.filled_count(cube), model.len(), "seed {seed} step {step}: filled_count after batch");
                    let mut scanned: HashMap<Vec<ElementId>, f64> = HashMap::new();
                    for (p, v) in storage.iter_cube(cube) {
                        if let CellValue::Numeric(v) = v {
                            assert!(scanned.insert(p.to_vec(), v).is_none(), "iter_cube yielded a path twice");
                        }
                    }
                    assert_eq!(scanned, model, "seed {seed} step {step}: iter_cube");
                    let (n, sum) = storage.par_fold_cells(
                        cube,
                        || (0usize, 0.0f64),
                        |acc: &mut (usize, f64), _p: &[ElementId], v: &CellValue| {
                            acc.0 += 1;
                            if let CellValue::Numeric(v) = v {
                                acc.1 += v;
                            }
                        },
                        |a, b| (a.0 + b.0, a.1 + b.1),
                    );
                    // scan_in_order: complete, no duplicates, repeatable, and early stop yields exactly the first N of that order
                    let mut ordered: Vec<(Vec<ElementId>, f64)> = Vec::new();
                    storage.scan_in_order(cube, &[], |p, v| {
                        if let CellValue::Numeric(v) = v {
                            ordered.push((p.to_vec(), *v));
                        }
                        true
                    });
                    assert_eq!(ordered.len(), model.len(), "seed {seed} step {step}: scan_in_order count");
                    assert_eq!(ordered.iter().cloned().collect::<HashMap<_, _>>(), model, "seed {seed} step {step}: scan_in_order content");
                    let mut again: Vec<(Vec<ElementId>, f64)> = Vec::new();
                    storage.scan_in_order(cube, &[], |p, v| {
                        if let CellValue::Numeric(v) = v {
                            again.push((p.to_vec(), *v));
                        }
                        true
                    });
                    assert_eq!(again, ordered, "seed {seed} step {step}: scan_in_order order must be repeatable");
                    for limit in [1usize, 7, 50] {
                        let mut first: Vec<Vec<ElementId>> = Vec::new();
                        storage.scan_in_order(cube, &[], |p, _| {
                            first.push(p.to_vec());
                            first.len() < limit
                        });
                        assert_eq!(first.len(), limit.min(model.len()), "seed {seed} step {step}: early stop at {limit}");
                        assert_eq!(first, ordered.iter().take(first.len()).map(|(p, _)| p.clone()).collect::<Vec<_>>(), "seed {seed} step {step}: first {limit}");
                    }
                    // a pruning prefix on the leading dimension may skip blocks but never a matching cell
                    let allowed: Vec<ElementId> = vec![dims[0][1], dims[0][4]];
                    let mut pruned: HashMap<Vec<ElementId>, f64> = HashMap::new();
                    storage.scan_in_order(cube, std::slice::from_ref(&allowed), |p, v| {
                        if let (true, CellValue::Numeric(v)) = (allowed.contains(&p[0]), v) {
                            pruned.insert(p.to_vec(), *v);
                        }
                        true
                    });
                    let expected: HashMap<Vec<ElementId>, f64> = model.iter().filter(|(p, _)| allowed.contains(&p[0])).map(|(p, v)| (p.clone(), *v)).collect();
                    assert_eq!(pruned, expected, "seed {seed} step {step}: scan_in_order with a prefix");
                    assert_eq!(n, model.len(), "seed {seed} step {step}: par_fold_cells count");
                    assert_eq!(sum, model.values().sum::<f64>(), "seed {seed} step {step}: par_fold_cells sum");
                }
            }
            // The count must also survive a snapshot round trip (never persisted unsettled).
            let restored = Storage::from_snapshot(storage.snapshot());
            assert_eq!(restored.filled_count(cube), model.len(), "seed {seed}: after snapshot/restore");
            assert_eq!(restored.get_many(cube, &all_paths), storage.get_many(cube, &all_paths));
        }
    }

    #[test]
    fn unset_cell_does_not_exist() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        assert!(storage.get(cube, &[e0]).is_none());
    }

    #[test]
    fn set_then_get_round_trips() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0, e1] = fake_ids::<ElementId, 2>();
        storage.set(cube, vec![e0, e1], CellValue::Numeric(42.0));
        assert_eq!(storage.get(cube, &[e0, e1]), Some(CellValue::Numeric(42.0)));
        assert_eq!(storage.filled_count(cube), 1);
    }

    #[test]
    fn setting_to_zero_clears_the_cell() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.set(cube, vec![e0], CellValue::Numeric(5.0));
        assert_eq!(storage.filled_count(cube), 1);
        storage.set(cube, vec![e0], CellValue::Numeric(0.0));
        assert_eq!(storage.filled_count(cube), 0);
        assert!(storage.get(cube, &[e0]).is_none());
    }

    #[test]
    fn add_and_record_on_an_unset_cell_starts_from_zero() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        let new_value = storage.add_and_record(cube, vec![e0], 5.0);
        assert_eq!(new_value, 5.0);
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(5.0)));
    }

    #[test]
    fn add_and_record_accumulates_across_calls() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.add_and_record(cube, vec![e0], 5.0);
        storage.add_and_record(cube, vec![e0], 3.0);
        let new_value = storage.add_and_record(cube, vec![e0], -2.0);
        assert_eq!(new_value, 6.0);
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(6.0)));
    }

    #[test]
    fn add_and_record_landing_on_zero_clears_the_cell() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.add_and_record(cube, vec![e0], 5.0);
        assert_eq!(storage.filled_count(cube), 1);
        storage.add_and_record(cube, vec![e0], -5.0);
        assert_eq!(storage.filled_count(cube), 0);
        assert!(storage.get(cube, &[e0]).is_none());
    }

    #[test]
    fn add_and_record_treats_a_string_cell_as_zero() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.set(cube, vec![e0], CellValue::String("hello".to_string()));
        let new_value = storage.add_and_record(cube, vec![e0], 4.0);
        assert_eq!(new_value, 4.0);
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(4.0)));
    }

    #[test]
    fn add_and_record_journals_into_covering_locks() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.set(cube, vec![e0], CellValue::Numeric(10.0));
        let mut overlap = std::collections::HashSet::new();
        overlap.insert(e0);
        let lock_id = storage.create_lock(cube, false, vec![vec![e0]], vec![overlap]);
        storage.add_and_record(cube, vec![e0], 5.0);
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(15.0)));
        storage.rollback_lock(cube, lock_id, None).unwrap();
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(10.0)));
    }

    #[test]
    fn setting_to_empty_string_clears_the_cell() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.set(cube, vec![e0], CellValue::String("hello".to_string()));
        assert_eq!(storage.filled_count(cube), 1);
        storage.set(cube, vec![e0], CellValue::String(String::new()));
        assert_eq!(storage.filled_count(cube), 0);
    }

    #[test]
    fn clear_cube_drops_cells_but_leaves_the_cube_present() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.cube_created(cube, 1);
        storage.set(cube, vec![e0], CellValue::Numeric(1.0));
        storage.clear_cube(cube);
        assert_eq!(storage.filled_count(cube), 0);
        assert!(storage.get(cube, &[e0]).is_none());
        // Setting a cell after clear must still work -- clear_cube must
        // not have also dropped the cube's arena entry (that's
        // cube_destroyed's job, a different operation).
        storage.set(cube, vec![e0], CellValue::Numeric(2.0));
        assert_eq!(storage.filled_count(cube), 1);
    }

    #[test]
    fn destroying_a_cube_drops_its_cells() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.set(cube, vec![e0], CellValue::Numeric(1.0));
        storage.cube_destroyed(cube);
        assert_eq!(storage.filled_count(cube), 0);
        assert!(storage.get(cube, &[e0]).is_none());
    }

    #[test]
    fn different_cubes_do_not_share_cells() {
        let storage = Storage::new();
        let [cube_a, cube_b] = fake_ids::<CubeId, 2>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.set(cube_a, vec![e0], CellValue::Numeric(1.0));
        assert!(storage.get(cube_b, &[e0]).is_none());
    }

    #[test]
    fn snapshot_round_trips_through_from_snapshot() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.set(cube, vec![e0], CellValue::Numeric(7.0));
        storage.create_lock(cube, false, vec![vec![e0]], vec![[e0].into_iter().collect()]);

        let snapshot = storage.snapshot();
        let restored = Storage::from_snapshot(snapshot);

        assert_eq!(restored.get(cube, &[e0]), Some(CellValue::Numeric(7.0)));
        assert_eq!(restored.locks_for(cube).len(), 1);
    }

    // -----------------------------------------------------------------
    // Locks & rollback
    // -----------------------------------------------------------------

    fn set_of(ids: &[ElementId]) -> HashSet<ElementId> {
        ids.iter().copied().collect()
    }

    #[test]
    fn areas_overlap_requires_every_dimension_to_share_an_element() {
        let [e0, e1, e2, e3] = fake_ids::<ElementId, 4>();
        // dim0 shares e0, dim1 shares nothing -> no overlap.
        let a = vec![set_of(&[e0]), set_of(&[e1])];
        let b = vec![set_of(&[e0]), set_of(&[e2])];
        assert!(!areas_overlap(false, &a, false, &b));

        // Both dimensions share an element -> overlap.
        let c = vec![set_of(&[e0]), set_of(&[e1, e3])];
        assert!(areas_overlap(false, &a, false, &c));
    }

    #[test]
    fn areas_overlap_is_symmetric() {
        let [e0, e1, e2] = fake_ids::<ElementId, 3>();
        let a = vec![set_of(&[e0, e1])];
        let b = vec![set_of(&[e1, e2])];
        assert_eq!(areas_overlap(false, &a, false, &b), areas_overlap(false, &b, false, &a));
    }

    #[test]
    fn a_whole_lock_overlaps_everything() {
        let [e0] = fake_ids::<ElementId, 1>();
        let a = vec![set_of(&[e0])];
        assert!(areas_overlap(true, &[], false, &a));
        assert!(areas_overlap(false, &a, true, &[]));
        assert!(areas_overlap(true, &[], true, &[]));
    }

    proptest::proptest! {
        /// The lock-overlap check (the classic mistake: comparing an area to
        /// itself instead of to the other lock's area): checks `areas_overlap` against an
        /// independent brute-force reference (build every concrete point
        /// in each area's cartesian product as a set, overlap iff the two
        /// point-sets intersect) across random small areas -- not just
        /// symmetry, since the original self-compare bug would satisfy a
        /// symmetry-only check vacuously (comparing a value to itself is
        /// always symmetric).
        #[test]
        fn areas_overlap_matches_brute_force_point_set_reference(
            a0 in proptest::collection::vec(0u32..4, 1..3),
            a1 in proptest::collection::vec(0u32..4, 1..3),
            b0 in proptest::collection::vec(0u32..4, 1..3),
            b1 in proptest::collection::vec(0u32..4, 1..3),
        ) {
            let ids = fake_ids::<ElementId, 4>();
            let lookup = |raw: &[u32]| -> HashSet<ElementId> { raw.iter().map(|&i| ids[i as usize]).collect() };

            let a = vec![lookup(&a0), lookup(&a1)];
            let b = vec![lookup(&b0), lookup(&b1)];

            let brute_force = {
                let points_a: HashSet<(ElementId, ElementId)> = a[0].iter().flat_map(|&x| a[1].iter().map(move |&y| (x, y))).collect();
                let points_b: HashSet<(ElementId, ElementId)> = b[0].iter().flat_map(|&x| b[1].iter().map(move |&y| (x, y))).collect();
                points_a.intersection(&points_b).next().is_some()
            };

            proptest::prop_assert_eq!(areas_overlap(false, &a, false, &b), brute_force);
        }
    }

    #[test]
    fn record_and_set_journals_only_into_covering_locks() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0, e1] = fake_ids::<ElementId, 2>();
        storage.set(cube, vec![e0], CellValue::Numeric(10.0));

        let covering = storage.create_lock(cube, false, vec![vec![e0]], vec![set_of(&[e0])]);
        let non_covering = storage.create_lock(cube, false, vec![vec![e1]], vec![set_of(&[e1])]);

        storage.record_and_set(cube, vec![e0], CellValue::Numeric(20.0));

        assert_eq!(storage.find_lock(cube, covering).unwrap().undo_log.len(), 1);
        assert_eq!(storage.find_lock(cube, non_covering).unwrap().undo_log.len(), 0);
    }

    #[test]
    fn deleting_absent_paths_from_a_compacted_cube_does_not_grow_hot_without_bound() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let mut sm: SlotMap<ElementId, ()> = SlotMap::default();
        let stored = sm.insert(());
        storage.set(cube, vec![stored], CellValue::Numeric(1.0));
        storage.shrink_cube(cube); // cold tier exists from here on
        for _ in 0..HOT_COMPACT_TRIGGER + 10 {
            let absent = sm.insert(());
            storage.set(cube, vec![absent], CellValue::Numeric(0.0));
        }
        let p = storage.partition(cube).unwrap();
        let partition = p.read().unwrap();
        assert!(partition.hot.total_len() < HOT_COMPACT_TRIGGER, "hot holds {} entries", partition.hot.total_len());
        drop(partition);
        assert_eq!(storage.filled_count(cube), 1);
        assert_eq!(storage.get(cube, &[stored]), Some(CellValue::Numeric(1.0)));
    }

    #[test]
    fn clear_area_removes_only_stored_cells_inside_the_area_and_can_be_rolled_back() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [a0, a1, b0, b1] = fake_ids::<ElementId, 4>();
        storage.set(cube, vec![a0, b0], CellValue::Numeric(1.0));
        storage.set(cube, vec![a1, b0], CellValue::Numeric(2.0));
        storage.shrink_cube(cube); // both into cold
        storage.set(cube, vec![a0, b1], CellValue::String("s".into()));
        storage.set(cube, vec![a1, b1], CellValue::Numeric(4.0));
        let lock = storage.create_lock(cube, true, Vec::new(), Vec::new());

        // Dimension 0 restricted to a0, dimension 1 unrestricted.
        let removed = storage.clear_area(cube, &[Some(set_of(&[a0])), None]);
        assert_eq!(removed, 2);
        assert_eq!(storage.get(cube, &[a0, b0]), None);
        assert_eq!(storage.get(cube, &[a0, b1]), None);
        assert_eq!(storage.get(cube, &[a1, b0]), Some(CellValue::Numeric(2.0)));
        assert_eq!(storage.filled_count(cube), 2);
        // An empty set in any dimension matches nothing.
        assert_eq!(storage.clear_area(cube, &[Some(HashSet::new()), None]), 0);

        storage.rollback_lock(cube, lock, None).unwrap();
        assert_eq!(storage.get(cube, &[a0, b0]), Some(CellValue::Numeric(1.0)));
        assert_eq!(storage.get(cube, &[a0, b1]), Some(CellValue::String("s".into())));
        assert_eq!(storage.filled_count(cube), 4);
    }

    /// Stage, 2026-09-26: a nightly area clear on a cube whose cells had all been cleared and
    /// compacted away (cold = an empty run) panicked indexing the empty restart list -- while
    /// holding the partition write lock, poisoning the cube for every later call until restart.
    #[test]
    fn clear_area_on_a_cube_compacted_to_an_empty_cold_run() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [a0, a1, b0] = fake_ids::<ElementId, 3>();
        storage.set(cube, vec![a0, b0], CellValue::Numeric(1.0));
        storage.shrink_cube(cube);
        assert_eq!(storage.clear_area(cube, &[None, None]), 1);
        storage.shrink_cube(cube); // cold is now Some(empty run)
        assert_eq!(storage.clear_area(cube, &[Some(set_of(&[a1])), None]), 0);
        assert_eq!(storage.clear_area(cube, &[None, None]), 0);
        // the cube is still fully usable afterwards
        storage.set(cube, vec![a1, b0], CellValue::Numeric(2.0));
        assert_eq!(storage.get(cube, &[a1, b0]), Some(CellValue::Numeric(2.0)));
        assert_eq!(storage.clear_area(cube, &[Some(set_of(&[a1])), None]), 1);
    }

    #[test]
    fn rollback_with_no_steps_undoes_everything_and_removes_the_lock() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.set(cube, vec![e0], CellValue::Numeric(10.0));
        let lock = storage.create_lock(cube, false, vec![vec![e0]], vec![set_of(&[e0])]);

        storage.record_and_set(cube, vec![e0], CellValue::Numeric(20.0));
        storage.record_and_set(cube, vec![e0], CellValue::Numeric(30.0));
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(30.0)));

        storage.rollback_lock(cube, lock, None).unwrap();
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(10.0)));
        assert!(storage.find_lock(cube, lock).is_none());
    }

    #[test]
    fn rollback_never_created_cell_removes_it_entirely() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        let lock = storage.create_lock(cube, false, vec![vec![e0]], vec![set_of(&[e0])]);

        storage.record_and_set(cube, vec![e0], CellValue::Numeric(5.0));
        storage.rollback_lock(cube, lock, None).unwrap();

        assert!(storage.get(cube, &[e0]).is_none());
    }

    #[test]
    fn partial_rollback_undoes_only_the_requested_steps_and_keeps_the_lock() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.set(cube, vec![e0], CellValue::Numeric(10.0));
        let lock = storage.create_lock(cube, false, vec![vec![e0]], vec![set_of(&[e0])]);

        storage.record_and_set(cube, vec![e0], CellValue::Numeric(20.0)); // step 1
        storage.record_and_set(cube, vec![e0], CellValue::Numeric(30.0)); // step 2

        storage.rollback_lock(cube, lock, Some(1)).unwrap();
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(20.0)));
        // Lock survives a partial rollback -- only an omitted `steps` removes it.
        assert_eq!(storage.find_lock(cube, lock).unwrap().undo_log.len(), 1);
    }

    #[test]
    fn commit_discards_the_undo_log_and_keeps_the_written_values() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.set(cube, vec![e0], CellValue::Numeric(10.0));
        let lock = storage.create_lock(cube, false, vec![vec![e0]], vec![set_of(&[e0])]);
        storage.record_and_set(cube, vec![e0], CellValue::Numeric(20.0));

        storage.commit_lock(cube, lock).unwrap();
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(20.0)));
        assert!(storage.find_lock(cube, lock).is_none());
    }

    #[test]
    fn rollback_or_commit_of_an_unknown_lock_id_errors() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        assert!(storage.rollback_lock(cube, 999, None).is_err());
        assert!(storage.commit_lock(cube, 999).is_err());
    }

    #[test]
    fn destroying_a_cube_drops_its_locks_too() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        let lock = storage.create_lock(cube, false, vec![vec![e0]], vec![set_of(&[e0])]);
        storage.cube_destroyed(cube);
        assert!(storage.find_lock(cube, lock).is_none());
    }

    // -----------------------------------------------------------------
    // Columnar storage (rows + index): proves the swap_remove-based
    // delete path in `CubePartition::remove_cell` never desyncs `rows`
    // from `index`, even under heavy insert/delete churn or when a lock
    // is watching a cell that ends up on the receiving end of an
    // unrelated cell's swap_remove.
    // -----------------------------------------------------------------

    #[test]
    fn interleaved_insert_delete_churn_leaves_rows_and_index_consistent() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let mut sm: slotmap::SlotMap<ElementId, ()> = slotmap::SlotMap::default();
        let elements: Vec<ElementId> = (0..200).map(|_| sm.insert(())).collect();

        // Write all 200 cells.
        for (i, &e) in elements.iter().enumerate() {
            storage.set(cube, vec![e], CellValue::Numeric((i + 1) as f64));
        }
        // Delete every other one -- each delete swap_removes a row, so
        // this repeatedly exercises the "fix up the swapped-in row's
        // index entry" path.
        for (i, &e) in elements.iter().enumerate() {
            if i % 2 == 0 {
                storage.set(cube, vec![e], CellValue::Numeric(0.0));
            }
        }
        // Re-write the deleted half with new values (now hits the
        // "insert" branch of set_cell again after having been removed).
        for (i, &e) in elements.iter().enumerate() {
            if i % 2 == 0 {
                storage.set(cube, vec![e], CellValue::Numeric((1000 + i) as f64));
            }
        }
        // Delete the first quarter of the original elements too.
        for (i, &e) in elements.iter().enumerate() {
            if i < 50 {
                storage.set(cube, vec![e], CellValue::Numeric(0.0));
            }
        }

        // Expected live set: indices < 50 were deleted by the final pass
        // (regardless of parity) and are gone; of the rest, odd indices
        // keep their original (i+1) value and even indices hold their
        // re-written (1000+i) value.
        let mut expected: Vec<(CellPath, CellValue)> = elements
            .iter()
            .enumerate()
            .filter_map(|(i, &e)| {
                if i < 50 {
                    None
                } else if i % 2 == 1 {
                    Some((CellPath::from_elem(e, 1), CellValue::Numeric((i + 1) as f64)))
                } else {
                    Some((CellPath::from_elem(e, 1), CellValue::Numeric((1000 + i) as f64)))
                }
            })
            .collect();
        expected.sort_by_key(|(path, _)| path.clone());

        let mut got = storage.iter_cube(cube);
        got.sort_by_key(|(path, _)| path.clone());

        assert_eq!(storage.filled_count(cube), expected.len());
        assert_eq!(got, expected);

        // Every surviving element must also still be point-lookupable
        // (exercises `index` directly, not just `iter_cube`'s `rows`
        // scan).
        for (path, value) in &expected {
            assert_eq!(storage.get(cube, path.as_slice()), Some(value.clone()));
        }
        // Every deleted element must be gone from point lookup too.
        for (i, &e) in elements.iter().enumerate() {
            if i < 50 {
                assert!(storage.get(cube, &[e]).is_none());
            }
        }
    }

    // -----------------------------------------------------------------
    // Numeric/string split: `numeric_rows`/`string_rows` must
    // stay independently consistent, including across a type change that
    // moves a path from one array to the other.
    // -----------------------------------------------------------------

    #[test]
    fn numeric_and_string_cells_coexist_in_same_partition() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0, e1, e2, e3] = fake_ids::<ElementId, 4>();

        storage.set(cube, vec![e0], CellValue::Numeric(1.0));
        storage.set(cube, vec![e1], CellValue::String("hello".to_string()));
        storage.set(cube, vec![e2], CellValue::Numeric(2.0));
        storage.set(cube, vec![e3], CellValue::String("world".to_string()));

        assert_eq!(storage.filled_count(cube), 4);
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(1.0)));
        assert_eq!(storage.get(cube, &[e1]), Some(CellValue::String("hello".to_string())));
        assert_eq!(storage.get(cube, &[e2]), Some(CellValue::Numeric(2.0)));
        assert_eq!(storage.get(cube, &[e3]), Some(CellValue::String("world".to_string())));

        let mut got = storage.iter_cube(cube);
        got.sort_by_key(|(path, _)| path.clone());
        let mut expected = vec![
            (CellPath::from_elem(e0, 1), CellValue::Numeric(1.0)),
            (CellPath::from_elem(e1, 1), CellValue::String("hello".to_string())),
            (CellPath::from_elem(e2, 1), CellValue::Numeric(2.0)),
            (CellPath::from_elem(e3, 1), CellValue::String("world".to_string())),
        ];
        expected.sort_by_key(|(path, _)| path.clone());
        assert_eq!(got, expected);
    }

    #[test]
    fn changing_a_cells_type_moves_it_to_the_other_array_without_disturbing_siblings() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0, e1, e2] = fake_ids::<ElementId, 3>();

        // e0/e1 share the numeric array, e2 is alone in the string array --
        // e0 will change type, which must swap_remove it out of
        // numeric_rows (fixing up e1's index entry, the row that gets
        // swapped into e0's old slot) without touching e2 at all.
        storage.set(cube, vec![e0], CellValue::Numeric(1.0));
        storage.set(cube, vec![e1], CellValue::Numeric(2.0));
        storage.set(cube, vec![e2], CellValue::String("stays".to_string()));

        storage.set(cube, vec![e0], CellValue::String("now a string".to_string()));
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::String("now a string".to_string())));
        assert_eq!(storage.get(cube, &[e1]), Some(CellValue::Numeric(2.0)));
        assert_eq!(storage.get(cube, &[e2]), Some(CellValue::String("stays".to_string())));
        assert_eq!(storage.filled_count(cube), 3);

        // And back the other way: e2 (a lone string cell) becomes numeric.
        storage.set(cube, vec![e2], CellValue::Numeric(42.0));
        assert_eq!(storage.get(cube, &[e2]), Some(CellValue::Numeric(42.0)));
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::String("now a string".to_string())));
        assert_eq!(storage.get(cube, &[e1]), Some(CellValue::Numeric(2.0)));
        assert_eq!(storage.filled_count(cube), 3);
    }

    #[test]
    fn interleaved_insert_delete_churn_with_mixed_numeric_and_string_cells_stays_consistent() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let mut sm: slotmap::SlotMap<ElementId, ()> = slotmap::SlotMap::default();
        let elements: Vec<ElementId> = (0..200).map(|_| sm.insert(())).collect();

        // Even indices numeric, odd indices strings -- both arrays churn
        // together under the same delete/re-insert/type-change pattern the
        // single-array test above already proves for numerics alone.
        let value_for = |i: usize| -> CellValue {
            if i.is_multiple_of(2) { CellValue::Numeric((i + 1) as f64) } else { CellValue::String(format!("s{i}")) }
        };

        for (i, &e) in elements.iter().enumerate() {
            storage.set(cube, vec![e], value_for(i));
        }
        // Delete every fourth one (swap_remove churn in both arrays).
        for (i, &e) in elements.iter().enumerate() {
            if i % 4 == 0 {
                storage.set(cube, vec![e], CellValue::Numeric(0.0));
            }
        }
        // Flip every remaining cell's type (numeric<->string), exercising
        // set_cell's cross-array move on a large, mixed population.
        for (i, &e) in elements.iter().enumerate() {
            if i % 4 != 0 {
                let flipped = if i % 2 == 0 { CellValue::String(format!("flipped{i}")) } else { CellValue::Numeric(i as f64 + 500.0) };
                storage.set(cube, vec![e], flipped);
            }
        }

        let expected: Vec<(CellPath, CellValue)> = elements
            .iter()
            .enumerate()
            .filter_map(|(i, &e)| {
                if i % 4 == 0 {
                    None
                } else if i % 2 == 0 {
                    Some((CellPath::from_elem(e, 1), CellValue::String(format!("flipped{i}"))))
                } else {
                    Some((CellPath::from_elem(e, 1), CellValue::Numeric(i as f64 + 500.0)))
                }
            })
            .collect();

        assert_eq!(storage.filled_count(cube), expected.len());
        for (path, value) in &expected {
            assert_eq!(storage.get(cube, path.as_slice()), Some(value.clone()));
        }
        for (i, &e) in elements.iter().enumerate() {
            if i % 4 == 0 {
                assert!(storage.get(cube, &[e]).is_none());
            }
        }
        let mut got = storage.iter_cube(cube);
        let mut expected_sorted = expected;
        got.sort_by_key(|(path, _)| path.clone());
        expected_sorted.sort_by_key(|(path, _)| path.clone());
        assert_eq!(got, expected_sorted);
    }

    // -----------------------------------------------------------------
    // Hash collisions: `index` is keyed by a path's hash, not
    // the path itself, so a bucket can (rarely, with a real 64-bit hash)
    // hold more than one path's `CellRef`. A real collision can't be
    // triggered naturally in a unit test (astronomically unlikely at any
    // scale a test can afford). `index_get` always looks up a query
    // path's *own* real hash bucket -- there is no way to make it search
    // a different path's bucket -- so the only way to meaningfully force
    // a multi-candidate bucket is to plant *decoys* (other real cells'
    // `CellRef`s) inside the path we're actually going to query's own,
    // correctly-hashed bucket, then prove that query still resolves to
    // its own entry and nothing else's.
    // -----------------------------------------------------------------

    #[test]
    fn a_multi_entry_index_bucket_disambiguates_the_queried_path_from_decoys() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0, e1, e2] = fake_ids::<ElementId, 3>();

        // e1 first, then e0 -- e0 ends up as the *last* row in the
        // numeric table, so removing it later triggers no swap_remove
        // shuffle of e1's row (see the comment at that point below for
        // why that matters for this test's manually-injected decoys).
        storage.set(cube, vec![e1], CellValue::Numeric(2.0));
        storage.set(cube, vec![e0], CellValue::Numeric(1.0));
        storage.set(cube, vec![e2], CellValue::String("hi".to_string()));

        let partition_arc = storage.partition(cube).expect("cube must already have a partition");
        {
            let mut partition = partition_arc.write().expect("cube partition lock poisoned");
            let e1_ref = partition.hot.index_get(&[e1]).expect("e1 must already be indexed");
            let e2_ref = partition.hot.index_get(&[e2]).expect("e2 must already be indexed");
            // Plant e1's and e2's real CellRefs as decoys inside e0's own
            // bucket, alongside e0's real entry -- e1/e2 keep their own
            // separate, correct entries too, so this only affects queries
            // for e0 (the bucket `index_get(&[e0])` actually looks at).
            let h0 = hash_path(&[e0]);
            let bucket = partition.hot.index.get_mut(&h0).expect("e0 must already be indexed");
            bucket.push(e1_ref);
            bucket.push(e2_ref);
        }

        // get_cell must pick e0's own entry out of a 3-candidate bucket
        // by comparing each candidate's real, reconstructed path -- never
        // just the first one found.
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(1.0)));

        // set_cell's update-in-place path must still only touch e0's own
        // value.
        storage.set(cube, vec![e0], CellValue::Numeric(99.0));
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(99.0)));

        // remove_cell must remove only e0's own entry (matched by exact
        // CellRef equality, not "whatever's first in the bucket") --
        // e1's and e2's decoy entries must survive in the same bucket.
        // Since e0 is the *last* numeric row, this remove needs no
        // swap_remove shuffle of another real row (which would only
        // update that row's own, separate bucket -- not this manually
        // injected decoy copy, since the decoy was never a real tracked
        // entry to begin with).
        storage.set(cube, vec![e0], CellValue::Numeric(0.0)); // clears/removes e0
        assert!(storage.get(cube, &[e0]).is_none());
        let partition = partition_arc.read().expect("cube partition lock poisoned");
        let remaining = partition.hot.index.get(&hash_path(&[e0])).map(|b: &SmallVec<[CellRef; 1]>| b.len()).unwrap_or(0);
        assert_eq!(remaining, 2, "removing e0 must leave its bucket's decoy entries (e1's and e2's) untouched");
    }

    #[test]
    fn a_locked_cells_undo_log_is_unaffected_by_an_unrelated_cells_swap_remove() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0, e1, e2] = fake_ids::<ElementId, 3>();

        // e0 is the row that will later get swap_removed into by e1's
        // deletion (row storage order follows insertion order: e0 then
        // e1 then e2, so deleting e1 -- the middle row -- swaps the last
        // row, e2, into e1's old slot, which is the interesting case;
        // deleting the actual last-inserted row is the trivial no-fixup
        // case already covered elsewhere).
        storage.set(cube, vec![e0], CellValue::Numeric(1.0));
        storage.set(cube, vec![e1], CellValue::Numeric(2.0));
        storage.set(cube, vec![e2], CellValue::Numeric(3.0));

        let lock_on_e0 = storage.create_lock(cube, false, vec![vec![e0]], vec![set_of(&[e0])]);
        let lock_on_e2 = storage.create_lock(cube, false, vec![vec![e2]], vec![set_of(&[e2])]);

        // Delete e1 (unrelated to either lock) -- triggers a swap_remove
        // that moves e2's row into e1's old slot.
        storage.record_and_set(cube, vec![e1], CellValue::Numeric(0.0));
        assert!(storage.get(cube, &[e1]).is_none());

        // Now write through both locks -- each must still journal
        // against its own cell's correct pre-write value, proving
        // `index`/`rows` weren't desynced by the unrelated swap_remove.
        storage.record_and_set(cube, vec![e0], CellValue::Numeric(10.0));
        storage.record_and_set(cube, vec![e2], CellValue::Numeric(30.0));

        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(10.0)));
        assert_eq!(storage.get(cube, &[e2]), Some(CellValue::Numeric(30.0)));

        storage.rollback_lock(cube, lock_on_e0, None).unwrap();
        storage.rollback_lock(cube, lock_on_e2, None).unwrap();

        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(1.0)));
        assert_eq!(storage.get(cube, &[e2]), Some(CellValue::Numeric(3.0)));
    }

    // -----------------------------------------------------------------
    // Parallel scan: `iter_cube` takes rayon's parallel path once a
    // cube's stored-cell count reaches PARALLEL_SCAN_THRESHOLD. Every
    // other test above exercises the sequential branch implicitly (small
    // fixture cubes); this proves the parallel branch returns exactly
    // the same content, just via a different code path.
    // -----------------------------------------------------------------

    #[test]
    fn iter_cube_above_the_parallel_threshold_returns_every_stored_cell_exactly_once() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let mut sm: slotmap::SlotMap<ElementId, ()> = slotmap::SlotMap::default();
        let n = PARALLEL_SCAN_THRESHOLD * 3 + 7; // comfortably above the threshold, not a round multiple
        let elements: Vec<ElementId> = (0..n).map(|_| sm.insert(())).collect();

        for (i, &e) in elements.iter().enumerate() {
            // i + 1, never 0.0 -- Storage::set treats a numeric 0.0 as
            // "clear this cell" (CellValue::is_default), so a plain `i`
            // would silently drop element 0's cell and undercount by one.
            storage.set(cube, vec![e], CellValue::Numeric((i + 1) as f64));
        }

        let mut got = storage.iter_cube(cube);
        assert_eq!(got.len(), n);
        got.sort_by_key(|(path, _)| path.clone());

        let mut expected: Vec<(CellPath, CellValue)> =
            elements.iter().enumerate().map(|(i, &e)| (CellPath::from_elem(e, 1), CellValue::Numeric((i + 1) as f64))).collect();
        expected.sort_by_key(|(path, _)| path.clone());

        assert_eq!(got, expected);
    }

    // -----------------------------------------------------------------
    // Concurrency: proves the per-cube sharding is both correct
    // under real concurrent access AND actually independent per cube --
    // not just "doesn't panic," but "produces the exact result a
    // sequential execution would."
    // -----------------------------------------------------------------

    #[test]
    fn concurrent_writes_to_different_cubes_never_interfere() {
        let storage = Storage::new();
        let cubes: [CubeId; 8] = fake_ids();
        let [e0] = fake_ids::<ElementId, 1>();
        const WRITES_PER_CUBE: i64 = 500;

        std::thread::scope(|scope| {
            for (i, &cube) in cubes.iter().enumerate() {
                let storage = &storage;
                scope.spawn(move || {
                    for n in 0..WRITES_PER_CUBE {
                        // Each cube's thread writes a value that encodes
                        // which cube + which iteration it's on, so a
                        // cross-cube data race would be detectable (not
                        // just a panic) by seeing the wrong cube's value.
                        storage.set(cube, vec![e0], CellValue::Numeric((i as i64 * 10_000 + n) as f64));
                    }
                });
            }
        });

        for (i, &cube) in cubes.iter().enumerate() {
            let value = storage.get(cube, &[e0]).expect("each cube's thread wrote a value");
            match value {
                CellValue::Numeric(v) => {
                    let cube_tag = (v as i64) / 10_000;
                    assert_eq!(cube_tag, i as i64, "cube {i}'s final value has another cube's tag -- cross-cube interference");
                }
                other => panic!("expected numeric, got {other:?}"),
            }
        }
    }

    // -----------------------------------------------------------------
    // Batched reads/writes: one lock acquisition for
    // many cells in the same cube, instead of one per cell.
    // -----------------------------------------------------------------

    #[test]
    fn get_many_matches_looped_get_in_input_order() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0, e1, e2] = fake_ids::<ElementId, 3>();
        storage.set(cube, vec![e0], CellValue::Numeric(1.0));
        storage.set(cube, vec![e2], CellValue::Numeric(3.0));
        // e1 deliberately left unset.

        let paths = vec![vec![e2], vec![e0], vec![e1]];
        let batched = storage.get_many(cube, &paths);
        let looped: Vec<_> = paths.iter().map(|p| storage.get(cube, p)).collect();
        assert_eq!(batched, looped);
        assert_eq!(batched, vec![Some(CellValue::Numeric(3.0)), Some(CellValue::Numeric(1.0)), None]);
    }

    #[test]
    fn get_many_on_a_never_created_cube_returns_all_none() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0, e1] = fake_ids::<ElementId, 2>();
        assert_eq!(storage.get_many(cube, &[vec![e0], vec![e1]]), vec![None, None]);
    }

    #[test]
    fn record_and_set_many_matches_looped_record_and_set() {
        let storage_batched = Storage::new();
        let storage_looped = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let elements: [ElementId; 10] = fake_ids();

        let entries: Vec<(Vec<ElementId>, CellValue)> = elements.iter().enumerate().map(|(i, &e)| (vec![e], CellValue::Numeric(i as f64))).collect();

        storage_batched.record_and_set_many(cube, entries.clone());
        for (path, value) in entries {
            storage_looped.record_and_set(cube, path, value);
        }

        for &e in &elements {
            assert_eq!(storage_batched.get(cube, &[e]), storage_looped.get(cube, &[e]));
        }
    }

    #[test]
    fn record_and_set_many_journals_into_covering_locks_like_record_and_set() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0, e1] = fake_ids::<ElementId, 2>();
        storage.set(cube, vec![e0], CellValue::Numeric(10.0));
        storage.set(cube, vec![e1], CellValue::Numeric(20.0));

        let covering_both = storage.create_lock(cube, false, vec![vec![e0, e1]], vec![set_of(&[e0, e1])]);
        let covering_e0_only = storage.create_lock(cube, false, vec![vec![e0]], vec![set_of(&[e0])]);

        storage.record_and_set_many(cube, vec![(vec![e0], CellValue::Numeric(11.0)), (vec![e1], CellValue::Numeric(21.0))]);

        assert_eq!(storage.find_lock(cube, covering_both).unwrap().undo_log.len(), 2);
        assert_eq!(storage.find_lock(cube, covering_e0_only).unwrap().undo_log.len(), 1);

        storage.rollback_lock(cube, covering_both, None).unwrap();
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(10.0)));
        assert_eq!(storage.get(cube, &[e1]), Some(CellValue::Numeric(20.0)));
    }

    /// The correctness property batching adds beyond raw speed: with
    /// per-cell locking, a concurrent reader could catch a large splash
    /// half-applied (some cells already updated, some not yet). Batching
    /// the whole write under one partition-lock acquisition makes it
    /// atomic with respect to any other access to this cube -- a reader
    /// only ever sees "all old values" or "all new values," never a mix.
    #[test]
    fn record_and_set_many_is_atomic_relative_to_other_writes_on_the_same_cube() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let elements: [ElementId; 50] = fake_ids();

        let stop = AtomicBool::new(false);
        let saw_mixed = AtomicBool::new(false);

        std::thread::scope(|scope| {
            let storage = &storage;
            let stop = &stop;
            let saw_mixed = &saw_mixed;

            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let paths: Vec<Vec<ElementId>> = elements.iter().map(|&e| vec![e]).collect();
                    let values = storage.get_many(cube, &paths);
                    let as_numbers: Vec<f64> = values
                        .into_iter()
                        .map(|v| match v {
                            Some(CellValue::Numeric(v)) => v,
                            _ => 0.0,
                        })
                        .collect();
                    let all_zero = as_numbers.iter().all(|&v| v == 0.0);
                    let all_one = as_numbers.iter().all(|&v| v == 1.0);
                    if !all_zero && !all_one {
                        saw_mixed.store(true, Ordering::Relaxed);
                    }
                }
            });

            for _ in 0..500 {
                let ones = elements.iter().map(|&e| (vec![e], CellValue::Numeric(1.0))).collect::<Vec<_>>();
                storage.record_and_set_many(cube, ones);
                let zeros = elements.iter().map(|&e| (vec![e], CellValue::Numeric(0.0))).collect::<Vec<_>>();
                storage.record_and_set_many(cube, zeros);
            }

            stop.store(true, Ordering::Relaxed);
        });

        assert!(!saw_mixed.load(Ordering::Relaxed), "reader observed a batch write half-applied -- record_and_set_many isn't atomic relative to concurrent readers");
    }

    #[test]
    fn concurrent_writes_to_different_cells_of_the_same_cube_lose_no_updates() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let elements: [ElementId; 16] = fake_ids();
        const INCREMENTS: usize = 200;

        std::thread::scope(|scope| {
            for &element in &elements {
                let storage = &storage;
                scope.spawn(move || {
                    for _ in 0..INCREMENTS {
                        // Read-modify-write on this cell specifically --
                        // a lost update here (from two threads racing on
                        // the SAME cell) would under-count.
                        let current = match storage.get(cube, &[element]) {
                            Some(CellValue::Numeric(v)) => v,
                            _ => 0.0,
                        };
                        storage.set(cube, vec![element], CellValue::Numeric(current + 1.0));
                    }
                });
            }
        });

        // Each element has its own dedicated thread (no two threads ever
        // write the same cell concurrently here), so every cell must end
        // up at exactly INCREMENTS -- proves the per-cube lock correctly
        // serializes access within a cube with no lost updates, and that
        // different cells' threads didn't corrupt each other's counters.
        for &element in &elements {
            assert_eq!(storage.get(cube, &[element]), Some(CellValue::Numeric(INCREMENTS as f64)));
        }
    }

    // -----------------------------------------------------------------
    // Capacity management: `record_and_set_many` reserving up front and
    // `Storage::shrink_cube` reclaiming growth slack afterward must not
    // change any observable content -- only how much capacity `rows`/
    // `index` hold underneath it.
    // -----------------------------------------------------------------

    #[test]
    fn reserve_additional_then_bulk_insert_matches_unreserved_insert() {
        let reserved = Storage::new();
        let unreserved = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let elements: [ElementId; 500] = fake_ids();

        let entries: Vec<(Vec<ElementId>, CellValue)> = elements.iter().enumerate().map(|(i, &e)| (vec![e], CellValue::Numeric(i as f64 + 1.0))).collect();

        // record_and_set_many always reserves internally now -- this test
        // is really about a batch big enough to have needed several
        // doubling-growth reallocations landing on exactly the same
        // content as one that never needed to grow at all.
        reserved.record_and_set_many(cube, entries.clone());
        for (path, value) in entries {
            unreserved.set(cube, path, value);
        }

        let mut got = reserved.iter_cube(cube);
        let mut expected = unreserved.iter_cube(cube);
        got.sort_by_key(|(path, _)| path.clone());
        expected.sort_by_key(|(path, _)| path.clone());
        assert_eq!(got, expected);
        assert_eq!(reserved.filled_count(cube), elements.len());
    }

    #[test]
    fn memory_estimate_follows_the_cells_through_compaction() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let elements: [ElementId; 1000] = fake_ids();
        storage.cube_created(cube, 2);
        assert_eq!(storage.memory(cube).unwrap().hot_cells, 0);

        for (i, &e) in elements.iter().enumerate() {
            storage.set(cube, vec![e, elements[i % 10]], CellValue::Numeric(i as f64 + 1.0));
        }
        let hot = storage.memory(cube).unwrap();
        assert_eq!((hot.hot_cells, hot.cold_cells, hot.cold_bytes), (1000, 0, 0));
        // At least two 8-byte ids and an 8-byte value per cell.
        assert!(hot.hot_bytes >= 1000 * 24, "{hot:?}");

        storage.shrink_cube(cube); // compaction: everything moves to cold
        let cold = storage.memory(cube).unwrap();
        assert_eq!((cold.hot_cells, cold.cold_cells), (0, 1000));
        assert!(cold.cold_bytes > 0 && cold.cold_bytes < hot.hot_bytes, "compressed is smaller: {hot:?} -> {cold:?}");
        assert!(storage.memory(fake_ids::<CubeId, 2>()[1]).is_none());
    }

    #[test]
    fn shrink_cube_preserves_all_cells_and_lookups() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let elements: [ElementId; 200] = fake_ids();

        for (i, &e) in elements.iter().enumerate() {
            storage.set(cube, vec![e], CellValue::Numeric(i as f64 + 1.0));
        }
        // Delete half, so shrink_to_fit has real slack to reclaim, not
        // just whatever capacity growth left over from inserts alone.
        for (i, &e) in elements.iter().enumerate() {
            if i % 2 == 0 {
                storage.set(cube, vec![e], CellValue::Numeric(0.0));
            }
        }

        let mut before = storage.iter_cube(cube);
        before.sort_by_key(|(path, _)| path.clone());

        storage.shrink_cube(cube);

        let mut after = storage.iter_cube(cube);
        after.sort_by_key(|(path, _)| path.clone());
        assert_eq!(before, after);
        assert_eq!(storage.filled_count(cube), elements.len() / 2);

        // Every surviving element must still be point-lookupable (proves
        // shrink_cube's index rebuild, not just rows, stayed correct).
        for (i, &e) in elements.iter().enumerate() {
            if i % 2 == 1 {
                assert_eq!(storage.get(cube, &[e]), Some(CellValue::Numeric(i as f64 + 1.0)));
            } else {
                assert!(storage.get(cube, &[e]).is_none());
            }
        }

        // A write after shrinking must still work -- shrink_cube must not
        // have left rows/index in a state that only tolerates reads.
        storage.set(cube, vec![elements[0]], CellValue::Numeric(999.0));
        assert_eq!(storage.get(cube, &[elements[0]]), Some(CellValue::Numeric(999.0)));
    }

    #[test]
    fn shrink_cube_on_a_never_created_cube_is_a_harmless_no_op() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        storage.shrink_cube(cube); // must not panic
        assert_eq!(storage.filled_count(cube), 0);
    }

    #[test]
    fn reserve_cube_up_front_avoids_batch_by_batch_overshoot() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        const TOTAL: usize = 1000;
        const BATCH: usize = 50;
        let elements: [ElementId; TOTAL] = fake_ids();

        // Mirrors a migration's shape: the true total is known (from the
        // source cube's filled-cell count) before a single batch is written.
        storage.reserve_cube(cube, TOTAL);
        for chunk in elements.chunks(BATCH) {
            let batch: Vec<(Vec<ElementId>, CellValue)> = chunk.iter().enumerate().map(|(i, &e)| (vec![e], CellValue::Numeric(i as f64 + 1.0))).collect();
            storage.record_and_set_many(cube, batch);
        }

        assert_eq!(storage.filled_count(cube), TOTAL);
        let cap = storage.capacity(cube).unwrap();
        // reserve_exact_additional up front should mean rows never grew
        // beyond exactly what was reserved -- record_and_set_many's own
        // per-batch reserve is a no-op once capacity is already
        // sufficient, so no amortized-doubling overshoot should occur.
        assert_eq!(cap.hot_numeric_capacity, TOTAL, "hot's numeric table overshot its up-front exact reservation");
        for &e in &elements {
            assert!(storage.get(cube, &[e]).is_some());
        }
    }

    #[test]
    fn reserve_cube_on_a_new_cube_does_not_disturb_later_reads() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0] = fake_ids::<ElementId, 1>();
        storage.reserve_cube(cube, 10);
        assert_eq!(storage.filled_count(cube), 0);
        assert!(storage.get(cube, &[e0]).is_none());
        storage.set(cube, vec![e0], CellValue::Numeric(1.0));
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(1.0)));
    }

    // -----------------------------------------------------------------
    // Two-tier hot/cold storage: `shrink_cube_preserves_all_
    // cells_and_lookups` above already exercises one real compaction
    // end-to-end; these tests target compaction-specific risk a plain
    // "before == after" check doesn't: a *second* compaction correctly
    // merging new hot writes into an already-compacted cold run (not just
    // building cold from scratch), a delete of a cold-resident cell being
    // shadowed by a hot tombstone rather than either resurrecting on the
    // next read or panicking, and a randomized set/remove/compact sequence
    // agreeing with a plain-HashMap reference model at every step (the
    // highest-value test given that a tiering bug would be silent data
    // loss or resurrection, not a crash).
    // -----------------------------------------------------------------

    #[test]
    fn a_second_compaction_correctly_merges_new_hot_writes_into_an_already_compacted_cold_run() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        // 7 ids from one shared `fake_ids` call -- `new_elem` (below) needs
        // to be guaranteed distinct from `elements`, which two separate
        // `fake_ids` calls can't promise: each call spins up its own fresh
        // `SlotMap`, so their first-inserted keys carry identical `idx`/
        // `version` and are literally the same `ElementId`.
        let ids: [ElementId; 7] = fake_ids();
        let (elements, new_elem) = (&ids[..6], ids[6]);

        // First generation: write 0..6, compact -- all 6 now live in cold.
        for (i, &e) in elements.iter().enumerate() {
            storage.set(cube, vec![e], CellValue::Numeric(i as f64 + 1.0));
        }
        storage.shrink_cube(cube); // forces CubePartition::compact
        for (i, &e) in elements.iter().enumerate() {
            assert_eq!(storage.get(cube, &[e]), Some(CellValue::Numeric(i as f64 + 1.0)));
        }

        // Second generation, applied entirely to the now-empty hot tier:
        // overwrite element 0 (type change to string), delete element 1
        // (a cold-resident delete -- must shadow, not panic), leave
        // elements 2..6 untouched in cold, add a brand new element 6.
        storage.set(cube, vec![elements[0]], CellValue::String("changed".to_string()));
        storage.set(cube, vec![elements[1]], CellValue::Numeric(0.0)); // clears/removes
        storage.set(cube, vec![new_elem], CellValue::Numeric(100.0));

        // Compact again -- this must merge the second generation's hot
        // writes into the *existing* cold run (cold::merge's `old`
        // parameter), not just discard it.
        storage.shrink_cube(cube);

        assert_eq!(storage.get(cube, &[elements[0]]), Some(CellValue::String("changed".to_string())));
        assert!(storage.get(cube, &[elements[1]]).is_none());
        for (i, &e) in elements.iter().enumerate().skip(2) {
            assert_eq!(storage.get(cube, &[e]), Some(CellValue::Numeric(i as f64 + 1.0)));
        }
        assert_eq!(storage.get(cube, &[new_elem]), Some(CellValue::Numeric(100.0)));
        assert_eq!(storage.filled_count(cube), 6); // 6 original - 1 deleted + 1 new

        let mut got = storage.iter_cube(cube);
        got.sort_by_key(|(path, _)| path.clone());
        assert_eq!(got.len(), 6);
    }

    #[test]
    fn compacting_an_already_compacted_cube_is_a_cheap_no_op_that_preserves_data() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let elements: [ElementId; 5] = fake_ids();

        for (i, &e) in elements.iter().enumerate() {
            storage.set(cube, vec![e], CellValue::Numeric(i as f64 + 1.0));
        }
        storage.shrink_cube(cube); // first real compaction: hot -> cold
        let cap_after_first = storage.capacity(cube).unwrap();
        assert_eq!(cap_after_first.cold_len, 5);

        // Calling compact again with nothing new in hot must not touch the
        // existing cold run at all -- exactly the "safe to call liberally"
        // property ICube.compact()/IDimension.compact() are meant to have
        // (callers are expected to call this at every natural "done for
        // now" point, not sparingly).
        storage.shrink_cube(cube);
        let cap_after_second = storage.capacity(cube).unwrap();
        assert_eq!(cap_after_second, cap_after_first, "compacting an already-empty hot tier must be a true no-op, not a wasted full re-encode");

        for (i, &e) in elements.iter().enumerate() {
            assert_eq!(storage.get(cube, &[e]), Some(CellValue::Numeric(i as f64 + 1.0)));
        }
    }

    #[test]
    fn deleting_a_cold_resident_cell_shadows_it_until_the_next_compaction() {
        let storage = Storage::new();
        let [cube] = fake_ids::<CubeId, 1>();
        let [e0, e1] = fake_ids::<ElementId, 2>();

        storage.set(cube, vec![e0], CellValue::Numeric(1.0));
        storage.set(cube, vec![e1], CellValue::Numeric(2.0));
        storage.shrink_cube(cube); // e0, e1 now cold-resident

        // Delete e0 -- must shadow the cold entry with a hot tombstone
        // immediately, not require a second compaction to take effect.
        storage.set(cube, vec![e0], CellValue::Numeric(0.0));
        assert!(storage.get(cube, &[e0]).is_none());
        assert_eq!(storage.get(cube, &[e1]), Some(CellValue::Numeric(2.0)));
        assert_eq!(storage.filled_count(cube), 1);

        let mut got = storage.iter_cube(cube);
        got.sort_by_key(|(path, _)| path.clone());
        assert_eq!(got, vec![(CellPath::from_slice(&[e1]), CellValue::Numeric(2.0))]);

        // A re-write of the tombstoned path must un-delete it (a fresh
        // real value takes priority over its own prior tombstone).
        storage.set(cube, vec![e0], CellValue::Numeric(42.0));
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(42.0)));
        assert_eq!(storage.filled_count(cube), 2);

        // The tombstone-then-resurrect sequence must also survive a real
        // compaction (proves the tombstone doesn't leak into the merged
        // cold run as a phantom "still deleted" marker once superseded).
        storage.shrink_cube(cube);
        assert_eq!(storage.get(cube, &[e0]), Some(CellValue::Numeric(42.0)));
        assert_eq!(storage.filled_count(cube), 2);
    }

    /// A randomized `Set`/`Remove`/`Compact` sequence over a small element
    /// pool, checked after every single step against a plain `HashMap`
    /// reference model -- the highest-value test here, given that a real tiering bug here would
    /// be silent data loss or resurrection, not a crash a simpler test
    /// would catch.
    #[derive(Debug, Clone)]
    enum ModelOp {
        Set(usize, i64),
        Remove(usize),
        Compact,
    }

    fn model_op_strategy() -> impl proptest::strategy::Strategy<Value = ModelOp> {
        use proptest::strategy::Strategy;
        proptest::prop_oneof![
            (0usize..6, -3i64..3).prop_map(|(i, v)| ModelOp::Set(i, v)),
            (0usize..6).prop_map(ModelOp::Remove),
            proptest::strategy::Just(ModelOp::Compact),
        ]
    }

    proptest::proptest! {
        #[test]
        fn hot_cold_tiering_matches_a_plain_hashmap_reference_model(ops in proptest::collection::vec(model_op_strategy(), 1..80)) {
            let storage = Storage::new();
            let [cube] = fake_ids::<CubeId, 1>();
            let elements: [ElementId; 6] = fake_ids();
            let mut model: HashMap<ElementId, f64> = HashMap::new();

            for op in ops {
                match op {
                    ModelOp::Set(i, v) => {
                        let value = v as f64;
                        storage.set(cube, vec![elements[i]], CellValue::Numeric(value));
                        if value == 0.0 {
                            model.remove(&elements[i]);
                        } else {
                            model.insert(elements[i], value);
                        }
                    }
                    ModelOp::Remove(i) => {
                        storage.set(cube, vec![elements[i]], CellValue::Numeric(0.0));
                        model.remove(&elements[i]);
                    }
                    ModelOp::Compact => {
                        storage.shrink_cube(cube);
                    }
                }

                for &e in &elements {
                    let expected = model.get(&e).map(|&v| CellValue::Numeric(v));
                    proptest::prop_assert_eq!(storage.get(cube, &[e]), expected, "point lookup disagreed with reference model after an op");
                }
                proptest::prop_assert_eq!(storage.filled_count(cube), model.len(), "filled_count disagreed with reference model after an op");

                let mut got: Vec<(ElementId, CellValue)> = storage.iter_cube(cube).into_iter().map(|(p, v)| (p[0], v)).collect();
                got.sort_by_key(|(e, _)| *e);
                let mut expected: Vec<(ElementId, CellValue)> = model.iter().map(|(&e, &v)| (e, CellValue::Numeric(v))).collect();
                expected.sort_by_key(|(e, _)| *e);
                proptest::prop_assert_eq!(got, expected, "iter_cube disagreed with reference model after an op");
            }
        }
    }
}
