//! Protocol-agnostic business logic: id resolution, cell read/write,
//! hierarchy expansion, area clears and lock overlap. Everything here
//! takes/returns plain domain types (`DatabaseId`, `Vec<ElementId>`,
//! `Option<CellValue>`, ...) with zero wire-format formatting -- that
//! belongs to whichever API layer sits on top of this crate.

use fluxolap_domain::cube::{Cube, CubeTable};
use fluxolap_domain::dimension::{Dimension, DimensionTable};
use fluxolap_domain::{CubeId, DatabaseId, DimensionId, DomainError, ElementId};
use fluxolap_domain::database::DatabaseTable;
use fluxolap_storage::{CellValue, Storage};
use fluxolap_error::FluxolapError;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Error mapping (DomainError -> FluxolapError, per entity type)
// ---------------------------------------------------------------------------

pub fn map_database_error(e: DomainError) -> FluxolapError {
    match e {
        DomainError::NotFound => FluxolapError::database_not_found(),
        DomainError::EmptyName => FluxolapError::invalid_database_name("database name is empty"),
        DomainError::DuplicateName(name) => FluxolapError::database_name_in_use(&name),
        DomainError::UnsupportedType(t) => FluxolapError::invalid_database_type(t),
        DomainError::Cycle => unreachable!("DatabaseTable has no hierarchy, cannot cycle"),
        DomainError::NoDimensions => unreachable!("databases don't have dimensions, cubes do"),
        DomainError::InvalidCoordinates => unreachable!("databases don't have cell coordinates, cubes do"),
        DomainError::DuplicateWireId(_) => unreachable!("only Dimension::create_element_with_wire_id (elements) can fail this way"),
    }
}

pub fn map_dimension_error(e: DomainError) -> FluxolapError {
    match e {
        DomainError::NotFound => FluxolapError::dimension_not_found(),
        DomainError::EmptyName => FluxolapError::invalid_dimension_name("dimension name is empty"),
        DomainError::DuplicateName(name) => FluxolapError::dimension_name_in_use(&name),
        DomainError::UnsupportedType(t) => FluxolapError::invalid_dimension_type(t),
        DomainError::Cycle => unreachable!("DimensionTable has no hierarchy, cannot cycle"),
        DomainError::NoDimensions => unreachable!("a dimension doesn't itself have dimensions, cubes do"),
        DomainError::InvalidCoordinates => unreachable!("dimensions don't have cell coordinates, cubes do"),
        DomainError::DuplicateWireId(_) => unreachable!("only Dimension::create_element_with_wire_id (elements) can fail this way"),
    }
}

pub fn map_element_error(e: DomainError) -> FluxolapError {
    match e {
        DomainError::NotFound => FluxolapError::element_not_found(),
        DomainError::EmptyName => FluxolapError::invalid_element_name("element name is empty"),
        DomainError::DuplicateName(name) => FluxolapError::element_name_in_use(&name),
        DomainError::UnsupportedType(t) => FluxolapError::invalid_element_type(t),
        DomainError::Cycle => FluxolapError::element_circular_reference(),
        DomainError::NoDimensions => unreachable!("elements don't have dimensions, cubes do"),
        DomainError::InvalidCoordinates => unreachable!("elements don't have cell coordinates, cubes do"),
        // Only reachable through the embedded FFI (explicit-id element creation); there's no dedicated error code for it, so it surfaces
        // as an invalid-element error carrying the explanation.
        DomainError::DuplicateWireId(id) => FluxolapError::invalid_element_name(format!("element id {id} is already in use")),
    }
}

pub fn map_cube_error(e: DomainError) -> FluxolapError {
    match e {
        DomainError::NotFound => FluxolapError::cube_not_found(),
        DomainError::EmptyName => FluxolapError::invalid_cube_name("cube name is empty"),
        DomainError::DuplicateName(name) => FluxolapError::cube_name_in_use(&name),
        DomainError::UnsupportedType(t) => FluxolapError::invalid_cube_type(t),
        DomainError::NoDimensions => FluxolapError::cube_empty("cube must have at least one dimension"),
        DomainError::Cycle => unreachable!("CubeTable has no hierarchy, cannot cycle"),
        DomainError::InvalidCoordinates => unreachable!("CubeTable::create/destroy never validates cell coordinates"),
        DomainError::DuplicateWireId(_) => unreachable!("only Dimension::create_element_with_wire_id (elements) can fail this way"),
    }
}

// ---------------------------------------------------------------------------
// Id resolution (id-wins-else-name, per entity type)
// ---------------------------------------------------------------------------

/// Id wins if present, else fall back to name, else -- deliberately not
/// a "parameter missing" error -- `database not found`.
pub fn resolve_database_id(databases: &DatabaseTable, database: Option<u32>, name_database: Option<&str>) -> Result<DatabaseId, FluxolapError> {
    if let Some(wire_id) = database {
        return databases.find_by_wire_id(wire_id).ok_or_else(FluxolapError::database_not_found);
    }
    if let Some(name) = name_database {
        return databases.find_by_name(name).ok_or_else(FluxolapError::database_not_found);
    }
    Err(FluxolapError::database_not_found())
}

/// Same id-wins-else-name pattern as `resolve_database_id`, scoped to a database.
pub fn resolve_dimension_id(
    dimensions: &DimensionTable,
    database_id: DatabaseId,
    dimension: Option<u32>,
    name_dimension: Option<&str>,
) -> Result<DimensionId, FluxolapError> {
    if let Some(wire_id) = dimension {
        return dimensions.find_by_wire_id(database_id, wire_id).ok_or_else(FluxolapError::dimension_not_found);
    }
    if let Some(name) = name_dimension {
        return dimensions.find_by_name(database_id, name).ok_or_else(FluxolapError::dimension_not_found);
    }
    Err(FluxolapError::dimension_not_found())
}

/// An element by id, or else by name (`element` wins when both are given).
pub fn resolve_element_id(dim: &Dimension, element: Option<u32>, name_element: Option<&str>) -> Result<ElementId, FluxolapError> {
    if let Some(wire_id) = element {
        return dim.find_element_by_wire_id(wire_id).ok_or_else(FluxolapError::element_not_found);
    }
    if let Some(name) = name_element {
        return dim.find_element_by_name(name).ok_or_else(FluxolapError::element_not_found);
    }
    Err(FluxolapError::element_not_found())
}

