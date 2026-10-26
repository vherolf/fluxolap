//! Immutable, sorted, prefix/delta-compressed cold cell storage. Measured
//! at 28.9 bytes/cell on a real 26.16M-cell, 19-dimension database.
//! Built only by `CubePartition::compact` (`lib.rs`), never mutated
//! afterward -- a `ColdRun` is always wholesale-replaced by compaction,
//! never edited in place, which is what keeps this module's logic free of
//! the in-place page-splice/rebuild machinery a trie-based store needs for
//! out-of-order writes (typically its biggest source of complexity and
//! bugs).
//!
//! Encoding, one row (`n_dims` element ids + one value) at a time, in
//! sorted order: every `RESTART_INTERVAL`-th row is a "restart" (all
//! `n_dims` element ids stored as raw absolute `u64`s, no delta); every
//! other row stores each dimension as a one-byte tag (`Same`/`Succ`/a
//! signed delta of 1/2/4 bytes/`Abs8`) relative to the *previous row's*
//! value at that dimension -- the classic front-coding + delta-successor
//! trick, just applied as a flat per-row record instead of a trie. The
//! value follows, itself tagged (`U8`/`I32`/`F32`/`F64`/`Str`) by
//! magnitude/type (value-level, not sort-order-dependent).
//! A sparse `restarts` index (one entry per `RESTART_INTERVAL` rows, not
//! per cell) gives `get`'s point lookups an O(log restarts) starting point
//! instead of a full scan.
//!
//! **Ordering note (a real bug caught during design, worth documenting):**
//! all sorting/comparison in this module operates on each path's
//! `element_id.data().as_ffi()` `u64` values (`path_ffi`), *not*
//! `ElementId`'s own derived `Ord`. `KeyData` derives `Ord` as
//! `(idx, version)` lexicographic, but `as_ffi()` packs `(version << 32) |
//! idx` -- a *different* ordering (version-major) whenever any element's
//! `version` isn't 1. Since encoding a row's dimensions already needs the
//! `u64` form for delta arithmetic, using that same `u64` form as the
//! single source of truth for sort order and restart/early-exit
//! comparisons avoids the two orderings ever silently disagreeing.

use crate::{CellPath, CellValue};
use fluxolap_domain::ElementId;
use slotmap::{Key, KeyData};

/// Every this-many-th row is a restart (absolute-encoded, indexed) in a
/// *newly built* run. A point lookup decodes forward from the closest
/// restart, so its cost is linear in this interval: measured on 1.5M cells
/// x 8 dims, a lookup took 18.8us at 1024, 5.7us at 256, 3.3us at 128 and
/// 2.1us at 64 (a hash-indexed hot tier answers in ~0.4us). The original
/// value of 1024 was tuned on bytes/cell alone (128 wasted ~1.2 bytes/cell
/// on the restart index) and never weighed lookup latency -- every cold
/// point read, and every write that has to check for an existing cold
/// cell, paid for it. 128 keeps lookups in the low microseconds for
/// roughly +1 byte/cell.
///
/// Not a format constant: a decoder never assumes this value, it derives
/// the interval a run was actually built with from its own restart list
/// (`ColdRun::interval`), so runs persisted with an older interval (1024)
/// still decode correctly.
const RESTART_INTERVAL: usize = 128;

/// A path's dimensions as `u64`s (`element_id.data().as_ffi()` per
/// dimension) -- the single canonical form used for delta encoding,
/// sorting, and comparison throughout this module (see the module doc's
/// ordering note for why this matters). `pub(crate)` so `CubePartition::
/// compact` (`lib.rs`) can sort a hot-tier batch into this same canonical
/// order before handing it to `merge` -- sorting by `ElementId`'s own
/// derived `Ord` there would risk exactly the silent-disagreement bug this
/// module's doc comment warns about.
pub(crate) fn path_ffi(path: &[ElementId]) -> Vec<u64> {
    path.iter().map(|e| e.data().as_ffi()).collect()
}

/// Same canonical ordering as `path_ffi`, but comparing two paths directly
/// via `Iterator::cmp` instead of materializing a `Vec<u64>` for either
/// side -- no allocation, and short-circuits on the first differing
/// dimension. Used wherever paths are only ever *compared*, not encoded
/// (sorting a batch before a merge, or the merge's own step-by-step
/// comparison): allocating a real `Vec<u64>` per comparison there was a
/// measured, real memory spike during compaction (a single dense cube's
/// compaction: sorting ~832K entries via
/// `sort_by_cached_key(path_ffi)` held all ~832K of those `Vec<u64>`s
/// alive simultaneously as the sort's temporary decoration array).
pub(crate) fn path_ffi_cmp(a: &[ElementId], b: &[ElementId]) -> std::cmp::Ordering {
    a.iter().map(|e| e.data().as_ffi()).cmp(b.iter().map(|e| e.data().as_ffi()))
}

fn ffi_to_path(ids: &[u64]) -> CellPath {
    ids.iter().map(|&v| ElementId::from(KeyData::from_ffi(v))).collect()
}

