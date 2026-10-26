//! The mutable "hot" tier (columnar rows + hash-bucketed index), holding only
//! recent/pending writes rather than every cell, since the bulk of a
//! cube's cells live compressed in `cold::ColdRun` once compacted. Small
//! by construction (compaction keeps it that way), so its per-cell
//! overhead not mattering much is exactly the point.

use crate::{hash_path, CellPath, CellValue, ColumnarRows};
use fluxolap_domain::ElementId;
use smallvec::SmallVec;
use std::collections::HashMap;

/// Which of `HotTier`'s three tables a path's row lives in, and at what
/// index. `Tombstone` marks a path as deleted *while it (or a value for
/// it) exists in the cold tier* -- cold pages are immutable, so a delete
/// of a cold-resident cell can't remove it there; it shadows it with a
/// tombstone instead, resolved away for real the next time this cube
/// compacts (see `cold::merge`'s `shadowed` set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellRef {
    Numeric(u32),
    Str(u32),
    Tombstone(u32),
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct HotTier {
    numeric: ColumnarRows<f64>,
    string: ColumnarRows<String>,
    /// Values are `()` -- a tombstone only needs to occupy an index slot
    /// and remember its path (via `ColumnarRows::path_at`) for collision
    /// resolution; `Vec<()>` is zero-sized-type-optimized to effectively
    /// free, so this reuses all of `ColumnarRows`'s existing push/
    /// swap_remove machinery instead of a bespoke tombstone list.
    tombstones: ColumnarRows<()>,
    /// `pub(crate)` (rather than fully private) only so `lib.rs`'s own
    /// white-box collision test (`a_multi_entry_index_bucket_disambiguates_
    /// the_queried_path_from_decoys`) can plant decoy bucket entries
    /// directly -- no other code outside this module touches it.
    #[serde(skip)]
    pub(crate) index: HashMap<u64, SmallVec<[CellRef; 1]>>,
}

impl HotTier {
    pub(crate) fn with_dims(n_dims: usize) -> Self {
        Self { numeric: ColumnarRows::with_dims(n_dims), string: ColumnarRows::with_dims(n_dims), tombstones: ColumnarRows::with_dims(n_dims), index: HashMap::new() }
    }

    pub(crate) fn n_dims(&self) -> usize {
        self.numeric.dim_columns.len()
    }

    /// Every row across all three tables -- the size that actually drives
    /// memory and the compaction trigger, tombstones included (a tombstone
    /// is a real, if small, hot-tier row).
    pub(crate) fn total_len(&self) -> usize {
        self.numeric.len() + self.string.len() + self.tombstones.len()
    }

    fn path_of(&self, cell_ref: CellRef) -> CellPath {
        match cell_ref {
            CellRef::Numeric(i) => self.numeric.path_at(i as usize),
            CellRef::Str(i) => self.string.path_at(i as usize),
            CellRef::Tombstone(i) => self.tombstones.path_at(i as usize),
        }
    }

    pub(crate) fn index_get(&self, path: &[ElementId]) -> Option<CellRef> {
        let bucket = self.index.get(&hash_path(path))?;
        bucket.iter().copied().find(|&r| self.path_of(r).as_slice() == path)
    }

    fn index_insert(&mut self, path: &[ElementId], cell_ref: CellRef) {
        self.index.entry(hash_path(path)).or_default().push(cell_ref);
    }

    fn index_remove_ref(&mut self, path: &[ElementId], cell_ref: CellRef) {
        let h = hash_path(path);
        if let Some(bucket) = self.index.get_mut(&h) {
            if let Some(pos) = bucket.iter().position(|&r| r == cell_ref) {
                bucket.swap_remove(pos);
                if bucket.is_empty() {
                    self.index.remove(&h);
                }
            }
        }
    }

    /// `Some` value only for a live (numeric/string) ref -- `None` for a
    /// tombstone, since that's precisely what "definitely absent" means.
    pub(crate) fn value_at(&self, cell_ref: CellRef) -> Option<CellValue> {
        match cell_ref {
            CellRef::Numeric(i) => Some(CellValue::Numeric(self.numeric.values[i as usize])),
            CellRef::Str(i) => Some(CellValue::String(self.string.values[i as usize].clone())),
            CellRef::Tombstone(_) => None,
        }
    }

