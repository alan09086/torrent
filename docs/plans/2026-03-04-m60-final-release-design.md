# M60: Final Release — `torrent` v0.65.0

## Vision

Publish ferrite as `torrent` on crates.io — the default BitTorrent library for Rust. This milestone covers the rename, performance restoration, code quality hardening, fuzz scaffolding, and publication.

## Version

0.65.0 (not 1.0 — that comes after real-world validation through magnetor and external adopters).

---

## Phase 1: Identity (Rename `ferrite` → `torrent`)

Must land first — everything after uses the new names.

### Pre-Flight: Crate Name Availability Check

**Before starting any rename work**, verify all 11 crate names are available on crates.io:
- `torrent`, `torrent-bencode`, `torrent-core`, `torrent-wire`, `torrent-tracker`, `torrent-dht`, `torrent-storage`, `torrent-utp`, `torrent-nat`, `torrent-session`, `torrent-sim`
- Run `cargo search torrent --limit 10` and check each name individually
- If any name is taken, stop and choose an alternative namespace before proceeding

### Task 1: Crate Rename (Cargo.toml + Directories)

- Rename all 12 `crates/ferrite-*` directories to `crates/torrent-*`
- Rename `crates/ferrite` (facade) to `crates/torrent`
- Rename `crates/ferrite-cli` to `crates/torrent-cli`
- Update every `Cargo.toml`:
  - `name` field (e.g. `ferrite-session` → `torrent-session`)
  - Inter-crate dependency references
  - Workspace member paths in root `Cargo.toml`
- Root `Cargo.toml`: version → 0.65.0
- `torrent-cli`:
  - Binary name: `ferrite` → `torrent`
  - `publish = false` (not going to crates.io)
- Add crates.io metadata to all publishable crates:
  - `repository`, `homepage`, `documentation`, `keywords`, `categories`
- Add `rust-version = "1.85"` (MSRV — minimum for edition 2024) to all 11 publishable crates
- Verify `LICENSE` file exists at workspace root and is referenced correctly

### Task 2: Source Code Path Updates

- All `use ferrite_*::` → `use torrent_*::`
- All `use ferrite::` → `use torrent::`
- All `ferrite::prelude` → `torrent::prelude`
- Internal references in:
  - Doc comments and intra-doc links
  - Error messages and Display impls
  - Tracing spans and log messages
  - Example files, benchmark files, integration tests
  - `ferrite-sim` → `torrent-sim` internal references
- Verify example programs compile: `cargo build --examples`

### Task 3: Documentation & Repository Updates

- README.md — all crate name references, badges, code samples
- CHANGELOG.md — new 0.65.0 entry documenting the rename
- CLAUDE.md — update crate names throughout
- `docs/` plan files — references to ferrite (informational, low priority)
- CLI help text and `--version` output
- Archive old `ferrite` repos on Codeberg + GitHub
- Create new `torrent` repos, set up remotes

**Commit strategy**: Single atomic commit for the rename (T1+T2), separate commit for docs (T3).

---

## Phase 2: Performance (Non-Blocking Transfer Pipeline)

Re-implement the M57/M58 non-blocking pipeline. The 11 bugs that caused the original M57 revert are all fixed in v0.63.1–v0.64.0. The M58 design doc (`docs/plans/2026-03-04-m58-non-blocking-transfer-pipeline-design.md`) is the blueprint.

### Task 4: Non-Blocking Transfer Pipeline

Core architectural change — replace blocking disk I/O in TorrentActor with fire-and-forget channel sends:

- **Async disk types**: `DiskWriteError`, `VerifyResult` types
- **New DiskJob variants**: `WriteAsync`, `VerifyAsync`, `VerifyV2Async`
- **DiskHandle methods**: `enqueue_write()`, `enqueue_verify()`, `enqueue_verify_v2()`, `set_async_channels()`
- **DiskActor handlers**: `spawn_blocking` for async variants with flush-before-hash
- **TorrentActor channels**: `write_error_rx/tx`, `verify_result_rx/tx` wired into `select!` loop
- **`handle_piece_data()` rewrite**: zero-copy `Bytes` move, fire-and-forget `enqueue_write()` with `try_send()` backpressure fallback
- **Async v1 verification**: `enqueue_verify()` for V1Only; blocking kept for V2Only/Hybrid (Merkle needs sequential state)
- **In-memory v2 block hashing**: SHA-256 at receive time (~15us) instead of disk round-trip
- **Hybrid tracking**: `v2_passed_pending_v1` HashSet for v1/v2 inconsistency detection

Existing M58 implementation plan: `docs/plans/2026-03-04-m58-non-blocking-transfer-pipeline.md`

### Task 5: O(1) Peer Dedup

- **O(1) peer dedup** — `HashSet<SocketAddr>` in `handle_add_peers()` replacing linear scan
- ~~TCP 5s connect timeout~~ — already implemented (`peer_connect_timeout` in Settings, enforced in `try_connect_peers()`)

### Task 6: Live Verification + Criterion Benchmarks

- **Live download test**: Arch Linux ISO magnet — must achieve >30 MB/s (current ~29 MB/s, target 50–80 MB/s)
- **Criterion benchmark suite**:
  - Disk write throughput (blocking vs enqueue)
  - Bencode encode/decode
  - SHA-1 / SHA-256 hashing throughput
  - Piece picker selection
