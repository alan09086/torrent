# M67: Crypto Optimization — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace slow SHA1/SHA256 assembly with BoringSSL-grade crypto via `ring`, and eliminate per-write Vec allocation in RC4 stream.

**Architecture:** Switch `sha1`/`sha2` crates to `ring` in `torrent-core` and `torrent-wire`. The `ring` crate uses BoringSSL's hand-optimized assembly (AVX2 on Broadwell, SHA-NI on Zen 1+/Ice Lake+). Also add `.cargo/config.toml` with `target-cpu=native` for optimal codegen across all crates. Fix the RC4 write path to use a reusable buffer instead of allocating per write.

**Tech Stack:** `ring` crate (BoringSSL assembly), Rust unsafe for in-place buffer encryption

**Profiling baseline (v0.69.0):**
- SHA1: 44.5% CPU (8.05s), 174 MB/s — `sha1-asm` software fallback
- RC4: 11.8% CPU (2.14s) — custom Rust, plus Vec alloc per write
- OpenSSL SHA1 on same CPU: 1.75s, 800 MB/s (4.6x faster)

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Create | `.cargo/config.toml` | Set `target-cpu=native` for all builds |
| Modify | `crates/torrent-core/Cargo.toml` | Replace `sha1`/`sha2` with `ring` |
| Modify | `crates/torrent-core/src/lib.rs` | Rewrite `sha1()`, `sha1_chunks()`, `sha256()` |
| Modify | `crates/torrent-wire/Cargo.toml` | Replace `sha1` with `ring` |
| Modify | `crates/torrent-wire/src/mse/crypto.rs` | Rewrite `mse_hash()` etc. to use `ring` |
| Modify | `crates/torrent-wire/src/mse/stream.rs` | Eliminate Vec alloc in `poll_write` |
| Modify | Root `Cargo.toml` | Add `ring` to workspace dependencies |

---

## Task 1: Add `target-cpu=native` Build Config

**Files:**
- Create: `.cargo/config.toml`

This enables AVX2/SHA-NI codegen for ALL crates, not just the crypto ones.

- [ ] **Step 1: Create `.cargo/config.toml`**

```toml
[build]
rustflags = ["-C", "target-cpu=native"]
```

- [ ] **Step 2: Verify build succeeds**

Run: `cargo build -p torrent-core --release 2>&1 | tail -5`
Expected: Successful build

- [ ] **Step 3: Verify tests still pass**

Run: `cargo test --workspace 2>&1 | grep "^test result:" | tail -5`
Expected: All 1395 tests pass

- [ ] **Step 4: Commit**

```bash
git add .cargo/config.toml
git commit -m "build: set target-cpu=native for optimal codegen (M67)"
```

---

## Task 2: Switch `torrent-core` from `sha1`/`sha2` to `ring`

**Files:**
- Modify: `Cargo.toml` (root workspace)
- Modify: `crates/torrent-core/Cargo.toml`
- Modify: `crates/torrent-core/src/lib.rs`

### Background

The `ring` crate provides SHA1 and SHA256 via `ring::digest`. Key API differences:
- `ring::digest::digest(&SHA1_FOR_LEGACY_USE_ONLY, data)` → returns `Digest` (`.as_ref()` → `&[u8]`)
- For incremental hashing: `ring::digest::Context::new(&SHA1_FOR_LEGACY_USE_ONLY)` → `.update(data)` → `.finish()`
- SHA256: `ring::digest::digest(&SHA256, data)`

The function `sha1_for_legacy_use_only` name is ring's way of flagging SHA1's cryptographic weakness — it works fine, the name is just a warning.

- [ ] **Step 1: Add `ring` to workspace dependencies**

In root `Cargo.toml`, under `[workspace.dependencies]`:

```toml
ring = "0.17"
```

- [ ] **Step 2: Update `torrent-core/Cargo.toml`**

Remove `sha1` and `sha2` dependencies. Add `ring`:

Replace:
```toml
sha1 = { version = "0.10", features = ["asm"] }
sha2 = { version = "0.10", features = ["asm"] }
```