    /// Physically removes whichever table `cell_ref` names at `path`
    /// (numeric, string, *or* tombstone -- all three tables need the same
    /// swap-remove-with-index-fixup treatment), fixing up both `cell_ref`'s
    /// own index entry and (if a row got swapped into the freed slot) that
    /// row's index entry too. Shared by `set_cell`'s type-change branches
    /// and the physical-delete path of `remove_live`.
    fn remove_from_table(&mut self, path: &[ElementId], cell_ref: CellRef) {
        self.index_remove_ref(path, cell_ref);
        match cell_ref {
            CellRef::Numeric(i) => {
                let (_, swapped) = self.numeric.swap_remove(i as usize);
                if let Some((old_i, swapped_path)) = swapped {
                    self.index_remove_ref(&swapped_path, CellRef::Numeric(old_i as u32));
                    self.index_insert(&swapped_path, CellRef::Numeric(i));
                }
            }
            CellRef::Str(i) => {
                let (_, swapped) = self.string.swap_remove(i as usize);
                if let Some((old_i, swapped_path)) = swapped {
                    self.index_remove_ref(&swapped_path, CellRef::Str(old_i as u32));
                    self.index_insert(&swapped_path, CellRef::Str(i));
                }
            }
            CellRef::Tombstone(i) => {
                let (_, swapped) = self.tombstones.swap_remove(i as usize);
                if let Some((old_i, swapped_path)) = swapped {
                    self.index_remove_ref(&swapped_path, CellRef::Tombstone(old_i as u32));
                    self.index_insert(&swapped_path, CellRef::Tombstone(i));
                }
            }
        }
    }

    /// Inserts a new cell or overwrites/retypes an existing one. Always
    /// lands in `numeric`/`string` (never leaves a tombstone in place --
    /// writing a real value to a previously-deleted path un-deletes it).
    pub(crate) fn set_cell(&mut self, path: CellPath, value: CellValue) {
        match (self.index_get(path.as_slice()), value) {
            (Some(CellRef::Numeric(i)), CellValue::Numeric(v)) => {
                self.numeric.values[i as usize] = v;
            }
            (Some(CellRef::Str(i)), CellValue::String(s)) => {
                self.string.values[i as usize] = s;
            }
            (Some(old_ref), CellValue::Numeric(v)) => {
                // old_ref is Str or Tombstone -- move to numeric.
                self.remove_from_table(path.as_slice(), old_ref);
                let new_i = self.numeric.push(path.as_slice(), v);
                self.index_insert(path.as_slice(), CellRef::Numeric(new_i));
            }
            (Some(old_ref), CellValue::String(s)) => {
                // old_ref is Numeric or Tombstone -- move to string.
                self.remove_from_table(path.as_slice(), old_ref);
                let new_i = self.string.push(path.as_slice(), s);
                self.index_insert(path.as_slice(), CellRef::Str(new_i));
            }
            (None, CellValue::Numeric(v)) => {
                let i = self.numeric.push(path.as_slice(), v);
                self.index_insert(path.as_slice(), CellRef::Numeric(i));
            }
            (None, CellValue::String(s)) => {
                let i = self.string.push(path.as_slice(), s);
                self.index_insert(path.as_slice(), CellRef::Str(i));
            }
        }
    }

    /// Physically removes a live (non-tombstone) cell that `index_get`
    /// already confirmed exists, returning its old value.
    pub(crate) fn remove_live(&mut self, path: &[ElementId], cell_ref: CellRef) -> CellValue {
        let value = self.value_at(cell_ref).expect("caller already confirmed this is a live (non-tombstone) ref");
        self.remove_from_table(path, cell_ref);
        value
    }

    /// Marks `path` as deleted -- used when `path` has no hot entry but
    /// does exist in the cold tier, so cold's (immutable) value needs
    /// shadowing rather than physical removal.
    pub(crate) fn insert_tombstone(&mut self, path: &[ElementId]) {
        let i = self.tombstones.push(path, ());
        self.index_insert(path, CellRef::Tombstone(i));
    }