- Benchmarks in `benches/` directory, not gating CI yet (that's post-0.65.0)

**Expected impact**: 2–3x speedup from removing the ~1ms write await per 16 KiB block.

---

## Phase 3: Code Quality

### Task 7: `rustfmt.toml` + Format Enforcement

Add `rustfmt.toml` at workspace root:
```toml
max_width = 100
imports_granularity = "Crate"
group_imports = "StdExternalCrate"
```

Run `cargo fmt --all`. Single mechanical commit.

### Task 8: Clippy Lint Expansion

Add to workspace `Cargo.toml` or `clippy.toml`:
- **Deny**: `clippy::await_holding_lock`, `clippy::large_enum_variant`
- **Warn**: `clippy::needless_pass_by_value`, `clippy::inefficient_to_string`

Fix all deny-level violations. Warn-level are informational only.

### Task 9: Panic Audit (Network-Facing Code)

Search for `unwrap()`, `expect()`, `panic!()` in network-facing crates:
- `torrent-wire` — peer message parsing
- `torrent-tracker` — tracker response parsing
- `torrent-dht` — KRPC parsing
- `torrent-bencode` — decode paths
- `torrent-session` — peer task, incoming connections

Replace with proper error propagation. Internal-only code (test helpers, infallible paths with proof) can keep `unwrap()`.

### Task 10: Resource Exhaustion Limits Audit

Audit and add configurable bounds for:
- Maximum metadata size (BEP 9 — cap at e.g. 100 MiB)
- Maximum piece length (sanity bound)
- Maximum peer message size (prevent OOM from malicious frames)
- Outstanding request limits per peer

Add bounds checks where missing, wire to `Settings` where appropriate.

---

## Phase 4: Fuzz Scaffolding

### Task 11: Fuzz Architecture Setup

- `cargo fuzz init` at workspace level
- Create 4 fuzz target stubs (compile + run, no assertions beyond "no panic"):

| Target | Crate | Parsing Surface |
|--------|-------|----------------|
| `fuzz_bencode` | torrent-bencode | Arbitrary bencode input |
| `fuzz_peer_wire` | torrent-wire | Peer wire message frames |
| `fuzz_tracker` | torrent-tracker | HTTP + UDP tracker responses |
| `fuzz_metadata` | torrent-core | .torrent v1/v2/hybrid + magnets |

- Seed corpus from existing test fixtures
- Write `docs/fuzzing.md` — how to run fuzz targets, future OSS-Fuzz plan
- **Actual fuzzing campaigns and OSS-Fuzz integration deferred to M61**

---

## Phase 5: Publication

### Task 12: README Overhaul

- Vision statement — "The default BitTorrent library for Rust"
- Feature matrix — 27 BEPs, v1/v2/hybrid, streaming, simulation
- Architecture diagram — crate dependency graph with new names
- Getting started — code samples with `torrent::` imports
- Performance section — benchmark table (vs rqbit, libtorrent)
- Security posture — fuzzing, panic-free network paths, SSRF mitigation
- Configuration — Settings overview, presets
- Stability guarantees — "0.x, API may change"

### Task 13: Architecture Documentation

- `docs/architecture.md` — crate roles, actor model, data flow, concurrency
  - Include **crash containment** section: `spawn_blocking` panic behaviour, channel closure semantics, backpressure fallback documentation for the async pipeline
- `docs/configuration.md` — Settings fields reference, presets, runtime mutation
- `docs/security.md` — threat model, mitigations (SSRF, IP filter, smart ban, encryption)

Concise reference material, not tutorials.

### Task 14: crates.io Publication

- Pre-flight: `cargo audit` + `cargo deny check` (supply chain hygiene)
- `cargo publish --dry-run` on all 11 crates (excludes `torrent-cli`)
- Publish in dependency order:
  1. `torrent-bencode`
  2. `torrent-core`
  3. `torrent-wire`, `torrent-tracker`, `torrent-dht`, `torrent-storage`, `torrent-utp`, `torrent-nat`
  4. `torrent-session`
  5. `torrent-sim`
  6. `torrent` (facade)
- Git tag `v0.65.0`, push tags to both remotes
- Verify docs.rs builds for all crates
- Archive old ferrite repos on Codeberg + GitHub
- Create new `torrent` repos with redirects

---

## Out of Scope (M61+)

- OSS-Fuzz integration
- Fuzz campaign execution
- Sanitiser testing (ASan, TSan, LeakSan)
- Soak testing (24–72h stress tests)
- 1.0 version bump (after magnetor validation)
- CI pipeline (GitHub Actions / Codeberg CI)

---

## Success Criteria

- All 12 crates compile and pass tests under `torrent-*` names
- `cargo test --workspace` passes (1378+ tests)
- `cargo build --examples` compiles all example programs
- `cargo clippy --workspace -- -D warnings` clean
- Live BT download achieves >30 MB/s (target 50–80 MB/s)
- No `unwrap()`/`expect()`/`panic!()` on network-facing code paths
- `rust-version` (MSRV) set on all 11 publishable crates
- `cargo audit` reports no critical vulnerabilities
- All 11 library crates published on crates.io
- docs.rs builds successfully for all crates
- Fuzz targets compile and run without panics on seed corpus
