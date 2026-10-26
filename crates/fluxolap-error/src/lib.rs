//! The error model: the numeric error codes (`ErrorCode`) and the typed
//! `FluxolapError` every engine operation returns. How an error is
//! rendered is up to the API layer (`fluxolap-api`: RFC 9457 problem
//! details, with the number as `error_code`).

/// Numeric error codes, kept stable across versions (clients may match on
/// them; they are `error_code` in problem details), grouped by what failed:
/// 1xxx requests, 2xxx databases, 3xxx dimensions, 4xxx elements, 5xxx
/// cubes, cells and locks. Kept as a distinct type (not a bare integer) so
/// a wrong-code mistake is at least a single place to check, and so call
/// sites can't accidentally pass a non-error integer where a code is
/// expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorCode(pub u32);

impl ErrorCode {
    pub const ILLEGAL_PARAMETER_VALUE: ErrorCode = ErrorCode(1003);
    pub const PARAMETER_MISSING: ErrorCode = ErrorCode(1016);
    pub const INVALID_DATABASE_NAME: ErrorCode = ErrorCode(2000);
    pub const DATABASE_NOT_FOUND: ErrorCode = ErrorCode(2001);
    pub const DATABASE_NAME_IN_USE: ErrorCode = ErrorCode(2005);
    pub const INVALID_DATABASE_TYPE: ErrorCode = ErrorCode(2009);
    pub const INVALID_TYPE: ErrorCode = ErrorCode(1005); // generic "invalid type", reused for dimension type
    pub const DIMENSION_NOT_FOUND: ErrorCode = ErrorCode(3002);
    pub const INVALID_DIMENSION_NAME: ErrorCode = ErrorCode(3003);
    pub const DIMENSION_NAME_IN_USE: ErrorCode = ErrorCode(3005);
    pub const DIMENSION_IN_USE: ErrorCode = ErrorCode(3006);
    pub const ELEMENT_CIRCULAR_REFERENCE: ErrorCode = ErrorCode(4001);
    pub const ELEMENT_NAME_IN_USE: ErrorCode = ErrorCode(4002);
    pub const ELEMENT_NOT_FOUND: ErrorCode = ErrorCode(4004);
    pub const INVALID_ELEMENT_NAME: ErrorCode = ErrorCode(4006);
    pub const INVALID_ELEMENT_TYPE: ErrorCode = ErrorCode(4008);
    pub const INVALID_COORDINATES: ErrorCode = ErrorCode(1006);
    pub const CUBE_NOT_FOUND: ErrorCode = ErrorCode(5000);
    pub const INVALID_CUBE_NAME: ErrorCode = ErrorCode(5001);
    pub const CUBE_EMPTY: ErrorCode = ErrorCode(5003);
    pub const CUBE_NAME_IN_USE: ErrorCode = ErrorCode(5008);
    pub const INVALID_CUBE_TYPE: ErrorCode = ErrorCode(5021);
    pub const MAX_CELL_REACHED: ErrorCode = ErrorCode(90001);
    pub const SPLASH_NOT_POSSIBLE: ErrorCode = ErrorCode(5012);
    pub const CUBE_LOCK_NOT_FOUND: ErrorCode = ErrorCode(5013);
    pub const CUBE_WRONG_LOCK: ErrorCode = ErrorCode(5015);
    pub const CUBE_BLOCKED_BY_LOCK: ErrorCode = ErrorCode(5016);
    pub const INTERNAL: ErrorCode = ErrorCode(2);
    pub const API_CALL_NOT_IMPLEMENTED: ErrorCode = ErrorCode(1021);
    pub const INVALID_MODE: ErrorCode = ErrorCode(4013);
    pub const AUTHORIZATION_FAILED: ErrorCode = ErrorCode(1004);
}

/// A typed, non-panicking error every engine operation returns instead of
/// indexing unchecked into caller-supplied data. Carries the numeric error
/// code, type string and message; rendering it is the API layer's job.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct FluxolapError {
    pub code: ErrorCode,
    pub error_type: &'static str,
    pub message: String,
}

impl FluxolapError {
    pub fn new(code: ErrorCode, error_type: &'static str, message: impl Into<String>) -> Self {
        Self { code, error_type, message: message.into() }
    }

    /// Not raised by fluxolap at the moment; kept with the other codes.
    pub fn parameter_missing(name: &str) -> Self {
        Self::new(ErrorCode::PARAMETER_MISSING, "parameter missing", format!("parameter '{name}' is missing"))
    }

    pub fn database_not_found() -> Self {
        Self::new(ErrorCode::DATABASE_NOT_FOUND, "database not found", "database not found")
    }