    /// Empties every table -- called by `CubePartition::compact` right
    /// after folding hot's live entries into a new cold run, so this also
    /// `shrink_to_fit`s: without it, a hot tier that just grew to
    /// `HOT_COMPACT_TRIGGER` cells would sit at that same (now entirely
    /// unused) `Vec`/`HashMap` capacity forever, defeating the whole point
    /// of bounding hot's *peak* size via compaction in the first place.
    pub(crate) fn clear(&mut self) {
        self.numeric.clear();
        self.numeric.shrink_to_fit();
        self.string.clear();
        self.string.shrink_to_fit();
        self.tombstones.clear();
        self.tombstones.shrink_to_fit();
        self.index.clear();
        self.index.shrink_to_fit();
    }

    pub(crate) fn reserve(&mut self, additional: usize) {
        self.numeric.reserve(additional);
        self.index.reserve(additional);
    }

    pub(crate) fn reserve_exact(&mut self, additional: usize) {
        self.numeric.reserve_exact(additional);
        self.index.reserve(additional);
    }

    /// Rebuilds `index` from `numeric`/`string`/`tombstones` -- needed
    /// after deserializing a `HotTier` (whose `index` field is
    /// `#[serde(skip)]`'d and comes back empty via `Default`, unlike the
    /// other three tables which are real serialized data), via
    /// `Storage::from_snapshot`.
    pub(crate) fn rebuild_index(&mut self) {
        let mut index = HashMap::with_capacity(self.total_len());
        for i in 0..self.numeric.len() {
            let path = self.numeric.path_at(i);
            index.entry(hash_path(&path)).or_insert_with(SmallVec::new).push(CellRef::Numeric(i as u32));
        }
        for i in 0..self.string.len() {
            let path = self.string.path_at(i);
            index.entry(hash_path(&path)).or_insert_with(SmallVec::new).push(CellRef::Str(i as u32));
        }
        for i in 0..self.tombstones.len() {
            let path = self.tombstones.path_at(i);
            index.entry(hash_path(&path)).or_insert_with(SmallVec::new).push(CellRef::Tombstone(i as u32));
        }
        self.index = index;
    }

    // -- Diagnostics (see `crate::PartitionCapacity`); never read on any
    // request-serving path.

    pub(crate) fn numeric_len(&self) -> usize {
        self.numeric.len()
    }

    pub(crate) fn numeric_capacity(&self) -> usize {
        self.numeric.capacity()
    }

    pub(crate) fn string_len(&self) -> usize {
        self.string.len()
    }

    pub(crate) fn string_capacity(&self) -> usize {
        self.string.capacity()
    }

    pub(crate) fn tombstones_len(&self) -> usize {
        self.tombstones.len()
    }

    pub(crate) fn index_len(&self) -> usize {
        self.index.len()
    }

    pub(crate) fn index_capacity(&self) -> usize {
        self.index.capacity()
    }

    /// Estimated heap bytes: the columns by capacity, the string cells'
    /// texts, and the index (hashbrown keeps 1/8 of its buckets free and one
    /// control byte per bucket; a bucket that spilled to the heap is rare
    /// and not counted).
    pub(crate) fn heap_bytes(&self) -> usize {
        let buckets = if self.index.capacity() == 0 { 0 } else { (self.index.capacity() * 8 / 7).next_power_of_two() };
        let index = buckets * (std::mem::size_of::<(u64, SmallVec<[CellRef; 1]>)>() + 1);
        let texts: usize = self.string.values.iter().map(String::capacity).sum();
        self.numeric.heap_bytes() + self.string.heap_bytes() + texts + self.tombstones.heap_bytes() + index
    }

    /// Number of live (non-tombstone) hot entries.
    pub(crate) fn live_len(&self) -> usize {
        self.numeric.len() + self.string.len()
    }

    /// Every path this tier holds an entry for -- live cells and tombstones alike.
    pub(crate) fn all_paths(&self) -> Vec<CellPath> {
        let mut paths = Vec::with_capacity(self.total_len());
        paths.extend((0..self.numeric.len()).map(|i| self.numeric.path_at(i)));
        paths.extend((0..self.string.len()).map(|i| self.string.path_at(i)));
        paths.extend((0..self.tombstones.len()).map(|i| self.tombstones.path_at(i)));
        paths
    }

