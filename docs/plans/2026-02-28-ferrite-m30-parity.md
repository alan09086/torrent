# M30 Parity: libtorrent-rasterbar Gap Closure

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Close all actionable gaps between ferrite's `CreateTorrent` and libtorrent-rasterbar's `create_torrent` for v1 torrents.

**Architecture:** Seven independent hardening items (A–G) applied to the existing `CreateTorrent` builder in `ferrite-core`. Each adds a test first, then the minimal implementation. No new crate dependencies. Parallel hashing uses `std::thread::scope` from std.

**Tech Stack:** Rust (edition 2024), `std::thread::scope`, `sha1` crate (already a dep), `tempfile` (dev-dep)

---

## Overview

| Task | Gap | Severity | New Tests |
|------|-----|----------|-----------|
| 1 | A — Piece size upper limit (128 MiB max) | Hardening | 1 |
| 2 | B — `no_attributes` flag | Cross-platform | 1 |
| 3 | G — Symlink correctness (both modes) | Correctness bug | 2 |
| 4 | E — BEP 35 SSL cert (`set_root_cert`) | Feature | 1 |
| 5 | D — BEP 38 similar torrents + collections | Feature | 1 |
| 6 | C — Construct from existing torrent | Feature | 2 |
| 7 | F — Parallel hashing | Performance | 1 |

Total: 9 new tests (14 existing → 23)

**Files touched across all tasks:**

| File | Tasks |
|------|-------|
| `crates/ferrite-core/src/create.rs` | 1–7 |
| `crates/ferrite-core/src/metainfo.rs` | 4, 5 |

No facade changes needed — `CreateTorrent` and `CreateTorrentResult` are already re-exported.

---

### Task 1: Piece size upper limit (Gap A)

libtorrent rejects piece sizes > 128 MiB. Ferrite has no upper bound.

**Files:**
- Modify: `crates/ferrite-core/src/create.rs` (validation + test)

**Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `create.rs`:

```rust
#[test]
fn piece_size_upper_limit() {
    let f = make_test_file();
    // 128 MiB should be fine
    let ok = CreateTorrent::new()
        .add_file(f.path())
        .set_piece_size(128 * 1024 * 1024)
        .set_creation_date(1000000)
        .generate();
    assert!(ok.is_ok());

    // 256 MiB should fail
    let err = CreateTorrent::new()
        .add_file(f.path())
        .set_piece_size(256 * 1024 * 1024)
        .set_creation_date(1000000)
        .generate();
    assert!(err.is_err());
    assert!(err.unwrap_err().to_string().contains("128 MiB"));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferrite-core create::tests::piece_size_upper_limit -- --nocapture`
Expected: FAIL — 256 MiB currently accepted.

**Step 3: Write minimal implementation**

In `generate_with_progress()`, expand the existing piece size validation block (line ~281):

```rust
// Validate piece size if explicitly set
if let Some(ps) = self.piece_size {
    if ps < 16384 || !ps.is_power_of_two() {
        return Err(Error::CreateTorrent(
            "piece size must be a power of 2 and at least 16384".into(),
        ));
    }
    if ps > 128 * 1024 * 1024 {
        return Err(Error::CreateTorrent(
            "piece size must not exceed 128 MiB".into(),
        ));
    }
}
```

