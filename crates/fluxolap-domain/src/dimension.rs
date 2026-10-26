use crate::error::DomainError;
use crate::ids::{DatabaseId, DimensionId, ElementId};
use serde::{Deserialize, Serialize};
use slotmap::SlotMap;
use std::collections::{HashMap, HashSet};

/// Type codes for a dimension's `type` field (0=normal, 3=user info) --
/// the same numbering as `database::DB_TYPE_*`.
pub const DIM_TYPE_NORMAL: u32 = 0;
pub const DIM_TYPE_USER_INFO: u32 = 3;
/// `SYSTEM`/`ATTRIBUTE`: unlike `NORMAL`/`USER_INFO`, these describe the
/// engine's own auto-created entities (the database-wide `"#_DIMENSION_"`
/// system dimension, and each normal dimension's own `"#_<name>_"`
/// attribute dimension). They can also be created directly: an embedding
/// application can create these entities itself, client-side, using these
/// explicit type codes rather than creating them as `NORMAL` and losing the
/// distinction on the next cold reload.
pub const DIM_TYPE_SYSTEM: u32 = 1;
pub const DIM_TYPE_ATTRIBUTE: u32 = 2;

/// An element's type. `Consolidated` elements can't be created directly
/// via `Dimension::create_element` (NUMERIC/STRING leaves only) -- an
/// element becomes `Consolidated` automatically when it's given children
/// via `set_children`/`add_children`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
pub enum ElementType {
    Numeric = 1,
    String = 2,
    Consolidated = 4,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Element {
    pub wire_id: u32,
    pub name: String,
    /// Position within the dimension's overall element ordering. Mirrors
    /// `Element::getPosition()`. Starts as insertion order; `move_element`
    /// can reorder it.
    pub position: u32,
    pub elem_type: ElementType,
}

/// Hierarchy edges, kept out of `Element` itself on purpose -- see
/// PLAN.md's Architecture section (hierarchy out of the element struct is
/// good for cache locality and mutation isolation). `children`/`parents`
/// are kept as mutual inverses by construction: every mutation method
/// updates both sides together, so there's no path that can leave them
/// out of sync ("the inverse index must always agree," enforced by only
/// ever touching both sides in the same method).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Relations {
    children: HashMap<ElementId, Vec<(ElementId, f64)>>,
    parents: HashMap<ElementId, Vec<ElementId>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dimension {
    pub wire_id: u32,
    pub database_id: DatabaseId,
    pub name: String,
    pub dim_type: u32,
    pub token: u32,
    elements: SlotMap<ElementId, Element>,
    next_element_wire_id: u32,
    relations: Relations,
    /// wire_id -> `ElementId`, maintained incrementally by `create_element`/
    /// `destroy_element` (the only two places `elements` gains or loses an
    /// entry). Without this, `find_element_by_wire_id` -- called once per
    /// dimension per cell by an embedding application's bulk cell-write
    /// path -- was `elements.iter().find(...)`, an O(n) scan measured (via
    /// `xtask writebench`) at ~0.28 ns per element scanned: negligible for
    /// a small dimension, but ~50-55 us/lookup extrapolated to a real
    /// ~200,000-element dimension -- paid on *every* cell write touching
    /// that dimension, and the dominant cost behind a real bulk load
    /// running ~36% slower than it should have. `#[serde(skip)]`
    /// like `fluxolap-storage`'s `HotTier::index` (same reasoning: cheap
    /// to rebuild from `elements` after deserializing, not worth
    /// persisting) -- rebuilt via `rebuild_wire_id_index`, called from
    /// `fluxolap-engine::state`'s `LoadedDatabase::from_snapshot`.
    #[serde(skip)]
    by_wire_id: HashMap<u32, ElementId>,
    /// name -> `ElementId`, the same kind of secondary index as
    /// `by_wire_id` (maintained by `create_element`/`destroy_element`/
    /// `rename_element`, rebuilt by `rebuild_wire_id_index`). Without it
    /// `find_element_by_name` was an O(n) scan, and every `create_element`
    /// calls it for its uniqueness check -- so loading n elements was
    /// O(n^2): 100,000 elements took ~40 s through the REST API's batch
    /// create.
    #[serde(skip)]
    by_name: HashMap<String, ElementId>,
    /// Unix seconds of the last change to this dimension (its creation, a
    /// rename, elements created, destroyed, renamed, moved or retyped,
    /// consolidations changed -- every change that bumps `token`), `0` if
    /// none is known. Not part of the serialized form, so that files written
    /// before it existed still load: a per-database file carries these times
    /// in a trailing section of its own (`fluxolap-engine`'s `state.rs`), as
    /// it does the cubes' (`fluxolap-storage`'s `CubePartition::modified`).
    #[serde(skip)]
    modified: u64,
}

impl Dimension {
    pub fn element_count(&self) -> usize {
        self.elements.len()
    }

    /// See the field. Public for persistence, which stores the time beside the
    /// dimension.
    pub fn modified(&self) -> u64 {
        self.modified
    }

    pub fn set_modified(&mut self, unix_seconds: u64) {
        self.modified = unix_seconds;
    }

    /// A change: a new `token`, and now as the modification time.
    fn changed(&mut self) {
        self.token += 1;
        self.modified = crate::unix_timestamp();
    }

    pub fn create_element(&mut self, name: &str, elem_type_raw: u32) -> Result<ElementId, DomainError> {
        if name.is_empty() {
            return Err(DomainError::EmptyName);
        }
        let elem_type = match elem_type_raw {
            1 => ElementType::Numeric,
            2 => ElementType::String,
            other => return Err(DomainError::UnsupportedType(other)),
        };
        if self.find_element_by_name(name).is_some() {
            return Err(DomainError::DuplicateName(name.to_string()));
        }

        let wire_id = self.next_element_wire_id;
        self.next_element_wire_id += 1;
        let position = self.elements.len() as u32;
        let id = self.elements.insert(Element { wire_id, name: name.to_string(), position, elem_type });
        self.by_wire_id.insert(wire_id, id);
        self.by_name.insert(name.to_string(), id);
        self.changed();
        Ok(id)
    }

    /// Like `create_element`, but with a caller-chosen wire id instead of
    /// the next sequential one -- for migrating a database from another
    /// OLAP backend while keeping every element's id, since ids
    /// leak out of the engine (e.g. into relational drillthrough tables).
    /// Fails with `DuplicateWireId` if `wire_id` is already taken in this
    /// dimension. Advances `next_element_wire_id` past it (never backwards)
    /// so later plain `create_element` calls can't collide -- gaps in the
    /// id space are fine, ids are never required to be dense.
    pub fn create_element_with_wire_id(&mut self, name: &str, elem_type_raw: u32, wire_id: u32) -> Result<ElementId, DomainError> {
        // Checked before create_element so a collision leaves no side effects.
        // The name/type validation there runs first in practice for the
        // same-name case, which is the more useful error to surface.
        if name.is_empty() {
            return Err(DomainError::EmptyName);
        }
        if self.find_element_by_name(name).is_none() && self.find_element_by_wire_id(wire_id).is_some() {
            return Err(DomainError::DuplicateWireId(wire_id));
        }
        let reserved = self.next_element_wire_id;
        self.next_element_wire_id = wire_id;
        let result = self.create_element(name, elem_type_raw);
        // create_element bumped it to wire_id + 1 on success; on failure restore.
        self.next_element_wire_id = if result.is_ok() { reserved.max(wire_id.saturating_add(1)) } else { reserved };
        result
    }

