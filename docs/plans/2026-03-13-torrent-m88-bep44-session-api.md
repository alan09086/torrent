# M88: BEP 44 Session API — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire session-level `dht_put`/`dht_get` methods (immutable + mutable) that forward to `DhtHandle`'s existing BEP 44 implementation and fire the pre-defined alert variants.

**Architecture:** The DHT crate already implements full BEP 44 via `DhtHandle::put_immutable()`, `get_immutable()`, `put_mutable()`, `get_mutable()`. The session layer needs: 4 new `SessionCommand` variants, 4 new `SessionHandle` public methods, 4 `SessionActor` handlers that forward to `DhtHandle` and fire alerts, and a new `Error::DhtDisabled` variant. Alert variants (`DhtPutComplete`, `DhtMutablePutComplete`, `DhtGetResult`, `DhtMutableGetResult`, `DhtItemError`) already exist in `alert.rs`.

**Tech Stack:** Rust, torrent-session crate

---

### Task 1: Add `Error::DhtDisabled` variant

**Files:**
- Modify: `crates/torrent-session/src/error.rs` (or wherever `Error` enum is defined — search for `pub enum Error`)

- [ ] **Step 1: Add the variant**

```rust
/// DHT is disabled in session settings.
#[error("DHT is disabled")]
DhtDisabled,
```

- [ ] **Step 2: Run clippy to confirm it compiles**

```bash
cargo clippy -p torrent-session -- -D warnings
```

- [ ] **Step 3: Commit**

```bash
git add crates/torrent-session/src/error.rs
git commit -m "feat: add Error::DhtDisabled variant (M88)"
```

---

### Task 2: Add `SessionCommand` variants for BEP 44

**Files:**
- Modify: `crates/torrent-session/src/session.rs`

- [ ] **Step 1: Add four command variants to `SessionCommand` enum**

Add these after the existing DHT-related commands (near `ForceDhtAnnounce`):

```rust
/// Store an immutable item in the DHT (BEP 44).
DhtPutImmutable {
    value: Vec<u8>,
    reply: oneshot::Sender<crate::Result<Id20>>,
},
/// Retrieve an immutable item from the DHT (BEP 44).
DhtGetImmutable {
    target: Id20,
    reply: oneshot::Sender<crate::Result<Option<Vec<u8>>>>,
},
/// Store a mutable item in the DHT (BEP 44).
DhtPutMutable {
    keypair_bytes: [u8; 32],
    value: Vec<u8>,
    seq: i64,
    salt: Vec<u8>,
    reply: oneshot::Sender<crate::Result<Id20>>,
},
/// Retrieve a mutable item from the DHT (BEP 44).
DhtGetMutable {
    public_key: [u8; 32],
    salt: Vec<u8>,
    #[allow(clippy::type_complexity)]
    reply: oneshot::Sender<crate::Result<Option<(Vec<u8>, i64)>>>,
},
```

- [ ] **Step 2: Add placeholder match arms to keep the build green**

In the SessionActor dispatch loop, add temporary match arms so the project compiles:

```rust
Some(SessionCommand::DhtPutImmutable { reply, .. }) => {
    let _ = reply.send(Err(crate::Error::DhtDisabled));
}
Some(SessionCommand::DhtGetImmutable { reply, .. }) => {
    let _ = reply.send(Err(crate::Error::DhtDisabled));
}
Some(SessionCommand::DhtPutMutable { reply, .. }) => {
    let _ = reply.send(Err(crate::Error::DhtDisabled));
}
Some(SessionCommand::DhtGetMutable { reply, .. }) => {
    let _ = reply.send(Err(crate::Error::DhtDisabled));
}
```

- [ ] **Step 3: Verify it compiles**

```bash
cargo check -p torrent-session
```

Expected: Compiles cleanly.

- [ ] **Step 4: Commit**

```bash
git add crates/torrent-session/src/session.rs
git commit -m "feat: add SessionCommand variants for BEP 44 DHT items (M88)"
```

---

### Task 3: Add `SessionHandle` public methods

**Files:**
- Modify: `crates/torrent-session/src/session.rs`

- [ ] **Step 1: Add four public methods to `SessionHandle`**

Follow the existing pattern (e.g., `force_dht_announce`):

