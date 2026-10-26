Suggested improvements we maybe decide about later

# fluxolap review: bugs and improvements

fluxolap 1.0 was reviewed with Sonnet 5.5. The findings come from a read-only sweep of the server crates, and the top items were re-checked against the code. Nothing was built or run, and fluxoterm, fluxoscope and the LibreOffice client were only skimmed.

## Verified findings (code re-read)

1. **Non-atomic saves**: `crates/fluxolap-engine/src/state.rs` (around lines 469 and 729) uses `File::create(path)` directly. A crash or full disk mid-save truncates the only copy. On the next start the unreadable file is skipped and its registry entry pruned, so the data is lost.
2. **Lost-update race in `save_if_dirty`** (`state.rs`, around lines 463-471): the dirty flag is cleared after the snapshot, and dimensions, cubes and storage are cloned under three separate locks. A write in between is not saved and not re-marked dirty, and the file can be internally inconsistent.
3. **`save_all` aborts on the first error** (`state.rs`, around line 535): `save_if_dirty(id)?` means one failing database stops the rest from being saved at shutdown.
4. **Splash overwrites existing data** (`crates/fluxolap-engine/src/ops.rs`, around line 439): `old_sum != 0.0` is used to mean "area is empty". Cells that sum to 0 (for example +1/-1 weights with values 5 and 5) take the empty-area branch and are overwritten with `target / weight_sum`.
5. **Duplicate element positions** (`crates/fluxolap-domain/src/dimension.rs`, around line 139): `position = elements.len()` collides with existing positions after a delete, which breaks `move_element` and element ordering.
6. **`in_flight` metric leak** (`crates/fluxolap-api/src/handlers/stats.rs`, around line 55): if the client disconnects, the future is dropped before `fetch_sub`, so the counter drifts up forever. A drop guard would fix it.

## Reported by the exploration pass (not re-verified)

- Lock poisoning cascade: `std::sync::RwLock` with `.expect` everywhere, so one panic bricks a database. This contradicts the "nothing panics" claim in PLAN.md.
- No `spawn_blocking`: zstd, bincode, rayon scans and whole-dimension clones run on tokio workers.
- The import endpoint takes unbounded bincode with no structural validation (OOM or panic from a crafted file). With no auth and CORS `*`, this is a remote DoS.
- `resolve_loaded` race: two first accesses both load, and the second insert wins, so writes to the first are lost.
- Unbounded recursion in `level()` and `expand_to_base()` (stack overflow on deep chains, exponential on diamonds).
- `delete_elements` leaves orphan cells at consolidated elements. Lock creation has a check-then-create race. The undo log grows without bound.
- `sum_consolidated` allocates O(blocks x cells) per query, and `with_dimension_copy` clones the whole dimension on every element edit.
- Cube creation does not reject duplicate or foreign dimension ids. System and attribute cubes and dimensions can be deleted through the generic endpoints.
- Dockerfile: floating `rust:1-slim`, unchecked trunk download, runs as root, no `.dockerignore`, no HEALTHCHECK.
- `main.rs`: a bad `FLUXOLAP_PORT` silently falls back to 7777, and a registry load error starts empty and later overwrites the data.
- CI only runs on release tags, so the roughly 190 tests never run on push or PR.

## Suggested order if fixes are wanted

1. **CI**: add a push/PR workflow running `cargo test --workspace` and clippy.
2. **Persistence**: atomic save (temp file, fsync, rename). Mark dirty before the snapshot and clear it under one consistent snapshot. Make `save_all` continue past errors and report them at the end.
3. **Splash**: decide "empty" by stored-cell count rather than `old_sum != 0.0`, with a regression test.
4. **Element position**: use `max(position) + 1` or renumber on delete, with a test.
5. **Metrics**: replace the `in_flight` counter with a drop guard.
6. **Import hardening**: size limits and structural validation after deserialize.
7. **Robustness (larger)**: handle lock poisoning, use `spawn_blocking` for heavy work, and make `level` and `expand_to_base` iterative.
8. **Dockerfile**: pin versions, add a non-root user and `.dockerignore`.

## Verification once fixes are made

- `cargo test --workspace` and `cargo clippy --workspace -- -D warnings`.
- New regression tests: cancelling-weight splash, delete-then-create element position, truncated database file on load, save with one failing database.
- Manual: kill the server mid-save and confirm the previous file survives.