    /// Changes an existing leaf element's type between NUMERIC and STRING
    /// in place -- the "or simply change type of existing elements" half
    /// of the Java client's `IDimension.appendElements` contract (the "add
    /// new elements" half is just `create_element`; callers dispatch to one
    /// or the other after a name lookup, as the connector's FFI layer
    /// does).
    ///
    /// `new_type_raw` is the type bucket (1 = numeric, 2 = string) --
    /// deliberately *not* a 3-way `ElementType`, because a client's loader
    /// folds `NUMERIC` and `CONSOLIDATED` into the same "1" bucket
    /// (`IDimension.appendElements`: "add new elements ... or simply change
    /// type"). A dimension reload
    /// legitimately re-sends every element with its *pre-hierarchy* source
    /// type, including `CONSOLIDATED` placeholders for parents whose real
    /// children get attached by a later, separate `updateConsolidations`
    /// call (`ElementLoad` then `ConsolidationLoad`, in that order) -- so
    /// requesting bucket "1" against an element that's *already*
    /// `Consolidated` must be a harmless no-op (this is exactly what makes
    /// a dimension reload idempotent), not an error. Only a genuine bucket
    /// change away from `Consolidated` (bucket "2"/string) is rejected:
    /// turning a hierarchy parent into a string leaf while it still has
    /// children isn't a supported operation, so that would silently
    /// orphan them rather than being a supported request.
    pub fn change_element_type(&mut self, id: ElementId, new_type_raw: u32) -> Result<(), DomainError> {
        let new_type = match new_type_raw {
            1 => ElementType::Numeric,
            2 => ElementType::String,
            other => return Err(DomainError::UnsupportedType(other)),
        };
        let el = self.elements.get(id).ok_or(DomainError::NotFound)?;
        if el.elem_type == ElementType::Consolidated {
            if new_type == ElementType::Numeric {
                return Ok(());
            }
            return Err(DomainError::UnsupportedType(ElementType::Consolidated as u32));
        }
        self.elements.get_mut(id).expect("just found by the same id").elem_type = new_type;
        self.changed();
        Ok(())
    }

    /// Removes every element matching `type_filter` (or every element if
    /// `None`), unlinking each from the hierarchy exactly like
    /// `destroy_element` -- clearing a dimension, with an optional type
    /// filter to clear only one element type at a time. Leaves gaps in surviving elements'
    /// `position` values, same as destroying one element at a time via
    /// destroying one element already does -- no new inconsistency introduced.
    pub fn clear(&mut self, type_filter: Option<u32>) {
        let to_remove: Vec<ElementId> = self
            .elements
            .iter()
            .filter(|(_, el)| type_filter.is_none_or(|t| el.elem_type as u32 == t))
            .map(|(id, _)| id)
            .collect();
        self.destroy_elements(&to_remove).expect("ids were just collected from this dimension's own arena");
    }

    pub fn destroy_element(&mut self, id: ElementId) -> Result<(), DomainError> {
        self.destroy_elements(&[id])
    }

    /// Destroys many elements at once; `NotFound` (destroying nothing) if
    /// any id doesn't exist. Unlinks them from the hierarchy on both sides
    /// -- an element can be simultaneously a parent (has children) and a
    /// child (has parents); both edge sets must be cleaned up so no
    /// dangling reference to a destroyed id can ever be read back. Each
    /// surviving neighbour's edge list is filtered once for the whole
    /// batch: filtering it once per destroyed element made deleting many
    /// children of one big consolidation O(n^2). A parent left without
    /// children stops being `Consolidated` (`promote_to_consolidated`).
    pub fn destroy_elements(&mut self, ids: &[ElementId]) -> Result<(), DomainError> {
        if ids.iter().any(|&id| !self.elements.contains_key(id)) {
            return Err(DomainError::NotFound);
        }
        let doomed: HashSet<ElementId> = ids.iter().copied().collect();
        let mut children_to_fix: HashSet<ElementId> = HashSet::new();
        let mut parents_to_fix: HashSet<ElementId> = HashSet::new();
        for &id in &doomed {
            let removed = self.elements.remove(id).expect("checked above");
            self.by_wire_id.remove(&removed.wire_id);
            self.by_name.remove(&removed.name);
            if let Some(children) = self.relations.children.remove(&id) {
                children_to_fix.extend(children.into_iter().map(|(c, _)| c));
            }
            if let Some(parents) = self.relations.parents.remove(&id) {
                parents_to_fix.extend(parents);
            }
        }
        for child in children_to_fix.difference(&doomed) {
            if let Some(plist) = self.relations.parents.get_mut(child) {
                plist.retain(|p| !doomed.contains(p));
            }
        }
        for parent in parents_to_fix.difference(&doomed) {
            if let Some(clist) = self.relations.children.get_mut(parent) {
                clist.retain(|(c, _)| !doomed.contains(c));
            }
            self.promote_to_consolidated(*parent);
        }
        self.changed();
        Ok(())
    }

    pub fn rename_element(&mut self, id: ElementId, new_name: &str) -> Result<(), DomainError> {
        if new_name.is_empty() {
            return Err(DomainError::EmptyName);
        }
        if self.find_element_by_name(new_name).is_some_and(|existing| existing != id) {
            return Err(DomainError::DuplicateName(new_name.to_string()));
        }
        let el = self.elements.get_mut(id).ok_or(DomainError::NotFound)?;
        self.by_name.remove(&el.name);
        el.name = new_name.to_string();
        self.by_name.insert(new_name.to_string(), id);
        self.changed();
        Ok(())
    }

    pub fn get_element(&self, id: ElementId) -> Option<&Element> {
        self.elements.get(id)
    }

    pub fn find_element_by_name(&self, name: &str) -> Option<ElementId> {
        self.by_name.get(name).copied()
    }

    pub fn find_element_by_wire_id(&self, wire_id: u32) -> Option<ElementId> {
        self.by_wire_id.get(&wire_id).copied()
    }

    /// Rebuilds `by_wire_id` and `by_name` from `elements` -- needed after deserializing
    /// a `Dimension` (`#[serde(skip)]`'d, comes back empty via `Default`).
    /// See that field's doc comment. `pub(crate)` rather than fully
    /// private so `DimensionTable::rebuild_wire_id_indices` (its own
    /// caller, in this same crate) can reach it.
    pub(crate) fn rebuild_wire_id_index(&mut self) {
        self.by_wire_id = self.elements.iter().map(|(id, e)| (e.wire_id, id)).collect();
        self.by_name = self.elements.iter().map(|(id, e)| (e.name.clone(), id)).collect();
    }

    pub fn list_elements(&self) -> impl Iterator<Item = (ElementId, &Element)> {
        self.elements.iter()
    }

    // -- Hierarchy -----------------------------------------------------------

