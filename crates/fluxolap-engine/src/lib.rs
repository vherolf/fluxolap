//! The fluxolap engine: server state and per-database persistence
//! (`state`), cell/hierarchy business logic (`ops`), and database/dimension
//! creation with system/attribute auto-provisioning (`provision`).
//! Protocol-agnostic -- any API layer builds on top of this crate.

pub mod ops;
pub mod provision;
mod state;

pub use state::{read_database_name, AppState, LoadedDatabase};
