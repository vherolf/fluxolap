//! Bulk cell-write benchmark: separates the per-cell cost of classifying a
//! write (`ops::expand_cube_path` + `ops::is_plain_leaf_path`, which
//! decide between the cheap bulk path and real splash handling) from
//! `Storage::record_and_set_many`'s own cost, for a wide cube (~19
//! dimensions) loaded with all-leaf fact writes. Written to check why
//! end-to-end cell-write throughput fell short of the storage's own
//! hot-tier insert cost (~0.27 us/cell): every cell, even a plain leaf
//! write, pays `expand_cube_path` first, and `Dimension::expand_to_base`
//! heap-allocates a `Vec` per dimension even for a leaf.
//!
//! Usage: `cargo run -p xtask -- writebench [cells dims]` -- defaults to
//! 1,000,000 cells / 19 dims if not given.

use fluxolap_domain::cube::{CubeTable, CUBE_TYPE_NORMAL};
use fluxolap_domain::dimension::{DimensionTable, DIM_TYPE_NORMAL};
use fluxolap_domain::{CubeId, DatabaseId, ElementId};
use fluxolap_engine::ops;
use fluxolap_storage::{CellValue, Storage};
use std::hint::black_box;
use std::time::Instant;

const BATCH_SIZE: usize = 50_000;

