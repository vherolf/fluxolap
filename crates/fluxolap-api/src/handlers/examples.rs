//! The example databases the server offers to import: the database files
//! (`*.bin.zst`) in its example directories. A browser can't read the
//! server's disk, so a web client lists them here and has the server
//! import one by file name -- only files in those directories, never an
//! arbitrary path.

use crate::dto::{DatabaseDto, ExampleDto, ExampleList};
use crate::error::{ApiError, ApiResult, Path};
use crate::PREFIX;
use axum::extract::{Extension, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use fluxolap_engine::AppState;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

/// The directories to offer database files from, in order; for a file
/// name found in several, the first one wins.
#[derive(Debug, Clone, Default)]
pub struct ExampleDirs(pub Arc<Vec<PathBuf>>);

/// Every database file in `dirs` (not a data directory's `registry.bin.zst`)
/// with the name of the database inside, each file name once, sorted by
/// database name. Missing directories and files that aren't database
/// files are skipped.
fn scan(dirs: &[PathBuf]) -> Vec<(String, PathBuf, String, u64)> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
            if !file.ends_with(".bin.zst") || file == "registry.bin.zst" || !path.is_file() || seen.contains(&file) {
                continue;
            }
            let Some(name) = fluxolap_engine::read_database_name(&path) else { continue };
            let size = entry.metadata().map_or(0, |m| m.len());
            seen.insert(file.clone());
            files.push((file, path, name, size));
        }
    }
    files.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
    files
}

/// Lists the example databases the server can import.
#[utoipa::path(get, path = "/api/v1/examples", tag = "examples", responses((status = 200, body = ExampleList)))]
pub async fn list(State(state): State<AppState>, Extension(dirs): Extension<ExampleDirs>) -> axum::Json<ExampleList> {
    let files = scan(&dirs.0);
    let databases = state.databases.read().expect("databases lock poisoned");
    let names: HashSet<&str> = databases.list().map(|db| db.name.as_str()).collect();
    let items: Vec<ExampleDto> = files.into_iter().map(|(file, _, name, size)| ExampleDto { loaded: names.contains(name.as_str()), file, name, size }).collect();
    let total = items.len();
    axum::Json(ExampleList { items, total })
}

/// Imports an example database, like `POST /databases/import` with that
/// file as the body.
#[utoipa::path(post, path = "/api/v1/examples/{file}/import", tag = "examples", params(("file" = String, Path, description = "The example's file name")),
    responses(
        (status = 201, body = DatabaseDto, headers(("Location" = String))),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json", description = "No such example"),
        (status = 409, body = crate::error::Problem, content_type = "application/problem+json", description = "A database with the example's name exists"),
    ))]
pub async fn import(State(state): State<AppState>, Extension(dirs): Extension<ExampleDirs>, Path(file): Path<String>) -> ApiResult<impl IntoResponse> {
    let not_found = || ApiError::new(StatusCode::NOT_FOUND, "example_not_found", format!("no example database file '{file}'"));
    let (_, path, _, _) = scan(&dirs.0).into_iter().find(|(f, ..)| *f == file).ok_or_else(not_found)?;
    let bytes = std::fs::read(&path).map_err(|_| not_found())?;
    let id = state.import_database_bytes(&bytes)?;
    let databases = state.databases.read().expect("databases lock poisoned");
    let dto = DatabaseDto::new(databases.get(id).expect("just imported"));
    let location = format!("{PREFIX}/databases/{}", dto.id);
    Ok((StatusCode::CREATED, [(header::LOCATION, location)], axum::Json(dto)))
}
