/// Shared across `database`/`dimension` (and future domain modules) so
/// `fluxolap-engine` has one place to map domain failures onto wire error
/// codes, instead of each module inventing its own error shape.
#[derive(Debug)]
pub enum DomainError {
    NotFound,
    EmptyName,
    DuplicateName(String),
    /// An explicitly requested wire id is already used by another entry of
    /// the same table (see `Dimension::create_element_with_wire_id`).
    DuplicateWireId(u32),
    UnsupportedType(u32),
    /// Adding a child would create a cycle in the element hierarchy
    /// (self-reference, or the proposed child is already a transitive
    /// ancestor of the proposed parent).
    Cycle,
    /// A cube was created with an empty dimension list.
    NoDimensions,
    /// A cell path's length doesn't match the cube's dimensionality,
    /// or one of its element ids doesn't belong to the corresponding
    /// dimension.
    InvalidCoordinates,
}