fn encode_dim_step(out: &mut Vec<u8>, prev: Option<u64>, cur: u64) {
    match prev {
        None => out.extend_from_slice(&cur.to_le_bytes()),
        Some(p) => {
            let delta = cur as i128 - p as i128;
            if delta == 0 {
                out.push(0); // Same
            } else if delta == 1 {
                out.push(1); // Succ
            } else if let Ok(d) = i8::try_from(delta) {
                out.push(2);
                out.push(d as u8);
            } else if let Ok(d) = i16::try_from(delta) {
                out.push(3);
                out.extend_from_slice(&d.to_le_bytes());
            } else if let Ok(d) = i32::try_from(delta) {
                out.push(4);
                out.extend_from_slice(&d.to_le_bytes());
            } else {
                out.push(5); // Abs8: delta too large, fall back to a raw absolute value
                out.extend_from_slice(&cur.to_le_bytes());
            }
        }
    }
}

/// Returns the decoded absolute value and how many bytes were consumed.
fn decode_dim_step(data: &[u8], offset: usize, prev: Option<u64>, is_restart: bool) -> (u64, usize) {
    if is_restart {
        return (u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap()), 8);
    }
    match data[offset] {
        0 => (prev.expect("Same tag only valid on a non-restart row"), 1),
        1 => (prev.expect("Succ tag only valid on a non-restart row") + 1, 1),
        2 => {
            let d = data[offset + 1] as i8;
            ((prev.unwrap() as i128 + d as i128) as u64, 2)
        }
        3 => {
            let d = i16::from_le_bytes(data[offset + 1..offset + 3].try_into().unwrap());
            ((prev.unwrap() as i128 + d as i128) as u64, 3)
        }
        4 => {
            let d = i32::from_le_bytes(data[offset + 1..offset + 5].try_into().unwrap());
            ((prev.unwrap() as i128 + d as i128) as u64, 5)
        }
        5 => (u64::from_le_bytes(data[offset + 1..offset + 9].try_into().unwrap()), 9),
        other => unreachable!("invalid dim-step tag {other}"),
    }
}

fn encode_value(out: &mut Vec<u8>, value: &CellValue) {
    match value {
        CellValue::Numeric(d) => {
            if *d >= 0.0 && *d <= 255.0 && d.fract() == 0.0 {
                out.push(0);
                out.push(*d as u8);
            } else if d.is_finite() && *d == (*d as i32) as f64 {
                out.push(1);
                out.extend_from_slice(&(*d as i32).to_le_bytes());
            } else if d.is_finite() && *d == (*d as f32) as f64 {
                out.push(2);
                out.extend_from_slice(&(*d as f32).to_le_bytes());
            } else {
                out.push(3);
                out.extend_from_slice(&d.to_le_bytes());
            }
        }
        CellValue::String(s) => {
            out.push(4);
            let bytes = s.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
    }
}

fn decode_value(data: &[u8], offset: usize) -> (CellValue, usize) {
    match data[offset] {
        0 => (CellValue::Numeric(data[offset + 1] as f64), 2),
        1 => {
            let v = i32::from_le_bytes(data[offset + 1..offset + 5].try_into().unwrap());
            (CellValue::Numeric(v as f64), 5)
        }
        2 => {
            let v = f32::from_le_bytes(data[offset + 1..offset + 5].try_into().unwrap());
            (CellValue::Numeric(v as f64), 5)
        }
        3 => {
            let v = f64::from_le_bytes(data[offset + 1..offset + 9].try_into().unwrap());
            (CellValue::Numeric(v), 9)
        }
        4 => {
            let len = u32::from_le_bytes(data[offset + 1..offset + 5].try_into().unwrap()) as usize;
            let s = String::from_utf8(data[offset + 5..offset + 5 + len].to_vec()).expect("we encoded this UTF-8 ourselves");
            (CellValue::String(s), 5 + len)
        }
        other => unreachable!("invalid value tag {other}"),
    }
}

/// Byte length of the encoded value at `offset`, without materializing it
/// (no `String` allocation for a string value) -- used to skip rows while
/// scanning towards a target.
fn value_len(data: &[u8], offset: usize) -> usize {
    match data[offset] {
        0 => 2,
        1 | 2 => 5,
        3 => 9,
        4 => 5 + u32::from_le_bytes(data[offset + 1..offset + 5].try_into().unwrap()) as usize,
        other => unreachable!("invalid value tag {other}"),
    }
}

/// Forward-only decode state over one `ColdRun`: the current row's ids live
/// in one reusable buffer, so scanning N rows costs no allocation per row
/// (the previous `decode_row` allocated a fresh `Vec<u64>` for every row it
/// touched). Every consumer -- point lookups, sorted batch lookups, full
/// scans, the parallel fold -- goes through this one decode loop.
struct Cursor<'a> {
    run: &'a ColdRun,
    interval: usize,
    /// If `decoded`: the row whose ids are in `ids`. Otherwise: the next row to decode.
    row: usize,
    /// Byte offset where row `row` starts.
    offset: usize,
    /// The most recently decoded row's ids -- also the delta base for the next row.
    ids: Vec<u64>,
    /// Byte offset of the decoded row's value (valid iff `decoded`).
    value_offset: usize,
    decoded: bool,
    /// Whether the cursor has been placed anywhere yet (`seek` starts with a restart lookup).
    positioned: bool,
    /// The previous `seek`'s target, if any. Lets a following, not-smaller target
    /// that falls before the (already decoded) current row be answered as
    /// absent immediately -- every row before the current one is smaller
    /// than the previous target, hence smaller than this one too.
    last_target: Option<Vec<u64>>,
}