Note: the existing code uses `let-else` chaining (`if let Some(ps) = self.piece_size && ...`). Split into two separate checks for clarity.

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferrite-core create::tests::piece_size_upper_limit -- --nocapture`
Expected: PASS

**Step 5: Run full suite**

Run: `cargo test -p ferrite-core create -- --nocapture`
Expected: 15 tests pass (14 existing + 1 new)

**Step 6: Commit**

```bash
git add crates/ferrite-core/src/create.rs
git commit -m "feat: reject piece sizes > 128 MiB (M30 parity)"
```

---

### Task 2: `no_attributes` flag (Gap B)

libtorrent's `no_attributes` flag ignores filesystem attributes (executable, hidden) for cross-platform consistency.

**Files:**
- Modify: `crates/ferrite-core/src/create.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn no_attributes_flag() {
    let dir = tempfile::tempdir().unwrap();
    // Create a hidden file and an executable file
    let hidden = dir.path().join(".hidden_file");
    fs::write(&hidden, b"secret").unwrap();
    let exec = dir.path().join("run.sh");
    fs::write(&exec, b"#!/bin/sh").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exec, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Without no_attributes: attrs should be present
    let with_attrs = CreateTorrent::new()
        .add_directory(dir.path())
        .set_name("attrs")
        .set_piece_size(32768)
        .set_creation_date(1000000)
        .generate()
        .unwrap();
    let files = with_attrs.meta.info.files.as_ref().unwrap();
    assert!(files.iter().any(|f| f.attr.is_some()), "should have attributes");

    // With no_attributes: no attrs anywhere
    let without_attrs = CreateTorrent::new()
        .no_attributes(true)
        .add_directory(dir.path())
        .set_name("no-attrs")
        .set_piece_size(32768)
        .set_creation_date(1000000)
        .generate()
        .unwrap();
    let files2 = without_attrs.meta.info.files.as_ref().unwrap();
    assert!(
        files2.iter().all(|f| f.attr.is_none()),
        "should have no attributes, got: {:?}",
        files2.iter().map(|f| (&f.path, &f.attr)).collect::<Vec<_>>()
    );
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferrite-core create::tests::no_attributes_flag -- --nocapture`
Expected: FAIL — `no_attributes()` method doesn't exist.

**Step 3: Write minimal implementation**

Add field to `CreateTorrent` struct:

```rust
no_attributes: bool,
```

Initialize to `false` in `new()`.

Add builder method (after `include_symlinks`):

```rust
/// Ignore filesystem attributes (hidden, executable) for cross-platform consistency.
pub fn no_attributes(mut self, enabled: bool) -> Self {
    self.no_attributes = enabled;
    self
}
```

Modify `add_file()` — change the `detect_attr` call:

```rust
let attr = if self.no_attributes { None } else { detect_attr(&canonical, &meta) };
```

Modify `walk_directory()` signature — add `no_attributes: bool` parameter:

```rust
fn walk_directory(
    base: &Path,
    prefix: &[String],
    out: &mut Vec<InputFile>,
    include_mtime: bool,
    include_symlinks: bool,
    no_attributes: bool,
)
```

In `walk_directory`, for normal files (the `file_type.is_file()` branch):

```rust
let attr = if no_attributes { None } else { detect_attr(&entry_path, &meta) };
```

In `walk_directory`, for symlinks (the `file_type.is_symlink() && include_symlinks` branch):

```rust
attr: if no_attributes { None } else { Some("l".into()) },
```

Wait — symlink "l" attribute is semantic, not filesystem-derived. libtorrent's `no_attributes` skips `flag_hidden` and `flag_executable` but preserves `flag_symlink` and `flag_pad_file`. So: keep "l" and "p" attributes even when `no_attributes` is true. Only suppress "h" and "x".

Revised `detect_attr`:

```rust
fn detect_attr(path: &Path, meta: &fs::Metadata, no_attributes: bool) -> Option<String> {
    if no_attributes {
        return None;
    }
    // ... existing logic ...
}
```

And symlink attr stays `Some("l")` regardless.

Update the two call sites of `walk_directory` in `add_directory()`:

```rust
walk_directory(&canonical, &[], &mut files, self.include_mtime, self.include_symlinks, self.no_attributes);
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferrite-core create::tests::no_attributes_flag -- --nocapture`
Expected: PASS

**Step 5: Run full suite**

Run: `cargo test -p ferrite-core create -- --nocapture`
Expected: 16 tests pass

**Step 6: Commit**

```bash
git add crates/ferrite-core/src/create.rs
git commit -m "feat: add no_attributes() for cross-platform torrent creation (M30 parity)"
```

---

### Task 3: Symlink correctness (Gap G)

Two bugs:
1. When `include_symlinks(false)` (default): symlinked files/dirs are silently dropped instead of being followed.
2. When `include_symlinks(true)`: symlink data is included in piece hashing, but BEP 47 says it must NOT be.

**Files:**
- Modify: `crates/ferrite-core/src/create.rs`

**Step 1: Write the failing tests**

```rust
#[cfg(unix)]
#[test]
fn symlink_followed_when_disabled() {
    use std::os::unix::fs as unix_fs;

    let dir = tempfile::tempdir().unwrap();
    let real_file = dir.path().join("real.txt");
    fs::write(&real_file, b"real content here").unwrap();
    // Create symlink: link.txt -> real.txt
    unix_fs::symlink(&real_file, dir.path().join("link.txt")).unwrap();

    // include_symlinks=false (default): symlink should be followed, data included
    let result = CreateTorrent::new()
        .add_directory(dir.path())
        .set_name("follow-links")
        .set_piece_size(32768)
        .set_creation_date(1000000)
        .generate()
        .unwrap();

    let files = result.meta.info.files.as_ref().unwrap();
    // Both real.txt and link.txt should appear (link followed as regular file)
    assert_eq!(files.len(), 2, "expected 2 files, got: {:?}", files.iter().map(|f| &f.path).collect::<Vec<_>>());
    // Both should have real content length
    assert!(files.iter().all(|f| f.length == 17), "all files should have length 17");
    // Neither should have symlink attr
    assert!(files.iter().all(|f| f.attr.as_deref() != Some("l")));
}

#[cfg(unix)]
#[test]
fn symlink_zero_length_when_enabled() {
    use std::os::unix::fs as unix_fs;

    let dir = tempfile::tempdir().unwrap();
    let real_file = dir.path().join("real.txt");
    fs::write(&real_file, b"real content here").unwrap();
    unix_fs::symlink(&real_file, dir.path().join("link.txt")).unwrap();

    // include_symlinks=true: symlink recorded with zero length, data NOT included
    let result = CreateTorrent::new()
        .include_symlinks(true)
        .add_directory(dir.path())
        .set_name("record-links")
        .set_piece_size(32768)
        .set_creation_date(1000000)
        .generate()
        .unwrap();

    let files = result.meta.info.files.as_ref().unwrap();
    assert_eq!(files.len(), 2);
    let link = files.iter().find(|f| f.path.contains(&"link.txt".to_string())).unwrap();
    assert_eq!(link.length, 0, "symlink should have zero length");
    assert_eq!(link.attr.as_deref(), Some("l"));
    assert!(link.symlink_path.is_some());

    let real = files.iter().find(|f| f.path.contains(&"real.txt".to_string())).unwrap();
    assert_eq!(real.length, 17, "real file should have actual length");

    // Total length should only include real file data
    assert_eq!(result.meta.info.total_length(), 17);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p ferrite-core create::tests::symlink_ -- --nocapture`
Expected: Both FAIL — first because symlinks are dropped, second because symlink has non-zero length.

**Step 3: Write minimal implementation**

Rewrite the `walk_directory` function's main loop body. The key change is handling what happens when an entry is a symlink:

```rust
for entry in entries {
    let file_name = entry.file_name().to_string_lossy().into_owned();
    let mut path_components = prefix.to_vec();
    path_components.push(file_name);

    let entry_path = entry.path();
    let file_type = match entry.file_type() {
        Ok(ft) => ft,
        Err(_) => continue,
    };

    if file_type.is_symlink() {
        if include_symlinks {
            // BEP 47: record symlink with zero length, data NOT included
            let target = fs::read_link(&entry_path).ok().map(|t| {
                t.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            });
            out.push(InputFile {
                disk_path: PathBuf::new(), // no disk path — not read during hashing
                torrent_path: path_components,
                length: 0,
                mtime: None,
                attr: Some("l".into()),
                symlink_path: target,
                is_pad: false,
            });
        } else {
            // Follow symlink: treat target as regular file/dir
            let target_meta = match fs::metadata(&entry_path) {
                Ok(m) => m,
                Err(_) => continue, // broken symlink
            };
            if target_meta.is_dir() {
                walk_directory(
                    &entry_path, &path_components, out,
                    include_mtime, include_symlinks, no_attributes,
                );
            } else if target_meta.is_file() {
                let mtime = if include_mtime {
                    target_meta.modified().ok().and_then(|t| {
                        t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
                    })
                } else {
                    None
                };
                let attr = if no_attributes { None } else { detect_attr(&entry_path, &target_meta) };
                out.push(InputFile {
                    disk_path: fs::canonicalize(&entry_path).unwrap_or(entry_path),
                    torrent_path: path_components,
                    length: target_meta.len(),
                    mtime,
                    attr,
                    symlink_path: None,
                    is_pad: false,
                });
            }
        }
    } else if file_type.is_dir() {
        walk_directory(
            &entry_path, &path_components, out,
            include_mtime, include_symlinks, no_attributes,
        );
    } else if file_type.is_file() {
        if let Ok(meta) = fs::metadata(&entry_path) {
            let mtime = if include_mtime {
                meta.modified().ok().and_then(|t| {
                    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
                })
            } else {
                None
            };
            let attr = if no_attributes { None } else { detect_attr(&entry_path, &meta) };
            out.push(InputFile {
                disk_path: fs::canonicalize(&entry_path).unwrap_or(entry_path),
                torrent_path: path_components,
                length: meta.len(),
                mtime,
                attr,
                symlink_path: None,
                is_pad: false,
            });
        }
    }
}
```

Key changes:
- Symlink branch is checked FIRST (before is_dir/is_file)
- When `include_symlinks=false`: follows symlink, checks target type, treats as regular file/dir
- When `include_symlinks=true`: records with `length: 0`, empty `disk_path`, `attr: "l"` — zero-length files contribute nothing to piece hashing

**Step 4: Run tests to verify they pass**

Run: `cargo test -p ferrite-core create::tests::symlink_ -- --nocapture`
Expected: PASS

**Step 5: Run full suite**

Run: `cargo test -p ferrite-core create -- --nocapture`
Expected: 18 tests pass (may be 16 on non-unix due to `#[cfg(unix)]`)

**Step 6: Commit**

```bash
git add crates/ferrite-core/src/create.rs
git commit -m "fix: correct symlink handling per BEP 47 (M30 parity)"
```

---

### Task 4: BEP 35 SSL cert (Gap E)

Add `set_root_cert()` to embed an X.509 PEM certificate in `info["ssl-cert"]`.

**Files:**
- Modify: `crates/ferrite-core/src/metainfo.rs` (new field on InfoDict)
- Modify: `crates/ferrite-core/src/create.rs` (builder method + test)

**Step 1: Write the failing test**

In `create.rs` tests:

```rust
#[test]
fn ssl_cert_in_output() {
    let f = make_test_file();
    let pem = "-----BEGIN CERTIFICATE-----\nMIIBxTCCAWugAwIBAgIJALP...\n-----END CERTIFICATE-----";
    let result = CreateTorrent::new()
        .add_file(f.path())
        .set_piece_size(65536)
        .set_root_cert(pem)
        .set_creation_date(1000000)
        .generate()
        .unwrap();

    // Verify via round-trip
    let parsed = torrent_from_bytes(&result.bytes).unwrap();
    assert_eq!(parsed.info.ssl_cert.as_deref(), Some(pem));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferrite-core create::tests::ssl_cert_in_output -- --nocapture`
Expected: FAIL — `ssl_cert` field and `set_root_cert()` don't exist.

**Step 3: Write minimal implementation**

**metainfo.rs** — add field to `InfoDict`:

```rust
/// BEP 35: X.509 PEM certificate for SSL torrents.
#[serde(rename = "ssl-cert", skip_serializing_if = "Option::is_none", default)]
pub ssl_cert: Option<String>,
```

**create.rs** — add field to `CreateTorrent`:

```rust
ssl_cert: Option<String>,
```

Initialize to `None` in `new()`.

Add builder method:

```rust
/// Set the X.509 PEM certificate for SSL torrents (BEP 35).
pub fn set_root_cert(mut self, cert: impl Into<String>) -> Self {
    self.ssl_cert = Some(cert.into());
    self
}
```

In `generate_with_progress()`, when building `InfoDict` (both single-file and multi-file branches), add:

```rust
ssl_cert: self.ssl_cert.clone(),
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferrite-core create::tests::ssl_cert_in_output -- --nocapture`
Expected: PASS

**Step 5: Run full suite**

Run: `cargo test -p ferrite-core -- --nocapture`
Expected: All tests pass (metainfo + create tests)

**Step 6: Commit**

```bash
git add crates/ferrite-core/src/metainfo.rs crates/ferrite-core/src/create.rs
git commit -m "feat: add BEP 35 SSL cert support (M30 parity)"
```

---

### Task 5: BEP 38 similar torrents + collections (Gap D)

Add `add_similar_torrent(Id20)` and `add_collection(String)` for cross-seeding hints.

**Files:**
- Modify: `crates/ferrite-core/src/metainfo.rs`
- Modify: `crates/ferrite-core/src/create.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn similar_torrents_and_collections() {
    let f = make_test_file();
    let similar_hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
    let result = CreateTorrent::new()
        .add_file(f.path())
        .set_piece_size(65536)
        .add_similar_torrent(similar_hash)
        .add_collection("My Collection")
        .set_creation_date(1000000)
        .generate()
        .unwrap();

    // Verify via round-trip
    let parsed = torrent_from_bytes(&result.bytes).unwrap();
    let similar = parsed.info.similar.as_ref().unwrap();
    assert_eq!(similar.len(), 1);
    assert_eq!(similar[0], similar_hash);
    let collections = parsed.info.collections.as_ref().unwrap();
    assert_eq!(collections, &vec!["My Collection".to_string()]);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferrite-core create::tests::similar_torrents_and_collections -- --nocapture`
Expected: FAIL — fields/methods don't exist.

**Step 3: Write minimal implementation**

**metainfo.rs** — add to `InfoDict`:

```rust
/// BEP 38: info hashes of similar torrents (cross-seeding hints).
#[serde(skip_serializing_if = "Option::is_none", default)]
pub similar: Option<Vec<Id20>>,

/// BEP 38: collection identifiers.
#[serde(skip_serializing_if = "Option::is_none", default)]
pub collections: Option<Vec<String>>,
```

**create.rs** — add fields to `CreateTorrent`:

```rust
similar_torrents: Vec<Id20>,
collections: Vec<String>,
```

Initialize both to `Vec::new()` in `new()`.

Add builder methods:

```rust
/// Add a similar torrent info hash (BEP 38, cross-seeding hint).
pub fn add_similar_torrent(mut self, info_hash: Id20) -> Self {
    self.similar_torrents.push(info_hash);
    self
}

/// Add a collection identifier (BEP 38).
pub fn add_collection(mut self, name: impl Into<String>) -> Self {
    self.collections.push(name.into());
    self
}
```

In `generate_with_progress()`, when building `InfoDict` (both branches), add:

```rust
similar: if self.similar_torrents.is_empty() { None } else { Some(self.similar_torrents.clone()) },
collections: if self.collections.is_empty() { None } else { Some(self.collections.clone()) },
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferrite-core create::tests::similar_torrents_and_collections -- --nocapture`
Expected: PASS

**Step 5: Run full suite**

Run: `cargo test -p ferrite-core -- --nocapture`
Expected: All pass

**Step 6: Commit**

```bash
git add crates/ferrite-core/src/metainfo.rs crates/ferrite-core/src/create.rs
git commit -m "feat: add BEP 38 similar torrents and collections (M30 parity)"
```

---

### Task 6: Construct from existing torrent (Gap C)

libtorrent can create a `create_torrent` from an existing `torrent_info`, preserving the info dict verbatim. This is useful for re-creating `.torrent` files with modified outer metadata (different trackers, seeds) while keeping the same info hash.

**Files:**
- Modify: `crates/ferrite-core/src/create.rs`

**Step 1: Write the failing tests**

```rust
#[test]
fn from_existing_torrent_preserves_info_hash() {
    let f = make_test_file();
    let original = CreateTorrent::new()
        .add_file(f.path())
        .set_piece_size(65536)
        .set_private(true)
        .set_source("OrigTracker")
        .add_tracker("http://orig.example.com/announce", 0)
        .set_creation_date(1000000)
        .generate()
        .unwrap();

    // Reconstruct from existing — different trackers, same info hash
    let rebuilt = CreateTorrent::from_torrent(&original.meta)
        .add_tracker("http://new.example.com/announce", 0)
        .add_tracker("http://backup.example.com/announce", 1)
        .set_comment("Re-created")
        .set_creation_date(2000000)
        .generate()
        .unwrap();

    // Info hash MUST be identical
    assert_eq!(rebuilt.meta.info_hash, original.meta.info_hash);
    // Info dict contents preserved
    assert_eq!(rebuilt.meta.info.private, Some(1));
    assert_eq!(rebuilt.meta.info.source.as_deref(), Some("OrigTracker"));
    // Outer metadata changed
    assert_eq!(rebuilt.meta.announce.as_deref(), Some("http://new.example.com/announce"));
    assert_eq!(rebuilt.meta.comment.as_deref(), Some("Re-created"));
}

#[test]
fn from_existing_torrent_round_trip() {
    let f = make_test_file();
    let original = CreateTorrent::new()
        .add_file(f.path())
        .set_piece_size(65536)
        .add_web_seed("http://seed.example.com/")
        .set_creation_date(1000000)
        .generate()
        .unwrap();

    // Re-create with no modifications to outer metadata
    let rebuilt = CreateTorrent::from_torrent(&original.meta)
        .set_creation_date(1000000)
        .generate()
        .unwrap();

    // Both bytes should parse to same info hash
    let parsed = torrent_from_bytes(&rebuilt.bytes).unwrap();
    assert_eq!(parsed.info_hash, original.meta.info_hash);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p ferrite-core create::tests::from_existing -- --nocapture`
Expected: FAIL — `from_torrent()` doesn't exist.

**Step 3: Write minimal implementation**

Add an `existing_info` field to `CreateTorrent`:

```rust
/// Pre-built info dict (from `from_torrent()`). Skips file collection and hashing.
existing_info: Option<InfoDict>,
```

Initialize to `None` in `new()`.

Add the constructor:

```rust
/// Create a builder from an existing torrent, preserving the info dict verbatim.
///
/// The info hash will be identical to the original. Use builder methods to
/// modify outer metadata (trackers, seeds, comment, creator, creation date).
/// Info dict fields (name, private, source, etc.) are NOT modifiable — they
/// come from the original torrent.
pub fn from_torrent(meta: &TorrentMetaV1) -> Self {
    Self {
        existing_info: Some(meta.info.clone()),
        ..Self::new()
    }
}
```

In `generate_with_progress()`, add an early return path at the top (after the existing `is_empty` check):

```rust
// Fast path: reconstruct from existing torrent (info dict already built)
if let Some(info) = self.existing_info {
    let info_bytes = ferrite_bencode::to_bytes(&info)
        .map_err(|e| Error::CreateTorrent(format!("serialize info: {e}")))?;
    let info_hash = crate::sha1(&info_bytes);

    let (announce, announce_list) = build_tracker_lists(&self.trackers);
    let creation_date = self.creation_date.or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs() as i64)
    });

    let output = TorrentOutput {
        announce,
        announce_list,
        comment: self.comment.clone(),
        created_by: self.creator.clone(),
        creation_date,
        httpseeds: if self.http_seeds.is_empty() { None } else { Some(self.http_seeds.clone()) },
        info,
        nodes: if self.dht_nodes.is_empty() { None } else { Some(self.dht_nodes.clone()) },
        url_list: if self.web_seeds.is_empty() { None } else { Some(self.web_seeds.clone()) },
    };

    let bytes = ferrite_bencode::to_bytes(&output)
        .map_err(|e| Error::CreateTorrent(format!("serialize torrent: {e}")))?;

    let meta = TorrentMetaV1 {
        info_hash,
        announce: self.trackers.first().map(|(url, _)| url.clone()),
        announce_list: output.announce_list,
        comment: self.comment,
        created_by: self.creator,
        creation_date,
        info: output.info,
        url_list: self.web_seeds,
        httpseeds: self.http_seeds,
    };

    return Ok(CreateTorrentResult { meta, bytes });
}
```

Also update the `files.is_empty()` check to only apply when not using existing info:

```rust
if self.existing_info.is_none() && self.files.is_empty() {
    return Err(Error::CreateTorrent("no files added".into()));
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p ferrite-core create::tests::from_existing -- --nocapture`
Expected: PASS

**Step 5: Run full suite**

Run: `cargo test -p ferrite-core create -- --nocapture`
Expected: All pass

**Step 6: Commit**

```bash
git add crates/ferrite-core/src/create.rs
git commit -m "feat: add CreateTorrent::from_torrent() for info dict preservation (M30 parity)"
```

---

### Task 7: Parallel hashing (Gap F)

libtorrent pipelines 4 hash requests per I/O thread. Ferrite currently hashes sequentially. Add `set_hashing_threads(n)` with a batch-parallel approach: read pieces sequentially (optimal for disk), hash in parallel batches with `std::thread::scope`.

**Files:**
- Modify: `crates/ferrite-core/src/create.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn parallel_hashing_matches_sequential() {
    // Create a file large enough for multiple pieces
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("large.bin");
    // 256 KiB file = 8 pieces at 32 KiB piece size
    let data: Vec<u8> = (0..262144u32).map(|i| (i % 256) as u8).collect();
    fs::write(&file_path, &data).unwrap();

    // Sequential (default, 1 thread)
    let sequential = CreateTorrent::new()
        .add_file(&file_path)
        .set_piece_size(32768)
        .set_creation_date(1000000)
        .generate()
        .unwrap();

    // Parallel (4 threads)
    let parallel = CreateTorrent::new()
        .add_file(&file_path)
        .set_piece_size(32768)
        .set_hashing_threads(4)
        .set_creation_date(1000000)
        .generate()
        .unwrap();

    // Must produce identical output
    assert_eq!(parallel.meta.info_hash, sequential.meta.info_hash);
    assert_eq!(parallel.meta.info.pieces, sequential.meta.info.pieces);
    assert_eq!(parallel.bytes, sequential.bytes);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferrite-core create::tests::parallel_hashing_matches_sequential -- --nocapture`
Expected: FAIL — `set_hashing_threads()` doesn't exist.

**Step 3: Write implementation**

Add field to `CreateTorrent`:

```rust
hashing_threads: usize,
```

Initialize to `1` in `new()`.

Add builder method:

```rust
/// Set the number of threads for piece hashing (default: 1, sequential).
///
/// Pieces are read sequentially (optimal for disk I/O) and hashed in
/// parallel batches of `4 * threads` pieces.
pub fn set_hashing_threads(mut self, threads: usize) -> Self {
    self.hashing_threads = threads.max(1);
    self
}
```

Extract a `PieceReader` struct to encapsulate the file-reading state machine. This cleans up the hashing loop and enables sharing between sequential and parallel paths:

```rust
/// Sequential piece reader — reads pieces in order across file boundaries.
struct PieceReader<'a> {
    files: &'a [InputFile],
    piece_size: u64,
    total_size: u64,
    num_pieces: usize,
    file_idx: usize,
    file_offset: u64,
    file_handle: Option<fs::File>,
}

impl<'a> PieceReader<'a> {
    fn new(files: &'a [InputFile], piece_size: u64, total_size: u64, num_pieces: usize) -> Self {
        Self {
            files,
            piece_size,
            total_size,
            num_pieces,
            file_idx: 0,
            file_offset: 0,
            file_handle: None,
        }
    }

    /// Read the next piece's data. Returns the raw bytes (may be shorter than
    /// piece_size for the last piece).
    fn read_next(&mut self, piece_index: usize) -> Result<Vec<u8>> {
        let piece_len = if piece_index == self.num_pieces - 1 {
            (self.total_size - (piece_index as u64) * self.piece_size) as usize
        } else {
            self.piece_size as usize
        };

        let mut buf = vec![0u8; piece_len];
        let mut buf_offset = 0;

        while buf_offset < piece_len {
            if self.file_idx >= self.files.len() {
                break;
            }
            let file = &self.files[self.file_idx];
            let remaining_in_file = file.length - self.file_offset;
            let to_read = (piece_len - buf_offset).min(remaining_in_file as usize);

            if file.is_pad || file.length == 0 {
                // Pad files and zero-length files (symlinks): fill with zeros
                buf[buf_offset..buf_offset + to_read].fill(0);
            } else {
                if self.file_handle.is_none() {
                    self.file_handle = Some(fs::File::open(&file.disk_path)?);
                    if self.file_offset > 0 {
                        use std::io::Seek;
                        self.file_handle
                            .as_mut()
                            .unwrap()
                            .seek(std::io::SeekFrom::Start(self.file_offset))?;
                    }
                }
                let handle = self.file_handle.as_mut().unwrap();
                handle.read_exact(&mut buf[buf_offset..buf_offset + to_read])?;
            }

            buf_offset += to_read;
            self.file_offset += to_read as u64;

            if self.file_offset >= file.length {
                self.file_idx += 1;
                self.file_offset = 0;
                self.file_handle = None;
            }
        }

        Ok(buf)
    }

    /// Skip a piece without reading (for pre-computed hashes).
    fn skip(&mut self, piece_index: usize) {
        let piece_len = if piece_index == self.num_pieces - 1 {
            (self.total_size - (piece_index as u64) * self.piece_size) as usize
        } else {
            self.piece_size as usize
        };
        let mut remaining = piece_len;
        while remaining > 0 && self.file_idx < self.files.len() {
            let in_file = self.files[self.file_idx].length - self.file_offset;
            let skip = remaining.min(in_file as usize);
            self.file_offset += skip as u64;
            remaining -= skip;
            if self.file_offset >= self.files[self.file_idx].length {
                self.file_idx += 1;
                self.file_offset = 0;
                self.file_handle = None;
            }
        }
    }
}
```

Replace the entire hashing section of `generate_with_progress()` (from `let mut pieces = ...` through the end of the while loop) with:

```rust
// Hash pieces
let mut pieces = Vec::with_capacity(num_pieces * 20);
let mut reader = PieceReader::new(&files_with_pads, piece_size, total_size, num_pieces);

if self.hashing_threads <= 1 {
    // Sequential path
    for idx in 0..num_pieces {
        if let Some(&hash) = self.pre_hashes.get(&(idx as u32)) {
            reader.skip(idx);
            pieces.extend_from_slice(hash.as_bytes());
        } else {
            let data = reader.read_next(idx)?;
            let hash = crate::sha1(&data);
            pieces.extend_from_slice(hash.as_bytes());
        }
        cb(idx + 1, num_pieces);
    }
} else {
    // Parallel path: sequential reads, batched parallel hashing
    let batch_size = 4 * self.hashing_threads;
    let mut idx = 0;

    while idx < num_pieces {
        let batch_end = (idx + batch_size).min(num_pieces);

        // Phase 1: Read batch sequentially
        let mut batch: Vec<(Option<Id20>, Vec<u8>)> = Vec::with_capacity(batch_end - idx);
        for piece_idx in idx..batch_end {
            if let Some(&hash) = self.pre_hashes.get(&(piece_idx as u32)) {
                reader.skip(piece_idx);
                batch.push((Some(hash), Vec::new()));
            } else {
                let data = reader.read_next(piece_idx)?;
                batch.push((None, data));
            }
        }

        // Phase 2: Hash in parallel
        let hashes: Vec<Id20> = std::thread::scope(|s| {
            let handles: Vec<_> = batch
                .iter()
                .map(|(pre_hash, data)| {
                    s.spawn(move || {
                        if let Some(hash) = pre_hash {
                            *hash
                        } else {
                            crate::sha1(data)
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect()
        });

        // Phase 3: Collect results + progress callbacks
        for (i, hash) in hashes.into_iter().enumerate() {
            pieces.extend_from_slice(hash.as_bytes());
            cb(idx + i + 1, num_pieces);
        }

        idx = batch_end;
    }
}
```

This also removes the old `piece_buf`, `current_file_idx`, `current_file_offset`, `current_file_handle`, `piece_index` variables and the `advance_cursors()` function (now replaced by `PieceReader::skip()`).

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferrite-core create::tests::parallel_hashing_matches_sequential -- --nocapture`
Expected: PASS

**Step 5: Run full suite + clippy**

Run: `cargo test -p ferrite-core create -- --nocapture && cargo clippy -p ferrite-core -- -D warnings`
Expected: All 23 tests pass, zero warnings.

`advance_cursors()` may trigger dead-code warning after refactoring — delete it if unused.

**Step 6: Commit**

```bash
git add crates/ferrite-core/src/create.rs
git commit -m "feat: add parallel piece hashing with std::thread::scope (M30 parity)"
```

---

## Final Verification

```bash
cd /mnt/TempNVME/projects/ferrite
cargo test --workspace                          # all tests pass (754 + 9 new = 763+)
cargo clippy --workspace -- -D warnings         # zero warnings
cargo test -p ferrite-core create               # 23 create tests specifically
```

No version bump — this is hardening within the existing 0.32.0 release.

---

## Post-Parity Summary

After all 7 tasks, ferrite's `CreateTorrent` covers:

| libtorrent feature | ferrite | status |
|---|---|---|
| Auto piece size | `auto_piece_size()` | match |
| Piece size limits (16KiB–128MiB) | validation in `generate()` | **NEW** |
| `no_attributes` | `no_attributes(bool)` | **NEW** |
| Symlink zero-length (BEP 47) | `include_symlinks(true)` | **FIXED** |
| Symlink follow (default) | `include_symlinks(false)` | **FIXED** |
| SSL cert (BEP 35) | `set_root_cert()` | **NEW** |
| Similar torrents (BEP 38) | `add_similar_torrent()` | **NEW** |
| Collections (BEP 38) | `add_collection()` | **NEW** |
| Construct from existing | `from_torrent()` | **NEW** |
| Parallel hashing | `set_hashing_threads(n)` | **NEW** |
| Source tag | `set_source()` | exceeds (explicit API) |
| Private (BEP 27) | `set_private()` | match |
| Trackers (BEP 12) | `add_tracker()` | match |
| Web seeds (BEP 19) | `add_web_seed()` | match |
| HTTP seeds (BEP 17) | `add_http_seed()` | match |
| DHT nodes (BEP 5) | `add_dht_node()` | match |
| Pad files (BEP 47) | `set_pad_file_limit()` | match |
| File attributes (BEP 47) | auto-detected | match |
| Pre-computed hashes | `set_hash()` | match |
| Progress callback | `generate_with_progress()` | match |
| v2 / hybrid (BEP 52) | — | deferred to M33-35 |