/// Same id-wins-else-name pattern as `resolve_database_id`.
pub fn resolve_cube_id(cubes: &CubeTable, database_id: DatabaseId, cube: Option<u32>, name_cube: Option<&str>) -> Result<CubeId, FluxolapError> {
    if let Some(wire_id) = cube {
        return cubes.find_by_wire_id(database_id, wire_id).ok_or_else(FluxolapError::cube_not_found);
    }
    if let Some(name) = name_cube {
        return cubes.find_by_name(database_id, name).ok_or_else(FluxolapError::cube_not_found);
    }
    Err(FluxolapError::cube_not_found())
}

// ---------------------------------------------------------------------------
// Cell coordinates: hierarchy expansion, read, write
// ---------------------------------------------------------------------------

/// Maps a path's internal `ElementId`s to their wire-facing ids, in cube
/// dimension order.
pub fn cube_wire_path(dimensions: &DimensionTable, cube: &Cube, path: &[ElementId]) -> Vec<u32> {
    path.iter()
        .zip(cube.dimensions.iter())
        .map(|(&eid, &did)| dimensions.get(did).expect("cube references a live dimension").get_element(eid).expect("valid element").wire_id)
        .collect()
}

/// Cartesian product of a list of per-dimension candidate lists. Any
/// empty input list makes the whole product empty, matching the
/// mathematical definition (not a "no restriction" wildcard).
pub fn cartesian_product<T: Clone>(lists: &[Vec<T>]) -> Vec<Vec<T>> {
    let mut result: Vec<Vec<T>> = vec![Vec::new()];
    for list in lists {
        let mut next = Vec::with_capacity(result.len() * list.len());
        for partial in &result {
            for item in list {
                let mut p = partial.clone();
                p.push(item.clone());
                next.push(p);
            }
        }
        result = next;
    }
    result
}

/// **Builds the full leaf cross product**: for a consolidated coordinate over several large
/// dimensions that is billions of paths, and a failed allocation aborts the process. Only for
/// coordinates known to be small; check `is_leaf_path`, or work on per-dimension leaf sets
/// against the stored cells like `write_cell`/`read_cell`/`clear_area`.
///
/// Expands a full cube coordinate into its base (leaf) paths with
/// combined weights: the cartesian product of each dimension's
/// coordinate's own expansion (`Dimension::expand_to_base`). A path made
/// entirely of leaf coordinates expands to itself with weight 1.0.
pub fn expand_cube_path(dimensions: &DimensionTable, cube: &Cube, path: &[ElementId]) -> Vec<(Vec<ElementId>, f64)> {
    let per_dimension: Vec<Vec<(ElementId, f64)>> = cube
        .dimensions
        .iter()
        .zip(path.iter())
        .map(|(&did, &eid)| {
            let dim = dimensions.get(did).expect("cube references a live dimension");
            dim.expand_to_base(eid)
        })
        .collect();

    let mut result: Vec<(Vec<ElementId>, f64)> = vec![(Vec::new(), 1.0)];
    for dim_expansion in per_dimension {
        let mut next = Vec::with_capacity(result.len() * dim_expansion.len().max(1));
        for (partial_path, partial_weight) in &result {
            for &(elem, w) in &dim_expansion {
                let mut p = partial_path.clone();
                p.push(elem);
                next.push((p, partial_weight * w));
            }
        }
        result = next;
    }
    result
}

pub fn is_plain_leaf_path(expansion: &[(Vec<ElementId>, f64)]) -> bool {
    expansion.len() == 1 && expansion[0].1 == 1.0
}

/// Per-dimension leaf -> combined-weight map, built from
/// `Dimension::expand_to_base` for one dimension's requested coordinate.
/// Weights are *summed* per leaf, not overwritten: a diamond-shaped
/// hierarchy can reach the same leaf via more than one path, and
/// `expand_to_base` deliberately returns one entry per path (see its own
/// doc comment/tests) precisely so a shared descendant's contribution gets
/// counted once per path, not collapsed to one.
fn leaf_weights(dim: &Dimension, id: ElementId) -> HashMap<ElementId, f64> {
    let mut map = HashMap::new();
    for (leaf, w) in dim.expand_to_base(id) {
        *map.entry(leaf).or_insert(0.0) += w;
    }
    map
}

/// Leading cube dimensions whose set of relevant leaf elements is small enough (at most a
/// quarter of the dimension's elements) that skipping storage blocks outside it beats scanning
/// everything -- what `Storage::par_fold_cells_in` wants as its `prefix`. Stops at the first
/// dimension that isn't selective: cold storage is sorted by dimension 0 first, so a later
/// dimension can only be exploited once every dimension before it is pinned down too, and an
/// unselective one in between would make the pruning cost more than it saves. `leaves(d)` is
/// the set of leaf elements of cube dimension `d` that the caller's query can touch.
pub fn selective_prefix(dimensions: &DimensionTable, cube: &Cube, leaves: impl Fn(usize) -> Vec<ElementId>) -> Vec<Vec<ElementId>> {
    let mut prefix = Vec::new();
    for (d, &dim_id) in cube.dimensions.iter().enumerate() {
        let Some(dim) = dimensions.get(dim_id) else { break };
        let allowed = leaves(d);
        if allowed.len().saturating_mul(4) > dim.element_count() {
            break;
        }
        prefix.push(allowed);
    }
    prefix
}

