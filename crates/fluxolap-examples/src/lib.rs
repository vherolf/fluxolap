//! Builds fluxolap's example databases from the source packages under
//! `examples/sources/` (see `examples/sources/package.py` for the format)
//! into per-database files in the engine's own persistence format, which
//! the server imports at startup (`FLUXOLAP_EXAMPLES`).
//!
//! Examples that exist only as a database, not as source data, are
//! database files (`.bin.zst`, as `GET .../export` writes them) under
//! `examples/databases/`. They are imported and saved again with the rest,
//! so a release always ships them in its own file format -- and a format
//! change that can't read them any more fails the build, not a user.
//!
//! Everything goes through the same engine calls a user's own database
//! does -- `provision::create_database`/`create_dimension` (so system and
//! attribute dimensions exist as usual) and `ops::write_cells` -- and only
//! base cells are written: consolidated values are computed by the
//! hierarchy, never stored.

use flate2::read::GzDecoder;
use fluxolap_domain::cube::CUBE_TYPE_NORMAL;
use fluxolap_domain::database::DB_TYPE_NORMAL;
use fluxolap_domain::dimension::{Dimension, ElementType, DIM_TYPE_NORMAL};
use fluxolap_domain::{DatabaseId, DimensionId, ElementId};
use fluxolap_engine::{ops, provision, AppState, LoadedDatabase};
use fluxolap_storage::CellValue;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

pub type Error = Box<dyn std::error::Error>;

/// Cells handed to `ops::write_cells` per call.
const WRITE_CHUNK: usize = 500_000;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub database: String,
    pub order: u32,
    pub description: String,
    pub sources: Vec<serde_json::Value>,
    pub dimensions: Vec<FileRef>,
    pub cubes: Vec<CubeRef>,
    #[serde(default)]
    pub attributes: Vec<AttributeRef>,
}

#[derive(Debug, Deserialize)]
pub struct FileRef {
    pub name: String,
    pub file: String,
}

#[derive(Debug, Deserialize)]
pub struct CubeRef {
    pub name: String,
    pub dimensions: Vec<String>,
    pub file: String,
}

#[derive(Debug, Deserialize)]
pub struct AttributeRef {
    pub dimension: String,
    pub attribute: String,
    pub file: String,
}

/// `examples/sources` in this repository.
pub fn default_sources_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/sources")
}

/// Every source package under `sources` (a subdirectory with a
/// `manifest.json`), in manifest `order`.
pub fn packages(sources: &Path) -> Result<Vec<(PathBuf, Manifest)>, Error> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(sources)? {
        let dir = entry?.path();
        let manifest = dir.join("manifest.json");
        if manifest.is_file() {
            let m: Manifest = serde_json::from_reader(File::open(&manifest)?).map_err(|e| format!("{}: {e}", manifest.display()))?;
            out.push((dir, m));
        }
    }
    out.sort_by_key(|(_, m)| m.order);
    Ok(out)
}

/// Where the ready-made example database files are: `databases/` next to
/// `sources` (`examples/databases/` for `default_sources_dir()`).
pub fn databases_dir(sources: &Path) -> PathBuf {
    sources.with_file_name("databases")
}

/// Builds every package under `sources` into `out_dir`, then adds the
/// ready-made database files from `databases_dir(sources)`, replacing any
/// database files a previous build left there. Returns the written
/// database files, sorted.
pub fn build_all(sources: &Path, out_dir: &Path) -> Result<Vec<PathBuf>, Error> {
    std::fs::create_dir_all(out_dir)?;
    for entry in std::fs::read_dir(out_dir)? {
        let path = entry?.path();
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".bin.zst")) {
            std::fs::remove_file(&path)?;
        }
    }
    let state = AppState::load_from_dir(out_dir)?;
    for (dir, manifest) in packages(sources)? {
        build_package(&state, &dir, &manifest).map_err(|e| format!("{}: {e}", manifest.database))?;
    }
    let ready_made = databases_dir(sources);
    if ready_made.is_dir() {
        for file in database_files(&ready_made)? {
            match state.import_database_file(&file) {
                Ok(Some(_)) => {}
                Ok(None) => return Err(format!("{}: a database with its name is built from the sources too", file.display()).into()),
                Err(e) => return Err(format!("{}: can't be read ({e}) -- export it again from a server that reads it", file.display()).into()),
            }
        }
    }
    state.save_all()?;
    database_files(out_dir)
}