fn run_one(cell_count: usize, dim_count: usize) {
    let mut dim_table = DimensionTable::new();
    let mut cube_table = CubeTable::new();

    let database_id: DatabaseId = {
        let mut sm: slotmap::SlotMap<DatabaseId, ()> = slotmap::SlotMap::default();
        sm.insert(())
    };

    // Each dimension gets a small, realistic-cardinality pool of leaf
    // elements (cycled across cells, not 1:1 with cell_count) -- a real
    // fact-table dimension has hundreds to low-thousands of members, not
    // millions; cell_count distinct elements per dimension would make
    // *this setup loop* O(cell_count^2) via Dimension::create_element's own
    // linear find_element_by_name duplicate check, which would dwarf (and
    // completely confound) the thing actually being measured below. Every
    // created element is a leaf by construction (nothing ever gets a
    // child) regardless of pool size, which is all `expand_cube_path`
    // cares about here.
    const POOL_SIZE: usize = 2_000;
    let mut pools: Vec<Vec<ElementId>> = Vec::with_capacity(dim_count);
    let mut dim_ids = Vec::with_capacity(dim_count);
    for d in 0..dim_count {
        let dim_id = dim_table.create(database_id, &format!("dim{d}"), DIM_TYPE_NORMAL).expect("create dimension");
        let dim = dim_table.get_mut(dim_id).expect("just created");
        let mut ids = Vec::with_capacity(POOL_SIZE);
        for i in 0..POOL_SIZE {
            ids.push(dim.create_element(&format!("e{i}"), 1).expect("create element"));
        }
        dim_ids.push(dim_id);
        pools.push(ids);
    }

    let cube_id: CubeId = cube_table.create(database_id, "bench_cube", dim_ids, CUBE_TYPE_NORMAL).expect("create cube");
    let cube = cube_table.get(cube_id).expect("just created");

    // A per-dimension multiplicative hash of the cell index, not a plain
    // `i % pool.len()` cycle -- with every dimension's pool the same size
    // and cycling in lockstep, a plain modulo would degenerate into only
    // POOL_SIZE distinct paths total (repeated cell_count/POOL_SIZE times
    // each), turning phase 2's writes mostly into overwrites of the same
    // handful of paths rather than the real bulk load's overwhelmingly
    // fresh-insert pattern.
    let paths: Vec<Vec<ElementId>> = (0..cell_count)
        .map(|i| {
            pools
                .iter()
                .enumerate()
                .map(|(d, pool)| {
                    // Multiplicative (Fibonacci) hashing's low bits are poor
                    // quality / short-periodic -- taking `% pool.len()`
                    // directly off the raw product (an earlier version of
                    // this) degenerated into only a few thousand distinct
                    // paths total instead of the intended ~cell_count.
                    // Using the well-mixed *upper* 32 bits fixes it.
                    let h = (i as u64).wrapping_mul(0x9E3779B97F4A7C15).wrapping_add((d as u64).wrapping_mul(0xBF58476D1CE4E5B9));
                    let mixed = (h >> 32) as usize;
                    pool[mixed % pool.len()]
                })
                .collect()
        })
        .collect();

    // Phase 1: the real classification step every cell pays in
    // fluxolap_cube_cells_write_batch, in isolation -- no storage write at
    // all, just expand_cube_path + is_plain_leaf_path, exactly as the FFI
    // shim's loop calls them.
    let start_classify = Instant::now();
    let mut leaf_count = 0usize;
    for path in &paths {
        let expansion = ops::expand_cube_path(&dim_table, cube, path);
        if black_box(ops::is_plain_leaf_path(&expansion)) {
            leaf_count += 1;
        }
    }
    let classify_elapsed = start_classify.elapsed();
    assert_eq!(leaf_count, cell_count, "every generated path must be a plain leaf for this benchmark to be meaningful");

    // Phase 2: Storage::record_and_set_many alone, the fast bulk path
    // classification feeds into -- same batching shape membench uses.
    let storage = Storage::new();
    storage.reserve_cube(cube_id, cell_count);
    let start_write = Instant::now();
    let mut start = 0;
    while start < cell_count {
        let end = (start + BATCH_SIZE).min(cell_count);
        let batch: Vec<(Vec<ElementId>, CellValue)> = paths[start..end].iter().map(|p| (p.clone(), CellValue::Numeric(1.0))).collect();
        storage.record_and_set_many(cube_id, batch);
        start = end;
    }
    let write_elapsed = start_write.elapsed();
    // Not asserted equal to cell_count: for a small dim_count the
    // (POOL_SIZE^dim_count)-sized combination space is small enough that
    // genuine birthday-paradox collisions among cell_count draws are
    // expected, not a bug -- irrelevant to what's being measured here
    // (expand_cube_path's cost is identical per call regardless), so just
    // reported for visibility.
    let filled = storage.filled_count(cube_id);

    let classify_per_cell_ns = classify_elapsed.as_nanos() as f64 / cell_count as f64;
    let write_per_cell_ns = write_elapsed.as_nanos() as f64 / cell_count as f64;
    println!(
        "cells={cell_count:>9} dims={dim_count:>2} (filled={filled:>9})  classify(expand_cube_path)={:>8.2}s ({:>7.1} ns/cell)  record_and_set_many={:>8.2}s ({:>7.1} ns/cell)  ratio={:.1}x",
        classify_elapsed.as_secs_f64(),
        classify_per_cell_ns,
        write_elapsed.as_secs_f64(),
        write_per_cell_ns,
        classify_per_cell_ns / write_per_cell_ns,
    );
}

/// Measures `Dimension::find_element_by_wire_id`'s real cost directly --
/// `resolve_path` (the FFI shim's wire-id -> ElementId resolution, called
/// once per dimension per cell, *before* `expand_cube_path`) calls it, and
/// `Dimension` has no secondary index by wire_id at all: `elements` is a
/// plain `SlotMap`, so every lookup is `self.elements.iter().find(...)` --
/// O(dimension size), not O(1). Real dimensions can reach ~200,000
/// elements, so this checks whether that scan cost -- paid
/// once per dimension per cell, on every write -- is large enough to
/// explain the gap `writebench`'s classify/write numbers alone don't cover.
fn run_lookup_scan(n_elements: usize, n_lookups: usize) {
    let mut dim_table = DimensionTable::new();
    let database_id: DatabaseId = {
        let mut sm: slotmap::SlotMap<DatabaseId, ()> = slotmap::SlotMap::default();
        sm.insert(())
    };
    let dim_id = dim_table.create(database_id, "bigdim", DIM_TYPE_NORMAL).expect("create dimension");
    let dim = dim_table.get_mut(dim_id).expect("just created");
    for i in 0..n_elements {
        dim.create_element(&format!("e{i}"), 1).expect("create element");
    }
    // Real wire ids are sequential from 0 (see create_element), so probe
    // wire ids spread across the whole dimension, not clustered near the
    // front (which would understate the average scan cost).
    let probe_ids: Vec<u32> = (0..n_lookups).map(|i| ((i as u64).wrapping_mul(0x9E3779B97F4A7C15) >> 32) as u32 % n_elements as u32).collect();

    let start = Instant::now();
    let mut found = 0usize;
    for &wire_id in &probe_ids {
        if black_box(dim.find_element_by_wire_id(wire_id)).is_some() {
            found += 1;
        }
    }
    let elapsed = start.elapsed();
    assert_eq!(found, n_lookups, "every probed wire_id must exist in the dimension");
    println!(
        "dim_elements={n_elements:>9}  lookups={n_lookups:>8}  find_element_by_wire_id total={:>7.3}s  ({:>8.1} ns/lookup)",
        elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / n_lookups as f64,
    );
}