/// Reads a (possibly consolidated) coordinate: a plain-leaf path reuses
/// the direct-set behavior (including string values); a genuinely
/// consolidated path sums `child_value * combined_weight` across its
/// base-path expansion (numeric only). `exists` means "numeric value is
/// non-zero", so an aggregate that
/// happens to sum to exactly zero reports as not-existing, same as an
/// unset leaf.
///
/// **Scales with the number of *stored* cells in the cube, not with the
/// cross product of every queried dimension's leaf count.** The earlier
/// implementation called `expand_cube_path` (the cartesian product of each
/// dimension's own `expand_to_base` expansion) and looked up every
/// resulting leaf path individually -- correct, but catastrophic for a
/// real multi-dimensional cube: a "grand total" read (several dimensions
/// each rolled up simultaneously, exactly what an OLAP UI's default
/// collapsed view requests) multiplies each dimension's leaf count
/// together, easily reaching billions of synthetic paths for a cube with
/// only a few thousand *actually stored* cells -- found via a real ETL
/// job's analytics view exhausting all available memory and swapping the
/// machine, not by a unit test (every test cube built so far was
/// single-dimension, which never exercises the cross-dimension
/// multiplication at all). Instead: build each dimension's requested
/// coordinate's leaf-weight map once (bounded by that one dimension's own
/// leaf count), then make a single pass over `Storage::iter_cube` --
/// bounded by real data volume, which is what actually determines cost for
/// a sparse cube -- accumulating `stored_value * combined_weight` for every
/// stored cell whose coordinate is covered in every dimension.
pub fn read_cell(dimensions: &DimensionTable, storage: &Storage, cube_id: CubeId, cube: &Cube, path: &[ElementId]) -> Option<CellValue> {
    let per_dim_weights: Vec<HashMap<ElementId, f64>> = cube
        .dimensions
        .iter()
        .zip(path.iter())
        .map(|(&did, &eid)| {
            let dim = dimensions.get(did).expect("cube references a live dimension");
            leaf_weights(dim, eid)
        })
        .collect();

    // Plain-leaf fast path: every dimension's requested coordinate is
    // already a leaf (a single entry at weight 1.0) -- direct lookup,
    // string values included, same as before.
    if per_dim_weights.iter().all(|w| w.len() == 1 && w.values().next() == Some(&1.0)) {
        let leaf_path: Vec<ElementId> = per_dim_weights.iter().map(|w| *w.keys().next().unwrap()).collect();
        return storage.get(cube_id, &leaf_path);
    }

    // Distributes every stored cell into this one accumulator -- a
    // chunk-and-merge aggregation, expressed as rayon's
    // `filter_map().sum()`: each stored
    // cell's contribution is independent of every other's, so summing in
    // parallel and combining partial sums is exactly as correct as
    // summing sequentially, just spread across every core for a cube
    // with enough stored cells to be worth it.
    // Streamed straight out of storage (no Vec of every stored cell): the cold tier is decoded
    // block-parallel into per-block partial sums, which is what makes this cheap on a large,
    // compacted cube -- see `Storage::par_fold_cells`.
    let prefix = selective_prefix(dimensions, cube, |d| per_dim_weights[d].keys().copied().collect());
    let sum: f64 = storage.par_fold_cells_in(
        cube_id,
        &prefix,
        || 0.0f64,
        |acc: &mut f64, stored_path: &[ElementId], value: &CellValue| {
            let CellValue::Numeric(v) = value else { return };
            let mut combined_weight = 1.0;
            for (d, elem) in stored_path.iter().enumerate() {
                match per_dim_weights[d].get(elem) {
                    Some(&w) => combined_weight *= w,
                    None => return,
                }
            }
            *acc += v * combined_weight;
        },
        |a, b| a + b,
    );
    if sum == 0.0 {
        None
    } else {
        Some(CellValue::Numeric(sum))
    }
}

/// Default splash limit: the most base cells one splash may write -- 1000 MB
/// at 16 bytes per cell. `set_splash_cell_limit` overrides
/// it at runtime (the embedded connector sets it from `colibri.flux.splash-limit`).
pub const DEFAULT_SPLASH_CELL_LIMIT: usize = 1000 * 1024 * 1024 / 16;

/// Base cells per write while splashing into an empty area; see `write_cell_with_limit`.
const SPLASH_WRITE_CHUNK: usize = 1 << 20;

static SPLASH_CELL_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_SPLASH_CELL_LIMIT);

pub fn splash_cell_limit() -> usize {
    SPLASH_CELL_LIMIT.load(Ordering::Relaxed)
}

pub fn set_splash_cell_limit(limit: usize) {
    SPLASH_CELL_LIMIT.store(limit, Ordering::Relaxed);
}

/// Number of paths in the cartesian product of `lists`, `None` if it doesn't fit a `usize`.
/// Check this before building a product (`cartesian_product`) of caller-controlled size.
pub fn product_size<T>(lists: &[Vec<T>]) -> Option<usize> {
    lists.iter().try_fold(1usize, |acc, list| acc.checked_mul(list.len()))
}

/// Whether every coordinate of `path` is a leaf of its dimension -- i.e. whether
/// `expand_cube_path` would return the path itself at weight 1.0, without building it.
pub fn is_leaf_path(dimensions: &DimensionTable, cube: &Cube, path: &[ElementId]) -> bool {
    path.iter().zip(cube.dimensions.iter()).all(|(&eid, &did)| {
        let dim = dimensions.get(did).expect("cube references a live dimension");
        dim.children_of(eid).is_empty()
    })
}

/// Writes a (possibly consolidated) coordinate. A plain-leaf path reuses
/// the direct-set behavior (numeric or string). A genuinely consolidated
/// path implements the classic "default" splash mode:
/// - target == 0.0: clear every stored base cell (sparse, `Storage::clear_area`).
/// - target != 0.0 and the current aggregate is non-zero: scale every
///   stored base cell by `target / old_aggregate` (sparse, "multiply
///   existing"; a stored string cell in the area is removed, as
///   the old per-path splash did).
/// - target != 0.0 and the current aggregate is 0.0: every base cell gets
///   `target / sum of combined weights` ("set base" after dividing by the
///   total weight), so the aggregate reads back as exactly `target` -- the
///   one case that writes every base cell, so it is refused with
///   `SPLASH_NOT_POSSIBLE` above `splash_cell_limit()` cells, and when the
///   weights sum to 0. Written in chunks, so it
///   is not atomic for concurrent readers.
///
/// Nothing here builds the leaf cross product of the coordinate except that
/// last, limited case: expanding a root coordinate of a real cube that way
/// (`expand_cube_path`) is tens of billions of paths, and a failed allocation
/// aborts the whole process -- the embedding JVM with it.
///
/// Splashing is numeric-only. `raw_value` is a plain string rather than a
/// typed value: `write_cell` parses it with `str::parse::<f64>`, and
/// Rust's float formatter round-trips exactly, so a caller with a number
/// passes `format!("{v}")` without losing precision.
pub fn write_cell(
    dimensions: &DimensionTable,
    storage: &Storage,
    cube_id: CubeId,
    cube: &Cube,
    path: Vec<ElementId>,
    raw_value: String,
) -> Result<(), FluxolapError> {
    write_cell_with_limit(dimensions, storage, cube_id, cube, path, raw_value, splash_cell_limit())
}