impl<'a> Cursor<'a> {
    fn new(run: &'a ColdRun) -> Self {
        Self { run, interval: run.interval(), row: 0, offset: 0, ids: vec![0; run.n_dims], value_offset: 0, decoded: false, positioned: false, last_target: None }
    }

    fn position_at_restart(&mut self, ri: usize) {
        let r = &self.run.restarts[ri];
        self.row = r.row;
        self.offset = r.offset;
        self.decoded = false;
        self.positioned = true;
    }

    /// Decodes the ids of row `self.row` into `self.ids`. False at the end of the run.
    fn decode_ids(&mut self) -> bool {
        if self.row >= self.run.len {
            return false;
        }
        let is_restart = self.row.is_multiple_of(self.interval);
        let mut cursor = self.offset;
        for d in 0..self.run.n_dims {
            let prev = if is_restart { None } else { Some(self.ids[d]) };
            let (v, consumed) = decode_dim_step(&self.run.stream, cursor, prev, is_restart);
            self.ids[d] = v;
            cursor += consumed;
        }
        self.value_offset = cursor;
        self.decoded = true;
        true
    }

    fn value(&self) -> CellValue {
        decode_value(&self.run.stream, self.value_offset).0
    }

    /// Moves past the currently decoded row.
    fn advance(&mut self) {
        self.offset = self.value_offset + value_len(&self.run.stream, self.value_offset);
        self.row += 1;
        self.decoded = false;
    }

    /// Finds `target` (canonical `u64` form). Cheap for a sequence of
    /// ascending targets: while the next target lies in the block the
    /// cursor is already in, decoding just continues forward from where the
    /// previous seek stopped; a target at or beyond the next restart, or
    /// before the cursor (out-of-order use), re-positions via binary search
    /// over the restarts. So a sorted batch costs one pass over the rows it
    /// actually needs, not one restart-to-target scan per lookup.
    fn seek(&mut self, target: &[u64]) -> Option<CellValue> {
        let after_last = self.last_target.as_deref().is_some_and(|last| last <= target);
        match &mut self.last_target {
            Some(last) => {
                last.clear();
                last.extend_from_slice(target);
            }
            None => self.last_target = Some(target.to_vec()),
        }
        if self.decoded && self.ids.as_slice() > target && after_last {
            return None;
        }
        let mut reposition = !self.positioned || (self.decoded && self.ids.as_slice() > target);
        if !reposition && self.interval != usize::MAX {
            let next_block = self.row / self.interval + 1;
            if let Some(next) = self.run.restarts.get(next_block) {
                reposition = target >= next.ids.as_slice();
            }
        }
        if reposition {
            let ri = match self.run.restarts.binary_search_by(|r| r.ids.as_slice().cmp(target)) {
                Ok(i) => i,
                Err(0) => return None, // target is before this run's first path entirely
                Err(i) => i - 1,
            };
            self.position_at_restart(ri);
        }
        loop {
            if !self.decoded && !self.decode_ids() {
                return None;
            }
            match self.ids.as_slice().cmp(target) {
                std::cmp::Ordering::Equal => return Some(self.value()),
                std::cmp::Ordering::Greater => return None,
                std::cmp::Ordering::Less => self.advance(),
            }
        }
    }
}

/// The lexicographically smallest tuple in the cartesian product of the (sorted, non-empty)
/// `allowed` sets that is `>= x` (compared over the first `allowed.len()` ids), or `None` if
/// there is none. The "seek" primitive of `ColdRun::relevant_blocks`.
fn smallest_allowed_ge(allowed: &[Vec<u64>], x: &[u64]) -> Option<Vec<u64>> {
    let p = allowed.len();
    // Longest prefix of `x` whose every id is allowed.
    let mut m = 0;
    while m < p && allowed[m].binary_search(&x[m]).is_ok() {
        m += 1;
    }
    if m == p {
        return Some(x[..p].to_vec());
    }
    // `x[m]` is not allowed, so a tuple >= x must take a strictly larger allowed id at some
    // position i <= m (keeping x's prefix before i); the deepest such position gives the smallest.
    for i in (0..=m).rev() {
        let idx = allowed[i].partition_point(|&a| a <= x[i]);
        if idx < allowed[i].len() {
            let mut out = x[..i].to_vec();
            out.push(allowed[i][idx]);
            out.extend(allowed[i + 1..].iter().map(|a| a[0]));
            return Some(out);
        }
    }
    None
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Restart {
    row: usize,
    /// This restart row's path, in canonical `u64` form (see the module
    /// doc's ordering note) -- used both for the binary search in `get`
    /// and to seed delta decoding starting from this restart.
    ids: Vec<u64>,
    offset: usize,
}

/// One immutable, sorted-by-path run of cells, built once by
/// `CubePartition::compact` and never mutated afterward -- see the module
/// doc for the encoding. "Rebuild wholesale on compact" rather than
/// "mutate in place" keeps it simple (see the module doc).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ColdRun {
    n_dims: usize,
    len: usize,
    /// Serialized as one byte string, not element by element -- see `bulk_bytes`.
    #[serde(with = "bulk_bytes")]
    stream: Vec<u8>,
    restarts: Vec<Restart>,
}