    pub fn invalid_database_name(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::INVALID_DATABASE_NAME, "invalid database name", message)
    }

    pub fn database_name_in_use(name: &str) -> Self {
        Self::new(ErrorCode::DATABASE_NAME_IN_USE, "database name in use", format!("database '{name}' already exists"))
    }

    pub fn invalid_database_type(t: u32) -> Self {
        Self::new(ErrorCode::INVALID_DATABASE_TYPE, "invalid database type", format!("unsupported database type ({t})"))
    }

    /// The generic "invalid type" code, for a dimension type.
    pub fn invalid_dimension_type(t: u32) -> Self {
        Self::new(ErrorCode::INVALID_TYPE, "invalid type", format!("wrong dimension type ({t})"))
    }

    pub fn dimension_not_found() -> Self {
        Self::new(ErrorCode::DIMENSION_NOT_FOUND, "dimension not found", "dimension not found")
    }

    pub fn invalid_dimension_name(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::INVALID_DIMENSION_NAME, "invalid dimension name", message)
    }

    pub fn dimension_name_in_use(name: &str) -> Self {
        Self::new(ErrorCode::DIMENSION_NAME_IN_USE, "dimension name in use", format!("dimension '{name}' already exists"))
    }

    /// A dimension can't be destroyed while a cube still references it.
    pub fn dimension_in_use(cube_name: &str) -> Self {
        Self::new(ErrorCode::DIMENSION_IN_USE, "dimension in use", format!("dimension is used by cube '{cube_name}'"))
    }

    pub fn element_not_found() -> Self {
        Self::new(ErrorCode::ELEMENT_NOT_FOUND, "element not found", "element not found")
    }

    pub fn invalid_element_name(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::INVALID_ELEMENT_NAME, "invalid element name", message)
    }

    pub fn element_name_in_use(name: &str) -> Self {
        Self::new(ErrorCode::ELEMENT_NAME_IN_USE, "element name in use", format!("element '{name}' already exists"))
    }

    pub fn invalid_element_type(t: u32) -> Self {
        Self::new(ErrorCode::INVALID_ELEMENT_TYPE, "invalid element type", format!("unsupported element type ({t})"))
    }

    pub fn element_circular_reference() -> Self {
        Self::new(ErrorCode::ELEMENT_CIRCULAR_REFERENCE, "circular reference", "cycle detected in the element hierarchy")
    }

    pub fn cube_not_found() -> Self {
        Self::new(ErrorCode::CUBE_NOT_FOUND, "cube not found", "cube not found")
    }

    pub fn invalid_cube_name(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::INVALID_CUBE_NAME, "invalid cube name", message)
    }

    pub fn cube_empty(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::CUBE_EMPTY, "cube empty", message)
    }

    pub fn cube_name_in_use(name: &str) -> Self {
        Self::new(ErrorCode::CUBE_NAME_IN_USE, "cube name in use", format!("cube '{name}' already exists"))
    }

    pub fn invalid_cube_type(t: u32) -> Self {
        Self::new(ErrorCode::INVALID_CUBE_TYPE, "invalid cube type", format!("unsupported cube type ({t})"))
    }

    pub fn invalid_coordinates(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::INVALID_COORDINATES, "invalid coordinates", message)
    }

    /// A client asked for an unreasonably large area (more cells than
    /// `fluxolap-engine`'s `ops::MAX_AREA_CELLS`).
    pub fn max_cell_reached() -> Self {
        Self::new(ErrorCode::MAX_CELL_REACHED, "reached maximum cell count", "reached maximum cell count")
    }

    /// Splashing a value would write more base cells than allowed.
    pub fn splash_not_possible(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::SPLASH_NOT_POSSIBLE, "splash not possible", message)
    }

    /// Committing or rolling back a lock that doesn't exist.
    pub fn cube_lock_not_found() -> Self {
        Self::new(ErrorCode::CUBE_LOCK_NOT_FOUND, "cube lock not found", "cube lock not found")
    }

    /// A new lock would overlap an existing one (see `fluxolap-storage`'s
    /// `areas_overlap`).
    pub fn cube_wrong_lock() -> Self {
        Self::new(ErrorCode::CUBE_WRONG_LOCK, "could not create lock", "could not create lock")
    }

    /// Blocked by a lock -- e.g. rolling back a whole-cube lock, which can
    /// only be released by committing it.
    pub fn cube_blocked_by_lock(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::CUBE_BLOCKED_BY_LOCK, "is blocked because of a locked area", message)
    }

    /// Something that isn't the request's fault, e.g. a database file that can't be read or
    /// written.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::INTERNAL, "internal error", message)
    }

    /// Not raised by fluxolap at the moment; kept with the other codes.
    pub fn api_call_not_implemented() -> Self {
        Self::new(ErrorCode::API_CALL_NOT_IMPLEMENTED, "api call not implemented", "api call not implemented")
    }

    /// Not raised by fluxolap at the moment; kept with the other codes.
    pub fn invalid_mode(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::INVALID_MODE, "invalid mode", message)
    }

    /// Not raised by fluxolap at the moment (it has no authorization); kept
    /// with the other codes.
    pub fn authorization_failed(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::AUTHORIZATION_FAILED, "authorization failed", message)
    }
}