/// The per-database files in `dir` (every `*.bin.zst` but the registry), sorted.
pub fn database_files(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    files.retain(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".bin.zst") && n != "registry.bin.zst"));
    files.sort();
    Ok(files)
}

/// Builds one source package as a new database in `state`.
pub fn build_package(state: &AppState, dir: &Path, manifest: &Manifest) -> Result<DatabaseId, Error> {
    let db = provision::create_database(state, &manifest.database, DB_TYPE_NORMAL)?;
    let loaded = {
        let databases = state.databases.read().expect("databases lock poisoned");
        state.resolve_loaded(&databases, db)?
    };

    let mut dims: HashMap<&str, DimensionId> = HashMap::new();
    for d in &manifest.dimensions {
        let id = provision::create_dimension(&loaded, db, &d.name, DIM_TYPE_NORMAL)?;
        load_dimension(&loaded, id, &dir.join(&d.file)).map_err(|e| format!("dimension {}: {e}", d.name))?;
        dims.insert(&d.name, id);
    }

    for a in &manifest.attributes {
        load_attribute(&loaded, db, &a.dimension, &a.attribute, &dir.join(&a.file)).map_err(|e| format!("attribute {}.{}: {e}", a.dimension, a.attribute))?;
    }

    for c in &manifest.cubes {
        let dim_ids = c
            .dimensions
            .iter()
            .map(|n| dims.get(n.as_str()).copied().ok_or_else(|| format!("cube {} uses unknown dimension {n}", c.name)))
            .collect::<Result<Vec<_>, _>>()?;
        let n = dim_ids.len();
        let cube_id = loaded.cubes.write().expect("cubes lock poisoned").create(db, &c.name, dim_ids, CUBE_TYPE_NORMAL).map_err(ops::map_cube_error)?;
        loaded.storage.cube_created(cube_id, n);
        load_cells(&loaded, cube_id, &dir.join(&c.file)).map_err(|e| format!("cube {}: {e}", c.name))?;
    }
    loaded.mark_dirty();
    Ok(db)
}

fn gz_csv(path: &Path) -> Result<csv::Reader<GzDecoder<File>>, Error> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(csv::Reader::from_reader(GzDecoder::new(file)))
}

fn element_type(raw: &str) -> Result<u32, Error> {
    match raw {
        "numeric" => Ok(ElementType::Numeric as u32),
        "string" => Ok(ElementType::String as u32),
        other => Err(format!("unknown element type '{other}'").into()),
    }
}

/// Creates a dimension's elements in first-appearance order, then gives
/// every parent its children (with weights) in row order.
fn load_dimension(loaded: &LoadedDatabase, dim_id: DimensionId, path: &Path) -> Result<(), Error> {
    let mut rows = Vec::new();
    for record in gz_csv(path)?.records() {
        let r = record?;
        let weight: f64 = if r[3].is_empty() { 1.0 } else { r[3].parse().map_err(|_| format!("bad weight '{}'", &r[3]))? };
        rows.push((r[0].to_string(), r[1].to_string(), r[2].to_string(), weight));
    }

    let mut dimensions = loaded.dimensions.write().expect("dimensions lock poisoned");
    let dim = dimensions.get_mut(dim_id).expect("just created");
    for (element, kind, _, _) in &rows {
        if dim.find_element_by_name(element).is_none() {
            dim.create_element(element, element_type(kind)?).map_err(|e| format!("element '{element}': {e:?}"))?;
        }
    }
    let mut children: Vec<(ElementId, Vec<(ElementId, f64)>)> = Vec::new();
    let mut slot: HashMap<ElementId, usize> = HashMap::new();
    for (element, _, parent, weight) in &rows {
        if parent.is_empty() {
            continue;
        }
        let p = dim.find_element_by_name(parent).ok_or_else(|| format!("parent '{parent}' of '{element}' is not an element"))?;
        let c = dim.find_element_by_name(element).expect("created above");
        let i = *slot.entry(p).or_insert_with(|| {
            children.push((p, Vec::new()));
            children.len() - 1
        });
        children[i].1.push((c, *weight));
    }
    for (parent, list) in &children {
        dim.set_children(*parent, list).map_err(|e| format!("children of '{}': {e:?}", dim.get_element(*parent).map_or("?", |e| e.name.as_str())))?;
    }
    Ok(())
}