/// (De)serializes a `Vec<u8>` as a single byte string instead of a sequence of `u8`s. Serde's
/// default for `Vec<u8>` is element by element, which for a cold stream of hundreds of MB meant
/// hundreds of millions of one-byte `read`s when loading a database -- and made load time depend
/// heavily on how well the compiler inlined that loop (~1.7 s with the default release profile,
/// ~5.6 s with `lto = "thin"` + `codegen-units = 1`, for the same 115 MB file). The bytes on
/// disk are **identical** either way: bincode writes both a `Vec<u8>` and a byte string as a
/// `u64` length followed by the raw bytes, so existing files load unchanged and files written now
/// stay readable by older builds (test: `bulk_bytes_layout_matches_a_plain_vec`).
mod bulk_bytes {
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        struct BytesVisitor;
        impl<'de> serde::de::Visitor<'de> for BytesVisitor {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(v)
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }
            // Formats without a native byte string (JSON, ...) present it as a sequence.
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(1 << 20));
                while let Some(b) = seq.next_element()? {
                    out.push(b);
                }
                Ok(out)
            }
        }
        deserializer.deserialize_byte_buf(BytesVisitor)
    }
}

impl ColdRun {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn stream_bytes(&self) -> usize {
        self.stream.len()
    }

    /// Heap bytes held: the stream and the restart index, by capacity.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.stream.capacity() + self.restarts.iter().map(|r| std::mem::size_of::<Restart>() + r.ids.capacity() * std::mem::size_of::<u64>()).sum::<usize>()
    }

    /// The restart interval this run was actually built with, derived from
    /// its own restart list (see `RESTART_INTERVAL`): consecutive restarts
    /// are exactly one interval apart. A run with a single restart holds
    /// at most one interval of rows, so no row past 0 is ever a restart.
    fn interval(&self) -> usize {
        match self.restarts.as_slice() {
            [first, second, ..] => second.row - first.row,
            _ => usize::MAX,
        }
    }

    /// Point lookup: binary-searches `restarts` for the closest restart at
    /// or before `path`'s canonical form, then decodes forward. Rows are
    /// globally sorted, so decoding stops as soon as a decoded row's ids
    /// exceed the target -- bounds the scan to at most one restart interval.
    pub(crate) fn get(&self, path: &[ElementId]) -> Option<CellValue> {
        Cursor::new(self).seek(&path_ffi(path))
    }

    /// Batched lookup: resolves `paths[i]` for every `i` in `which` into
    /// `out[i]`, visiting the targets in sorted order through one shared
    /// cursor (see `Cursor::seek`) instead of scanning from a restart per
    /// path. What consolidation reads and splash writes -- tens of
    /// thousands of leaf lookups against one cube -- go through.
    pub(crate) fn get_many_into(&self, paths: &[&[ElementId]], which: &[usize], out: &mut [Option<CellValue>]) {
        if which.is_empty() {
            return;
        }
        let n = self.n_dims;
        let mut flat: Vec<u64> = Vec::with_capacity(which.len() * n);
        for &i in which {
            flat.extend(paths[i].iter().map(|e| e.data().as_ffi()));
        }
        let mut order: Vec<usize> = (0..which.len()).collect();
        order.sort_unstable_by(|&a, &b| flat[a * n..(a + 1) * n].cmp(&flat[b * n..(b + 1) * n]));
        let mut cursor = Cursor::new(self);
        for j in order {
            out[which[j]] = cursor.seek(&flat[j * n..(j + 1) * n]);
        }
    }

    /// Sequential full decode, in sorted order -- what `CubePartition`'s
    /// merge-scan (`iter_cube`) and `compact` (merging an old run into a
    /// new one) both funnel through.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (CellPath, CellValue)> + '_ {
        let mut cursor = Cursor::new(self);
        std::iter::from_fn(move || {
            if !cursor.decode_ids() {
                return None;
            }
            let item = (ffi_to_path(&cursor.ids), cursor.value());
            cursor.advance();
            Some(item)
        })
    }

    /// Same output as `iter().collect()`, but decodes each restart-delimited
    /// block independently in parallel via rayon -- restart boundaries are
    /// exactly the points where decoding can resume without any prior
    /// state, so blocks have no sequential dependency on each other. Used by
    /// `Storage::iter_cube` once a cold run is large enough that a
    /// full-scan's single-threaded decode cost would otherwise dominate.
    pub(crate) fn iter_parallel(&self) -> Vec<(CellPath, CellValue)> {
        use rayon::prelude::*;
        (0..self.restarts.len())
            .into_par_iter()
            .flat_map(|ri| {
                let end_row = self.restarts.get(ri + 1).map(|r| r.row).unwrap_or(self.len);
                let mut cursor = Cursor::new(self);
                cursor.position_at_restart(ri);
                let mut out = Vec::with_capacity(end_row - cursor.row);
                while cursor.row < end_row && cursor.decode_ids() {
                    out.push((ffi_to_path(&cursor.ids), cursor.value()));
                    cursor.advance();
                }
                out
            })
            .collect()
    }

    /// The restart blocks that can contain a row whose leading `allowed.len()` ids are each in the
    /// corresponding sorted `allowed` set -- everything else can be skipped without decoding a
    /// single row. Rows are sorted lexicographically by path, so block `ri` holds exactly the rows
    /// between restart `ri`'s path and restart `ri + 1`'s: comparing prefixes against the restart
    /// list (already in memory, no extra index) is enough to jump from one candidate block to the
    /// next ("leapfrog"), costing time proportional to the number of relevant blocks rather than
    /// to the size of the run. Conservative: a returned block may still hold no match, the caller
    /// re-checks every row; a block that is not returned never does. Returns block index ranges
    /// (`start..end`, ascending, non-overlapping). An empty `allowed` slice means "no restriction".
    pub(crate) fn relevant_blocks(&self, allowed: &[Vec<u64>]) -> Vec<std::ops::Range<usize>> {
        let n_blocks = self.restarts.len();
        let p = allowed.len().min(self.n_dims);
        // An empty run (e.g. every cell cleared) has no blocks at all -- the leapfrog below would
        // otherwise still hand out block 0 and the caller would index into the empty restarts.
        if n_blocks == 0 || allowed[..p].iter().any(|a| a.is_empty()) {
            return Vec::new();
        }
        if p == 0 {
            #[allow(clippy::single_range_in_vec_init)] // one range covering every block, not the numbers 0..n_blocks
            return vec![0..n_blocks];
        }
        let allowed = &allowed[..p];
        let prefix = |ri: usize| &self.restarts[ri].ids[..p];
        let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
        // Block `b` holds the rows between `prefix(b)` and `prefix(b + 1)` inclusive (the last block
        // runs to the end), so it can hold a match iff some allowed tuple lies in that interval.
        // Start at the block that could hold the smallest allowed tuple overall.
        let first: Vec<u64> = allowed.iter().map(|a| a[0]).collect();
        let mut b = self.restarts.partition_point(|r| r.ids[..p] < first[..]).saturating_sub(1);
        loop {
            match ranges.last_mut() {
                Some(last) if last.end == b => last.end = b + 1,
                _ => ranges.push(b..b + 1),
            }
            if b + 1 >= n_blocks {
                break;
            }
            // Block `b + 1` starts at `hi`. Anything relevant after block `b` is >= `hi`.
            let hi = prefix(b + 1);
            let Some(t) = smallest_allowed_ge(allowed, hi) else { break };
            if t[..] == *hi {
                // The next block itself starts on an allowed tuple: relevant, take it and go on.
                b += 1;
            } else {
                // No allowed tuple in [hi, t): jump to the last block starting before `t`, the
                // first one whose interval can reach it (b + 1 <= that block, since hi < t).
                b = self.restarts.partition_point(|r| r.ids[..p] < t[..]).saturating_sub(1);
            }
        }
        ranges
    }

    /// Visits the rows in sorted order -- only those blocks `relevant_blocks(prefix)` selects, see there -- handing each to `f`, and
    /// stops as soon as `f` returns false (which this then returns). The sequential, early-exit counterpart of `par_fold`: for a
    /// caller that only wants the first N matches, a full parallel scan is wasted work, and the sorted order makes "the first N"
    /// well-defined and repeatable.
    pub(crate) fn scan_in_order<F: FnMut(&[ElementId], &CellValue) -> bool>(&self, prefix: &[Vec<u64>], f: &mut F) -> bool {
        let mut path = vec![ElementId::default(); self.n_dims];
        for range in self.relevant_blocks(prefix) {
            for ri in range {
                let end_row = self.restarts.get(ri + 1).map(|r| r.row).unwrap_or(self.len);
                let mut cursor = Cursor::new(self);
                cursor.position_at_restart(ri);
                while cursor.row < end_row && cursor.decode_ids() {
                    for (slot, &id) in path.iter_mut().zip(&cursor.ids) {
                        *slot = ElementId::from(KeyData::from_ffi(id));
                    }
                    if !f(&path, &cursor.value()) {
                        return false;
                    }
                    cursor.advance();
                }
            }
        }
        true
    }

    /// Streams every row through `fold` without materializing the cube:
    /// each restart-delimited block is decoded independently in parallel
    /// into one reusable path buffer (no per-row allocation), folded into a
    /// per-block accumulator, and the accumulators combined with `reduce`.
    /// The counterpart of `iter_parallel` for callers that aggregate -- a
    /// view/area read touches every stored cell but keeps only a handful of
    /// results, so building a `Vec` of every cell (the old `iter_cube` +
    /// fold shape) was pure overhead: one heap allocation per cell for a
    /// cube with more than 8 dimensions, and gigabytes of transient memory
    /// on a large cube.
    ///
    /// `prefix` restricts the scan to blocks that can hold a row whose leading ids are each in
    /// the given sorted sets (see `relevant_blocks`; empty = every block). It only *skips*
    /// blocks: `fold` still sees every row of a visited block and must apply its own filter.
    pub(crate) fn par_fold<A, I, F, R>(&self, prefix: &[Vec<u64>], identity: &I, fold: &F, reduce: &R) -> A
    where
        A: Send,
        I: Fn() -> A + Sync + Send,
        F: Fn(&mut A, &[ElementId], &CellValue) + Sync + Send,
        R: Fn(A, A) -> A + Sync + Send,
    {
        use rayon::prelude::*;
        let blocks: Vec<usize> = self.relevant_blocks(prefix).into_iter().flatten().collect();
        blocks
            .into_par_iter()
            .map(|ri| {
                let end_row = self.restarts.get(ri + 1).map(|r| r.row).unwrap_or(self.len);
                let mut acc = identity();
                let mut cursor = Cursor::new(self);
                cursor.position_at_restart(ri);
                let mut path = vec![ElementId::default(); self.n_dims];
                while cursor.row < end_row && cursor.decode_ids() {
                    for (slot, &id) in path.iter_mut().zip(&cursor.ids) {
                        *slot = ElementId::from(KeyData::from_ffi(id));
                    }
                    fold(&mut acc, &path, &cursor.value());
                    cursor.advance();
                }
                acc
            })
            .reduce(identity, reduce)
    }
}