/// `write_cell` with an explicit splash limit instead of the process-wide one.
pub fn write_cell_with_limit(
    dimensions: &DimensionTable,
    storage: &Storage,
    cube_id: CubeId,
    cube: &Cube,
    path: Vec<ElementId>,
    raw_value: String,
    limit: usize,
) -> Result<(), FluxolapError> {
    if is_leaf_path(dimensions, cube, &path) {
        let cell_value = match raw_value.parse::<f64>() {
            Ok(v) => CellValue::Numeric(v),
            Err(_) => CellValue::String(raw_value),
        };
        storage.record_and_set(cube_id, path, cell_value);
        return Ok(());
    }

    let target: f64 = raw_value
        .parse()
        .map_err(|_| FluxolapError::invalid_coordinates("splash value must be numeric for a consolidated coordinate"))?;

    let per_dim_weights: Vec<HashMap<ElementId, f64>> = cube
        .dimensions
        .iter()
        .zip(path.iter())
        .map(|(&did, &eid)| leaf_weights(dimensions.get(did).expect("cube references a live dimension"), eid))
        .collect();
    let leaves: Vec<Option<HashSet<ElementId>>> = per_dim_weights.iter().map(|w| Some(w.keys().copied().collect())).collect();

    if target == 0.0 {
        storage.clear_area(cube_id, &leaves);
        return Ok(());
    }

    let prefix = selective_prefix(dimensions, cube, |d| per_dim_weights[d].keys().copied().collect());
    let old_sum: f64 = storage.par_fold_cells_in(
        cube_id,
        &prefix,
        || 0.0f64,
        |acc: &mut f64, stored_path: &[ElementId], value: &CellValue| {
            let CellValue::Numeric(v) = value else { return };
            let mut combined_weight = 1.0;
            for (d, elem) in stored_path.iter().enumerate() {
                match per_dim_weights[d].get(elem) {
                    Some(&w) => combined_weight *= w,
                    None => return,
                }
            }
            *acc += v * combined_weight;
        },
        |a, b| a + b,
    );

    if old_sum != 0.0 {
        let factor = target / old_sum;
        storage.rewrite_area(cube_id, &leaves, |_, value| match value {
            CellValue::Numeric(v) => Some(CellValue::Numeric(v * factor)),
            CellValue::String(_) => None,
        });
        return Ok(());
    }

    // Empty area: every base cell gets target / (sum of all combined weights), so
    // the aggregate reads back as exactly `target` whatever the weights.
    let per_dim: Vec<Vec<ElementId>> = per_dim_weights.iter().map(|w| w.keys().copied().collect()).collect();
    let cells = product_size(&per_dim);
    if cells.is_none_or(|n| n > limit) {
        let count = cells.map_or_else(|| "more than 2^64".to_string(), |n| n.to_string());
        return Err(FluxolapError::splash_not_possible(format!(
            "splashing into this empty consolidated cell would write {count} base cells, more than the limit of {limit}"
        )));
    }
    let weight_sum: f64 = per_dim_weights.iter().map(|w| w.values().sum::<f64>()).product();
    if weight_sum == 0.0 {
        return Err(FluxolapError::splash_not_possible("sum of weights is 0.0, cannot splash"));
    }
    let value = CellValue::Numeric(target / weight_sum);
    let n = cells.expect("checked above");
    // Written in chunks, not as one list of every base cell: the list costs ~100 bytes per
    // cell on top of the storage itself, while a chunk keeps the peak close to the stored
    // size (the hot tier compacts into ~26-byte cold cells every `HOT_COMPACT_TRIGGER`
    // cells). The price: a concurrent reader can see the area partly filled.
    let mut index = vec![0usize; per_dim.len()];
    let mut remaining = n;
    while remaining > 0 {
        let chunk_len = remaining.min(SPLASH_WRITE_CHUNK);
        let mut chunk: Vec<(Vec<ElementId>, CellValue)> = Vec::with_capacity(chunk_len);
        for _ in 0..chunk_len {
            let p: Vec<ElementId> = index.iter().enumerate().map(|(d, &i)| per_dim[d][i]).collect();
            chunk.push((p, value.clone()));
            for d in (0..index.len()).rev() {
                index[d] += 1;
                if index[d] < per_dim[d].len() {
                    break;
                }
                index[d] = 0;
            }
        }
        storage.record_and_set_many(cube_id, chunk);
        remaining -= chunk_len;
    }
    Ok(())
}

/// Batched `read_cell`, results in input order. Plain-leaf paths are read
/// with one `Storage::get_many` call. All consolidated paths are computed
/// together in **one** scan of the cube's stored cells -- each stored cell
/// adds `value * weight` to every requested consolidated cell it rolls up
/// into -- instead of one full scan per consolidated cell (`read_cell`),
/// which made a grid of a few hundred totals over a large cube take
/// seconds.
pub fn read_cells(dimensions: &DimensionTable, storage: &Storage, cube_id: CubeId, cube: &Cube, paths: &[Vec<ElementId>]) -> Vec<Option<CellValue>> {
    let (leaf_idx, other_idx): (Vec<usize>, Vec<usize>) = (0..paths.len()).partition(|&i| is_leaf_path(dimensions, cube, &paths[i]));
    let leaf_paths: Vec<Vec<ElementId>> = leaf_idx.iter().map(|&i| paths[i].clone()).collect();
    let mut out: Vec<Option<CellValue>> = vec![None; paths.len()];
    for (i, value) in leaf_idx.into_iter().zip(storage.get_many(cube_id, &leaf_paths)) {
        out[i] = value;
    }
    if other_idx.is_empty() {
        return out;
    }
    let consolidated: Vec<&[ElementId]> = other_idx.iter().map(|&i| paths[i].as_slice()).collect();
    for (&i, sum) in other_idx.iter().zip(sum_consolidated(dimensions, storage, cube_id, cube, &consolidated)) {
        out[i] = (sum != 0.0).then_some(CellValue::Numeric(sum));
    }
    out
}

