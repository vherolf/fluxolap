use crate::error::DomainError;
use crate::ids::{CubeId, DatabaseId, DimensionId};
use serde::{Deserialize, Serialize};
use slotmap::SlotMap;

/// Type codes for a cube's `type` field (0=normal, 3=user info; 1/2 for
/// system/attribute cubes) -- same numbering as
/// `database::DB_TYPE_*`/`dimension::DIM_TYPE_*`.
pub const CUBE_TYPE_NORMAL: u32 = 0;
pub const CUBE_TYPE_USER_INFO: u32 = 3;
/// `SYSTEM`/`ATTRIBUTE`: same rationale as `dimension::DIM_TYPE_SYSTEM`/
/// `DIM_TYPE_ATTRIBUTE` -- the embedded connector client-side-provisions
/// each normal dimension's `"#_<name>_"` attribute cube itself (no server
/// auto-provisioning to rely on), using this explicit type code.
pub const CUBE_TYPE_SYSTEM: u32 = 1;
pub const CUBE_TYPE_ATTRIBUTE: u32 = 2;

/// A cube's load state (the same three values as `DatabaseStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
pub enum CubeStatus {
    Unloaded = 0,
    Loaded = 1,
    Changed = 2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cube {
    pub wire_id: u32,
    pub database_id: DatabaseId,
    pub name: String,
    /// Ordered -- a cell path's Nth coordinate is an element of
    /// `dimensions[N]`. Order is significant and part of the cube's
    /// identity (mirrors `Cube::getDimensions()`'s `IdentifiersType`,
    /// which is a vector, not a set).
    pub dimensions: Vec<DimensionId>,
    pub cube_type: u32,
    pub status: CubeStatus,
    pub token: u32,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CubeTable {
    cubes: SlotMap<CubeId, Cube>,
    next_wire_id: u32,
}

impl CubeTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&mut self, database_id: DatabaseId, name: &str, dimensions: Vec<DimensionId>, cube_type: u32) -> Result<CubeId, DomainError> {
        if name.is_empty() {
            return Err(DomainError::EmptyName);
        }
        if dimensions.is_empty() {
            return Err(DomainError::NoDimensions);
        }
        if cube_type != CUBE_TYPE_NORMAL && cube_type != CUBE_TYPE_USER_INFO && cube_type != CUBE_TYPE_SYSTEM && cube_type != CUBE_TYPE_ATTRIBUTE {
            return Err(DomainError::UnsupportedType(cube_type));
        }
        if self.find_by_name(database_id, name).is_some() {
            return Err(DomainError::DuplicateName(name.to_string()));
        }

        let wire_id = self.next_wire_id;
        self.next_wire_id += 1;
        let id = self.cubes.insert(Cube {
            wire_id,
            database_id,
            name: name.to_string(),
            dimensions,
            cube_type,
            status: CubeStatus::Changed,
            token: 1,
        });
        Ok(id)
    }

    pub fn destroy(&mut self, id: CubeId) -> Result<(), DomainError> {
        self.cubes.remove(id).map(|_| ()).ok_or(DomainError::NotFound)
    }

    pub fn rename(&mut self, id: CubeId, new_name: &str) -> Result<(), DomainError> {
        if new_name.is_empty() {
            return Err(DomainError::EmptyName);
        }
        let database_id = self.cubes.get(id).ok_or(DomainError::NotFound)?.database_id;
        if self.find_by_name(database_id, new_name).is_some_and(|existing| existing != id) {
            return Err(DomainError::DuplicateName(new_name.to_string()));
        }
        let cube = self.cubes.get_mut(id).expect("checked above");
        cube.name = new_name.to_string();
        cube.token += 1;
        Ok(())
    }

    pub fn get(&self, id: CubeId) -> Option<&Cube> {
        self.cubes.get(id)
    }

    pub fn find_by_name(&self, database_id: DatabaseId, name: &str) -> Option<CubeId> {
        self.cubes.iter().find(|(_, c)| c.database_id == database_id && c.name == name).map(|(id, _)| id)
    }

    pub fn find_by_wire_id(&self, database_id: DatabaseId, wire_id: u32) -> Option<CubeId> {
        self.cubes.iter().find(|(_, c)| c.database_id == database_id && c.wire_id == wire_id).map(|(id, _)| id)
    }

    /// Yields `(CubeId, &Cube)`, not just `&Cube` -- callers (`database/cubes`,
    /// `dimension/cubes`) need the id to look up `fluxolap-storage`'s
    /// `filled_count`, which lives in a separate lock from cube metadata.
    pub fn list_in_database(&self, database_id: DatabaseId) -> impl Iterator<Item = (CubeId, &Cube)> {
        self.cubes.iter().filter(move |(_, c)| c.database_id == database_id)
    }