/// `VmRSS` from this process's own `/proc/self/status`, in bytes -- same
/// technique as `membench` (mimalloc, this binary's global allocator in
/// `main.rs`, keeps the number honest -- see the comment there).
fn vm_rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status (Linux-only tool)");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().expect("parse VmRSS value");
            return kb * 1024;
        }
    }
    panic!("VmRSS not found in /proc/self/status");
}

/// Empirical memory cost of the new `Dimension::by_wire_id` index alone --
/// a bare `HashMap<u32, ElementId>` populated the same way
/// `rebuild_wire_id_index`/`create_element` do, isolated from every other
/// `Dimension` field so this number is just the index's own cost, not
/// conflated with `elements`/`relations`/etc. (which were already paid
/// before this change and aren't new).
fn run_index_memory(n_elements: usize) {
    let mut sm: slotmap::SlotMap<ElementId, ()> = slotmap::SlotMap::default();
    let ids: Vec<ElementId> = (0..n_elements).map(|_| sm.insert(())).collect();

    let rss_before = vm_rss_bytes();
    let mut index: std::collections::HashMap<u32, ElementId> = std::collections::HashMap::with_capacity(n_elements);
    for (wire_id, &id) in ids.iter().enumerate() {
        index.insert(wire_id as u32, id);
    }
    let rss_after = vm_rss_bytes();
    let delta = rss_after.saturating_sub(rss_before);
    println!(
        "by_wire_id index: {n_elements:>9} entries  +{:>7.2} MiB  ({:>5.1} bytes/entry)",
        delta as f64 / (1024.0 * 1024.0),
        delta as f64 / n_elements as f64,
    );
    drop(index);
}

pub fn run(args: &[String]) {
    if let [cells, dims] = args {
        let cell_count: usize = cells.parse().expect("cell count must be a positive integer");
        let dim_count: usize = dims.parse().expect("dim count must be a positive integer");
        run_one(cell_count, dim_count);
        return;
    }
    if !args.is_empty() {
        eprintln!("usage: cargo run -p xtask -- writebench [cells dims]");
        std::process::exit(2);
    }
    // 19 dims, a wide real-world cube; also show 2/8 dims for contrast.
    for &dim_count in &[2usize, 8, 19] {
        run_one(1_000_000, dim_count);
    }
    println!();
    // Kept modest: create_element is *itself* O(n) (same linear
    // find_element_by_name duplicate check), so building a dimension this
    // way is O(n^2) setup cost -- a real ~200,000-element dimension
    // isn't reachable this way
    // in reasonable time. The per-lookup trend below is close enough to
    // linear that extrapolating to that size from these numbers is valid.
    for &n in &[1_000usize, 5_000, 20_000, 50_000] {
        run_lookup_scan(n, 200_000);
    }
    println!();
    // 198,597 elements: a large real-world total (across all dimensions of
    // one database) -- an upper-bound stand-in for "one real dimension
    // this large," to see the new index's per-entry
    // memory cost at the scale that actually motivated this fix.
    for &n in &[1_000usize, 20_000, 198_597] {
        run_index_memory(n);
    }
}