/// The weighted sums for `paths` (each with at least one consolidated
/// element), from a single scan of the stored cells.
///
/// Per dimension, the distinct requested elements get a slot, and every
/// leaf maps to the slots (with combined weight) it rolls up into. A
/// stored cell's leaves then name, per dimension, the slots it feeds;
/// each combination of those is one requested cell (if it was asked for)
/// that gains `value * product of weights`. Requested cells are keyed by
/// their slots' mixed-radix number, so no path is ever built per cell.
fn sum_consolidated(dimensions: &DimensionTable, storage: &Storage, cube_id: CubeId, cube: &Cube, paths: &[&[ElementId]]) -> Vec<f64> {
    let n = cube.dimensions.len();
    let mut slots: Vec<HashMap<ElementId, usize>> = vec![HashMap::new(); n];
    for path in paths {
        for (d, &e) in path.iter().enumerate() {
            let next = slots[d].len();
            slots[d].entry(e).or_insert(next);
        }
    }
    // stride[d] = product of the slot counts after d; overflow is impossible
    // in practice (it would need more requested cells than memory), but
    // fall back to per-cell reads rather than miscount.
    let mut strides = vec![1usize; n];
    for d in (0..n.saturating_sub(1)).rev() {
        match strides[d + 1].checked_mul(slots[d + 1].len()) {
            Some(s) => strides[d] = s,
            None => {
                return paths
                    .iter()
                    .map(|p| match read_cell(dimensions, storage, cube_id, cube, p) {
                        Some(CellValue::Numeric(v)) => v,
                        _ => 0.0,
                    })
                    .collect()
            }
        }
    }
    let key = |path: &[ElementId]| -> usize { path.iter().enumerate().map(|(d, e)| slots[d][e] * strides[d]).sum() };
    // Distinct requested cells -> accumulator index.
    let mut target: HashMap<usize, usize> = HashMap::new();
    let index_of: Vec<usize> = paths
        .iter()
        .map(|p| {
            let next = target.len();
            *target.entry(key(p)).or_insert(next)
        })
        .collect();

    // leaf -> [(slot, combined weight)] per dimension.
    let mut feeds: Vec<HashMap<ElementId, Vec<(usize, f64)>>> = vec![HashMap::new(); n];
    for (d, &dim_id) in cube.dimensions.iter().enumerate() {
        let dim = dimensions.get(dim_id).expect("cube references a live dimension");
        for (&e, &slot) in &slots[d] {
            for (leaf, w) in leaf_weights(dim, e) {
                feeds[d].entry(leaf).or_default().push((slot, w));
            }
        }
    }
    let prefix = selective_prefix(dimensions, cube, |d| feeds[d].keys().copied().collect());

    let len = target.len();
    let sums = storage.par_fold_cells_in(
        cube_id,
        &prefix,
        || vec![0.0f64; len],
        |acc: &mut Vec<f64>, stored_path: &[ElementId], value: &CellValue| {
            let CellValue::Numeric(v) = value else { return };
            let mut lists: Vec<&[(usize, f64)]> = Vec::with_capacity(n);
            for (d, leaf) in stored_path.iter().enumerate() {
                match feeds[d].get(leaf) {
                    Some(list) => lists.push(list),
                    None => return,
                }
            }
            // Every combination of this cell's slots: usually one per dimension.
            let mut stack = vec![(0usize, 0usize, *v)];
            while let Some((d, k, weighted)) = stack.pop() {
                if d == n {
                    if let Some(&i) = target.get(&k) {
                        acc[i] += weighted;
                    }
                    continue;
                }
                for &(slot, w) in lists[d] {
                    stack.push((d + 1, k + slot * strides[d], weighted * w));
                }
            }
        },
        |mut a, b| {
            for (x, y) in a.iter_mut().zip(b) {
                *x += y;
            }
            a
        },
    );
    index_of.into_iter().map(|i| sums[i]).collect()
}

/// Batched `write_cell`, applied in input order. Consecutive plain-leaf
/// writes are buffered and stored with one `Storage::record_and_set_many`
/// call (one partition-lock acquisition, lock-journaled like every write);
/// a consolidated coordinate flushes the buffer first and then splashes,
/// so the outcome equals writing the entries one at a time. A leaf takes
/// the value as given (numeric or string); a consolidated coordinate
/// needs a numeric value, checked for every entry before anything is
/// written. Writing a type's default (0.0, empty string) clears the cell;
/// 0.0 on a consolidated coordinate clears its whole base area.
///
/// Not atomic against a splash failure (`SPLASH_NOT_POSSIBLE`): entries
/// before the failing one stay written. `Err` carries the failing index.
pub fn write_cells(
    dimensions: &DimensionTable,
    storage: &Storage,
    cube_id: CubeId,
    cube: &Cube,
    entries: Vec<(Vec<ElementId>, CellValue)>,
) -> Result<(), (usize, FluxolapError)> {
    let leaf: Vec<bool> = entries.iter().map(|(path, _)| is_leaf_path(dimensions, cube, path)).collect();
    if let Some(i) = entries.iter().zip(&leaf).position(|((_, v), &is_leaf)| !is_leaf && matches!(v, CellValue::String(_))) {
        return Err((i, FluxolapError::invalid_coordinates("splash value must be numeric for a consolidated coordinate")));
    }

    let mut pending: Vec<(Vec<ElementId>, CellValue)> = Vec::new();
    for (i, ((path, value), is_leaf)) in entries.into_iter().zip(leaf).enumerate() {
        if is_leaf {
            pending.push((path, value));
            continue;
        }
        if !pending.is_empty() {
            storage.record_and_set_many(cube_id, std::mem::take(&mut pending));
        }
        let CellValue::Numeric(target) = value else { unreachable!("string splash values rejected above") };
        write_cell(dimensions, storage, cube_id, cube, path, target.to_string()).map_err(|e| (i, e))?;
    }
    if !pending.is_empty() {
        storage.record_and_set_many(cube_id, pending);
    }
    Ok(())
}

/// Clears every stored cell inside an area: `area[d]` lists the (leaf or
/// consolidated) elements of dimension `d`, `None` meaning the whole
/// dimension. Same result as writing `"0"` through `write_cell` to every
/// path of the area's cartesian product (a consolidated element covers
/// all its leaves), but each dimension is expanded to a leaf *set* on its
/// own and only the cube's stored cells are visited, so neither the area's
/// product nor a consolidated coordinate's base-path product is ever built.
/// Returns the number of cells removed.
pub fn clear_area(dimensions: &DimensionTable, storage: &Storage, cube_id: CubeId, cube: &Cube, area: &[Option<Vec<ElementId>>]) -> usize {
    let leaves: Vec<Option<HashSet<ElementId>>> = area
        .iter()
        .zip(cube.dimensions.iter())
        .map(|(ids, &did)| {
            ids.as_ref().map(|ids| {
                let dim = dimensions.get(did).expect("cube references a live dimension");
                ids.iter().flat_map(|&eid| dim.expand_to_base(eid).into_iter().map(|(leaf, _)| leaf)).collect()
            })
        })
        .collect();
    storage.clear_area(cube_id, &leaves)
}

// ---------------------------------------------------------------------------
// Locks
// ---------------------------------------------------------------------------

