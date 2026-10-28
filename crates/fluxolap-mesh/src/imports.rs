//! What this node has imported from friends: for each database the friend's
//! `modified` as it was announced when the copy was made. A friend's database
//! is newer when its announced `modified` is higher than this one, so the
//! clocks of the nodes never have to agree. Kept in a file (if the node has a
//! data directory) so a restart doesn't import everything again.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub friend: String,
    /// The database's name at the friend.
    pub database: String,
    /// The name of the copy here.
    pub local_name: String,
    pub remote_modified: Option<u64>,
    pub imported_at: u64,
}

#[derive(Debug, Default)]
pub struct ImportLog {
    records: Vec<Record>,
    path: Option<PathBuf>,
}

impl ImportLog {
    /// Reads `path` if it exists; an unreadable file starts an empty log (the
    /// databases then count as not imported and are fetched again).
    pub fn load(path: Option<PathBuf>) -> Self {
        let records = path
            .as_ref()
            .and_then(|p| match std::fs::read_to_string(p) {
                Ok(text) => serde_json::from_str(&text).map_err(|e| tracing::warn!("mesh: {} is unreadable, starting empty: {e}", p.display())).ok(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    tracing::warn!("mesh: can't read {}: {e}", p.display());
                    None
                }
            })
            .unwrap_or_default();
        Self { records, path }
    }

    pub fn get(&self, friend: &str, database: &str) -> Option<&Record> {
        self.records.iter().find(|r| r.friend == friend && r.database == database)
    }

    pub fn local_names(&self) -> HashSet<String> {
        self.records.iter().map(|r| r.local_name.clone()).collect()
    }

    pub fn put(&mut self, record: Record) {
        self.records.retain(|r| !(r.friend == record.friend && r.database == record.database));
        self.records.push(record);
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let write = || -> std::io::Result<()> {
            let temp = path.with_extension("json.tmp");
            std::fs::write(&temp, serde_json::to_vec_pretty(&self.records).expect("records serialize"))?;
            std::fs::rename(temp, path)
        };
        if let Err(e) = write() {
            tracing::warn!("mesh: can't write {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(friend: &str, modified: u64) -> Record {
        Record { friend: friend.into(), database: "Sales".into(), local_name: format!("{friend}/Sales"), remote_modified: Some(modified), imported_at: 1 }
    }

    #[test]
    fn a_record_is_replaced_not_duplicated_and_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("fluxolap-imports-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mesh-imports.json");
        let mut log = ImportLog::load(Some(path.clone()));
        assert!(log.get("a", "Sales").is_none());
        log.put(record("a", 10));
        log.put(record("a", 20));
        log.put(record("b", 5));
        let log = ImportLog::load(Some(path));
        assert_eq!(log.get("a", "Sales").unwrap().remote_modified, Some(20));
        assert_eq!(log.local_names(), HashSet::from(["a/Sales".to_string(), "b/Sales".to_string()]));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
