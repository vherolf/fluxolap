//! The server's statistics (`GET /api/v1/stats`) as fluxoterm keeps them:
//! the latest reading, a short history for the sparklines, and what is
//! derived from two readings (requests per second, average latency).
//! Free of I/O and drawing, so it is tested on its own.

use fluxolap_api_types::{ObjectType, ServerStats};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How often the stats are read while the stats view is open ...
pub const FAST: Duration = Duration::from_secs(2);
/// ... and otherwise (for the memory in the header).
pub const SLOW: Duration = Duration::from_secs(5);
/// Readings kept for the sparklines.
const HISTORY: usize = 120;

/// One reading's counters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub at: Instant,
    pub rss: u64,
    pub requests: u64,
    pub errors: u64,
    pub busy_micros: u64,
}

/// What changed between two readings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub requests_per_sec: f64,
    pub errors_per_sec: f64,
    /// Average time to answer a request in that interval; none without requests.
    pub avg_latency_ms: Option<f64>,
}

impl Rates {
    /// Between readings `a` and `b` (later). `None` if the server restarted
    /// in between (its counters went down) or no time passed.
    pub fn between(a: &Sample, b: &Sample) -> Option<Self> {
        let secs = b.at.checked_duration_since(a.at)?.as_secs_f64();
        if secs <= 0.0 || b.requests < a.requests || b.errors < a.errors || b.busy_micros < a.busy_micros {
            return None;
        }
        let requests = (b.requests - a.requests) as f64;
        Some(Self {
            requests_per_sec: requests / secs,
            errors_per_sec: (b.errors - a.errors) as f64 / secs,
            avg_latency_ms: (requests > 0.0).then(|| (b.busy_micros - a.busy_micros) as f64 / requests / 1000.0),
        })
    }
}

#[derive(Debug)]
pub struct StatsState {
    pub latest: Option<ServerStats>,
    pub history: VecDeque<Sample>,
    /// False once the server answered that it has no `/stats` (older than fluxolap 21).
    pub supported: bool,
    /// A reading is on its way.
    pub pending: bool,
    pub last_poll: Option<Instant>,
}

impl Default for StatsState {
    fn default() -> Self {
        Self { latest: None, history: VecDeque::new(), supported: true, pending: false, last_poll: None }
    }
}

impl StatsState {
    /// Whether it's time for a new reading (`fast` while the stats view is open).
    pub fn due(&self, fast: bool, now: Instant) -> bool {
        let every = if fast { FAST } else { SLOW };
        self.supported && !self.pending && self.last_poll.is_none_or(|t| now.duration_since(t) >= every)
    }

    /// A reading arrived.
    pub fn record(&mut self, stats: ServerStats, at: Instant) {
        let sample = Sample {
            at,
            rss: stats.memory.rss_bytes.unwrap_or(0),
            requests: stats.requests.total,
            errors: stats.requests.client_errors + stats.requests.server_errors,
            busy_micros: stats.requests.busy_micros,
        };
        // A restarted server starts its counters over: start the history over too.
        if self.history.back().is_some_and(|last| Rates::between(last, &sample).is_none()) {
            self.history.clear();
        }
        self.history.push_back(sample);
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }
        self.latest = Some(stats);
    }

    /// Over the last interval.
    pub fn rates(&self) -> Option<Rates> {
        let n = self.history.len();
        (n >= 2).then(|| Rates::between(&self.history[n - 2], &self.history[n - 1])).flatten()
    }

    /// Requests per second per interval, for a sparkline.
    pub fn request_series(&self) -> Vec<u64> {
        self.history.iter().zip(self.history.iter().skip(1)).map(|(a, b)| Rates::between(a, b).map_or(0, |r| r.requests_per_sec.round() as u64)).collect()
    }

    pub fn rss_series(&self) -> Vec<u64> {
        self.history.iter().map(|s| s.rss).collect()
    }

    /// The memory the header shows: resident size and, if known, what it
    /// is measured against (a container limit, else the machine's memory).
    pub fn header_memory(&self) -> Option<(u64, Option<u64>)> {
        let memory = &self.latest.as_ref()?.memory;
        Some((memory.rss_bytes?, memory.limit_bytes.or(memory.system_total_bytes)))
    }
}

/// A line of the stats view's table: a database, then its cubes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Row {
    Database(usize),
    Cube(usize, usize),
}

/// The table's lines: databases by estimated memory, largest first, each
/// followed by its cubes (also largest first; automatic ones only with
/// `show_system`).
pub fn rows(stats: &ServerStats, show_system: bool) -> Vec<Row> {
    let mut dbs: Vec<usize> = (0..stats.databases.len()).collect();
    dbs.sort_by(|&a, &b| stats.databases[b].bytes.cmp(&stats.databases[a].bytes).then(a.cmp(&b)));
    let mut rows = Vec::new();
    for d in dbs {
        rows.push(Row::Database(d));
        let cubes = &stats.databases[d].cubes;
        let mut order: Vec<usize> = (0..cubes.len()).filter(|&c| show_system || cubes[c].cube_type == ObjectType::Normal).collect();
        order.sort_by(|&a, &b| (cubes[b].hot_bytes + cubes[b].cold_bytes).cmp(&(cubes[a].hot_bytes + cubes[a].cold_bytes)).then(a.cmp(&b)));
        rows.extend(order.into_iter().map(|c| Row::Cube(d, c)));
    }
    rows
}