With:
```toml
ring = { workspace = true }
```

- [ ] **Step 3: Rewrite hash functions in `torrent-core/src/lib.rs`**

Replace the three hash functions (around lines 55-79):

```rust
/// Compute SHA1 hash of input bytes.
pub fn sha1(data: &[u8]) -> Id20 {
    let hash = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, data);
    let mut id = [0u8; 20];
    id.copy_from_slice(hash.as_ref());
    Id20(id)
}

/// Compute SHA1 hash of multiple chunks without concatenating them.
///
/// Avoids allocating a large buffer when piece data is stored as separate blocks.
pub fn sha1_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Id20 {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    for chunk in chunks {
        ctx.update(chunk);
    }
    let hash = ctx.finish();
    let mut id = [0u8; 20];
    id.copy_from_slice(hash.as_ref());
    Id20(id)
}

/// Compute SHA-256 hash of input bytes (used by BitTorrent v2, BEP 52).
pub fn sha256(data: &[u8]) -> Id32 {
    let hash = ring::digest::digest(&ring::digest::SHA256, data);
    let mut id = [0u8; 32];
    id.copy_from_slice(hash.as_ref());
    Id32(id)
}
```

Also remove the `use sha1::Digest;` and `use sha2::Digest;` imports if they exist at module level (they're currently inside the functions).

- [ ] **Step 4: Check for any other `sha1::`/`sha2::` usage in torrent-core**

Search: `grep -rn "sha1::\|sha2::" crates/torrent-core/src/`
If any found, update them to use `ring::digest`.

The `sha256_chunks()` function may also exist (check `torrent-core/src/lib.rs` around line 80+). If so, convert it similarly:

```rust
pub fn sha256_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Id32 {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    for chunk in chunks {
        ctx.update(chunk);
    }
    let hash = ctx.finish();
    let mut id = [0u8; 32];
    id.copy_from_slice(hash.as_ref());
    Id32(id)
}
```

- [ ] **Step 5: Verify torrent-core tests pass**

Run: `cargo test -p torrent-core 2>&1 | tail -10`
Expected: All tests pass (hash computations produce identical results)

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/torrent-core/Cargo.toml crates/torrent-core/src/
git commit -m "perf: switch torrent-core from sha1/sha2 to ring (BoringSSL asm) (M67)"
```

---

## Task 3: Switch `torrent-wire` from `sha1` to `ring`

**Files:**
- Modify: `crates/torrent-wire/Cargo.toml`
- Modify: `crates/torrent-wire/src/mse/crypto.rs`

The MSE module uses SHA1 for handshake key derivation (`mse_hash()`, `mse_hash2()`, `mse_hash3()`).

- [ ] **Step 1: Update `torrent-wire/Cargo.toml`**

Remove `sha1` dependency. Add `ring`:

Replace:
```toml
sha1 = { version = "0.10", features = ["asm"] }
```

With:
```toml
ring = { workspace = true }
```

- [ ] **Step 2: Read current `mse/crypto.rs` hash functions**

Read the file to see the current implementations (lines ~17-40). They use `sha1::Sha1::new()` + `update()` + `finalize()`.

- [ ] **Step 3: Rewrite MSE hash functions**

Convert all `sha1::Sha1` usage to `ring::digest::Context`:

```rust
/// Compute HASH(data) = SHA1(data). Returns 20 bytes.
#[allow(dead_code)]
pub(crate) fn mse_hash(data: &[u8]) -> [u8; 20] {
    let hash = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, data);
    let mut out = [0u8; 20];
    out.copy_from_slice(hash.as_ref());
    out
}

/// Compute HASH(a, b) = SHA1(a || b). Returns 20 bytes.
pub(crate) fn mse_hash2(a: &[u8], b: &[u8]) -> [u8; 20] {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(a);
    ctx.update(b);
    let hash = ctx.finish();
    let mut out = [0u8; 20];
    out.copy_from_slice(hash.as_ref());
    out
}

