//! Typed arena ids for the domain model, backed by `slotmap`.
//!
//! Using distinct newtype keys (rather than a bare `u32`) means "passed a
//! `CubeId` where a `DimensionId` was expected" is a compile error, not a
//! runtime bug. Stale ids (from a destroyed element, say) fail lookup with
//! `None` instead of aliasing reused memory -- this is what makes the
//! use-after-free and dangling-iterator bug classes structurally
//! impossible here.

slotmap::new_key_type! {
    pub struct DatabaseId;
    pub struct DimensionId;
    pub struct CubeId;
    pub struct ElementId;
}