```rust
/// Store an immutable item in the DHT (BEP 44).
///
/// Returns the SHA-1 target hash of the stored item.
pub async fn dht_put_immutable(&self, value: Vec<u8>) -> crate::Result<Id20> {
    let (tx, rx) = oneshot::channel();
    self.cmd_tx
        .send(SessionCommand::DhtPutImmutable { value, reply: tx })
        .await
        .map_err(|_| crate::Error::Shutdown)?;
    rx.await.map_err(|_| crate::Error::Shutdown)?
}

/// Retrieve an immutable item from the DHT (BEP 44).
///
/// Returns the value if found, or `None` if not found in the DHT.
pub async fn dht_get_immutable(&self, target: Id20) -> crate::Result<Option<Vec<u8>>> {
    let (tx, rx) = oneshot::channel();
    self.cmd_tx
        .send(SessionCommand::DhtGetImmutable { target, reply: tx })
        .await
        .map_err(|_| crate::Error::Shutdown)?;
    rx.await.map_err(|_| crate::Error::Shutdown)?
}

/// Store a mutable item in the DHT (BEP 44).
///
/// `keypair_bytes` is the 32-byte Ed25519 seed. Returns the target hash.
pub async fn dht_put_mutable(
    &self,
    keypair_bytes: [u8; 32],
    value: Vec<u8>,
    seq: i64,
    salt: Vec<u8>,
) -> crate::Result<Id20> {
    let (tx, rx) = oneshot::channel();
    self.cmd_tx
        .send(SessionCommand::DhtPutMutable {
            keypair_bytes,
            value,
            seq,
            salt,
            reply: tx,
        })
        .await
        .map_err(|_| crate::Error::Shutdown)?;
    rx.await.map_err(|_| crate::Error::Shutdown)?
}

/// Retrieve a mutable item from the DHT (BEP 44).
///
/// Returns `(value, sequence_number)` if found, or `None`.
pub async fn dht_get_mutable(
    &self,
    public_key: [u8; 32],
    salt: Vec<u8>,
) -> crate::Result<Option<(Vec<u8>, i64)>> {
    let (tx, rx) = oneshot::channel();
    self.cmd_tx
        .send(SessionCommand::DhtGetMutable {
            public_key,
            salt,
            reply: tx,
        })
        .await
        .map_err(|_| crate::Error::Shutdown)?;
    rx.await.map_err(|_| crate::Error::Shutdown)?
}
```

- [ ] **Step 2: Commit**

```bash
git add crates/torrent-session/src/session.rs
git commit -m "feat: add SessionHandle public methods for BEP 44 DHT items (M88)"
```

---

### Task 4: Add `SessionActor` handlers with alert firing

**Files:**
- Modify: `crates/torrent-session/src/session.rs`

- [ ] **Step 1: Add match arms in the main dispatch loop**

In the `match cmd` block of the SessionActor's main loop, add:

```rust
Some(SessionCommand::DhtPutImmutable { value, reply }) => {
    let result = self.handle_dht_put_immutable(value).await;
    let _ = reply.send(result);
}
Some(SessionCommand::DhtGetImmutable { target, reply }) => {
    let result = self.handle_dht_get_immutable(target).await;
    let _ = reply.send(result);
}
Some(SessionCommand::DhtPutMutable { keypair_bytes, value, seq, salt, reply }) => {
    let result = self.handle_dht_put_mutable(keypair_bytes, value, seq, salt).await;
    let _ = reply.send(result);
}
Some(SessionCommand::DhtGetMutable { public_key, salt, reply }) => {
    let result = self.handle_dht_get_mutable(public_key, salt).await;
    let _ = reply.send(result);
}
```

- [ ] **Step 2: Implement the handler methods on `SessionActor`**