/// Compute HASH(a, b, c) = SHA1(a || b || c). Returns 20 bytes.
pub(crate) fn mse_hash3(a: &[u8], b: &[u8], c: &[u8]) -> [u8; 20] {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(a);
    ctx.update(b);
    ctx.update(c);
    let hash = ctx.finish();
    let mut out = [0u8; 20];
    out.copy_from_slice(hash.as_ref());
    out
}
```

- [ ] **Step 4: Verify torrent-wire tests pass**

Run: `cargo test -p torrent-wire 2>&1 | tail -10`
Expected: All tests pass (MSE handshake tests exercise these functions end-to-end)

- [ ] **Step 5: Commit**

```bash
git add crates/torrent-wire/Cargo.toml crates/torrent-wire/src/
git commit -m "perf: switch torrent-wire MSE hash from sha1 to ring (M67)"
```

---

## Task 4: Remove `sha1`/`sha2` crate dependencies from remaining crates

**Files:**
- Modify: `crates/torrent-utp/Cargo.toml` (has `sha1` in dev-dependencies)
- Possibly others — search first

- [ ] **Step 1: Find all remaining sha1/sha2 references**

Run: `grep -rn "sha1\|sha2" crates/*/Cargo.toml`

For each remaining reference:
- If it's a dev-dependency used in tests, switch to `ring`
- If it's unused, remove it

- [ ] **Step 2: Update each crate's Cargo.toml**

For `torrent-utp/Cargo.toml`, replace:
```toml
sha1 = { version = "0.10", features = ["asm"] }
```
With:
```toml
ring = { workspace = true }
```

Update any test code that uses `sha1::` to use `ring::digest::`.

- [ ] **Step 3: Full workspace test**

Run: `cargo test --workspace 2>&1 | tail -10`
Run: `cargo clippy --workspace -- -D warnings 2>&1 | tail -5`
Expected: All pass, zero warnings

- [ ] **Step 4: Verify sha1/sha2 crates are gone from dependency tree**

Run: `cargo tree -e features | grep -E "^.*(sha1|sha2) " | head -10`
Expected: No direct `sha1`/`sha2` crate dependencies remain (only transitive if `ring` pulls them, which it doesn't)

- [ ] **Step 5: Commit**

```bash
git add crates/*/Cargo.toml crates/*/src/ Cargo.lock
git commit -m "chore: remove sha1/sha2 crate deps, all crypto via ring (M67)"
```

---

## Task 5: Eliminate Vec allocation in RC4 `poll_write`

**Files:**
- Modify: `crates/torrent-wire/src/mse/stream.rs`

### Problem

In `MseStream::poll_write` (line 70-73), every write allocates a new Vec:

```rust
let mut encrypted = buf.to_vec();  // <-- allocation per write
cipher.apply(&mut encrypted);
Pin::new(&mut this.inner).poll_write(cx, &encrypted)
```

This is called for every outgoing message on every RC4-encrypted peer connection.

### Fix

Store a reusable buffer on the `MseStream` struct:

- [ ] **Step 1: Add write buffer field to `MseStream`**

```rust
pub struct MseStream<S> {
    inner: S,
    read_cipher: Option<Rc4>,
    write_cipher: Option<Rc4>,
    write_buf: Vec<u8>,
}
```

Update both constructors (`plaintext()` and `encrypted()`) to initialize `write_buf: Vec::new()`.

- [ ] **Step 2: Rewrite `poll_write` to reuse the buffer**

```rust
fn poll_write(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &[u8],
) -> Poll<io::Result<usize>> {
    let this = self.get_mut();

    if let Some(cipher) = &mut this.write_cipher {
        this.write_buf.clear();
        this.write_buf.extend_from_slice(buf);
        cipher.apply(&mut this.write_buf);
        Pin::new(&mut this.inner).poll_write(cx, &this.write_buf)
    } else {
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }
}
```

Note: `extend_from_slice` on a pre-allocated Vec reuses the existing capacity. After the
first write, subsequent writes of similar size cause zero allocations.

- [ ] **Step 3: Run existing tests**

Run: `cargo test -p torrent-wire -- mse 2>&1 | tail -10`
Expected: All MSE/stream tests pass (plaintext + encrypted roundtrip)

- [ ] **Step 4: Full workspace test + clippy**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/torrent-wire/src/mse/stream.rs
git commit -m "perf: eliminate per-write Vec allocation in RC4 stream (M67)"
```

---

## Task 6: Reduce Default Peer Count 200 → 128

**Files:**
- Modify: `crates/torrent-session/src/settings.rs`

- [ ] **Step 1: Update default**

In `settings.rs`, change `default_max_peers_per_torrent()` (around line 172):

```rust
fn default_max_peers_per_torrent() -> usize {
    128
}
```

- [ ] **Step 2: Update high-performance preset**

In `Settings::high_performance()` (around line 846), change from 500 to 250:

```rust
max_peers_per_torrent: 250,
```

- [ ] **Step 3: Update any test assertions that check the default**

Search: `grep -rn "max_peers_per_torrent\|200" crates/torrent-session/src/settings.rs`
Update assertions that compare to 200.

- [ ] **Step 4: Run tests**

Run: `cargo test -p torrent-session -- settings 2>&1 | tail -10`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/torrent-session/src/settings.rs
git commit -m "perf: reduce default max peers 200 → 128 (M67)"
```

---

## Task 7: Benchmark and Verify

- [ ] **Step 1: Build release with debug symbols**

```bash
CARGO_PROFILE_RELEASE_DEBUG=2 cargo build --release -p torrent-cli
```

- [ ] **Step 2: Run flamegraph**

```bash
flamegraph -o benchmarks/flamegraph-v0.70.0.svg -- \
    target/release/torrent download \
    "magnet:?xt=urn:btih:a4373c326657898d0c588c3ff892a0fac97ffa20&dn=archlinux-2026.03.01-x86_64.iso" \
    -o /tmp/torrent-bench/profile -q
```

- [ ] **Step 3: Run 3-trial benchmark**

```bash
for i in 1 2 3; do
    rm -rf /tmp/torrent-bench/trial$i && mkdir -p /tmp/torrent-bench/trial$i
    /usr/bin/time -v target/release/torrent download \
        "magnet:?xt=urn:btih:a4373c326657898d0c588c3ff892a0fac97ffa20&dn=archlinux-2026.03.01-x86_64.iso" \
        -o /tmp/torrent-bench/trial$i -q 2>&1 | grep -E "peak_peers|final_speed|Maximum resident|User time"
done
```

Expected: User CPU ~12s (down from 18.1s), RSS ~70 MiB.

- [ ] **Step 4: Verify SHA1 is no longer #1 hotspot**

Run: `perf report --stdio --no-children -g none --percent-limit 1`
Expected: SHA1 drops from 44.5% to ~10-15%.

- [ ] **Step 5: Save benchmark results**

Update `docs/benchmark-report-v0.70.0.md` with new results.

---

## Verification Checklist

1. `cargo test --workspace` — all tests pass (1395+)
2. `cargo clippy --workspace -- -D warnings` — clean
3. Full download test (Arch ISO) — completes without stall
4. Flamegraph captured — SHA1 no longer dominant
5. Benchmark: CPU time reduced from ~18s baseline
6. No `sha1`/`sha2` crate in direct dependencies

## Commit Plan

1. `build: set target-cpu=native for optimal codegen (M67)`
2. `perf: switch torrent-core from sha1/sha2 to ring (BoringSSL asm) (M67)`
3. `perf: switch torrent-wire MSE hash from sha1 to ring (M67)`
4. `chore: remove sha1/sha2 crate deps, all crypto via ring (M67)`
5. `perf: eliminate per-write Vec allocation in RC4 stream (M67)`
6. `perf: reduce default max peers 200 → 128 (M67)`
7. `chore: bump version to 0.70.0 (M67)`
8. `docs: update README and CHANGELOG for M67`