    /// Rewrites every entry's `database_id` to `new_database_id`. See
    /// `DimensionTable::retarget_database_id`'s doc comment -- same
    /// reasoning, needed for the same import step.
    pub fn retarget_database_id(&mut self, new_database_id: DatabaseId) {
        for (_, cube) in self.cubes.iter_mut() {
            cube.database_id = new_database_id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::DatabaseId as DbId;
    use slotmap::SlotMap;

    fn fake_database_id() -> DbId {
        let mut sm: SlotMap<DbId, ()> = SlotMap::default();
        sm.insert(())
    }

    fn fake_dimension_ids<const N: usize>() -> [DimensionId; N] {
        let mut sm: SlotMap<DimensionId, ()> = SlotMap::default();
        std::array::from_fn(|_| sm.insert(()))
    }

    #[test]
    fn create_assigns_sequential_wire_ids() {
        let db = fake_database_id();
        let [d0] = fake_dimension_ids();
        let mut t = CubeTable::new();
        let a = t.create(db, "a", vec![d0], CUBE_TYPE_NORMAL).unwrap();
        let b = t.create(db, "b", vec![d0], CUBE_TYPE_NORMAL).unwrap();
        assert_eq!(t.get(a).unwrap().wire_id, 0);
        assert_eq!(t.get(b).unwrap().wire_id, 1);
    }

    #[test]
    fn create_accepts_system_and_attribute_types_rejects_unknown() {
        let db = fake_database_id();
        let [d0] = fake_dimension_ids();
        let mut t = CubeTable::new();
        assert!(t.create(db, "#_Regions_", vec![d0], CUBE_TYPE_ATTRIBUTE).is_ok());
        assert!(t.create(db, "#_Sys_", vec![d0], CUBE_TYPE_SYSTEM).is_ok());
        assert!(matches!(t.create(db, "Bogus", vec![d0], 99), Err(DomainError::UnsupportedType(99))));
    }

    #[test]
    fn rejects_empty_dimension_list() {
        let db = fake_database_id();
        let mut t = CubeTable::new();
        assert!(matches!(t.create(db, "a", vec![], CUBE_TYPE_NORMAL), Err(DomainError::NoDimensions)));
    }

    #[test]
    fn rejects_duplicate_name_in_same_database() {
        let db = fake_database_id();
        let [d0] = fake_dimension_ids();
        let mut t = CubeTable::new();
        t.create(db, "Sales", vec![d0], CUBE_TYPE_NORMAL).unwrap();
        assert!(matches!(t.create(db, "Sales", vec![d0], CUBE_TYPE_NORMAL), Err(DomainError::DuplicateName(_))));
    }

    #[test]
    fn rename_to_own_current_name_is_allowed() {
        let db = fake_database_id();
        let [d0] = fake_dimension_ids();
        let mut t = CubeTable::new();
        let a = t.create(db, "Sales", vec![d0], CUBE_TYPE_NORMAL).unwrap();
        assert!(t.rename(a, "Sales").is_ok());
    }

    #[test]
    fn rename_to_another_cubes_name_in_same_database_is_rejected() {
        let db = fake_database_id();
        let [d0] = fake_dimension_ids();
        let mut t = CubeTable::new();
        let a = t.create(db, "Sales", vec![d0], CUBE_TYPE_NORMAL).unwrap();
        t.create(db, "Costs", vec![d0], CUBE_TYPE_NORMAL).unwrap();
        assert!(matches!(t.rename(a, "Costs"), Err(DomainError::DuplicateName(_))));
    }

    #[test]
    fn preserves_dimension_order() {
        let db = fake_database_id();
        let [d0, d1, d2] = fake_dimension_ids();
        let mut t = CubeTable::new();
        let id = t.create(db, "Sales", vec![d2, d0, d1], CUBE_TYPE_NORMAL).unwrap();
        assert_eq!(t.get(id).unwrap().dimensions, vec![d2, d0, d1]);
    }

    #[test]
    fn destroyed_id_is_not_reachable_again() {
        let db = fake_database_id();
        let [d0] = fake_dimension_ids();
        let mut t = CubeTable::new();
        let a = t.create(db, "a", vec![d0], CUBE_TYPE_NORMAL).unwrap();
        t.destroy(a).unwrap();
        let b = t.create(db, "b", vec![d0], CUBE_TYPE_NORMAL).unwrap();
        assert!(t.get(a).is_none());
        assert_eq!(t.get(b).unwrap().name, "b");
    }
}