/// Accumulates rows (in caller-guaranteed sorted order) into a new
/// `ColdRun`.
struct ColdRunBuilder {
    n_dims: usize,
    interval: usize,
    stream: Vec<u8>,
    restarts: Vec<Restart>,
    prev: Option<Vec<u64>>,
    row: usize,
}

impl ColdRunBuilder {
    fn new(n_dims: usize) -> Self {
        Self::with_interval(n_dims, RESTART_INTERVAL)
    }

    fn with_interval(n_dims: usize, interval: usize) -> Self {
        Self { n_dims, interval, stream: Vec::new(), restarts: Vec::new(), prev: None, row: 0 }
    }

    /// `path` must sort strictly after every previously-pushed path (by
    /// `path_ffi`) -- the caller (`CubePartition::compact`'s merge) is
    /// responsible for this invariant; debug-asserted here, not checked in
    /// release (this is an internal invariant, not untrusted input).
    fn push(&mut self, path: &[ElementId], value: &CellValue) {
        let ids = path_ffi(path);
        debug_assert!(self.prev.as_ref().is_none_or(|p| p.as_slice() < ids.as_slice()), "ColdRunBuilder::push requires strictly increasing sorted order");
        let is_restart = self.row.is_multiple_of(self.interval);
        if is_restart {
            self.restarts.push(Restart { row: self.row, ids: ids.clone(), offset: self.stream.len() });
        }
        let prev_ref = if is_restart { None } else { self.prev.as_deref() };
        for (d, &cur) in ids.iter().enumerate() {
            encode_dim_step(&mut self.stream, prev_ref.map(|p| p[d]), cur);
        }
        encode_value(&mut self.stream, value);
        self.prev = Some(ids);
        self.row += 1;
    }

