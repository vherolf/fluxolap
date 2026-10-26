//! Runs an `app::Cmd` against the server and turns the outcome into the
//! `app::Msg` the app expects.

use crate::app::{group, After, Cmd, DbFile, Msg, Target};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use crate::client::{Client, Error};

pub async fn execute(client: &Client, cmd: Cmd) -> Msg {
    match cmd {
        Cmd::LoadServer => Msg::Server(client.server().await),
        Cmd::LoadStats => Msg::Stats(client.stats().await.map(|s| s.map(Box::new))),
        Cmd::LoadDatabases => Msg::Databases(client.databases().await),
        Cmd::LoadContents { db } => {
            let result = async { Ok((client.dimensions(db).await?, client.cubes(db).await?)) }.await;
            Msg::Contents { db, result }
        }
        Cmd::LoadElements { db, dim, roots_only } => Msg::Elements { db, dim, roots_only, result: client.elements(db, dim, roots_only).await },
        Cmd::LoadCells { db, cube, area, token } => Msg::Cells { token, result: client.area(db, cube, area).await },
        Cmd::WriteCell { db, cube, path, value } => {
            let cleared = value.is_none();
            done(client.write_cell(db, cube, &path, value).await.map(|_| if cleared { "cell cleared".into() } else { "cell saved".into() }), After::ReloadCells)
        }
        Cmd::ClearCube { db, cube } => done(client.clear_cube(db, cube).await.map(|_| "cube cleared".into()), After::Refresh),
        Cmd::DeleteDatabase { db } => done(client.delete_database(db).await.map(|_| "database deleted".into()), After::Refresh),
        Cmd::DeleteDimension { db, dim } => done(client.delete_dimension(db, dim).await.map(|_| "dimension deleted".into()), After::Refresh),
        Cmd::DeleteCube { db, cube } => done(client.delete_cube(db, cube).await.map(|_| "cube deleted".into()), After::Refresh),
        Cmd::Rename { target, name } => {
            let result = match target {
                Target::Database(db) => client.rename_database(db, &name).await.map(|_| ()),
                Target::Dimension(db, dim) => client.rename_dimension(db, dim, &name).await.map(|_| ()),
                Target::Cube(db, cube) => client.rename_cube(db, cube, &name).await.map(|_| ()),
            };
            done(result.map(|_| format!("renamed to '{name}'")), After::Refresh)
        }
        Cmd::Import { path } => {
            let path = expand_home(&path);
            let result = match tokio::fs::read(&path).await {
                Ok(bytes) => client.import(bytes).await.map(|db| format!("imported database '{}'", db.name)),
                Err(e) => Err(Error(format!("can't read {path}: {e}"))),
            };
            done(result, After::Refresh)
        }
        Cmd::FindFiles { dirs } => Msg::Files(find_database_files(&dirs)),
        Cmd::Export { db, path } => {
            let path = expand_home(&path);
            let result = match client.export(db).await {
                Ok(bytes) => match tokio::fs::write(&path, &bytes).await {
                    Ok(()) => Ok(format!("exported {} bytes to {path}", group(bytes.len() as f64))),
                    Err(e) => Err(Error(format!("can't write {path}: {e}"))),
                },
                Err(e) => Err(e),
            };
            done(result, After::Refresh)
        }
    }
}

fn done(result: Result<String, Error>, then: After) -> Msg {
    Msg::Done { result, then }
}

/// `~/x` -> `$HOME/x`.
fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}

/// Where fluxoterm looks for database files to import, in order: the
/// server's `FLUXOLAP_EXAMPLES` directory, `examples/` next to the
/// fluxoterm binary (the release tarball's layout), the Docker image's
/// examples, `./examples` and `./target/examples` (a source checkout), and
/// the current directory.
pub fn default_search_dirs() -> Vec<String> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::var_os("FLUXOLAP_EXAMPLES").filter(|d| !d.is_empty()) {
        dirs.push(d.into());
    }
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        dirs.push(exe_dir.join("examples"));
    }
    dirs.push("/usr/share/fluxolap/examples".into());
    dirs.push("examples".into());
    dirs.push("target/examples".into());
    dirs.push(".".into());
    dirs.into_iter().map(|d| d.to_string_lossy().into_owned()).collect()
}

/// Every database file (`*.bin.zst`, but not a data directory's
/// `registry.bin.zst`) directly in `dirs`, each file once, sorted by name.
/// Missing directories are skipped.
pub fn find_database_files(dirs: &[String]) -> Vec<DbFile> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if !file_name.ends_with(".bin.zst") || file_name == "registry.bin.zst" || !path.is_file() {
                continue;
            }
            if !seen.insert(path.canonicalize().unwrap_or_else(|_| path.clone())) {
                continue;
            }
            let size = entry.metadata().map_or(0, |m| m.len());
            files.push(DbFile { path: path.to_string_lossy().into_owned(), name: database_name(file_name), size });
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    files
}

/// The database name a file most likely holds: its name without
/// `.bin.zst` and without the server's `db-<id>-` prefix. (The real name
/// is inside the file; this is only for display before importing.)
pub fn database_name(file_name: &str) -> String {
    let stem = file_name.strip_suffix(".bin.zst").unwrap_or(file_name);
    let unprefixed = stem.strip_prefix("db-").and_then(|rest| {
        let (id, name) = rest.split_once('-')?;
        (!id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) && !name.is_empty()).then_some(name)
    });
    unprefixed.unwrap_or(stem).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_become_database_names() {
        assert_eq!(database_name("db-3-Climate.bin.zst"), "Climate");
        assert_eq!(database_name("db-12-World_Population.bin.zst"), "World_Population");
        assert_eq!(database_name("Sales.bin.zst"), "Sales");
        assert_eq!(database_name("db-x-y.bin.zst"), "db-x-y");
    }

    #[test]
    fn finds_database_files_once_and_skips_registries() {
        let dir = std::env::temp_dir().join(format!("fluxoterm-find-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["db-0-B.bin.zst", "db-1-A.bin.zst", "registry.bin.zst", "notes.txt"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        let d = dir.to_string_lossy().into_owned();
        let found = find_database_files(&[d.clone(), d, "/no/such/dir".into()]);
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["A", "B"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