/// Guards area reads against a client requesting a cartesian product too
/// large to build in memory (`ERROR_MAX_CELL_REACHED`).
pub const MAX_AREA_CELLS: usize = 1_000_000;

/// Expands a raw per-dimension element-id area down to the *leaf*
/// elements it covers, one `HashSet` per dimension -- see
/// `CubeLock::overlap_leaves`'s doc comment in `fluxolap-storage` for why
/// leaves are sufficient here (fluxolap never stores anything but leaf
/// cell paths).
pub fn lock_overlap_leaves(dimensions: &DimensionTable, cube: &Cube, area: &[Vec<ElementId>]) -> Vec<HashSet<ElementId>> {
    area.iter()
        .zip(cube.dimensions.iter())
        .map(|(ids, &did)| {
            let dim = dimensions.get(did).expect("cube references a live dimension");
            ids.iter().flat_map(|&eid| dim.expand_to_base(eid).into_iter().map(|(leaf, _)| leaf)).collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two dimensions: D0 `Total` over L0 (weight 1) and L1 (weight 2), plus L2 outside it;
    /// D1 `Top` over M0 and M1. Returns (dimensions, cube_id, cube, [L0, L1, L2], [M0, M1], Total, Top).
    #[allow(clippy::type_complexity)]
    fn splash_fixture() -> (DimensionTable, CubeId, Cube, [ElementId; 3], [ElementId; 2], ElementId, ElementId) {
        use fluxolap_domain::cube::CUBE_TYPE_NORMAL;
        use fluxolap_domain::database::DB_TYPE_NORMAL;
        use fluxolap_domain::dimension::DIM_TYPE_NORMAL;
        let mut databases = DatabaseTable::new();
        let database_id = databases.create("Test", DB_TYPE_NORMAL).unwrap();
        let mut dimensions = DimensionTable::new();
        let d0 = dimensions.create(database_id, "D0", DIM_TYPE_NORMAL).unwrap();
        let dim = dimensions.get_mut(d0).unwrap();
        let l = [0, 1, 2].map(|i| dim.create_element(&format!("L{i}"), 1).unwrap());
        let total = dim.create_element("Total", 1).unwrap();
        dim.add_children(total, &[(l[0], 1.0), (l[1], 2.0)]).unwrap();
        let d1 = dimensions.create(database_id, "D1", DIM_TYPE_NORMAL).unwrap();
        let dim = dimensions.get_mut(d1).unwrap();
        let m = [0, 1].map(|i| dim.create_element(&format!("M{i}"), 1).unwrap());
        let top = dim.create_element("Top", 1).unwrap();
        dim.add_children(top, &[(m[0], 1.0), (m[1], 1.0)]).unwrap();
        let mut cubes = CubeTable::new();
        let cube_id = cubes.create(database_id, "Cube", vec![d0, d1], CUBE_TYPE_NORMAL).unwrap();
        let cube = cubes.get(cube_id).unwrap().clone();
        (dimensions, cube_id, cube, l, m, total, top)
    }

    #[test]
    fn splash_distributes_into_an_empty_area_scales_existing_cells_and_clears_on_zero() {
        let (dims, cube_id, cube, l, m, total, top) = splash_fixture();
        let storage = Storage::default();
        storage.cube_created(cube_id, 2);
        storage.record_and_set(cube_id, vec![l[2], m[0]], CellValue::Numeric(7.0)); // outside the area
        let at = |p: &[ElementId]| read_cell(&dims, &storage, cube_id, &cube, p);

        // Empty area: every base cell gets 60 / ((1 + 2) * (1 + 1)) = 10, the aggregate is 60.
        write_cell(&dims, &storage, cube_id, &cube, vec![total, top], "60".into()).unwrap();
        assert_eq!(at(&[l[1], m[1]]), Some(CellValue::Numeric(10.0)));
        assert_eq!(at(&[total, top]), Some(CellValue::Numeric(60.0)));

        // Existing values: scaled by 30 / 60.
        storage.record_and_set(cube_id, vec![l[0], m[0]], CellValue::Numeric(40.0)); // aggregate now 90
        write_cell(&dims, &storage, cube_id, &cube, vec![total, top], "30".into()).unwrap();
        let num = |v: Option<CellValue>| match v {
            Some(CellValue::Numeric(x)) => x,
            other => panic!("expected a number, got {other:?}"),
        };
        assert!((num(at(&[total, top])) - 30.0).abs() < 1e-9);
        assert!((num(at(&[l[0], m[0]])) - 40.0 / 3.0).abs() < 1e-9);

        // Zero clears the area, and only the area.
        write_cell(&dims, &storage, cube_id, &cube, vec![total, top], "0".into()).unwrap();
        assert_eq!(at(&[total, top]), None);
        assert_eq!(storage.filled_count(cube_id), 1);
        assert_eq!(at(&[l[2], m[0]]), Some(CellValue::Numeric(7.0)));
    }

    #[test]
    fn splash_into_an_empty_area_larger_than_one_write_chunk_fills_every_base_cell() {
        use fluxolap_domain::cube::CUBE_TYPE_NORMAL;
        use fluxolap_domain::database::DB_TYPE_NORMAL;
        use fluxolap_domain::dimension::DIM_TYPE_NORMAL;
        let mut databases = DatabaseTable::new();
        let database_id = databases.create("Test", DB_TYPE_NORMAL).unwrap();
        let mut dimensions = DimensionTable::new();
        let mut dim_ids = Vec::new();
        let mut roots = Vec::new();
        for (d, leaves) in [1100usize, 1000].into_iter().enumerate() {
            let id = dimensions.create(database_id, &format!("D{d}"), DIM_TYPE_NORMAL).unwrap();
            let dim = dimensions.get_mut(id).unwrap();
            let ids: Vec<ElementId> = (0..leaves).map(|i| dim.create_element(&format!("L{i}"), 1).unwrap()).collect();
            let root = dim.create_element("All", 1).unwrap();
            dim.add_children(root, &ids.iter().map(|&l| (l, 1.0)).collect::<Vec<_>>()).unwrap();
            dim_ids.push(id);
            roots.push(root);
        }
        let mut cubes = CubeTable::new();
        let cube_id = cubes.create(database_id, "Cube", dim_ids, CUBE_TYPE_NORMAL).unwrap();
        let cube = cubes.get(cube_id).unwrap().clone();
        let storage = Storage::default();
        storage.cube_created(cube_id, 2);

        const { assert!(1100 * 1000 > SPLASH_WRITE_CHUNK) };
        write_cell(&dimensions, &storage, cube_id, &cube, roots.clone(), "1100000".into()).unwrap();
        assert_eq!(storage.filled_count(cube_id), 1_100_000);
        assert_eq!(read_cell(&dimensions, &storage, cube_id, &cube, &roots), Some(CellValue::Numeric(1_100_000.0)));
    }

    #[test]
    fn splash_into_an_empty_area_above_the_limit_is_refused_and_writes_nothing() {
        let (dims, cube_id, cube, l, m, total, top) = splash_fixture();
        let storage = Storage::default();
        storage.cube_created(cube_id, 2);

        // 2 x 2 = 4 base cells, limit 3.
        let err = write_cell_with_limit(&dims, &storage, cube_id, &cube, vec![total, top], "60".into(), 3).unwrap_err();
        assert_eq!(err.code, fluxolap_error::ErrorCode::SPLASH_NOT_POSSIBLE);
        assert!(err.message.contains("4 base cells"), "{}", err.message);
        assert_eq!(storage.filled_count(cube_id), 0);

        // Scaling and clearing only touch stored cells: not limited.
        storage.record_and_set(cube_id, vec![l[0], m[0]], CellValue::Numeric(5.0));
        write_cell_with_limit(&dims, &storage, cube_id, &cube, vec![total, top], "10".into(), 0).unwrap();
        assert_eq!(read_cell(&dims, &storage, cube_id, &cube, &[total, top]), Some(CellValue::Numeric(10.0)));
        write_cell_with_limit(&dims, &storage, cube_id, &cube, vec![total, top], "0".into(), 0).unwrap();
        assert_eq!(storage.filled_count(cube_id), 0);
    }

    #[test]
    fn write_cells_applies_leaf_writes_and_splashes_in_order() {
        let (dims, cube_id, cube, l, m, total, top) = splash_fixture();
        let storage = Storage::default();
        storage.cube_created(cube_id, 2);
        let entries = vec![
            (vec![l[0], m[0]], CellValue::Numeric(5.0)),
            (vec![l[2], m[1]], CellValue::String("note".into())),
            // Splash sees the 5.0 written just before it: aggregate 5 -> 20 scales it by 4.
            (vec![total, top], CellValue::Numeric(20.0)),
            (vec![l[1], m[1]], CellValue::Numeric(1.0)),
        ];
        write_cells(&dims, &storage, cube_id, &cube, entries).unwrap();

        let paths = vec![vec![l[0], m[0]], vec![l[2], m[1]], vec![l[1], m[1]], vec![total, top]];
        assert_eq!(
            read_cells(&dims, &storage, cube_id, &cube, &paths),
            vec![Some(CellValue::Numeric(20.0)), Some(CellValue::String("note".into())), Some(CellValue::Numeric(1.0)), Some(CellValue::Numeric(22.0))]
        );
    }

    #[test]
    fn read_cells_single_scan_matches_reading_each_cell() {
        let (dims, cube_id, cube, l, m, total, top) = splash_fixture();
        let storage = Storage::default();
        storage.cube_created(cube_id, 2);
        for (i, &a) in l.iter().enumerate() {
            for (j, &b) in m.iter().enumerate() {
                storage.record_and_set(cube_id, vec![a, b], CellValue::Numeric((i * 10 + j + 1) as f64));
            }
        }
        storage.record_and_set(cube_id, vec![l[2], m[1]], CellValue::String("note".into()));
        let d0 = [l[0], l[1], l[2], total];
        let d1 = [m[0], m[1], top];
        let mut paths: Vec<Vec<ElementId>> = d0.iter().flat_map(|&a| d1.iter().map(move |&b| vec![a, b])).collect();
        paths.push(vec![total, top]); // a duplicate
        let batched = read_cells(&dims, &storage, cube_id, &cube, &paths);
        let one_by_one: Vec<Option<CellValue>> = paths.iter().map(|p| read_cell(&dims, &storage, cube_id, &cube, p)).collect();
        assert_eq!(batched, one_by_one);
        // Total = L0 + 2 * L1 over both M: (1 + 2) + 2 * (11 + 12) = 49.
        assert_eq!(batched[paths.len() - 1], Some(CellValue::Numeric(49.0)));
    }

    #[test]
    fn write_cells_rejects_a_string_splash_before_writing_anything() {
        let (dims, cube_id, cube, l, m, total, top) = splash_fixture();
        let storage = Storage::default();
        storage.cube_created(cube_id, 2);
        let entries = vec![(vec![l[0], m[0]], CellValue::Numeric(5.0)), (vec![total, top], CellValue::String("x".into()))];
        let (index, err) = write_cells(&dims, &storage, cube_id, &cube, entries).unwrap_err();
        assert_eq!(index, 1);
        assert_eq!(err.code, fluxolap_error::ErrorCode::INVALID_COORDINATES);
        assert_eq!(storage.filled_count(cube_id), 0);
    }

    #[test]
    fn cartesian_product_of_empty_input_is_one_empty_tuple() {
        let empty: Vec<Vec<u32>> = Vec::new();
        assert_eq!(cartesian_product(&empty), vec![Vec::<u32>::new()]);
    }

    #[test]
    fn cartesian_product_of_two_lists() {
        let lists = vec![vec![1u32, 2], vec![10u32, 20]];
        let got = cartesian_product(&lists);
        assert_eq!(got, vec![vec![1, 10], vec![1, 20], vec![2, 10], vec![2, 20]]);
    }

    #[test]
    fn cartesian_product_any_empty_list_makes_product_empty() {
        let lists = vec![vec![1u32, 2], Vec::new()];
        assert!(cartesian_product(&lists).is_empty());
    }

    /// Regression test for a real production incident: reading a
    /// consolidated coordinate on a multi-dimensional cube exhausted all
    /// available memory and swapped the machine, even though the cube held
    /// only a handful of stored cells. Root cause was `read_cell`'s old
    /// implementation materializing the full cartesian product of every
    /// queried dimension's leaf expansion (`expand_cube_path`) -- three
    /// dimensions rolled up simultaneously, each with `n` leaves, produced
    /// `n^3` synthetic paths regardless of how sparse the cube actually
    /// was. Every prior test exercising this code path used a
    /// single-dimension cube, which never multiplies dimension leaf counts
    /// together at all -- that's why this shape went unnoticed until a
    /// real multi-dimensional ETL cube hit it.
    ///
    /// Builds three dimensions with `LEAVES_PER_DIM` leaves each under one
    /// "Total" parent (so the old algorithm would enumerate
    /// `LEAVES_PER_DIM^3` = 8,000,000 synthetic paths), a 3-D cube over
    /// them, and stores cells at only 3 leaf coordinates. Reading
    /// `(Total, Total, Total)` must return the correct sum of exactly
    /// those 3 stored cells, and must do so in well under a second --
    /// the old algorithm, run against this shape, does not return in any
    /// practical amount of time (it was still consuming memory when this
    /// test's fixed counterpart was written).
    #[test]
    fn read_cell_on_a_multi_dimensional_consolidated_coordinate_scales_with_stored_cells_not_leaf_cross_product() {
        use fluxolap_domain::cube::CUBE_TYPE_NORMAL;
        use fluxolap_domain::database::DB_TYPE_NORMAL;
        use fluxolap_domain::dimension::DIM_TYPE_NORMAL;
        use fluxolap_storage::Storage;
        use std::time::Instant;

        const LEAVES_PER_DIM: usize = 200; // 200^3 = 8,000,000 naive cross-product paths

        let mut databases = DatabaseTable::new();
        let database_id = databases.create("Test", DB_TYPE_NORMAL).unwrap();

        let mut dimensions = DimensionTable::new();
        let mut totals = Vec::with_capacity(3);
        let mut first_leaves = Vec::with_capacity(3);
        let mut dimension_ids = Vec::with_capacity(3);
        for d in 0..3 {
            let dim_id = dimensions.create(database_id, &format!("Dim{d}"), DIM_TYPE_NORMAL).unwrap();
            let dim = dimensions.get_mut(dim_id).unwrap();
            let mut leaves = Vec::with_capacity(LEAVES_PER_DIM);
            for i in 0..LEAVES_PER_DIM {
                leaves.push(dim.create_element(&format!("Leaf{i}"), 1).unwrap());
            }
            let total = dim.create_element("Total", 1).unwrap();
            let children: Vec<(ElementId, f64)> = leaves.iter().map(|&l| (l, 1.0)).collect();
            dim.add_children(total, &children).unwrap();
            first_leaves.push(leaves[0]);
            totals.push(total);
            dimension_ids.push(dim_id);
        }

        let mut cubes = CubeTable::new();
        let cube_id = cubes.create(database_id, "Cube1", dimension_ids, CUBE_TYPE_NORMAL).unwrap();
        let cube = cubes.get(cube_id).unwrap().clone();

        let storage = Storage::default();
        storage.cube_created(cube_id, cube.dimensions.len());
        // Only 1 cell actually stored, at Leaf0 in every dimension -- the
        // cube is overwhelmingly sparse relative to the 8,000,000 possible
        // leaf combinations.
        storage.record_and_set(cube_id, vec![first_leaves[0], first_leaves[1], first_leaves[2]], CellValue::Numeric(20.0));

        let start = Instant::now();
        let result = read_cell(&dimensions, &storage, cube_id, &cube, &[totals[0], totals[1], totals[2]]);
        let elapsed = start.elapsed();

        assert_eq!(result, Some(CellValue::Numeric(20.0)));
        assert!(elapsed.as_secs() < 5, "read_cell on a rolled-up multi-dimensional coordinate took {elapsed:?} -- must scale with stored cell count, not the leaf cross product");
    }

    /// `read_cell`'s aggregation streams stored cells through
    /// `Storage::par_fold_cells`, which splits a large scan across rayon.
    /// Every other `read_cell` test above stays tiny and only ever
    /// exercises the single-chunk case; this one stores enough cells to
    /// span many chunks, so a bug specific to the parallel path (a race, or
    /// a contribution silently dropped at a chunk boundary) would actually
    /// be caught -- both while every cell is in the hot tier and after
    /// compaction moved them to the cold tier, plus a hot overwrite that
    /// must shadow its stale cold copy.
        #[test]
    fn read_cell_on_a_rolled_up_coordinate_sums_correctly_above_the_parallel_threshold() {
        use fluxolap_domain::cube::CUBE_TYPE_NORMAL;
        use fluxolap_domain::database::DB_TYPE_NORMAL;
        use fluxolap_domain::dimension::DIM_TYPE_NORMAL;
        use fluxolap_storage::Storage;

        let leaves_per_dim = 1000 + 37; // several chunks, not a round multiple

        let mut databases = DatabaseTable::new();
        let database_id = databases.create("Test", DB_TYPE_NORMAL).unwrap();

        let mut dimensions = DimensionTable::new();
        let dim_id = dimensions.create(database_id, "Dim0", DIM_TYPE_NORMAL).unwrap();
        let dim = dimensions.get_mut(dim_id).unwrap();
        let leaves: Vec<ElementId> = (0..leaves_per_dim).map(|i| dim.create_element(&format!("Leaf{i}"), 1).unwrap()).collect();
        let total = dim.create_element("Total", 1).unwrap();
        let children: Vec<(ElementId, f64)> = leaves.iter().map(|&l| (l, 1.0)).collect();
        dim.add_children(total, &children).unwrap();

        let mut cubes = CubeTable::new();
        let cube_id = cubes.create(database_id, "Cube1", vec![dim_id], CUBE_TYPE_NORMAL).unwrap();
        let cube = cubes.get(cube_id).unwrap().clone();

        let storage = Storage::default();
        storage.cube_created(cube_id, cube.dimensions.len());
        let mut expected_sum = 0.0;
        for (i, &leaf) in leaves.iter().enumerate() {
            let value = (i + 1) as f64; // never 0.0 -- a stored 0.0 would just clear the cell
            storage.record_and_set(cube_id, vec![leaf], CellValue::Numeric(value));
            expected_sum += value;
        }

        let result = read_cell(&dimensions, &storage, cube_id, &cube, &[total]);
        assert_eq!(result, Some(CellValue::Numeric(expected_sum)), "hot tier");

        storage.shrink_cube(cube_id); // compaction: every cell now lives in the cold tier
        let result = read_cell(&dimensions, &storage, cube_id, &cube, &[total]);
        assert_eq!(result, Some(CellValue::Numeric(expected_sum)), "cold tier");

        storage.record_and_set(cube_id, vec![leaves[0]], CellValue::Numeric(1000.0)); // was 1.0; must shadow the cold copy
        let result = read_cell(&dimensions, &storage, cube_id, &cube, &[total]);
        assert_eq!(result, Some(CellValue::Numeric(expected_sum + 999.0)), "cold tier plus a shadowing hot overwrite");
    }
}