/// A byte count for people: `834 B`, `12.4 KB`, `3.1 MB`, `2.0 GB` (powers of 1024).
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut size = n as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// `3d 4h`, `2h 5m`, `7m 12s`, `40s`.
pub fn duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m {s}s"),
        (0, _, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use fluxolap_api_types::{CubeStats, DatabaseStats, MemoryStats, RequestStats, TotalStats};

    pub fn server_stats(requests: u64, busy_micros: u64) -> ServerStats {
        let cube = |id: u32, name: &str, t: ObjectType, bytes: u64| CubeStats {
            id,
            name: name.into(),
            cube_type: t,
            dimensions: 2,
            cells: 10,
            hot_cells: 10,
            cold_cells: 0,
            hot_bytes: bytes,
            cold_bytes: 0,
            locks: 0,
        };
        let db = |id: u32, name: &str, cubes: Vec<CubeStats>| DatabaseStats {
            id,
            name: name.into(),
            loaded: true,
            dimensions: 2,
            elements: 5,
            cells: cubes.iter().map(|c| c.cells).sum(),
            bytes: cubes.iter().map(|c| c.hot_bytes + c.cold_bytes).sum(),
            cubes,
        };
        ServerStats {
            version: "21.0.0".into(),
            pid: 1,
            uptime_secs: 60,
            os: "linux".into(),
            arch: "x86_64".into(),
            cpus: 4,
            data_dir: None,
            memory: MemoryStats { rss_bytes: Some(100 << 20), system_total_bytes: Some(1 << 30), data_bytes: 3000, ..Default::default() },
            requests: RequestStats { total: requests, busy_micros, ..Default::default() },
            totals: TotalStats::default(),
            databases: vec![
                db(0, "Small", vec![cube(0, "#_DIMENSION_", ObjectType::System, 50), cube(3, "Tiny", ObjectType::Normal, 100)]),
                db(1, "Big", vec![cube(3, "A", ObjectType::Normal, 1000), cube(4, "B", ObjectType::Normal, 2000)]),
            ],
        }
    }

    #[test]
    fn rates_come_from_the_last_two_readings() {
        let mut state = StatsState::default();
        let t0 = Instant::now();
        state.record(server_stats(100, 50_000), t0);
        assert_eq!(state.rates(), None, "one reading isn't enough");
        state.record(server_stats(120, 90_000), t0 + Duration::from_secs(2));
        let rates = state.rates().unwrap();
        assert_eq!(rates.requests_per_sec, 10.0);
        assert_eq!(rates.avg_latency_ms, Some(2.0), "40 ms over 20 requests");
        state.record(server_stats(120, 90_000), t0 + Duration::from_secs(4));
        assert_eq!(state.rates().unwrap().avg_latency_ms, None, "no requests, no latency");
        assert_eq!(state.request_series(), vec![10, 0]);
    }

    #[test]
    fn a_restarted_server_starts_the_history_over() {
        let mut state = StatsState::default();
        let t0 = Instant::now();
        state.record(server_stats(500, 1000), t0);
        state.record(server_stats(3, 10), t0 + Duration::from_secs(2));
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.rates(), None);
    }

    #[test]
    fn polling_waits_its_interval_and_stops_on_an_old_server() {
        let mut state = StatsState::default();
        let t0 = Instant::now();
        assert!(state.due(false, t0));
        state.last_poll = Some(t0);
        assert!(!state.due(true, t0 + Duration::from_secs(1)));
        assert!(state.due(true, t0 + FAST));
        assert!(!state.due(false, t0 + FAST));
        state.pending = true;
        assert!(!state.due(true, t0 + SLOW));
        state.pending = false;
        state.supported = false;
        assert!(!state.due(true, t0 + SLOW));
    }

    #[test]
    fn rows_put_the_largest_first_and_hide_automatic_cubes() {
        let stats = server_stats(0, 0);
        assert_eq!(rows(&stats, false), vec![Row::Database(1), Row::Cube(1, 1), Row::Cube(1, 0), Row::Database(0), Row::Cube(0, 1)]);
        assert_eq!(rows(&stats, true).len(), 6);
    }

    #[test]
    fn formats() {
        assert_eq!(bytes(834), "834 B");
        assert_eq!(bytes(12_700), "12.4 KB");
        assert_eq!(bytes(3_250_000), "3.1 MB");
        assert_eq!(bytes(2 << 30), "2.0 GB");
        assert_eq!(duration(40), "40s");
        assert_eq!(duration(432), "7m 12s");
        assert_eq!(duration(7500), "2h 5m");
        assert_eq!(duration(3 * 86_400 + 4 * 3600 + 5), "3d 4h");
    }
}
