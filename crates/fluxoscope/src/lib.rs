//! fluxoscope: explore and chart fluxolap cubes in the browser. Written in
//! Rust (Leptos, compiled to WebAssembly); it talks to a fluxolap server
//! over the REST API with the server's own types (`fluxolap-api-types`).

pub mod api;
pub mod app;
pub mod charts;
pub mod explorer;
pub mod load;
pub mod model;
pub mod state;