    /// Every live (non-tombstone) entry as owned `(CellPath, CellValue)`
    /// pairs, plus the set of *tombstoned* paths -- exactly what
    /// `CubePartition::compact`'s merge needs: entries to fold in, and
    /// paths that shadow an old cold entry with no live replacement of its
    /// own (a genuine delete). A tombstoned path with no live counterpart
    /// has no other way to reach `merge` at all (tombstones are never
    /// pushed into `entries`), so this set can't be derived from `entries`
    /// alone the way a live-vs-cold collision can (`merge`'s own sorted
    /// comparison catches that case directly, by path equality, without
    /// needing every live path duplicated into a second collection here --
    /// see the git history for why an earlier version of this method
    /// built a `shadowed` set covering live paths too: for a
    /// multi-hundred-thousand-cell hot tier, that was a full second
    /// hash-table copy of every live path sitting in memory for the
    /// duration of one compaction, on top of `entries` itself and the
    /// cold run being built -- a real, measured memory spike during
    /// compaction, not just an unnecessary allocation).
    pub(crate) fn live_entries_and_tombstoned(&self) -> (Vec<(CellPath, CellValue)>, std::collections::HashSet<CellPath>) {
        let mut entries = Vec::with_capacity(self.numeric.len() + self.string.len());
        for i in 0..self.numeric.len() {
            entries.push((self.numeric.path_at(i), CellValue::Numeric(self.numeric.values[i])));
        }
        for i in 0..self.string.len() {
            entries.push((self.string.path_at(i), CellValue::String(self.string.values[i].clone())));
        }
        let tombstoned = (0..self.tombstones.len()).map(|i| self.tombstones.path_at(i)).collect();
        (entries, tombstoned)
    }

    /// Every live entry, without `live_entries_and_shadowed`'s set of shadowed paths -- for a
    /// partition with no cold tier there is nothing to shadow, and building that set (a
    /// second hash-table copy of every path) was most of a scan's cost on a never-compacted cube.
    pub(crate) fn live_entries(&self) -> Vec<(CellPath, CellValue)> {
        let mut entries = Vec::with_capacity(self.numeric.len() + self.string.len());
        for i in 0..self.numeric.len() {
            entries.push((self.numeric.path_at(i), CellValue::Numeric(self.numeric.values[i])));
        }
        for i in 0..self.string.len() {
            entries.push((self.string.path_at(i), CellValue::String(self.string.values[i].clone())));
        }
        entries
    }

    /// Like `live_entries_and_tombstoned`, but the second element is the
    /// full shadow set (every live path *and* every tombstoned path), for
    /// `Storage::iter_cube`'s different use: a plain filter over cold's
    /// already-separately-obtained iterator (`!shadowed.contains(p)`), not
    /// a sorted merge that can detect a live collision by comparison the
    /// way `compact` does. Kept as its own method (rather than the leaner
    /// `live_entries_and_tombstoned` plus a cheap live-only extension)
    /// since `iter_cube` doesn't sort hot's entries at all -- unlike
    /// compaction, which is comparatively rare (gated by
    /// `HOT_COMPACT_TRIGGER`), a full-database scan doesn't currently
    /// justify paying sort's extra cost just to reuse the leaner method.
    pub(crate) fn live_entries_and_shadowed(&self) -> (Vec<(CellPath, CellValue)>, std::collections::HashSet<CellPath>) {
        let mut entries = Vec::with_capacity(self.numeric.len() + self.string.len());
        let mut shadowed = std::collections::HashSet::with_capacity(self.total_len());
        for i in 0..self.numeric.len() {
            let path = self.numeric.path_at(i);
            shadowed.insert(path.clone());
            entries.push((path, CellValue::Numeric(self.numeric.values[i])));
        }
        for i in 0..self.string.len() {
            let path = self.string.path_at(i);
            shadowed.insert(path.clone());
            entries.push((path, CellValue::String(self.string.values[i].clone())));
        }
        for i in 0..self.tombstones.len() {
            shadowed.insert(self.tombstones.path_at(i));
        }
        (entries, shadowed)
    }
}