    pub fn children_of(&self, id: ElementId) -> &[(ElementId, f64)] {
        self.relations.children.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn parents_of(&self, id: ElementId) -> &[ElementId] {
        self.relations.parents.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Height above the leaves: 0 for a leaf, else `1 + max(level of each
    /// child)`. Matches the standard OLAP "level" convention (leaves are
    /// level 0). Recursive with no memoization -- fine at the usual scale
    /// (hierarchies are small, single-digit-thousands of elements at
    /// most); revisit if this ever shows up in a profile.
    pub fn level(&self, id: ElementId) -> u32 {
        let children = self.children_of(id);
        if children.is_empty() {
            0
        } else {
            1 + children.iter().map(|&(c, _)| self.level(c)).max().unwrap_or(0)
        }
    }

    /// Indent is reported identically to `level` for now (a distinct
    /// indent would likely follow consolidation-tree display order for
    /// elements with multiple parents) -- a documented simplification.
    /// Revisit if a client actually depends on indent differing from level.
    pub fn indent(&self, id: ElementId) -> u32 {
        self.level(id)
    }

    /// Same caveat as `indent`: reported identically to `level`. "Depth" is
    /// ill-defined for a plain height computation once an element can have
    /// multiple parents (a DAG, not a tree). Documented simplification.
    pub fn depth(&self, id: ElementId) -> u32 {
        self.level(id)
    }

    /// True if `target` is reachable by walking down zero-or-more steps
    /// from `start`'s children (i.e. `start` is `target`, or an ancestor
    /// of it). Used by `validate_children` to reject edges that would
    /// close a cycle: proposing `child` as a child of `parent` is only
    /// safe if `child` is not already an ancestor of `parent`.
    fn reaches(&self, start: ElementId, target: ElementId) -> bool {
        if start == target {
            return true;
        }
        let mut stack: Vec<ElementId> = self.children_of(start).iter().map(|&(c, _)| c).collect();
        let mut seen = std::collections::HashSet::new();
        while let Some(cur) = stack.pop() {
            if cur == target {
                return true;
            }
            if !seen.insert(cur) {
                continue;
            }
            stack.extend(self.children_of(cur).iter().map(|&(c, _)| c));
        }
        false
    }

    fn validate_children(&self, parent: ElementId, new_children: &[(ElementId, f64)]) -> Result<(), DomainError> {
        for &(child, _) in new_children {
            if !self.elements.contains_key(child) {
                return Err(DomainError::NotFound);
            }
            // child == parent is caught by `reaches` too (start == target
            // returns true trivially) but is spelled out here since it's
            // the most common/obvious cycle case.
            if child == parent || self.reaches(child, parent) {
                return Err(DomainError::Cycle);
            }
        }
        Ok(())
    }

    /// Upserts `(child, weight)` edges under `parent`: existing children
    /// keep their position in the list with an updated weight, new ones
    /// are appended. Also maintains the `parents` inverse index. Caller
    /// must have already validated via `validate_children`.
    fn upsert_edges(&mut self, parent: ElementId, new_children: &[(ElementId, f64)]) {
        let list = self.relations.children.entry(parent).or_default();
        // Position of each existing child in `list` -- a linear `find` per
        // new child made consolidating n children O(n^2).
        let mut index: HashMap<ElementId, usize> = list.iter().enumerate().map(|(i, &(c, _))| (c, i)).collect();
        for &(child, weight) in new_children {
            if let Some(&i) = index.get(&child) {
                list[i].1 = weight;
            } else {
                index.insert(child, list.len());
                list.push((child, weight));
                self.relations.parents.entry(child).or_default().push(parent);
            }
        }
    }

    /// Keeps `elem_type` in sync with whether `id` currently has any
    /// children -- both directions. Promotes to `Consolidated` when it
    /// gains its first child (see `ElementType`'s doc comment). Demotes back to `Numeric` when
    /// `set_children(id, &[])` (or `clear_all_children`, its bulk
    /// caller) removes its last child -- this half was previously
    /// missing entirely: this function only ever promoted, never
    /// demoted, so an element that had every child removed stayed
    /// permanently stuck reporting `ElementType::Consolidated` even
    /// though `children_of(id)` was correctly empty. Found via a real
    /// ETL job run that hit stale-looking `"is consolidated"` cell-write rejections for
    /// elements that should have been plain leaves.
    fn promote_to_consolidated(&mut self, id: ElementId) {
        let has_children = !self.children_of(id).is_empty();
        if let Some(el) = self.elements.get_mut(id) {
            if has_children {
                el.elem_type = ElementType::Consolidated;
            } else if el.elem_type == ElementType::Consolidated {
                el.elem_type = ElementType::Numeric;
            }
        }
    }

    /// Append semantics: merge `new_children` into `parent`'s
    /// *existing* children (upsert by id; children not mentioned are left
    /// alone). Weights default to 1.0 for any pair without an explicit
    /// weight -- caller is responsible for the "weights shorter than
    /// children" validation (`ErrorException::ERROR_PARAMETER_MISSING`,
    /// "missing weight") before calling this, since that's a wire-level
    /// concern, not a domain one.
    pub fn add_children(&mut self, parent: ElementId, new_children: &[(ElementId, f64)]) -> Result<(), DomainError> {
        if !self.elements.contains_key(parent) {
            return Err(DomainError::NotFound);
        }
        self.validate_children(parent, new_children)?;
        self.upsert_edges(parent, new_children);
        self.promote_to_consolidated(parent);
        self.changed();
        Ok(())
    }

    /// Replace semantics: `parent`'s children become *exactly*
    /// `new_children` -- anything not in the new list is unlinked.
    pub fn set_children(&mut self, parent: ElementId, new_children: &[(ElementId, f64)]) -> Result<(), DomainError> {
        if !self.elements.contains_key(parent) {
            return Err(DomainError::NotFound);
        }
        self.validate_children(parent, new_children)?;

        if let Some(old) = self.relations.children.remove(&parent) {
            for (child, _) in old {
                if let Some(plist) = self.relations.parents.get_mut(&child) {
                    plist.retain(|&p| p != parent);
                }
            }
        }
        self.upsert_edges(parent, new_children);
        self.promote_to_consolidated(parent);
        self.changed();
        Ok(())
    }

    /// Move: reposition `id` to `new_position`, shifting every
    /// element between the old and new position by one slot to keep
    /// positions a dense `0..element_count()` permutation. Out-of-range
    /// positions clamp to the last valid slot (clamping rather than
    /// rejecting, the common convention for OLAP element moves).
    pub fn move_element(&mut self, id: ElementId, new_position: u32) -> Result<(), DomainError> {
        let old_position = self.elements.get(id).ok_or(DomainError::NotFound)?.position;
        let max_position = self.elements.len().saturating_sub(1) as u32;
        let new_position = new_position.min(max_position);

        if new_position == old_position {
            return Ok(());
        }
        if new_position < old_position {
            for el in self.elements.values_mut() {
                if el.position >= new_position && el.position < old_position {
                    el.position += 1;
                }
            }
        } else {
            for el in self.elements.values_mut() {
                if el.position > old_position && el.position <= new_position {
                    el.position -= 1;
                }
            }
        }
        self.elements.get_mut(id).unwrap().position = new_position;
        self.changed();
        Ok(())
    }

    pub fn find_element_by_position(&self, position: u32) -> Option<ElementId> {
        self.elements.iter().find(|(_, e)| e.position == position).map(|(id, _)| id)
    }

    /// Expands a (possibly CONSOLIDATED) element down to its base
    /// (leaf) elements, multiplying weights along each path. A leaf
    /// expands to itself with weight 1.0. This is the core hierarchy
    /// traversal both a cell read's aggregation and
    /// a cell write's splashing build on -- see
    /// `expand_cube_path` in `fluxolap-engine` for how a single
    /// dimension's expansion becomes a full cube coordinate's expansion
    /// (cartesian product across dimensions).
    pub fn expand_to_base(&self, id: ElementId) -> Vec<(ElementId, f64)> {
        let children = self.children_of(id);
        if children.is_empty() {
            return vec![(id, 1.0)];
        }
        let mut result = Vec::new();
        for &(child, weight) in children {
            for (leaf, w) in self.expand_to_base(child) {
                result.push((leaf, weight * w));
            }
        }
        result
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DimensionTable {
    dimensions: SlotMap<DimensionId, Dimension>,
    next_wire_id: u32,
}

impl DimensionTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuilds every dimension's `by_wire_id` index -- see that field's
    /// doc comment. Called once, right after deserializing a
    /// `DatabaseSnapshot`, by `fluxolap-engine::state`'s
    /// `LoadedDatabase::from_snapshot`.
    pub fn rebuild_wire_id_indices(&mut self) {
        for dim in self.dimensions.values_mut() {
            dim.rebuild_wire_id_index();
        }
    }

    pub fn create(&mut self, database_id: DatabaseId, name: &str, dim_type: u32) -> Result<DimensionId, DomainError> {
        if name.is_empty() {
            return Err(DomainError::EmptyName);
        }
        if dim_type != DIM_TYPE_NORMAL && dim_type != DIM_TYPE_USER_INFO && dim_type != DIM_TYPE_SYSTEM && dim_type != DIM_TYPE_ATTRIBUTE {
            return Err(DomainError::UnsupportedType(dim_type));
        }
        if self.find_by_name(database_id, name).is_some() {
            return Err(DomainError::DuplicateName(name.to_string()));
        }

        let wire_id = self.next_wire_id;
        self.next_wire_id += 1;
        let id = self.dimensions.insert(Dimension {
            wire_id,
            database_id,
            name: name.to_string(),
            dim_type,
            token: 1,
            elements: SlotMap::default(),
            next_element_wire_id: 0,
            relations: Relations::default(),
            by_wire_id: HashMap::new(),
            by_name: HashMap::new(),
            modified: crate::unix_timestamp(),
        });
        Ok(id)
    }

    pub fn destroy(&mut self, id: DimensionId) -> Result<(), DomainError> {
        self.dimensions.remove(id).map(|_| ()).ok_or(DomainError::NotFound)
    }

    pub fn rename(&mut self, id: DimensionId, new_name: &str) -> Result<(), DomainError> {
        if new_name.is_empty() {
            return Err(DomainError::EmptyName);
        }
        let database_id = self.dimensions.get(id).ok_or(DomainError::NotFound)?.database_id;
        if self.find_by_name(database_id, new_name).is_some_and(|existing| existing != id) {
            return Err(DomainError::DuplicateName(new_name.to_string()));
        }
        let dim = self.dimensions.get_mut(id).expect("checked above");
        dim.name = new_name.to_string();
        dim.changed();
        Ok(())
    }

    pub fn get(&self, id: DimensionId) -> Option<&Dimension> {
        self.dimensions.get(id)
    }

    pub fn get_mut(&mut self, id: DimensionId) -> Option<&mut Dimension> {
        self.dimensions.get_mut(id)
    }

    /// Every dimension with its id, of all databases in this table.
    pub fn iter(&self) -> impl Iterator<Item = (DimensionId, &Dimension)> {
        self.dimensions.iter()
    }

    pub fn find_by_name(&self, database_id: DatabaseId, name: &str) -> Option<DimensionId> {
        self.dimensions.iter().find(|(_, d)| d.database_id == database_id && d.name == name).map(|(id, _)| id)
    }

    pub fn find_by_wire_id(&self, database_id: DatabaseId, wire_id: u32) -> Option<DimensionId> {
        self.dimensions
            .iter()
            .find(|(_, d)| d.database_id == database_id && d.wire_id == wire_id)
            .map(|(id, _)| id)
    }

    pub fn list_in_database(&self, database_id: DatabaseId) -> impl Iterator<Item = &Dimension> {
        self.dimensions.values().filter(move |d| d.database_id == database_id)
    }

    /// Rewrites every entry's `database_id` to `new_database_id`. A
    /// `DatabaseSnapshot` imported from a file (`IConnection.restoreDatabase`)
    /// still carries whatever `DatabaseId` its dimensions were created
    /// under when exported -- a fresh internal id distinct from that one is
    /// always assigned on import (the registry hands out a new slotmap key
    /// every time, same as any other newly created database), so every
    /// `find_by_name`/`find_by_wire_id`/`list_in_database` call (all
    /// filtered by `database_id`) would otherwise find nothing at all in
    /// the restored database. Called once, right after loading the
    /// snapshot, before it's installed as a `LoadedDatabase`.
    pub fn retarget_database_id(&mut self, new_database_id: DatabaseId) {
        for (_, dim) in self.dimensions.iter_mut() {
            dim.database_id = new_database_id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::DatabaseId as DbId;
    use slotmap::SlotMap;

    fn fake_database_id() -> DbId {
        fake_database_ids::<1>()[0]
    }

    /// Throwaway DatabaseIds for tests that don't need a real
    /// DatabaseTable -- any valid slotmap keys work since DimensionTable
    /// never dereferences them, only compares them. Must come from the
    /// SAME SlotMap: two independent `SlotMap::default()` instances each
    /// mint their own "first" key starting from the same initial state,
    /// so keys from different maps can silently collide (caught by
    /// `dimension_names_are_unique_per_database_not_globally` initially
    /// failing because db_a == db_b this way).
    fn fake_database_ids<const N: usize>() -> [DbId; N] {
        let mut sm: SlotMap<DbId, ()> = SlotMap::default();
        std::array::from_fn(|_| sm.insert(()))
    }

    fn new_dim() -> Dimension {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        let id = t.create(db, "Regions", DIM_TYPE_NORMAL).unwrap();
        t.get(id).unwrap().clone()
    }

    #[test]
    fn every_change_sets_the_modification_time_and_a_lookup_does_not() {
        let mut d = new_dim();
        assert_ne!(d.modified(), 0, "creating a dimension is a change");
        let changes: [fn(&mut Dimension); 5] = [
            |d| {
                d.create_element("North", 1).unwrap();
            },
            |d| d.rename_element(d.find_element_by_name("North").unwrap(), "Nord").unwrap(),
            |d| {
                let parent = d.create_element("Total", 1).unwrap();
                let child = d.find_element_by_name("Nord").unwrap();
                d.set_children(parent, &[(child, 1.0)]).unwrap();
            },
            |d| d.move_element(d.find_element_by_name("Total").unwrap(), 0).unwrap(),
            |d| d.destroy_element(d.find_element_by_name("Nord").unwrap()).unwrap(),
        ];
        for change in changes {
            d.set_modified(1);
            change(&mut d);
            assert!(d.modified() > 1);
        }
        d.set_modified(1);
        let _ = d.find_element_by_name("Total");
        let _ = d.expand_to_base(d.find_element_by_name("Total").unwrap());
        assert_eq!(d.modified(), 1);

        let mut t = DimensionTable::new();
        let id = t.create(fake_database_id(), "Regions", DIM_TYPE_NORMAL).unwrap();
        t.get_mut(id).unwrap().set_modified(1);
        t.rename(id, "Regionen").unwrap();
        assert!(t.get(id).unwrap().modified() > 1, "renaming the dimension is a change");
    }

    #[test]
    fn create_element_with_wire_id_keeps_requested_id_and_advances_counter() {
        let mut dim = new_dim();
        let a = dim.create_element_with_wire_id("a", 1, 40).unwrap();
        let b = dim.create_element_with_wire_id("b", 1, 7).unwrap(); // lower than a: order of creation != id order
        assert_eq!(dim.get_element(a).unwrap().wire_id, 40);
        assert_eq!(dim.get_element(b).unwrap().wire_id, 7);
        // counter must not move backwards after the lower id...
        let c = dim.create_element("c", 1).unwrap();
        assert_eq!(dim.get_element(c).unwrap().wire_id, 41);
        // ...and positions still follow creation order
        assert_eq!(dim.get_element(a).unwrap().position, 0);
        assert_eq!(dim.get_element(b).unwrap().position, 1);
    }

    #[test]
    fn create_element_with_wire_id_rejects_taken_id_without_side_effects() {
        let mut dim = new_dim();
        dim.create_element_with_wire_id("a", 1, 5).unwrap();
        assert!(matches!(dim.create_element_with_wire_id("b", 1, 5), Err(DomainError::DuplicateWireId(5))));
        assert!(dim.find_element_by_name("b").is_none());
        assert!(matches!(dim.create_element_with_wire_id("a", 1, 9), Err(DomainError::DuplicateName(_))));
        // failed calls left the counter alone
        let c = dim.create_element("c", 1).unwrap();
        assert_eq!(dim.get_element(c).unwrap().wire_id, 6);
    }

    #[test]
    fn create_dimension_assigns_sequential_wire_ids() {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        let a = t.create(db, "a", DIM_TYPE_NORMAL).unwrap();
        let b = t.create(db, "b", DIM_TYPE_NORMAL).unwrap();
        assert_eq!(t.get(a).unwrap().wire_id, 0);
        assert_eq!(t.get(b).unwrap().wire_id, 1);
    }

    #[test]
    fn create_accepts_system_and_attribute_types_rejects_unknown() {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        assert!(t.create(db, "#_DIMENSION_", DIM_TYPE_SYSTEM).is_ok());
        assert!(t.create(db, "#_Regions_", DIM_TYPE_ATTRIBUTE).is_ok());
        assert!(matches!(t.create(db, "Bogus", 99), Err(DomainError::UnsupportedType(99))));
    }

    #[test]
    fn dimension_names_are_unique_per_database_not_globally() {
        let [db_a, db_b] = fake_database_ids();
        let mut t = DimensionTable::new();
        assert!(t.create(db_a, "Regions", DIM_TYPE_NORMAL).is_ok());
        // Same name, different database -- must be allowed.
        assert!(t.create(db_b, "Regions", DIM_TYPE_NORMAL).is_ok());
        // Same name, same database -- must be rejected.
        assert!(matches!(t.create(db_a, "Regions", DIM_TYPE_NORMAL), Err(DomainError::DuplicateName(_))));
    }

    #[test]
    fn rename_to_own_current_name_is_allowed() {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        let a = t.create(db, "Regions", DIM_TYPE_NORMAL).unwrap();
        assert!(t.rename(a, "Regions").is_ok());
    }

    #[test]
    fn rename_to_another_dimensions_name_in_same_database_is_rejected() {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        let a = t.create(db, "Regions", DIM_TYPE_NORMAL).unwrap();
        t.create(db, "Months", DIM_TYPE_NORMAL).unwrap();
        assert!(matches!(t.rename(a, "Months"), Err(DomainError::DuplicateName(_))));
    }

    #[test]
    fn rename_to_a_name_used_in_another_database_is_allowed() {
        let [db_a, db_b] = fake_database_ids();
        let mut t = DimensionTable::new();
        let a = t.create(db_a, "Regions", DIM_TYPE_NORMAL).unwrap();
        t.create(db_b, "Months", DIM_TYPE_NORMAL).unwrap();
        assert!(t.rename(a, "Months").is_ok());
    }

    #[test]
    fn element_crud_round_trip() {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        let dim_id = t.create(db, "Regions", DIM_TYPE_NORMAL).unwrap();
        let dim = t.get_mut(dim_id).unwrap();

        let north = dim.create_element("North", ElementType::Numeric as u32).unwrap();
        let south = dim.create_element("South", ElementType::Numeric as u32).unwrap();
        assert_eq!(dim.get_element(north).unwrap().wire_id, 0);
        assert_eq!(dim.get_element(south).unwrap().wire_id, 1);
        assert_eq!(dim.element_count(), 2);

        dim.rename_element(north, "Northeast").unwrap();
        assert_eq!(dim.get_element(north).unwrap().name, "Northeast");

        dim.destroy_element(south).unwrap();
        assert_eq!(dim.element_count(), 1);
        assert!(dim.get_element(south).is_none());
    }

    #[test]
    fn clear_with_no_filter_removes_every_element() {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        let dim_id = t.create(db, "Regions", DIM_TYPE_NORMAL).unwrap();
        let dim = t.get_mut(dim_id).unwrap();
        dim.create_element("North", ElementType::Numeric as u32).unwrap();
        dim.create_element("South", ElementType::String as u32).unwrap();

        dim.clear(None);
        assert_eq!(dim.element_count(), 0);
    }

    #[test]
    fn clear_with_a_type_filter_only_removes_matching_elements() {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        let dim_id = t.create(db, "Regions", DIM_TYPE_NORMAL).unwrap();
        let dim = t.get_mut(dim_id).unwrap();
        let north = dim.create_element("North", ElementType::Numeric as u32).unwrap();
        let label = dim.create_element("Label", ElementType::String as u32).unwrap();

        dim.clear(Some(ElementType::String as u32));
        assert_eq!(dim.element_count(), 1);
        assert!(dim.get_element(north).is_some());
        assert!(dim.get_element(label).is_none());
    }

    #[test]
    fn clear_unlinks_hierarchy_edges_of_removed_elements() {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        let dim_id = t.create(db, "Regions", DIM_TYPE_NORMAL).unwrap();
        let dim = t.get_mut(dim_id).unwrap();
        let north = dim.create_element("North", ElementType::Numeric as u32).unwrap();
        let south = dim.create_element("South", ElementType::Numeric as u32).unwrap();
        let total = dim.create_element("Total", ElementType::Numeric as u32).unwrap();
        dim.add_children(total, &[(north, 1.0), (south, 1.0)]).unwrap();

        dim.clear(None);
        assert_eq!(dim.element_count(), 0);
        // A fresh element reusing a since-recycled slotmap slot must not
        // resurrect any stale hierarchy edge left behind by clear().
        let fresh = dim.create_element("Fresh", ElementType::Numeric as u32).unwrap();
        assert!(dim.children_of(fresh).is_empty());
        assert!(dim.parents_of(fresh).is_empty());
    }

    #[test]
    fn create_element_rejects_consolidated_type_in_m3() {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        let dim_id = t.create(db, "Regions", DIM_TYPE_NORMAL).unwrap();
        let dim = t.get_mut(dim_id).unwrap();
        assert!(matches!(dim.create_element("Total", 4), Err(DomainError::UnsupportedType(4))));
    }

    #[test]
    fn change_element_type_round_trips_numeric_and_string() {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        let dim_id = t.create(db, "Regions", DIM_TYPE_NORMAL).unwrap();
        let dim = t.get_mut(dim_id).unwrap();
        let north = dim.create_element("North", ElementType::Numeric as u32).unwrap();

        dim.change_element_type(north, 2).unwrap();
        assert_eq!(dim.get_element(north).unwrap().elem_type, ElementType::String);

        dim.change_element_type(north, 1).unwrap();
        assert_eq!(dim.get_element(north).unwrap().elem_type, ElementType::Numeric);
    }

    #[test]
    fn change_element_type_numeric_bucket_on_a_consolidated_element_is_a_no_op() {
        // A dimension reload re-sends CONSOLIDATED placeholders (bucketed
        // as "1", same as NUMERIC) for parents whose hierarchy is
        // established afterward by a separate updateConsolidations call --
        // this must not disturb an already-consolidated element from a
        // prior load, or reloading the same dimension twice would corrupt
        // it on the second run.
        let mut dim = new_dim();
        let total = dim.create_element("Total", ElementType::Numeric as u32).unwrap();
        let north = dim.create_element("North", ElementType::Numeric as u32).unwrap();
        dim.add_children(total, &[(north, 1.0)]).unwrap();

        dim.change_element_type(total, 1).unwrap();
        assert_eq!(dim.get_element(total).unwrap().elem_type, ElementType::Consolidated);
        assert_eq!(dim.children_of(total), &[(north, 1.0)]);
    }

    #[test]
    fn change_element_type_rejects_consolidated_element() {
        let mut dim = new_dim();
        let total = dim.create_element("Total", ElementType::Numeric as u32).unwrap();
        let north = dim.create_element("North", ElementType::Numeric as u32).unwrap();
        dim.add_children(total, &[(north, 1.0)]).unwrap();

        assert!(matches!(dim.change_element_type(total, 2), Err(DomainError::UnsupportedType(4))));
        // Rejected in place -- the element's type and children are untouched.
        assert_eq!(dim.get_element(total).unwrap().elem_type, ElementType::Consolidated);
        assert_eq!(dim.children_of(total), &[(north, 1.0)]);
    }

    #[test]
    fn change_element_type_rejects_unsupported_type_code() {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        let dim_id = t.create(db, "Regions", DIM_TYPE_NORMAL).unwrap();
        let dim = t.get_mut(dim_id).unwrap();
        let north = dim.create_element("North", ElementType::Numeric as u32).unwrap();

        assert!(matches!(dim.change_element_type(north, 4), Err(DomainError::UnsupportedType(4))));
        assert!(matches!(dim.change_element_type(north, 99), Err(DomainError::UnsupportedType(99))));
    }

    #[test]
    fn change_element_type_rejects_unknown_id() {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        let dim_id = t.create(db, "Regions", DIM_TYPE_NORMAL).unwrap();
        let dim = t.get_mut(dim_id).unwrap();
        let north = dim.create_element("North", ElementType::Numeric as u32).unwrap();
        dim.destroy_element(north).unwrap();

        assert!(matches!(dim.change_element_type(north, 1), Err(DomainError::NotFound)));
    }

    #[test]
    fn destroyed_element_id_is_not_reachable_again() {
        let db = fake_database_id();
        let mut t = DimensionTable::new();
        let dim_id = t.create(db, "Regions", DIM_TYPE_NORMAL).unwrap();
        let dim = t.get_mut(dim_id).unwrap();
        let a = dim.create_element("a", ElementType::Numeric as u32).unwrap();
        dim.destroy_element(a).unwrap();
        let b = dim.create_element("b", ElementType::Numeric as u32).unwrap();
        assert!(dim.get_element(a).is_none());
        assert_eq!(dim.get_element(b).unwrap().name, "b");
    }

    #[test]
    fn destroy_elements_unlinks_the_batch_and_demotes_emptied_parents() {
        let mut dim = new_dim();
        let [a, b, c, total, other] = ["a", "b", "c", "total", "other"].map(|n| dim.create_element(n, 1).unwrap());
        dim.set_children(total, &[(a, 1.0), (b, 1.0), (c, 1.0)]).unwrap();
        dim.set_children(other, &[(a, 1.0)]).unwrap();

        dim.destroy_elements(&[a, b, a]).unwrap();
        assert_eq!(dim.children_of(total), &[(c, 1.0)]);
        assert_eq!(dim.get_element(total).unwrap().elem_type, ElementType::Consolidated);
        assert!(dim.children_of(other).is_empty());
        assert_eq!(dim.get_element(other).unwrap().elem_type, ElementType::Numeric);
        assert!(dim.find_element_by_name("a").is_none());

        // An unknown id fails the whole batch.
        assert!(matches!(dim.destroy_elements(&[c, a]), Err(DomainError::NotFound)));
        assert!(dim.get_element(c).is_some());
    }

    // -- Hierarchy -------------------------------------------------------

    #[test]
    fn add_children_promotes_to_consolidated_and_links_both_directions() {
        let mut dim = new_dim();
        let total = dim.create_element("Total", ElementType::Numeric as u32).unwrap();
        let north = dim.create_element("North", ElementType::Numeric as u32).unwrap();
        let south = dim.create_element("South", ElementType::Numeric as u32).unwrap();

        dim.add_children(total, &[(north, 1.0), (south, 2.0)]).unwrap();

        assert_eq!(dim.get_element(total).unwrap().elem_type, ElementType::Consolidated);
        assert_eq!(dim.children_of(total), &[(north, 1.0), (south, 2.0)]);
        assert_eq!(dim.parents_of(north), &[total]);
        assert_eq!(dim.parents_of(south), &[total]);
    }

    #[test]
    fn add_children_upserts_without_duplicating_existing() {
        let mut dim = new_dim();
        let total = dim.create_element("Total", ElementType::Numeric as u32).unwrap();
        let north = dim.create_element("North", ElementType::Numeric as u32).unwrap();
        let south = dim.create_element("South", ElementType::Numeric as u32).unwrap();

        dim.add_children(total, &[(north, 1.0)]).unwrap();
        dim.add_children(total, &[(north, 5.0), (south, 1.0)]).unwrap();

        // north's weight updated in place, not duplicated; south appended.
        assert_eq!(dim.children_of(total), &[(north, 5.0), (south, 1.0)]);
    }

    #[test]
    fn set_children_replaces_not_merges() {
        let mut dim = new_dim();
        let total = dim.create_element("Total", ElementType::Numeric as u32).unwrap();
        let north = dim.create_element("North", ElementType::Numeric as u32).unwrap();
        let south = dim.create_element("South", ElementType::Numeric as u32).unwrap();

        dim.add_children(total, &[(north, 1.0), (south, 1.0)]).unwrap();
        dim.set_children(total, &[(south, 3.0)]).unwrap();

        assert_eq!(dim.children_of(total), &[(south, 3.0)]);
        // north must be fully unlinked, not just absent from the list --
        // check the inverse index too.
        assert_eq!(dim.parents_of(north), &[] as &[ElementId]);
    }

    /// Regression test for a real bug found via a live ETL job run --
    /// see promote_to_consolidated's
    /// doc comment. set_children(id, &[]) clearing an element's last
    /// child must demote it back to a leaf, not leave it permanently
    /// stuck reporting Consolidated with zero real children.
    #[test]
    fn set_children_to_empty_demotes_back_to_numeric() {
        let mut dim = new_dim();
        let total = dim.create_element("Total", ElementType::Numeric as u32).unwrap();
        let north = dim.create_element("North", ElementType::Numeric as u32).unwrap();

        dim.add_children(total, &[(north, 1.0)]).unwrap();
        assert_eq!(dim.get_element(total).unwrap().elem_type, ElementType::Consolidated);

        dim.set_children(total, &[]).unwrap();

        assert_eq!(dim.children_of(total), &[] as &[(ElementId, f64)]);
        assert_eq!(dim.get_element(total).unwrap().elem_type, ElementType::Numeric, "clearing every child must demote the element back to a leaf");
    }

    /// A `set_children` call that clears to empty on an element that was
    /// never consolidated in the first place (e.g. a plain leaf, or a
    /// String-typed element) must be a true no-op on `elem_type` --
    /// `promote_to_consolidated`'s demotion path is guarded specifically
    /// on "was Consolidated", not "has no children", so it must never
    /// clobber a leaf's real type (String in particular -- there is no
    /// evidence a String leaf should ever become Numeric).
    #[test]
    fn set_children_to_empty_on_an_already_leaf_element_does_not_change_its_type() {
        let mut dim = new_dim();
        let label = dim.create_element("Label", ElementType::String as u32).unwrap();

        dim.set_children(label, &[]).unwrap();

        assert_eq!(dim.get_element(label).unwrap().elem_type, ElementType::String);
    }

    #[test]
    fn rejects_self_reference() {
        let mut dim = new_dim();
        let a = dim.create_element("a", ElementType::Numeric as u32).unwrap();
        assert!(matches!(dim.add_children(a, &[(a, 1.0)]), Err(DomainError::Cycle)));
    }

    #[test]
    fn rejects_indirect_cycle() {
        let mut dim = new_dim();
        let a = dim.create_element("a", ElementType::Numeric as u32).unwrap();
        let b = dim.create_element("b", ElementType::Numeric as u32).unwrap();
        let c = dim.create_element("c", ElementType::Numeric as u32).unwrap();
        // a -> b -> c
        dim.add_children(a, &[(b, 1.0)]).unwrap();
        dim.add_children(b, &[(c, 1.0)]).unwrap();
        // c -> a would close the loop.
        assert!(matches!(dim.add_children(c, &[(a, 1.0)]), Err(DomainError::Cycle)));
    }

    #[test]
    fn allows_diamond_shared_child_not_a_cycle() {
        // top -> {left, right}, left -> bottom, right -> bottom.
        // A DAG with a shared descendant is NOT a cycle and must be
        // allowed (this is normal, common OLAP consolidation shape).
        let mut dim = new_dim();
        let top = dim.create_element("top", ElementType::Numeric as u32).unwrap();
        let left = dim.create_element("left", ElementType::Numeric as u32).unwrap();
        let right = dim.create_element("right", ElementType::Numeric as u32).unwrap();
        let bottom = dim.create_element("bottom", ElementType::Numeric as u32).unwrap();

        dim.add_children(top, &[(left, 1.0), (right, 1.0)]).unwrap();
        assert!(dim.add_children(left, &[(bottom, 1.0)]).is_ok());
        assert!(dim.add_children(right, &[(bottom, 1.0)]).is_ok());
        assert_eq!(dim.parents_of(bottom).len(), 2);
    }

    #[test]
    fn level_is_zero_for_leaves_and_one_plus_max_child_for_consolidated() {
        let mut dim = new_dim();
        let leaf_a = dim.create_element("a", ElementType::Numeric as u32).unwrap();
        let leaf_b = dim.create_element("b", ElementType::Numeric as u32).unwrap();
        let mid = dim.create_element("mid", ElementType::Numeric as u32).unwrap();
        let top = dim.create_element("top", ElementType::Numeric as u32).unwrap();

        assert_eq!(dim.level(leaf_a), 0);

        dim.add_children(mid, &[(leaf_a, 1.0)]).unwrap();
        assert_eq!(dim.level(mid), 1);

        dim.add_children(top, &[(mid, 1.0), (leaf_b, 1.0)]).unwrap();
        // top's tallest branch is through mid (level 1), so top is level 2.
        assert_eq!(dim.level(top), 2);
    }

    #[test]
    fn expand_to_base_of_a_leaf_is_itself_with_weight_one() {
        let mut dim = new_dim();
        let leaf = dim.create_element("a", ElementType::Numeric as u32).unwrap();
        assert_eq!(dim.expand_to_base(leaf), vec![(leaf, 1.0)]);
    }

    #[test]
    fn expand_to_base_multiplies_weights_along_the_path() {
        // top --2.0--> mid --3.0--> leaf: leaf's cumulative weight under
        // top must be 2.0 * 3.0 = 6.0, not just the direct edge weight.
        let mut dim = new_dim();
        let leaf = dim.create_element("leaf", ElementType::Numeric as u32).unwrap();
        let mid = dim.create_element("mid", ElementType::Numeric as u32).unwrap();
        let top = dim.create_element("top", ElementType::Numeric as u32).unwrap();
        dim.add_children(mid, &[(leaf, 3.0)]).unwrap();
        dim.add_children(top, &[(mid, 2.0)]).unwrap();

        assert_eq!(dim.expand_to_base(top), vec![(leaf, 6.0)]);
    }

    #[test]
    fn expand_to_base_of_diamond_shape_sums_both_paths_separately() {
        // top -> {left(w=1), right(w=1)}, left -> bottom(w=1), right ->
        // bottom(w=1): bottom is reachable via two distinct paths from
        // top, and must appear TWICE in the expansion (once per path),
        // not deduplicated -- that's what makes diamond consolidation
        // correctly double-count a shared descendant's contribution.
        let mut dim = new_dim();
        let bottom = dim.create_element("bottom", ElementType::Numeric as u32).unwrap();
        let left = dim.create_element("left", ElementType::Numeric as u32).unwrap();
        let right = dim.create_element("right", ElementType::Numeric as u32).unwrap();
        let top = dim.create_element("top", ElementType::Numeric as u32).unwrap();
        dim.add_children(left, &[(bottom, 1.0)]).unwrap();
        dim.add_children(right, &[(bottom, 1.0)]).unwrap();
        dim.add_children(top, &[(left, 1.0), (right, 1.0)]).unwrap();

        let expansion = dim.expand_to_base(top);
        assert_eq!(expansion.len(), 2, "bottom must appear once per path, not deduplicated");
        assert!(expansion.iter().all(|&(id, w)| id == bottom && w == 1.0));
    }

    #[test]
    fn move_element_shifts_others_and_keeps_positions_dense() {
        let mut dim = new_dim();
        let a = dim.create_element("a", ElementType::Numeric as u32).unwrap(); // 0
        let b = dim.create_element("b", ElementType::Numeric as u32).unwrap(); // 1
        let c = dim.create_element("c", ElementType::Numeric as u32).unwrap(); // 2

        dim.move_element(c, 0).unwrap(); // c, a, b
        assert_eq!(dim.get_element(c).unwrap().position, 0);
        assert_eq!(dim.get_element(a).unwrap().position, 1);
        assert_eq!(dim.get_element(b).unwrap().position, 2);

        let mut positions: Vec<u32> = dim.list_elements().map(|(_, e)| e.position).collect();
        positions.sort_unstable();
        assert_eq!(positions, vec![0, 1, 2]);
    }

    #[test]
    fn move_element_out_of_range_clamps_to_last_slot() {
        let mut dim = new_dim();
        let a = dim.create_element("a", ElementType::Numeric as u32).unwrap();
        dim.create_element("b", ElementType::Numeric as u32).unwrap();
        dim.move_element(a, 999).unwrap();
        assert_eq!(dim.get_element(a).unwrap().position, 1);
    }

    #[test]
    fn add_children_rejects_unknown_child_id() {
        // Two independent fresh SlotMaps mint keys in the same
        // deterministic sequence (index 0/gen 1, index 1/gen 1, ...), so
        // small dimensions from separate tables can have colliding
        // `ElementId`s by sheer coincidence -- bit an earlier test in
        // this file already (see `fake_database_ids`'s doc comment), and
        // a first attempt at *this* test too (offsetting `a` by one
        // insert wasn't enough: `foreign` at other's index 0 collided
        // with dim's own index-0 element instead). Fix: give `other` a
        // large number of throwaway inserts first so `foreign`'s index is
        // far outside any range `dim` (which only ever holds a handful of
        // elements) could reach.
        let mut dim = new_dim();
        let a = dim.create_element("a", ElementType::Numeric as u32).unwrap();

        let mut other = new_dim();
        for i in 0..100 {
            other.create_element(&format!("filler{i}"), ElementType::Numeric as u32).unwrap();
        }
        let foreign = other.create_element("foreign", ElementType::Numeric as u32).unwrap();

        assert!(matches!(dim.add_children(a, &[(foreign, 1.0)]), Err(DomainError::NotFound)));
    }

    /// Brute-force reachability, written independently of `Dimension::reaches`
    /// on purpose -- the property test below should not just be re-checking
    /// the same logic the code under test already uses to reject cycles.
    fn has_path(dim: &Dimension, from: ElementId, to: ElementId, visited: &mut std::collections::HashSet<ElementId>) -> bool {
        if !visited.insert(from) {
            return false;
        }
        for &(c, _) in dim.children_of(from) {
            if c == to || has_path(dim, c, to, visited) {
                return true;
            }
        }
        false
    }

    proptest::proptest! {
        /// After ANY sequence of add_children calls (including ones that
        /// legitimately get rejected as cycles/unknown ids -- the sequence
        /// doesn't avoid those, it just fires whatever proptest generates
        /// and ignores the Result), two invariants must hold no matter what:
        /// (1) `children`/`parents` remain exact inverses of each other,
        /// and (2) no element can reach itself by walking down its own
        /// children -- i.e. `add_children`'s cycle rejection is never
        /// bypassable by any interleaving. This is the direct analogue of
        /// PLAN.md's "hierarchy consistency" property test.
        #[test]
        fn hierarchy_stays_consistent_under_random_edits(
            ops in proptest::collection::vec((0usize..8, 0usize..8, 1.0f64..5.0), 1..60)
        ) {
            let mut dim = new_dim();
            let ids: Vec<ElementId> = (0..8)
                .map(|i| dim.create_element(&format!("e{i}"), ElementType::Numeric as u32).unwrap())
                .collect();

            for (pi, ci, w) in ops {
                let _ = dim.add_children(ids[pi], &[(ids[ci], w)]);
            }

            for &id in &ids {
                for &(c, _) in dim.children_of(id) {
                    proptest::prop_assert!(dim.parents_of(c).contains(&id), "child->parent inverse missing");
                }
                for &p in dim.parents_of(id) {
                    proptest::prop_assert!(dim.children_of(p).iter().any(|&(c, _)| c == id), "parent->child inverse missing");
                }
                proptest::prop_assert!(!has_path(&dim, id, id, &mut std::collections::HashSet::new()), "element reaches itself -- cycle slipped through");
            }
        }
    }

    /// Independent (iterative, not recursive) reference implementation of
    /// `Dimension::expand_to_base`, written separately so the property
    /// test below isn't just checking the production code against
    /// itself -- see PLAN.md's testing strategy: "two independent
    /// implementations of the same math is how you catch silent-wrong-
    /// result bugs, not just crashes."
    fn expand_to_base_reference(dim: &Dimension, root: ElementId) -> Vec<(ElementId, f64)> {
        let mut result = Vec::new();
        let mut stack = vec![(root, 1.0)];
        while let Some((id, weight)) = stack.pop() {
            let children = dim.children_of(id);
            if children.is_empty() {
                result.push((id, weight));
            } else {
                for &(child, w) in children {
                    stack.push((child, weight * w));
                }
            }
        }
        result
    }

    /// Sums weights per leaf (a shared leaf reached via multiple paths --
    /// e.g. the diamond shape -- contributes the sum of each path's
    /// weight, which is exactly what matters for the consolidation math
    /// `expand_to_base` exists to support; see
    /// `expand_to_base_of_diamond_shape_sums_both_paths_separately` for
    /// the test that specifically checks shared leaves are NOT collapsed
    /// before summing).
    fn weight_by_leaf(v: &[(ElementId, f64)]) -> std::collections::HashMap<ElementId, f64> {
        let mut map = std::collections::HashMap::new();
        for &(id, w) in v {
            *map.entry(id).or_insert(0.0) += w;
        }
        map
    }

    proptest::proptest! {
        /// For any randomly-built hierarchy (built the same way as
        /// `hierarchy_stays_consistent_under_random_edits`, including
        /// operations that get legitimately rejected), the production
        /// recursive `expand_to_base` and the independent iterative
        /// reference above must agree on the total weight reaching each
        /// leaf, for every element in the dimension -- this is the
        /// direct analogue of PLAN.md's "consolidation weight math"
        /// property test.
        #[test]
        fn expand_to_base_matches_independent_reference(
            ops in proptest::collection::vec((0usize..8, 0usize..8, 1.0f64..5.0), 1..60)
        ) {
            let mut dim = new_dim();
            let ids: Vec<ElementId> = (0..8)
                .map(|i| dim.create_element(&format!("e{i}"), ElementType::Numeric as u32).unwrap())
                .collect();

            for (pi, ci, w) in ops {
                let _ = dim.add_children(ids[pi], &[(ids[ci], w)]);
            }

            for &id in &ids {
                let production = weight_by_leaf(&dim.expand_to_base(id));
                let reference = weight_by_leaf(&expand_to_base_reference(&dim, id));
                // Tolerance-based, not exact HashMap equality: the two
                // implementations sum per-leaf contributions in different
                // orders (recursive pre-order vs. iterative stack pops),
                // and IEEE 754 addition isn't associative -- exact
                // equality would be a flaky test for reasons that have
                // nothing to do with a real bug.
                proptest::prop_assert_eq!(production.len(), reference.len());
                for (leaf, &w) in &production {
                    let r = reference.get(leaf).copied().unwrap_or(0.0);
                    proptest::prop_assert!((w - r).abs() < 1e-9, "weight mismatch for a leaf: production={w} reference={r}");
                }
            }
        }
    }
}