/// Writes string attribute values into `dimension`'s automatic
/// `#_<dimension>_` attribute cube, under a new `attribute` element.
fn load_attribute(loaded: &LoadedDatabase, db: DatabaseId, dimension: &str, attribute: &str, path: &Path) -> Result<(), Error> {
    let attr_name = format!("#_{dimension}_");
    let (dim_id, attr_dim_id, attr_el) = {
        let mut dimensions = loaded.dimensions.write().expect("dimensions lock poisoned");
        let dim_id = dimensions.find_by_name(db, dimension).ok_or("no such dimension")?;
        let attr_dim_id = dimensions.find_by_name(db, &attr_name).ok_or("dimension has no attribute dimension")?;
        let attr_dim = dimensions.get_mut(attr_dim_id).expect("just found");
        let attr_el = attr_dim.create_element(attribute, ElementType::String as u32).map_err(|e| format!("{e:?}"))?;
        (dim_id, attr_dim_id, attr_el)
    };
    let dimensions = loaded.dimensions.read().expect("dimensions lock poisoned");
    let cubes = loaded.cubes.read().expect("cubes lock poisoned");
    let cube_id = cubes.find_by_name(db, &attr_name).ok_or("dimension has no attribute cube")?;
    let cube = cubes.get(cube_id).expect("just found");
    if cube.dimensions != [attr_dim_id, dim_id] {
        return Err("unexpected attribute cube layout".into());
    }
    let dim = dimensions.get(dim_id).expect("just found");
    let mut entries = Vec::new();
    for record in gz_csv(path)?.records() {
        let r = record?;
        let el = dim.find_element_by_name(&r[0]).ok_or_else(|| format!("unknown element '{}'", &r[0]))?;
        entries.push((vec![attr_el, el], CellValue::String(r[1].to_string())));
    }
    ops::write_cells(&dimensions, &loaded.storage, cube_id, cube, entries).map_err(|(i, e)| format!("row {}: {e}", i + 2))?;
    Ok(())
}

/// Writes a cube's base cells. A cell whose path has a string element
/// holds a string, every other cell a number.
fn load_cells(loaded: &LoadedDatabase, cube_id: fluxolap_domain::CubeId, path: &Path) -> Result<(), Error> {
    let dimensions = loaded.dimensions.read().expect("dimensions lock poisoned");
    let cubes = loaded.cubes.read().expect("cubes lock poisoned");
    let cube = cubes.get(cube_id).expect("just created");
    let dims: Vec<&Dimension> = cube.dimensions.iter().map(|&d| dimensions.get(d).expect("cube dimension exists")).collect();

    let mut reader = gz_csv(path)?;
    let header = reader.headers()?.clone();
    if header.len() != dims.len() + 1 || dims.iter().zip(header.iter()).any(|(d, h)| d.name != h) {
        return Err(format!("header {:?} doesn't match the cube's dimensions", header).into());
    }
    let mut entries = Vec::with_capacity(WRITE_CHUNK);
    for (line, record) in reader.records().enumerate() {
        let r = record?;
        let mut cell_path = Vec::with_capacity(dims.len());
        let mut is_string = false;
        for (d, dim) in dims.iter().enumerate() {
            let el = dim.find_element_by_name(&r[d]).ok_or_else(|| format!("line {}: '{}' is not an element of {}", line + 2, &r[d], dim.name))?;
            is_string |= dim.get_element(el).expect("just found").elem_type == ElementType::String;
            cell_path.push(el);
        }
        if !ops::is_leaf_path(&dimensions, cube, &cell_path) {
            return Err(format!("line {}: a source package may only hold base cells, but {:?} is consolidated", line + 2, r).into());
        }
        let raw = &r[dims.len()];
        let value = if is_string {
            CellValue::String(raw.to_string())
        } else {
            CellValue::Numeric(raw.parse().map_err(|_| format!("line {}: '{raw}' is not a number", line + 2))?)
        };
        entries.push((cell_path, value));
        if entries.len() == WRITE_CHUNK {
            ops::write_cells(&dimensions, &loaded.storage, cube_id, cube, std::mem::take(&mut entries)).map_err(|(_, e)| e.to_string())?;
        }
    }
    ops::write_cells(&dimensions, &loaded.storage, cube_id, cube, entries).map_err(|(_, e)| e.to_string())?;
    Ok(())
}
