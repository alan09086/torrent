# M86: Dead Disk Backend Cleanup — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove dead `DiskIoBackend::move_storage()` trait method and all associated plumbing that was superseded by `TorrentActor::handle_move_storage()`.

**Architecture:** The move_storage operation is fully implemented in `TorrentActor::handle_move_storage()` (torrent.rs:2342-2429) which calls `relocate_files()` directly, then unregisters/re-registers storage with DiskManager. The `DiskIoBackend::move_storage()` trait method and its three stub impls, the `DiskJob::MoveStorage` variant, and `DiskHandle::move_storage()` are all dead code — nothing calls them.

**Tech Stack:** Rust, torrent-session crate

---

### Task 1: Remove `move_storage` from `DiskIoBackend` trait and all impls

**Files:**
- Modify: `crates/torrent-session/src/disk_backend.rs`

- [ ] **Step 1: Remove the trait method**

In `disk_backend.rs`, remove the `move_storage` method from the `DiskIoBackend` trait definition (line ~92):

```rust
// REMOVE this line from the trait:
fn move_storage(&self, info_hash: Id20, new_path: &Path) -> crate::Result<()>;
```

- [ ] **Step 2: Remove the DisabledDiskIo impl**

Remove lines ~171-173:

```rust
// REMOVE:
fn move_storage(&self, _info_hash: Id20, _new_path: &Path) -> crate::Result<()> {
    Ok(())
}
```

- [ ] **Step 3: Remove the PosixDiskIo impl**

Remove lines ~394-397:

```rust
// REMOVE:
fn move_storage(&self, _info_hash: Id20, _new_path: &Path) -> crate::Result<()> {
    // TODO: implement in future milestone
    Ok(())
}
```

- [ ] **Step 4: Remove the MmapDiskIo impl**

Remove lines ~538-541:

```rust
// REMOVE:
fn move_storage(&self, _info_hash: Id20, _new_path: &Path) -> crate::Result<()> {
    // TODO: implement in future milestone
    Ok(())
}
```

- [ ] **Step 5: Remove unused `Path` import if now unreferenced**

Check if `std::path::Path` is still used in the trait after removing `move_storage`. If not, remove the import.

---

### Task 2: Remove `DiskJob::MoveStorage` and `DiskHandle::move_storage()`

**Files:**
- Modify: `crates/torrent-session/src/disk.rs`

- [ ] **Step 1: Remove the `DiskJob::MoveStorage` variant**

Remove from the `DiskJob` enum (lines ~123-127):

```rust
// REMOVE:
MoveStorage {
    info_hash: Id20,
    new_path: PathBuf,
    reply: oneshot::Sender<torrent_storage::Result<()>>,
},
```

- [ ] **Step 2: Remove the `DiskHandle::move_storage()` method**

Remove lines ~478-493:

```rust
// REMOVE:
pub async fn move_storage(&self, new_path: PathBuf) -> torrent_storage::Result<()> {
    let (tx, rx) = oneshot::channel();
    let _ = self
        .tx
        .send(DiskJob::MoveStorage {
            info_hash: self.info_hash,
            new_path,
            reply: tx,
        })
        .await;
    rx.await
        .unwrap_or(Err(torrent_storage::Error::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "disk actor gone",
        ))))
}
```

- [ ] **Step 3: Remove the `DiskJob::MoveStorage` match arm in the dispatch loop**

Remove lines ~893-909:

```rust
// REMOVE:
DiskJob::MoveStorage {
    info_hash,
    new_path,
    reply,
} => {
    let backend = Arc::clone(&self.backend);
    let semaphore = self.semaphore.clone();
    tokio::spawn(async move {
        let permit = semaphore.acquire_owned().await.unwrap();
        let result =
            tokio::task::spawn_blocking(move || backend.move_storage(info_hash, &new_path))
                .await
                .unwrap();
        drop(permit);
        let _ = reply.send(to_storage_result(result));
    });
}
```

---

### Task 3: Verify and commit

- [ ] **Step 1: Run tests**

```bash
cargo test --workspace
```

Expected: All 1460+ tests pass. The `TorrentActor::handle_move_storage()` path is unaffected.

- [ ] **Step 2: Run clippy**

```bash
cargo clippy --workspace -- -D warnings
```

Expected: Clean. Removing dead code should not introduce warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/torrent-session/src/disk_backend.rs crates/torrent-session/src/disk.rs
git commit -m "feat: remove dead DiskIoBackend::move_storage plumbing (M86)

TorrentActor::handle_move_storage() handles moves directly via
relocate_files(). The DiskIoBackend trait method, three stub impls,
DiskJob::MoveStorage variant, and DiskHandle::move_storage() were
all unreachable dead code."
```
