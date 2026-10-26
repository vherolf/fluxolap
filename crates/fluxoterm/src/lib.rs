//! fluxoterm: a terminal viewer/editor for fluxolap servers, over the
//! REST API. `app` holds the state and behavior (no I/O), `exec` runs the
//! requests it asks for, `ui` draws it, and `main` ties them together.

pub mod app;
pub mod client;
pub mod exec;
pub mod stats;
pub mod tree;
pub mod ui;