```rust
async fn handle_dht_put_immutable(&self, value: Vec<u8>) -> crate::Result<Id20> {
    let dht = self.dht_v4.as_ref().ok_or(crate::Error::DhtDisabled)?;
    match dht.put_immutable(value).await {
        Ok(target) => {
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::DhtPutComplete { target },
            );
            Ok(target)
        }
        Err(e) => {
            let target = Id20::ZERO; // best-effort target for error alert
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::DhtItemError {
                    target,
                    message: format!("immutable put failed: {e}"),
                },
            );
            Err(crate::Error::Dht(e))
        }
    }
}

async fn handle_dht_get_immutable(&self, target: Id20) -> crate::Result<Option<Vec<u8>>> {
    let dht = self.dht_v4.as_ref().ok_or(crate::Error::DhtDisabled)?;
    match dht.get_immutable(target).await {
        Ok(value) => {
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::DhtGetResult {
                    target,
                    value: value.clone(),
                },
            );
            Ok(value)
        }
        Err(e) => {
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::DhtItemError {
                    target,
                    message: format!("immutable get failed: {e}"),
                },
            );
            Err(crate::Error::Dht(e))
        }
    }
}

async fn handle_dht_put_mutable(
    &self,
    keypair_bytes: [u8; 32],
    value: Vec<u8>,
    seq: i64,
    salt: Vec<u8>,
) -> crate::Result<Id20> {
    let dht = self.dht_v4.as_ref().ok_or(crate::Error::DhtDisabled)?;
    match dht.put_mutable(keypair_bytes, value, seq, salt).await {
        Ok(target) => {
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::DhtMutablePutComplete { target, seq },
            );
            Ok(target)
        }
        Err(e) => {
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::DhtItemError {
                    target: Id20::ZERO,
                    message: format!("mutable put failed: {e}"),
                },
            );
            Err(crate::Error::Dht(e))
        }
    }
}

async fn handle_dht_get_mutable(
    &self,
    public_key: [u8; 32],
    salt: Vec<u8>,
) -> crate::Result<Option<(Vec<u8>, i64)>> {
    let dht = self.dht_v4.as_ref().ok_or(crate::Error::DhtDisabled)?;
    let target = torrent_dht::bep44::compute_mutable_target(&public_key, &salt);
    match dht.get_mutable(public_key, salt).await {
        Ok(result) => {
            if let Some((ref value, seq)) = result {
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::DhtMutableGetResult {
                        target,
                        value: Some(value.clone()),
                        seq: Some(seq),
                        public_key,
                    },
                );
            } else {
                post_alert(
                    &self.alert_tx,
                    &self.alert_mask,
                    AlertKind::DhtMutableGetResult {
                        target,
                        value: None,
                        seq: None,
                        public_key,
                    },
                );
            }
            Ok(result)
        }
        Err(e) => {
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::DhtItemError {
                    target,
                    message: format!("mutable get failed: {e}"),
                },
            );
            Err(crate::Error::Dht(e))
        }
    }
}
```

**Note:** The `Error::Dht(e)` variant may need to be added if it doesn't exist. Check the Error enum — if there's no `Dht` variant wrapping `torrent_dht::Error`, add one with `#[from]`. Alternatively, map the error: `Err(crate::Error::from(e))` if a `From` impl exists.

**Note:** `torrent_dht::bep44::compute_mutable_target` may need to be verified — check the DHT crate's public API to confirm this function exists and its signature. If not public, compute the target manually: `SHA-1(public_key + salt)`.

- [ ] **Step 3: Commit**

```bash
git add crates/torrent-session/src/session.rs
git commit -m "feat: add SessionActor handlers for BEP 44 DHT items with alerts (M88)"
```

---

### Task 5: Write tests, remove TODO comment, and finalize

**Files:**
- Modify: `crates/torrent-session/src/alert.rs`
- Create or modify: test file accessible to session-level API

- [ ] **Step 1: Write test for DhtDisabled error**

```rust
#[tokio::test]
async fn test_dht_operations_fail_when_disabled() {
    // Start a session with enable_dht = false
    let result = session.dht_put_immutable(b"test".to_vec()).await;
    assert!(matches!(result, Err(Error::DhtDisabled)));
}
```

- [ ] **Step 2: Write test for immutable put/get round-trip**

```rust
#[tokio::test]
async fn test_dht_put_get_immutable() {
    // Start a session with DHT enabled
    let target = session.dht_put_immutable(b"hello world".to_vec()).await.unwrap();
    let result = session.dht_get_immutable(target).await.unwrap();
    assert_eq!(result, Some(b"hello world".to_vec()));
}
```

- [ ] **Step 3: Remove the TODO comment from alert.rs**

In `alert.rs` (line ~399), remove:

```rust
// REMOVE:
// TODO: Wire DHT item alerts when session-level dht_put/dht_get is added
```

- [ ] **Step 4: Run full test suite**

```bash
cargo test --workspace && cargo clippy --workspace -- -D warnings
```

Expected: All tests pass including the new BEP 44 tests.

- [ ] **Step 3: Commit**

```bash
git add crates/torrent-session/src/alert.rs
git commit -m "feat: remove TODO comment for DHT item alerts — now wired (M88)"
```
