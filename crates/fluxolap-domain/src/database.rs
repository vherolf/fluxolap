use crate::error::DomainError;
use crate::ids::DatabaseId;
use serde::{Deserialize, Serialize};
use slotmap::SlotMap;

/// Type codes for a database's `type` field (0=normal, 3=user info),
/// numbered like dimension and cube types. The system (1) and attribute
/// (2) codes don't apply to databases.
pub const DB_TYPE_NORMAL: u32 = 0;
pub const DB_TYPE_USER_INFO: u32 = 3;

/// A database's load state. A freshly created or renamed in-memory
/// database is `Changed`: it is loaded and has modifications since the
/// last save (or was just created).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
pub enum DatabaseStatus {
    Unloaded = 0,
    Loaded = 1,
    Changed = 2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Database {
    /// The client-facing sequential identifier (what appears on the wire
    /// as the `database` param/result). Distinct from the internal
    /// `DatabaseId` slotmap key: slotmap keys are generational and not
    /// meant to be exposed as small sequential integers, so this table
    /// hands out its own counter instead of leaking arena internals onto
    /// the wire.
    pub wire_id: u32,
    pub name: String,
    pub db_type: u32,
    pub status: DatabaseStatus,
    pub token: u32,
    /// Client-enforced write lock, persisted with the registry. Lives only in
    /// `Database` (not `DatabaseIdentity`), so a registry rebuilt from
    /// per-database files resets it to `false`.
    pub read_only: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DatabaseTable {
    databases: SlotMap<DatabaseId, Database>,
    next_wire_id: u32,
}

impl DatabaseTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&mut self, name: &str, db_type: u32) -> Result<DatabaseId, DomainError> {
        if name.is_empty() {
            return Err(DomainError::EmptyName);
        }
        if db_type != DB_TYPE_NORMAL && db_type != DB_TYPE_USER_INFO {
            return Err(DomainError::UnsupportedType(db_type));
        }
        if self.find_by_name(name).is_some() {
            return Err(DomainError::DuplicateName(name.to_string()));
        }

        let wire_id = self.next_wire_id;
        self.next_wire_id += 1;
        let id = self.databases.insert(Database {
            wire_id,
            name: name.to_string(),
            db_type,
            status: DatabaseStatus::Changed,
            token: 1,
            read_only: false,
        });
        Ok(id)
    }

    pub fn destroy(&mut self, id: DatabaseId) -> Result<(), DomainError> {
        self.databases.remove(id).map(|_| ()).ok_or(DomainError::NotFound)
    }

    /// Rebuild-only: inserts `db` verbatim, preserving its already-known
    /// `wire_id` rather than assigning a fresh sequential one the way
    /// `create` does, and advances `next_wire_id` past it so a later
    /// `create()` call never collides. Used only when reconstructing a
    /// lost/corrupt registry from every per-database file's own embedded
    /// `DatabaseIdentity` (`fluxolap-engine::state`'s
    /// `rebuild_registry_from_files`) -- skips `create`'s duplicate-name
    /// check, since a rebuild has no better option than trusting what
    /// each file says about itself.
    pub fn insert_with_wire_id(&mut self, db: Database) -> DatabaseId {
        self.next_wire_id = self.next_wire_id.max(db.wire_id + 1);
        self.databases.insert(db)
    }

    pub fn rename(&mut self, id: DatabaseId, new_name: &str) -> Result<(), DomainError> {
        if new_name.is_empty() {
            return Err(DomainError::EmptyName);
        }
        if self.find_by_name(new_name).is_some_and(|existing| existing != id) {
            return Err(DomainError::DuplicateName(new_name.to_string()));
        }
        let db = self.databases.get_mut(id).ok_or(DomainError::NotFound)?;
        db.name = new_name.to_string();
        db.status = DatabaseStatus::Changed;
        db.token += 1;
        Ok(())
    }

    pub fn set_read_only(&mut self, id: DatabaseId, read_only: bool) -> Result<(), DomainError> {
        let db = self.databases.get_mut(id).ok_or(DomainError::NotFound)?;
        db.read_only = read_only;
        db.token += 1;
        Ok(())
    }

    pub fn get(&self, id: DatabaseId) -> Option<&Database> {
        self.databases.get(id)
    }

    pub fn find_by_name(&self, name: &str) -> Option<DatabaseId> {
        self.databases.iter().find(|(_, d)| d.name == name).map(|(id, _)| id)
    }

    pub fn find_by_wire_id(&self, wire_id: u32) -> Option<DatabaseId> {
        self.databases.iter().find(|(_, d)| d.wire_id == wire_id).map(|(id, _)| id)
    }

    pub fn list(&self) -> impl Iterator<Item = &Database> {
        self.databases.values()
    }

    /// Every known database's id, loaded or not -- per-database
    /// load/unload needs this to drive "load/save every registered
    /// database" (e.g. `fluxolap-server`'s eager-load-everything startup,
    /// which has no idle-unload concept) without going through wire_ids.
    pub fn list_ids(&self) -> impl Iterator<Item = DatabaseId> + '_ {
        self.databases.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_assigns_sequential_wire_ids() {
        let mut t = DatabaseTable::new();
        let a = t.create("a", DB_TYPE_NORMAL).unwrap();
        let b = t.create("b", DB_TYPE_NORMAL).unwrap();
        assert_eq!(t.get(a).unwrap().wire_id, 0);
        assert_eq!(t.get(b).unwrap().wire_id, 1);
    }

    #[test]
    fn rejects_empty_name() {
        let mut t = DatabaseTable::new();
        assert!(matches!(t.create("", DB_TYPE_NORMAL), Err(DomainError::EmptyName)));
    }

    #[test]
    fn rejects_duplicate_name() {
        let mut t = DatabaseTable::new();
        t.create("dup", DB_TYPE_NORMAL).unwrap();
        assert!(matches!(t.create("dup", DB_TYPE_NORMAL), Err(DomainError::DuplicateName(_))));
    }

    #[test]
    fn rejects_unsupported_type() {
        let mut t = DatabaseTable::new();
        assert!(matches!(t.create("x", 1), Err(DomainError::UnsupportedType(1))));
        assert!(matches!(t.create("x", 99), Err(DomainError::UnsupportedType(99))));
    }

    #[test]
    fn destroyed_id_is_not_reachable_again() {
        // The direct regression guard for the arena-id-stability property
        // in PLAN.md's testing strategy: a stale id must never alias a
        // later insertion (slotmap gives this for free; test it anyway).
        let mut t = DatabaseTable::new();
        let a = t.create("a", DB_TYPE_NORMAL).unwrap();
        t.destroy(a).unwrap();
        let b = t.create("b", DB_TYPE_NORMAL).unwrap();
        assert!(t.get(a).is_none());
        assert_eq!(t.get(b).unwrap().name, "b");
    }

    #[test]
    fn rename_to_own_current_name_is_allowed() {
        let mut t = DatabaseTable::new();
        let a = t.create("a", DB_TYPE_NORMAL).unwrap();
        assert!(t.rename(a, "a").is_ok());
    }

    #[test]
    fn rename_to_another_databases_name_is_rejected() {
        let mut t = DatabaseTable::new();
        let a = t.create("a", DB_TYPE_NORMAL).unwrap();
        t.create("b", DB_TYPE_NORMAL).unwrap();
        assert!(matches!(t.rename(a, "b"), Err(DomainError::DuplicateName(_))));
    }
}
