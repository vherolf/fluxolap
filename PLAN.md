# fluxolap: design

fluxolap is a from-scratch OLAP server in Rust, with a REST API
(`/api/v1`) designed for bulk loading, and its clients: fluxoterm (a
terminal viewer and editor), fluxoscope (a web app) and fluxolap for
LibreOffice Calc. This file is the design: why fluxolap is built the
way it is, how it's tested, and what it deliberately doesn't do. How to
build, run and use it is in [README.md](README.md).

## Context

OLAP engines written in C or C++ tend to share a class of bugs: crashes on unvalidated input, use-after-free, double-free, iterators that index the wrong container, inverted range or overlap checks, unbounded memory use -- most of them structural consequences of hand-rolled memory/lifetime management and ad hoc validation, not one-off typos.

`fluxolap` is a from-scratch OLAP engine in Rust that keeps the parts of the classic MOLAP design that are genuinely good (hierarchy edges out of the element struct, id-based cross-references instead of a pointer graph, splashing) while using Rust's type system to eliminate whole bug categories by construction, and explicit test design to catch the categories Rust *can't* eliminate for free (logic bugs: flipped booleans, off-by-one, self-comparison, wrong validation).

## Architecture

### Crate layout (Cargo workspace)

```
fluxolap/
  Cargo.toml                 # workspace
  crates/
    fluxolap-domain/              # Server/Database/Cube/Dimension/Element model, no I/O
    fluxolap-storage/              # cube cell storage (sharded per cube, hot/cold tiers)
    fluxolap-error/                 # the error model: ErrorCode + FluxolapError
    fluxolap-engine/                # AppState + persistence (state), business logic incl. batch cell read/write (ops), dimension create/rename/destroy with provisioning (provision)
    fluxolap-api-types/             # the API's request/response types, shared by the server and the clients
    fluxolap-api/                   # REST API at /api/v1 (axum handlers, problem details, OpenAPI)
    fluxolap-examples/              # builds the example databases from examples/sources
    fluxolap-server/                # binary: config, main(), serves fluxolap-api and fluxoscope
    fluxoterm/                      # the terminal viewer/editor (ratatui)
    fluxoscope/                     # the web app (Leptos, WebAssembly)
  clients/libreoffice/          # fluxolap for LibreOffice Calc (Python extension)
  xtask/                        # dev benchmarks/diagnostics, the example-database build
```

The engine is split into domain model, storage and operations, which lets `fluxolap-domain`/`fluxolap-storage` be property-tested in complete isolation from async/HTTP concerns, which matters because nearly every bug category we're defending against lives in validation/indexing logic, not networking.

### Async runtime & HTTP: tokio + axum

axum's extractors plus a typed error: every handler that can fail returns `ApiResult<...>` (`Result<_, ApiError>`), nothing panics, every error path is typed. Engine operations return `FluxolapError`; `fluxolap-api` turns it into an RFC 9457 problem (HTTP status from the error code, `code`, `error_code`, `detail`, and `index` for batches).

### Concurrency: sharded locks, not MVCC

An MVCC-via-checked-out-clone design is elegant but complex, and complex locking is where overlap bugs hide (a lock-overlap check that compares an area to itself). fluxolap uses plain locks, sharded:

- **Sharded `RwLock`s**, *not* an `ArcSwap`/persistent-collection design. An inventory of the HTTP handlers found **no `.await` points in the handler layer** (every handler is `async fn` only to satisfy axum's trait; every body is fully synchronous, in-memory, microsecond-scale work) — with no I/O in any critical section, true lock-freedom buys little that well-scoped `std::sync::RwLock`s don't already give, while a persistent-collection rewrite would require abandoning or hand-re-deriving `SlotMap`'s stale-id safety guarantee (the direct thing defending against the use-after-free/dangling-iterator bug classes). Concretely: `AppState` now holds `databases`/`dimensions`/`cubes` as three *independent* `Arc<RwLock<...>>`s (a write to one no longer blocks a read of another), and `fluxolap-storage`'s `Storage` shards itself *per cube* internally (`RwLock<HashMap<CubeId, Arc<RwLock<CubePartition>>>>` — the outer lock is only ever touched briefly for cube create/destroy or an `Arc` lookup/clone; all actual cell/lock work happens under a specific cube's own inner lock). Domain metadata sharding stops at "one lock per table," not per-database — full per-database sharding was considered and rejected: it would require each database to own an independent `SlotMap` instance, and slotmap's generational-key safety only holds *within* one arena, so cross-database id confusion becomes structurally possible in a way it isn't today. Measured live: 300 concurrent writes to one cube add no measurable latency to a request against an unrelated cube.
- Splashing (interactive lock + rollback): an explicit `CubeLock` registry per cube plus an undo log (`Vec<(Vec<ElementId>, Option<CellValue>)>`) — commit = keep, rollback = replay. Lives inside each cube's `CubePartition`, so lock state shards along with everything else. The overlap check is `fluxolap-storage::areas_overlap`, property-tested against a brute-force reference (see Testing).

### Domain model: slotmap arenas, not a pointer graph

- `SlotMap<ElementId, Element>` etc. (via the `slotmap` crate) instead of a paged list plus an id index. Stale ids fail lookup (`None`) instead of aliasing freed memory — use-after-free and dangling-iterator bugs become impossible by construction.
- `Element`: small `Copy` struct (type/level/depth/indent), hierarchy edges in a separate per-dimension `Relations` map (children+weights, and the parent inverse) — hierarchy out of the element struct is good for cache locality and mutation isolation.
- Cross-references (Cube→Dimension, Cube→Database) are **ids, not `Rc`/pointers** — sidesteps ownership cycles, keeps snapshot "clone" cheap and shallow, directly defends against the shallow-clone double-free bug class.
- Typed newtype ids (`DatabaseId`, `DimensionId`, `ElementId`, `CubeId` via `slotmap::new_key_type!`) — catches "passed a cube id where a dimension id was expected" at compile time, which bare integer ids cannot.

## Testing strategy

### Property tests (`proptest`) — `fluxolap-domain`/`fluxolap-storage`

- **Hierarchy consistency**: after any proptest-generated sequence of append/move/replace/destroy — no cycles, child-set is the exact inverse of parent-set, level/depth/indent match a from-scratch recomputation.
- **Consolidation weight math**: consolidated cell = Σ(child × weight), checked against an independently-written naive recursive reference — two independent implementations of the same math is how you catch silent-wrong-result bugs (a wrong equality or overlap check), not just crashes.
- **Arena id stability**: destroying then recreating must never make the new element reachable via the old id (slotmap gives this for free; test it anyway as a regression guard).

### The "attack the known bug categories" pattern

For every handler taking a bulk/array param (bulk replace, bulk move, cell area, axes), write both:
- `accepts_well_formed_<x>` — happy path.
- `rejects_malformed_<x>` — too few/too many/empty/mismatched-paired-lengths/out-of-range/duplicate-where-unique-required, asserting a typed `Err`, not just "doesn't crash" (Rust already closes off the crash; what's tested is that the validation *logic* isn't inverted or off-by-one, since Rust's type system can't catch `if a.len() < b.len()` vs `<=`).
- For the highest-risk ones (bulk-replace weights), use a **property test over `(children_len, weights_len)` pairs**, not one hand-picked example — a length mismatch can fail in only one direction (weights shorter than children), and an example test would miss the other.
- **Lock-overlap symmetry test**: `overlaps(a, b) == overlaps(b, a)`, checked against a brute-force reference (enumerate small areas, intersect as sets) — not just symmetry, since the self-compare bug could satisfy symmetry vacuously. **[done: `fluxolap-storage::areas_overlap`'s `areas_overlap_matches_brute_force_point_set_reference` property test, plus a separate plain symmetry test.]**

### Concurrency testing

- `loom` not adopted -- the sharded-`RwLock` design was chosen specifically because it needs no hand-rolled lock-free/atomic machinery to reason about; nothing exists yet that would benefit from loom's exhaustive interleaving search over what std's `RwLock` already gets right.
- **[done, at the `fluxolap-storage` unit level]**: two `#[test]`s using `std::thread::scope` (no tokio needed, everything's synchronous) spawn real OS threads hammering `Storage` directly — one proves different cubes' concurrent writes never interfere (each thread's final value carries no other cube's data), the other proves same-cube concurrent writes to different cells lose no updates. Both assert exact expected final state, not just "no panic."
- **Not automated, but verified live**: an HTTP-level concurrent-client demonstration (300 background `curl` writes against one cube, timed requests against a completely unrelated cube during the barrage) showing no measurable latency increase — done manually, not committed as a repeatable CI test. Worth adding as a real `xtask`-style harness if concurrency regressions ever become a concern worth guarding against automatically.

### Fuzzing (`cargo-fuzz`)

- **Rule parser** — if a rule engine is ever built, keep its parser's entry point a pure `parse(&str) -> Result<Ast, ParseError>` so it's fuzzable.

Not worth fuzzing: domain/storage logic — proptest with domain-aware generators finds semantically-meaningful failures faster than byte-fuzzing a typed API.

## Explicit non-goals

**Permanent:**
- Authentication/authorization of any kind — no session store, no credential checking, no user rights system.
- Worker-process extensibility (dlopen'd extensions), GPU support, Windows support — bug-dense, platform-specific areas. fluxolap builds for Linux and macOS.

**Not now:**
- A rule/formula language (calculated cells such as `Mean = Sum / Count`); not built.
- HTTPS/TLS: plain HTTP (put a reverse proxy in front); if ever added, via `rustls`, never hand-rolled OpenSSL FFI.
- Reading other OLAP servers' save files (fluxolap has its own `serde`+`bincode` format; import through the API instead).