    fn finish(self) -> ColdRun {
        ColdRun { n_dims: self.n_dims, len: self.row, stream: self.stream, restarts: self.restarts }
    }
}

/// Merges an existing cold run (if any) with a freshly-sorted batch of live
/// hot entries, dropping any old-run path that's tombstoned (a genuine
/// delete with no live replacement -- `tombstoned`) or superseded by a
/// same-path live entry (detected directly via the sorted merge comparison
/// below, by path equality -- no need for `tombstoned` to also carry every
/// live path just to make that comparison; see `HotTier::
/// live_entries_and_tombstoned`'s doc comment for why that matters).
/// Produces a new, single sorted `ColdRun`. `hot_sorted` must already be
/// sorted by `path_ffi`.
pub(crate) fn merge(old: Option<&ColdRun>, hot_sorted: &[(CellPath, CellValue)], tombstoned: &std::collections::HashSet<CellPath>, n_dims: usize) -> ColdRun {
    let mut builder = ColdRunBuilder::new(n_dims);
    let mut cold_iter = old.map(ColdRun::iter).into_iter().flatten().peekable();
    let mut hot_iter = hot_sorted.iter().peekable();

    loop {
        while let Some((p, _)) = cold_iter.peek() {
            if tombstoned.contains(p) {
                cold_iter.next();
            } else {
                break;
            }
        }
        match (cold_iter.peek(), hot_iter.peek()) {
            (Some((cp, _)), Some((hp, _))) => {
                match path_ffi_cmp(cp, hp) {
                    std::cmp::Ordering::Less => {
                        let (p, v) = cold_iter.next().unwrap();
                        builder.push(&p, &v);
                    }
                    std::cmp::Ordering::Equal => {
                        // hot's live entry supersedes cold's stale copy of
                        // the same path -- discard the cold one unused.
                        cold_iter.next();
                        let (p, v) = hot_iter.next().unwrap();
                        builder.push(p, v);
                    }
                    std::cmp::Ordering::Greater => {
                        let (p, v) = hot_iter.next().unwrap();
                        builder.push(p, v);
                    }
                }
            }
            (Some(_), None) => {
                let (p, v) = cold_iter.next().unwrap();
                builder.push(&p, &v);
            }
            (None, Some(_)) => {
                let (p, v) = hot_iter.next().unwrap();
                builder.push(p, v);
            }
            (None, None) => break,
        }
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use slotmap::SlotMap;

    /// `n` sorted distinct 4-dimensional paths with irregular gaps (so every
    /// delta-tag width gets exercised), and a value for each: mostly numeric
    /// of assorted magnitudes, some strings.
    fn sample(n: usize) -> (Vec<(CellPath, CellValue)>, SlotMap<ElementId, ()>) {
        let mut keys: SlotMap<ElementId, ()> = SlotMap::with_key();
        let ids: Vec<ElementId> = (0..3000).map(|_| keys.insert(())).collect();
        let mut rows: Vec<(CellPath, CellValue)> = Vec::new();
        let mut x = 0x2545F4914F6CDD1Du64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut seen = std::collections::HashSet::new();
        while rows.len() < n {
            let path: CellPath = (0..4).map(|d| ids[(next() as usize % if d == 3 { 3000 } else { 40 }) % 3000]).collect();
            if !seen.insert(path.clone()) {
                continue;
            }
            let v = match next() % 7 {
                0 => CellValue::String(format!("s{}", next() % 1000)),
                1 => CellValue::Numeric((next() % 200) as f64),
                2 => CellValue::Numeric(-((next() % 100_000) as f64)),
                3 => CellValue::Numeric((next() % 1000) as f64 + 0.5),
                _ => CellValue::Numeric((next() % 10_000_000) as f64 / 7.0),
            };
            rows.push((path, v));
        }
        rows.sort_by(|(a, _), (b, _)| path_ffi_cmp(a, b));
        (rows, keys)
    }

    fn build(rows: &[(CellPath, CellValue)], interval: usize) -> ColdRun {
        let mut b = ColdRunBuilder::with_interval(4, interval);
        for (p, v) in rows {
            b.push(p, v);
        }
        b.finish()
    }

    /// A run decodes correctly whatever restart interval it was built with --
    /// in particular the old 1024, which is what already-persisted data uses.
    #[test]
    fn decoding_does_not_depend_on_the_current_restart_interval() {
        let (rows, mut keys) = sample(2500);
        let absent = keys.insert(());
        for interval in [1usize, 3, 128, 1024, 5000] {
            let run = build(&rows, interval);
            if rows.len() > interval {
                assert_eq!(run.interval(), interval, "interval must be derivable from the restart list");
            }
            let all: Vec<_> = run.iter().collect();
            assert_eq!(all, rows, "iter() at interval {interval}");
            for (p, v) in &rows {
                assert_eq!(run.get(p).as_ref(), Some(v), "get at interval {interval}");
            }
            let mut miss = rows[100].0.clone();
            miss[3] = absent;
            assert_eq!(run.get(&miss), None);
        }
    }

    #[test]
    fn batched_lookup_matches_single_lookups_for_shuffled_present_absent_and_duplicate_targets() {
        let (rows, mut keys) = sample(3000);
        let absent = keys.insert(());
        let run = build(&rows, RESTART_INTERVAL);
        let mut targets: Vec<CellPath> = Vec::new();
        for (i, (p, _)) in rows.iter().enumerate() {
            targets.push(p.clone());
            if i % 5 == 0 {
                let mut miss = p.clone();
                miss[2] = absent; // sorts after everything else in dim 2 -- a miss in the middle of the stream
                targets.push(miss);
            }
            if i % 11 == 0 {
                targets.push(p.clone()); // duplicate
            }
        }
        // deterministic shuffle
        let mut x = 88172645463325252u64;
        for i in (1..targets.len()).rev() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            targets.swap(i, x as usize % (i + 1));
        }
        let slices: Vec<&[ElementId]> = targets.iter().map(|t| t.as_slice()).collect();
        let which: Vec<usize> = (0..slices.len()).collect();
        let mut out = vec![None; slices.len()];
        run.get_many_into(&slices, &which, &mut out);
        for (t, got) in targets.iter().zip(&out) {
            assert_eq!(got, &run.get(t), "batched result differs from single lookup for {t:?}");
        }
        assert!(out.iter().any(|o| o.is_some()) && out.iter().any(|o| o.is_none()));
    }

    #[test]
    fn par_fold_visits_every_row_exactly_once() {
        let (rows, _keys) = sample(2600);
        for interval in [7usize, RESTART_INTERVAL, 1024] {
            let run = build(&rows, interval);
            let (count, sum) = run.par_fold(
                &[],
                &|| (0usize, 0.0f64),
                &|acc: &mut (usize, f64), path: &[ElementId], value: &CellValue| {
                    assert_eq!(path.len(), 4);
                    acc.0 += 1;
                    if let CellValue::Numeric(v) = value {
                        acc.1 += v;
                    }
                },
                &|a, b| (a.0 + b.0, a.1 + b.1),
            );
            let expected: f64 = rows.iter().map(|(_, v)| if let CellValue::Numeric(v) = v { *v } else { 0.0 }).sum();
            assert_eq!(count, rows.len(), "interval {interval}");
            assert!((sum - expected).abs() < 1e-6 * expected.abs().max(1.0), "interval {interval}: {sum} vs {expected}");
        }
    }

    /// Pruning must never lose a matching row and must actually skip work: for prefix lengths 1-3
    /// and allowed sets of assorted sizes, a fold that applies its own filter over the visited
    /// blocks finds exactly the rows a brute-force filter finds, while a selective prefix
    /// decodes only a small share of the run.
    #[test]
    fn prefix_pruning_finds_exactly_the_matching_rows_and_skips_the_rest() {
        let (rows, keys) = sample(20_000);
        let ids: Vec<ElementId> = keys.keys().take(40).collect();
        let run = build(&rows, RESTART_INTERVAL);
        let mut x = 0x1234_5678_9ABC_DEF1u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut checked_selective = false;
        for p in 1..=3usize {
            for size in [1usize, 2, 5, 13, 30, 40] {
                for _ in 0..4 {
                    let allowed: Vec<Vec<ElementId>> = (0..p)
                        .map(|_| {
                            let mut pick: Vec<ElementId> = Vec::new();
                            while pick.len() < size {
                                let e = ids[next() as usize % ids.len()];
                                if !pick.contains(&e) {
                                    pick.push(e);
                                }
                            }
                            pick
                        })
                        .collect();
                    let prefix: Vec<Vec<u64>> = allowed
                        .iter()
                        .map(|a| {
                            let mut v: Vec<u64> = a.iter().map(|e| slotmap::Key::data(e).as_ffi()).collect();
                            v.sort_unstable();
                            v
                        })
                        .collect();
                    let expected: usize = rows.iter().filter(|(path, _)| (0..p).all(|d| allowed[d].contains(&path[d]))).count();
                    let (found, visited) = run.par_fold(
                        &prefix,
                        &|| (0usize, 0usize),
                        &|acc: &mut (usize, usize), path: &[ElementId], _v: &CellValue| {
                            acc.1 += 1;
                            if (0..p).all(|d| allowed[d].contains(&path[d])) {
                                acc.0 += 1;
                            }
                        },
                        &|a, b| (a.0 + b.0, a.1 + b.1),
                    );
                    assert_eq!(found, expected, "prefix length {p}, {size} allowed per dimension");
                    assert!(visited >= expected);
                    if p >= 2 && size == 1 {
                        assert!(visited * 5 < rows.len(), "a one-element prefix on {p} dimensions still decoded {visited} of {} rows", rows.len());
                        checked_selective = true;
                    }
                }
            }
        }
        assert!(checked_selective);
        // an allowed set that matches nothing at all prunes everything without panicking
        let mut keys2 = keys;
        let stranger = keys2.insert(());
        let none = run.par_fold(&[vec![slotmap::Key::data(&stranger).as_ffi()]], &|| 0usize, &|acc: &mut usize, _p: &[ElementId], _v: &CellValue| *acc += 1, &|a, b| a + b);
        assert!(none <= run.len());
        assert_eq!(run.relevant_blocks(&[Vec::new()]).len(), 0);
    }

    /// The on-disk layout of a cold run is unchanged by `bulk_bytes`: a run serializes to exactly
    /// the bytes of the equivalent struct with a plain `Vec<u8>` stream (so files written by older
    /// builds still load and vice versa), and round-trips.
    #[test]
    fn bulk_bytes_layout_matches_a_plain_vec() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct PlainRun {
            n_dims: usize,
            len: usize,
            stream: Vec<u8>,
            restarts: Vec<Restart>,
        }
        let (rows, _keys) = sample(700);
        let run = build(&rows, RESTART_INTERVAL);
        let new_bytes = bincode::serialize(&run).unwrap();
        let plain = PlainRun { n_dims: run.n_dims, len: run.len, stream: run.stream.clone(), restarts: run.restarts.clone() };
        let old_bytes = bincode::serialize(&plain).unwrap();
        assert_eq!(new_bytes, old_bytes, "byte layout must not change");
        // old bytes -> new type, new bytes -> old type, and a plain round trip
        let from_old: ColdRun = bincode::deserialize(&old_bytes).unwrap();
        assert_eq!(from_old.iter().collect::<Vec<_>>(), rows);
        let back: PlainRun = bincode::deserialize(&new_bytes).unwrap();
        assert_eq!(back.stream, run.stream);
        let round: ColdRun = bincode::deserialize(&new_bytes).unwrap();
        assert_eq!(round.iter().collect::<Vec<_>>(), rows);
    }

    #[test]
    fn empty_run_and_single_row_run_behave() {
        let empty = build(&[], RESTART_INTERVAL);
        assert_eq!(empty.iter().count(), 0);
        // a prefix-restricted scan of an empty run (a cube area clear after every cell was
        // compacted away) must find nothing, not index into the empty restart list
        let (_, mut keys) = sample(1);
        let prefix = [vec![slotmap::Key::data(&keys.insert(())).as_ffi()]];
        assert!(empty.relevant_blocks(&prefix).is_empty());
        assert!(empty.relevant_blocks(&[]).is_empty());
        assert!(empty.scan_in_order(&prefix, &mut |_p: &[ElementId], _v: &CellValue| panic!("empty run yielded a row")));
        let n = empty.par_fold(&prefix, &|| 0usize, &|acc: &mut usize, _p: &[ElementId], _v: &CellValue| *acc += 1, &|a, b| a + b);
        assert_eq!(n, 0);
        let (rows, _k) = sample(1);
        let one = build(&rows, RESTART_INTERVAL);
        assert_eq!(one.get(&rows[0].0).as_ref(), Some(&rows[0].1));
        assert_eq!(one.interval(), usize::MAX);
    }
}
