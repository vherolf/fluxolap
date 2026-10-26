//! Empirical bytes/cell measurement for `fluxolap-storage`'s `Storage`:
//! checks memory work against a real number instead of napkin math.
//! Linux-only (reads `/proc/self/status`).
//!
//! Usage: `cargo run -p xtask -- membench [cells dims]` -- with no
//! arguments, runs the default sweep (1M/10M cells x 2/4/8 dims); with
//! `cells`/`dims` given, runs a single one-off measurement.

use fluxolap_domain::{CubeId, ElementId};
use fluxolap_storage::{CellValue, Storage};

/// A typical bulk-load batch size -- the point of this harness is to
/// measure the shape a bulk load or a large splash write produces, not a
/// per-cell `Storage::set` loop no real bulk caller uses.
const BATCH_SIZE: usize = 50_000;

/// `VmRSS` from this process's own `/proc/self/status`, in bytes.
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

fn human_mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// Fills one synthetic cube with `cell_count` cells across `dim_count`
/// dimensions, loaded in `BATCH_SIZE` chunks via `record_and_set_many` --
/// the same shape a real bulk load (a migration, a large splash
/// write) produces, and the only path the `reserve`/`shrink_cube`
/// work actually affects (a per-cell `Storage::set` loop never goes
/// through it). Every cell gets a distinct element on every dimension
/// (1:1 pools sized to `cell_count`) -- the worst case for path memory, no
/// coordinate reuse across cells to exploit.
///
/// Reports bytes/cell both right after the load (capturing any leftover
/// growth-slack from `record_and_set_many`'s reservations, which can
/// still over- or under-shoot across a multi-batch load) and again after
/// `Storage::shrink_cube`, so the two mechanisms can be told apart.
fn run_one(cell_count: usize, dim_count: usize) {
    // Built and dropped-in-full BEFORE the "before" RSS read, so the
    // measured delta reflects only Storage's own growth, not this
    // harness's own scratch element pools.
    let mut element_ids: slotmap::SlotMap<ElementId, ()> = slotmap::SlotMap::default();
    let pools: Vec<Vec<ElementId>> = (0..dim_count).map(|_| (0..cell_count).map(|_| element_ids.insert(())).collect()).collect();
    let mut cube_ids: slotmap::SlotMap<CubeId, ()> = slotmap::SlotMap::default();
    let cube = cube_ids.insert(());

    let rss_before = vm_rss_bytes();
    let storage = Storage::new();
    // Mirrors a migration's use of the source cube's filled-cell count:
    // the true total is known before a
    // single batch is written, so reserve it exactly once up front rather
    // than relying on record_and_set_many's own per-batch reservation
    // (which still amortized-doubles `rows`, confirmed via this same
    // harness to overshoot by up to 60% before this fix).
    storage.reserve_cube(cube, cell_count);
    let mut start = 0;
    while start < cell_count {
        let end = (start + BATCH_SIZE).min(cell_count);
        let batch: Vec<(Vec<ElementId>, CellValue)> = (start..end)
            .map(|i| {
                let path: Vec<ElementId> = pools.iter().map(|pool| pool[i]).collect();
                // i+1, never 0.0 -- a numeric 0.0 clears a cell (see
                // fluxolap-storage's own test suite), which would
                // silently undercount cell 0.
                (path, CellValue::Numeric((i + 1) as f64))
            })
            .collect();
        storage.record_and_set_many(cube, batch);
        start = end;
    }
    let rss_after_load = vm_rss_bytes();

    let filled = storage.filled_count(cube);
    assert_eq!(filled, cell_count, "filled_count disagrees with the number of cells just inserted");

    // `shrink_cube` is a real compaction (folds hot into
    // cold), not just capacity trimming -- "before" is entirely hot-tier
    // (nothing has compacted yet, since `cell_count` is expected to stay
    // under `HOT_COMPACT_TRIGGER` for this synthetic load), "after" should
    // show hot emptied out and cold now holding the compressed data.
    let cap_before = storage.capacity(cube).expect("cube partition exists after inserts");
    storage.shrink_cube(cube);
    let cap_after = storage.capacity(cube).expect("cube partition exists after shrink");
    println!(
        "  capacity before compact: hot numeric {}/{} ({:+.1}%)  hot index {}/{} ({:+.1}%)  cold {} cells",
        cap_before.hot_numeric_len,
        cap_before.hot_numeric_capacity,
        (cap_before.hot_numeric_capacity as f64 / cap_before.hot_numeric_len.max(1) as f64 - 1.0) * 100.0,
        cap_before.hot_index_len,
        cap_before.hot_index_capacity,
        (cap_before.hot_index_capacity as f64 / cap_before.hot_index_len.max(1) as f64 - 1.0) * 100.0,
        cap_before.cold_len,
    );
    println!(
        "  capacity after compact:  hot numeric {}/{}  hot index {}/{}  cold {} cells, {} bytes ({:.1} B/cell)",
        cap_after.hot_numeric_len,
        cap_after.hot_numeric_capacity,
        cap_after.hot_index_len,
        cap_after.hot_index_capacity,
        cap_after.cold_len,
        cap_after.cold_stream_bytes,
        cap_after.cold_stream_bytes as f64 / cap_after.cold_len.max(1) as f64,
    );
    let rss_after_shrink = vm_rss_bytes();

    let load_delta = rss_after_load.saturating_sub(rss_before);
    let shrink_delta = rss_after_shrink.saturating_sub(rss_before);
    println!(
        "cells={cell_count:>10}  dims={dim_count}  after_load={:>10} ({:>6.1} B/cell)  after_compact={:>10} ({:>6.1} B/cell)",
        human_mib(load_delta),
        load_delta as f64 / cell_count as f64,
        human_mib(shrink_delta),
        shrink_delta as f64 / cell_count as f64,
    );

    // Keep `storage` alive until after the last RSS read -- an earlier
    // drop would free the memory before it's measured.
    drop(storage);
}

pub fn run(args: &[String]) {
    if let [cells, dims] = args {
        let cell_count: usize = cells.parse().expect("cell count must be a positive integer");
        let dim_count: usize = dims.parse().expect("dim count must be a positive integer");
        run_one(cell_count, dim_count);
        return;
    }
    if !args.is_empty() {
        eprintln!("usage: cargo run -p xtask -- membench [cells dims]");
        std::process::exit(2);
    }

    for &dim_count in &[2usize, 4, 8] {
        for &cell_count in &[1_000_000usize, 10_000_000] {
            run_one(cell_count, dim_count);
        }
    }
}
