//! `GET /api/v1/stats`: what the server is using (memory, per database and
//! cube) and doing (request counters). Reads only what is already known --
//! no cell scans, and a database that isn't loaded stays unloaded.

use crate::dto::{CubeStats, DatabaseStats, MemoryStats, ObjectType, RequestStats, ServerStats, TotalStats};
use axum::extract::{Extension, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use fluxolap_engine::AppState;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Instant;

/// Request counters since the server started.
#[derive(Debug)]
pub struct Metrics {
    started: Instant,
    total: AtomicU64,
    client_errors: AtomicU64,
    server_errors: AtomicU64,
    in_flight: AtomicU64,
    busy_micros: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self { started: Instant::now(), total: 0.into(), client_errors: 0.into(), server_errors: 0.into(), in_flight: 0.into(), busy_micros: 0.into() }
    }
}

impl Metrics {
    fn snapshot(&self) -> RequestStats {
        RequestStats {
            total: self.total.load(Relaxed),
            client_errors: self.client_errors.load(Relaxed),
            server_errors: self.server_errors.load(Relaxed),
            in_flight: self.in_flight.load(Relaxed),
            busy_micros: self.busy_micros.load(Relaxed),
        }
    }
}

/// Whether a request counts: API calls, but not `/stats` itself (a client
/// watching the stats would otherwise count its own polling) or the docs.
fn counted(path: &str) -> bool {
    path.starts_with("/api/v1/") && path != "/api/v1/stats" && !path.starts_with("/api/v1/docs") && path != "/api/v1/openapi.json"
}

/// Middleware: counts each API request and the time spent answering it.
pub async fn track(metrics: Arc<Metrics>, request: Request, next: Next) -> Response {
    if !counted(request.uri().path()) {
        return next.run(request).await;
    }
    let start = Instant::now();
    metrics.in_flight.fetch_add(1, Relaxed);
    let response = next.run(request).await;
    metrics.in_flight.fetch_sub(1, Relaxed);
    metrics.total.fetch_add(1, Relaxed);
    metrics.busy_micros.fetch_add(start.elapsed().as_micros() as u64, Relaxed);
    if response.status().is_client_error() {
        metrics.client_errors.fetch_add(1, Relaxed);
    } else if response.status().is_server_error() {
        metrics.server_errors.fetch_add(1, Relaxed);
    }
    response
}

/// The server's memory, its databases and cubes, and request counters.
#[utoipa::path(get, path = "/api/v1/stats", tag = "server", responses((status = 200, body = ServerStats)))]
pub async fn stats(State(state): State<AppState>, Extension(metrics): Extension<Arc<Metrics>>) -> axum::Json<ServerStats> {
    axum::Json(collect_stats(&state, metrics.snapshot(), metrics.started.elapsed().as_secs()))
}

/// The server's statistics without HTTP: what `GET /api/v1/stats` answers, for
/// a host that embeds the engine and reaches it in-process (colibri's FFI
/// `fluxolap_stats`). `requests` and `uptime_secs` are the host's: the REST
/// API's counters here, the host's own elsewhere.
pub fn collect_stats(state: &AppState, requests: RequestStats, uptime_secs: u64) -> ServerStats {
    let mut totals = TotalStats::default();
    let mut databases = Vec::new();
    {
        let registry = state.databases.read().expect("databases lock poisoned");
        let loaded = state.loaded.read().expect("loaded lock poisoned");
        for id in registry.list_ids() {
            let Some(db) = registry.get(id) else { continue };
            let mut stats = DatabaseStats { id: db.wire_id, name: db.name.clone(), loaded: false, dimensions: 0, elements: 0, cells: 0, bytes: 0, cubes: Vec::new() };
            if let Some(ld) = loaded.get(&id) {
                stats.loaded = true;
                let dimensions = ld.dimensions.read().expect("dimensions lock poisoned");
                let cubes = ld.cubes.read().expect("cubes lock poisoned");
                for dim in dimensions.list_in_database(id) {
                    stats.dimensions += 1;
                    stats.elements += dim.element_count();
                }
                for (cube_id, cube) in cubes.list_in_database(id) {
                    let memory = ld.storage.memory(cube_id).unwrap_or_default();
                    let cube_stats = CubeStats {
                        id: cube.wire_id,
                        name: cube.name.clone(),
                        cube_type: ObjectType::from_cube_raw(cube.cube_type),
                        dimensions: cube.dimensions.len(),
                        cells: ld.storage.filled_count(cube_id),
                        hot_cells: memory.hot_cells,
                        cold_cells: memory.cold_cells,
                        hot_bytes: memory.hot_bytes as u64,
                        cold_bytes: memory.cold_bytes as u64,
                        locks: ld.storage.locks_for(cube_id).len(),
                    };
                    stats.cells += cube_stats.cells;
                    stats.bytes += cube_stats.hot_bytes + cube_stats.cold_bytes;
                    stats.cubes.push(cube_stats);
                }
                stats.cubes.sort_by_key(|c| c.id);
            }
            totals.databases += 1;
            totals.cubes += stats.cubes.len();
            totals.dimensions += stats.dimensions;
            totals.elements += stats.elements;
            totals.cells += stats.cells;
            databases.push(stats);
        }
    }
    databases.sort_by_key(|d| d.id);
    let mut memory = process_memory();
    memory.data_bytes = databases.iter().map(|d| d.bytes).sum();
    ServerStats {
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        uptime_secs,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
        data_dir: state.data_dir.as_ref().map(|d| d.display().to_string()),
        memory,
        requests,
        totals,
        databases,
    }
}

/// The process's and the machine's memory. Linux reports all of it
/// (`/proc`, and the cgroup limit inside a container); elsewhere only the
/// resident size is known.
#[cfg(target_os = "linux")]
fn process_memory() -> MemoryStats {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    // cgroup v2; "max" means no limit.
    let limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max").ok().and_then(|s| s.trim().parse::<u64>().ok());
    MemoryStats {
        rss_bytes: kb_field(&status, "VmRSS:"),
        peak_rss_bytes: kb_field(&status, "VmHWM:"),
        limit_bytes: limit,
        system_total_bytes: kb_field(&meminfo, "MemTotal:"),
        system_available_bytes: kb_field(&meminfo, "MemAvailable:"),
        data_bytes: 0,
    }
}

#[cfg(not(target_os = "linux"))]
fn process_memory() -> MemoryStats {
    MemoryStats { rss_bytes: memory_stats::memory_stats().map(|m| m.physical_mem as u64), ..Default::default() }
}

/// A `Name:   1234 kB` line of a `/proc` file, in bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn kb_field(text: &str, name: &str) -> Option<u64> {
    let line = text.lines().find_map(|l| l.strip_prefix(name))?;
    Some(line.trim().trim_end_matches("kB").trim().parse::<u64>().ok()? * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_fields_are_read_in_bytes() {
        let status = "Name:\tfluxolap\nVmHWM:\t  20480 kB\nVmRSS:\t   8192 kB\n";
        assert_eq!(kb_field(status, "VmRSS:"), Some(8 * 1024 * 1024));
        assert_eq!(kb_field(status, "VmHWM:"), Some(20 * 1024 * 1024));
        assert_eq!(kb_field(status, "VmSwap:"), None);
    }

    #[test]
    fn stats_polling_and_docs_are_not_counted() {
        assert!(counted("/api/v1/databases"));
        assert!(counted("/api/v1/nope"));
        assert!(!counted("/api/v1/stats"));
        assert!(!counted("/api/v1/docs"));
        assert!(!counted("/api/v1/openapi.json"));
        assert!(!counted("/ui/index.html"));
    }
}
