# M42: SSL Torrents — Implementation Plan

**Date:** 2026-03-01
**Milestone:** M42 (Phase 8: Connectivity & Privacy)
**Status:** M35 complete (v0.41.0, 895 tests, edition 2024)
**Dependencies:** `rustls`, `rcgen`, `x509-parser`, `rustls-pemfile`, `tokio-rustls`

## Overview

SSL torrents embed an X.509 CA certificate in the `.torrent` info dict (`ssl-cert` key). All peer connections for that torrent must use TLS, with the server presenting a certificate that chains to the embedded CA. The SNI field carries the hex-encoded info hash (40 chars) so a single SSL listen port can multiplex multiple torrents.

This milestone touches three crates: **ferrite-core** (metadata parsing), **ferrite-wire** (TLS transport layer), and **ferrite-session** (SSL listener, peer connection wrapping, settings, alerts).

---

## Phase 1: ferrite-core — SSL Certificate Parsing

### 1a. Add `ssl_cert` to `TorrentMetaV1`

**File:** `crates/ferrite-core/src/metainfo.rs`

The `ssl-cert` key lives in the **info dict**, not the outer torrent dict. It is a raw byte string containing PEM-encoded X.509 certificate data.

```rust
// In InfoDict, add:
/// BEP 35 / SSL torrent: PEM-encoded X.509 CA certificate.
/// When present, all peer connections must use TLS with certs chaining to this CA.
#[serde(rename = "ssl-cert", skip_serializing_if = "Option::is_none", default)]
#[serde(with = "serde_bytes")]
pub ssl_cert: Option<Vec<u8>>,
```

Also add the field to `TorrentMetaV1` for convenient access after parsing:

```rust
// In TorrentMetaV1, add:
/// PEM-encoded SSL CA certificate from the info dict, if present.
pub ssl_cert: Option<Vec<u8>>,
```

**Changes to `torrent_from_bytes()`** — propagate `ssl_cert` from `raw.info.ssl_cert`:

```rust
// In torrent_from_bytes(), add to the TorrentMetaV1 construction:
ssl_cert: raw.info.ssl_cert.clone(),
```

### 1b. Add `ssl_cert()` accessor to `TorrentMeta`

**File:** `crates/ferrite-core/src/detect.rs`

```rust
impl TorrentMeta {
    /// Get the SSL CA certificate bytes (PEM), if this is an SSL torrent.
    pub fn ssl_cert(&self) -> Option<&[u8]> {
        match self {
            TorrentMeta::V1(v1) => v1.ssl_cert.as_deref(),
            TorrentMeta::V2(v2) => v2.ssl_cert.as_deref(),
            TorrentMeta::Hybrid(v1, _) => v1.ssl_cert.as_deref(),
        }
    }

    /// Whether this torrent requires SSL peer connections.
    pub fn is_ssl(&self) -> bool {
        self.ssl_cert().is_some()
    }
}
```

### 1c. Add `ssl_cert` to `TorrentMetaV2` and `InfoDictV2`

**File:** `crates/ferrite-core/src/metainfo_v2.rs`

Add the same `ssl_cert` field to `InfoDictV2` (serde-deserialized) and `TorrentMetaV2` (propagated during parsing), mirroring the v1 approach.

### 1d. Add `ssl_cert` to `CreateTorrent` builder

**File:** `crates/ferrite-core/src/create.rs`

```rust
// In CreateTorrent struct, add:
ssl_cert: Option<Vec<u8>>,

// Builder method:
/// Set the SSL CA certificate (PEM-encoded) for SSL torrent creation.
/// When set, the `ssl-cert` key is written into the info dict.
pub fn set_ssl_cert(mut self, cert_pem: Vec<u8>) -> Self {
    self.ssl_cert = Some(cert_pem);
    self
}
```

The `ssl-cert` key must be serialized into `InfoDict` so it appears in the info dict bytes that are hashed for the info hash. Add to `InfoDict`'s `Serialize` impl (the `ssl_cert` field added in 1a handles this via serde).

### 1e. Tests (ferrite-core)

**File:** `crates/ferrite-core/src/metainfo.rs` (append to `mod tests`)

```rust
#[test]
fn ssl_cert_parsed_from_info_dict() {
    // Build a torrent with ssl-cert in the info dict.
    // "ssl-cert" sorts after "pieces" and "piece length" but before end of dict.
    let cert_pem = b"-----BEGIN CERTIFICATE-----\nMIIBtest\n-----END CERTIFICATE-----\n";
    let cert_len = cert_pem.len();

    // Minimal info dict with ssl-cert inserted (keys must be sorted)
    let mut info = Vec::new();
    info.extend_from_slice(b"d");
    info.extend_from_slice(b"6:lengthi1048576e");
    info.extend_from_slice(b"4:name4:test");
    info.extend_from_slice(b"12:piece lengthi262144e");
    info.extend_from_slice(b"6:pieces20:");
    info.extend_from_slice(&[0u8; 20]);
    info.extend_from_slice(format!("8:ssl-cert{}:", cert_len).as_bytes());
    info.extend_from_slice(cert_pem);
    info.extend_from_slice(b"e");

    let mut torrent = Vec::new();
    torrent.extend_from_slice(b"d4:info");
    torrent.extend_from_slice(&info);
    torrent.extend_from_slice(b"e");

    let meta = torrent_from_bytes(&torrent).unwrap();
    assert!(meta.ssl_cert.is_some());
    assert_eq!(meta.ssl_cert.as_deref().unwrap(), cert_pem);
    assert_eq!(meta.info.ssl_cert.as_deref().unwrap(), cert_pem);
}

#[test]
fn ssl_cert_absent_by_default() {
    let data = make_torrent_bytes_sorted(b"", b"");
    let meta = torrent_from_bytes(&data).unwrap();
    assert!(meta.ssl_cert.is_none());
    assert!(meta.info.ssl_cert.is_none());
}
```

**File:** `crates/ferrite-core/src/detect.rs` (append to tests)

```rust
#[test]
fn torrent_meta_ssl_accessors() {
    // Non-SSL torrent
    let data = make_minimal_v1_bytes();
    let meta = torrent_from_bytes_any(&data).unwrap();
    assert!(!meta.is_ssl());
    assert!(meta.ssl_cert().is_none());
}
```

**File:** `crates/ferrite-core/src/create.rs` (append to tests)

```rust
#[test]
fn create_torrent_with_ssl_cert() {
    let cert_pem = b"-----BEGIN CERTIFICATE-----\nMIIBtest\n-----END CERTIFICATE-----\n";
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.bin");
    std::fs::write(&file_path, vec![0u8; 65536]).unwrap();

    let result = CreateTorrent::new()
        .add_file(&file_path)
        .set_ssl_cert(cert_pem.to_vec())
        .generate()
        .unwrap();

    // Parse back and verify ssl-cert round-trips
    let parsed = torrent_from_bytes(&result.bytes).unwrap();
    assert_eq!(parsed.ssl_cert.as_deref().unwrap(), cert_pem.as_slice());
}
```

---

## Phase 2: ferrite-wire — TLS Transport Layer

### 2a. New module: `crates/ferrite-wire/src/ssl.rs`

This module provides the TLS wrapper for SSL torrent connections. It is structurally similar to the MSE module but uses rustls for real cryptographic transport.

**File:** `crates/ferrite-wire/src/ssl.rs`

```rust
//! TLS transport for SSL torrents.
//!
//! Wraps a TCP stream with rustls TLS, using the CA certificate from the
//! .torrent file as the trust anchor. SNI is set to the hex-encoded info hash.

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::server::TlsStream as ServerTlsStream;

use ferrite_core::Id20;

use crate::error::{Error, Result};

/// Configuration for an SSL torrent's TLS layer.
#[derive(Clone)]
pub struct SslConfig {
    /// The CA certificate from the .torrent file's info dict.
    pub ca_cert_pem: Vec<u8>,
    /// Our client/server certificate (PEM).
    pub our_cert_pem: Vec<u8>,
    /// Our private key (PEM).
    pub our_key_pem: Vec<u8>,
}

/// Build a rustls `ClientConfig` that trusts only the torrent's embedded CA.
pub fn build_client_config(config: &SslConfig) -> Result<Arc<rustls::ClientConfig>> {
    let ca_certs = parse_pem_certs(&config.ca_cert_pem)?;
    let our_certs = parse_pem_certs(&config.our_cert_pem)?;
    let our_key = parse_pem_key(&config.our_key_pem)?;

    let mut root_store = rustls::RootCertStore::empty();
    for cert in &ca_certs {
        root_store
            .add(cert.clone())
            .map_err(|e| Error::Ssl(format!("failed to add CA cert: {e}")))?;
    }

    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(our_certs, our_key)
        .map_err(|e| Error::Ssl(format!("client config error: {e}")))?;

    Ok(Arc::new(client_config))
}

/// Build a rustls `ServerConfig` using our cert and key.
pub fn build_server_config(config: &SslConfig) -> Result<Arc<rustls::ServerConfig>> {
    let ca_certs = parse_pem_certs(&config.ca_cert_pem)?;
    let our_certs = parse_pem_certs(&config.our_cert_pem)?;
    let our_key = parse_pem_key(&config.our_key_pem)?;

    // Build a verifier that requires client certs chaining to the torrent's CA
    let mut root_store = rustls::RootCertStore::empty();
    for cert in &ca_certs {
        root_store
            .add(cert.clone())
            .map_err(|e| Error::Ssl(format!("failed to add CA cert: {e}")))?;
    }

    let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
        .build()
        .map_err(|e| Error::Ssl(format!("client verifier error: {e}")))?;

    let server_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(our_certs, our_key)
        .map_err(|e| Error::Ssl(format!("server config error: {e}")))?;

    Ok(Arc::new(server_config))
}

/// Perform a TLS client handshake, setting SNI to the hex-encoded info hash.
pub async fn connect_tls<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    info_hash: Id20,
    client_config: Arc<rustls::ClientConfig>,
) -> Result<ClientTlsStream<S>> {
    let sni = info_hash.to_hex();
    let server_name = ServerName::try_from(sni.as_str())
        .map_err(|e| Error::Ssl(format!("invalid SNI: {e}")))?;

    let connector = TlsConnector::from(client_config);
    connector
        .connect(server_name.to_owned(), stream)
        .await
        .map_err(|e| Error::Ssl(format!("TLS handshake failed: {e}")))
}

/// Accept a TLS connection on the server side.
pub async fn accept_tls<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    server_config: Arc<rustls::ServerConfig>,
) -> Result<ServerTlsStream<S>> {
    let acceptor = TlsAcceptor::from(server_config);
    acceptor
        .accept(stream)
        .await
        .map_err(|e| Error::Ssl(format!("TLS accept failed: {e}")))
}

/// Generate a self-signed client certificate + private key using rcgen.
///
/// The certificate is valid for 365 days and has CN=ferrite-peer.
pub fn generate_self_signed_cert() -> Result<(Vec<u8>, Vec<u8>)> {
    use rcgen::{CertificateParams, KeyPair};

    let key_pair = KeyPair::generate()
        .map_err(|e| Error::Ssl(format!("key generation failed: {e}")))?;

    let mut params = CertificateParams::new(vec!["ferrite-peer".to_string()])
        .map_err(|e| Error::Ssl(format!("cert params error: {e}")))?;
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String("ferrite-peer".into()),
    );

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| Error::Ssl(format!("self-signed cert generation failed: {e}")))?;

    let cert_pem = cert.pem().into_bytes();
    let key_pem = key_pair.serialize_pem().into_bytes();

    Ok((cert_pem, key_pem))
}

/// Parse PEM-encoded certificates.
fn parse_pem_certs(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = std::io::BufReader::new(pem);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Ssl(format!("failed to parse PEM certs: {e}")))?;

    if certs.is_empty() {
        return Err(Error::Ssl("no certificates found in PEM data".into()));
    }

    Ok(certs)
}

/// Parse a PEM-encoded private key.
fn parse_pem_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>> {
    let mut reader = std::io::BufReader::new(pem);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| Error::Ssl(format!("failed to parse PEM key: {e}")))?
        .ok_or_else(|| Error::Ssl("no private key found in PEM data".into()))
}
```

### 2b. Add `Ssl` variant to `ferrite_wire::Error`

**File:** `crates/ferrite-wire/src/error.rs`

```rust
// Add variant:
#[error("SSL/TLS: {0}")]
Ssl(String),
```

### 2c. Export the ssl module

**File:** `crates/ferrite-wire/src/lib.rs`

```rust
pub mod ssl;
```

### 2d. Update `crates/ferrite-wire/Cargo.toml`

```toml
[dependencies]
# ... existing deps ...
rustls = "0.23"
tokio-rustls = "0.26"
rustls-pemfile = "2"
rcgen = "0.13"
```

### 2e. Tests (ferrite-wire)

**File:** `crates/ferrite-wire/src/ssl.rs` (append `#[cfg(test)] mod tests`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn generate_self_signed_cert_produces_valid_pem() {
        let (cert_pem, key_pem) = generate_self_signed_cert().unwrap();
        assert!(cert_pem.starts_with(b"-----BEGIN CERTIFICATE-----"));
        assert!(key_pem.starts_with(b"-----BEGIN PRIVATE KEY-----")
            || key_pem.starts_with(b"-----BEGIN RSA PRIVATE KEY-----")
            || key_pem.starts_with(b"-----BEGIN EC PRIVATE KEY-----"));

        // Verify they parse back
        let certs = parse_pem_certs(&cert_pem).unwrap();
        assert_eq!(certs.len(), 1);
        let _key = parse_pem_key(&key_pem).unwrap();
    }

    #[test]
    fn parse_pem_certs_rejects_empty() {
        assert!(parse_pem_certs(b"").is_err());
        assert!(parse_pem_certs(b"not a cert").is_err());
    }

    #[test]
    fn parse_pem_key_rejects_empty() {
        assert!(parse_pem_key(b"").is_err());
    }

    #[test]
    fn build_client_config_with_self_signed() {
        // Generate a CA cert and use it as both CA and client cert
        let (cert_pem, key_pem) = generate_self_signed_cert().unwrap();
        let config = SslConfig {
            ca_cert_pem: cert_pem.clone(),
            our_cert_pem: cert_pem,
            our_key_pem: key_pem,
        };
        let client_config = build_client_config(&config).unwrap();
        assert!(Arc::strong_count(&client_config) >= 1);
    }

    #[test]
    fn build_server_config_with_self_signed() {
        let (cert_pem, key_pem) = generate_self_signed_cert().unwrap();
        let config = SslConfig {
            ca_cert_pem: cert_pem.clone(),
            our_cert_pem: cert_pem,
            our_key_pem: key_pem,
        };
        let server_config = build_server_config(&config).unwrap();
        assert!(Arc::strong_count(&server_config) >= 1);
    }

    /// Generate a CA and a leaf cert signed by it, for integration tests.
    fn generate_ca_and_leaf() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        use rcgen::{CertificateParams, KeyPair, IsCa, BasicConstraints};

        // CA
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(vec![]).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.distinguished_name.push(
            rcgen::DnType::CommonName,
            rcgen::DnValue::Utf8String("Test CA".into()),
        );
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        // Leaf
        let leaf_key = KeyPair::generate().unwrap();
        let mut leaf_params = CertificateParams::new(vec!["ferrite-peer".to_string()]).unwrap();
        leaf_params.distinguished_name.push(
            rcgen::DnType::CommonName,
            rcgen::DnValue::Utf8String("ferrite-peer".into()),
        );
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();

        (
            ca_cert.pem().into_bytes(),
            leaf_cert.pem().into_bytes(),
            leaf_key.serialize_pem().into_bytes(),
        )
    }

    #[tokio::test]
    async fn tls_handshake_client_server_round_trip() {
        let (ca_pem, leaf_cert_pem, leaf_key_pem) = generate_ca_and_leaf();

        // Both sides use leaf cert signed by our CA
        let ssl_config = SslConfig {
            ca_cert_pem: ca_pem,
            our_cert_pem: leaf_cert_pem,
            our_key_pem: leaf_key_pem,
        };

        let client_tls_config = build_client_config(&ssl_config).unwrap();
        let server_tls_config = build_server_config(&ssl_config).unwrap();

        let info_hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let (client_raw, server_raw) = tokio::io::duplex(16384);

        let server_handle = tokio::spawn(async move {
            let mut tls_stream = accept_tls(server_raw, server_tls_config).await.unwrap();
            let mut buf = [0u8; 11];
            tls_stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"hello world");
            tls_stream.write_all(b"hello back").await.unwrap();
            tls_stream.flush().await.unwrap();
        });

        let mut client_stream = connect_tls(client_raw, info_hash, client_tls_config)
            .await
            .unwrap();
        client_stream.write_all(b"hello world").await.unwrap();
        client_stream.flush().await.unwrap();

        let mut buf = [0u8; 10];
        client_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello back");

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn tls_handshake_rejects_untrusted_cert() {
        // Generate two separate CAs — client trusts CA1 but server uses cert from CA2
        let (ca1_pem, leaf1_cert_pem, leaf1_key_pem) = generate_ca_and_leaf();
        let (ca2_pem, leaf2_cert_pem, leaf2_key_pem) = generate_ca_and_leaf();

        let client_config_data = SslConfig {
            ca_cert_pem: ca1_pem,            // client trusts CA1
            our_cert_pem: leaf1_cert_pem,
            our_key_pem: leaf1_key_pem,
        };
        let server_config_data = SslConfig {
            ca_cert_pem: ca2_pem,            // server has CA2 cert
            our_cert_pem: leaf2_cert_pem,
            our_key_pem: leaf2_key_pem,
        };

        let client_tls_config = build_client_config(&client_config_data).unwrap();
        let server_tls_config = build_server_config(&server_config_data).unwrap();

        let info_hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let (client_raw, server_raw) = tokio::io::duplex(16384);

        let server_handle = tokio::spawn(async move {
            let _ = accept_tls(server_raw, server_tls_config).await;
        });

        // Client handshake should fail because server cert is from untrusted CA
        let result = connect_tls(client_raw, info_hash, client_tls_config).await;
        assert!(result.is_err());

        let _ = server_handle.await;
    }
}
```

---

## Phase 3: ferrite-session — Settings, Alerts, SSL Listener

### 3a. Add SSL settings

**File:** `crates/ferrite-session/src/settings.rs`

Add three new fields to `Settings`, plus a default helper:

```rust
// New default helpers:
fn default_ssl_listen_port() -> u16 {
    0 // 0 = disabled
}

// In Settings struct, add new section after uTP tuning:
// ── SSL torrents ──
/// SSL listen port for SSL torrent incoming connections.
/// 0 = disabled (no SSL listener). When set, a TLS listener is bound
/// on this port for torrents with `ssl-cert` in their info dict.
#[serde(default = "default_ssl_listen_port")]
pub ssl_listen_port: u16,
/// Path to the PEM-encoded certificate file for SSL torrent connections.
/// If not set, a self-signed certificate is auto-generated on first use
/// and stored in `resume_data_dir` (or a temp directory).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub ssl_cert_path: Option<PathBuf>,
/// Path to the PEM-encoded private key file for SSL torrent connections.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub ssl_key_path: Option<PathBuf>,
```

Update `Default for Settings`:

```rust
// SSL torrents
ssl_listen_port: 0,
ssl_cert_path: None,
ssl_key_path: None,
```

Update `PartialEq for Settings` — add the three new field comparisons.

Add validation in `Settings::validate()`:

```rust
// SSL cert/key must both be set or both absent
if self.ssl_cert_path.is_some() != self.ssl_key_path.is_some() {
    return Err(crate::Error::InvalidSettings(
        "ssl_cert_path and ssl_key_path must both be set or both absent".into(),
    ));
}
```

### 3b. Add `SslTorrentError` alert

**File:** `crates/ferrite-session/src/alert.rs`

```rust
// In AlertKind enum, add:
// ── SSL ──
/// SSL/TLS error for an SSL torrent (handshake failure, cert validation, etc.).
SslTorrentError { info_hash: Id20, message: String },
```

In `AlertKind::category()`:

```rust
SslTorrentError { .. } => AlertCategory::ERROR,
```

### 3c. Add SSL dependencies to `ferrite-session/Cargo.toml`

```toml
[dependencies]
# ... existing ...
ferrite-wire = { path = "../ferrite-wire" }
# (ferrite-wire already pulls in rustls/tokio-rustls/rcgen, but we need
# tokio-rustls directly for the TlsAcceptor in the session listener)
tokio-rustls = "0.26"
rustls = "0.23"
```

### 3d. SSL certificate manager

**File:** `crates/ferrite-session/src/ssl_manager.rs` (new file)

This module manages the client certificate (load from disk or auto-generate) and builds per-torrent TLS configurations from the embedded CA cert.

```rust
//! SSL certificate management for SSL torrents.
//!
//! Handles loading or auto-generating client certificates and building
//! per-torrent TLS configurations from the CA cert embedded in .torrent files.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferrite_core::Id20;
use ferrite_wire::ssl::{self, SslConfig};

use crate::settings::Settings;

/// Manages SSL certificates for the session.
///
/// Created once at session start; provides per-torrent TLS configs on demand.
pub(crate) struct SslManager {
    /// Our client certificate (PEM bytes).
    our_cert_pem: Vec<u8>,
    /// Our private key (PEM bytes).
    our_key_pem: Vec<u8>,
}

impl SslManager {
    /// Create a new SSL manager from session settings.
    ///
    /// If `ssl_cert_path` / `ssl_key_path` are set, loads them from disk.
    /// Otherwise, generates a self-signed cert and optionally persists it
    /// to `resume_data_dir`.
    pub fn new(settings: &Settings) -> crate::Result<Self> {
        let (cert_pem, key_pem) = if let (Some(cert_path), Some(key_path)) =
            (&settings.ssl_cert_path, &settings.ssl_key_path)
        {
            let cert = std::fs::read(cert_path).map_err(|e| {
                crate::Error::Config(format!("failed to read SSL cert {}: {e}", cert_path.display()))
            })?;
            let key = std::fs::read(key_path).map_err(|e| {
                crate::Error::Config(format!("failed to read SSL key {}: {e}", key_path.display()))
            })?;
            (cert, key)
        } else {
            // Auto-generate self-signed cert
            let (cert, key) = ssl::generate_self_signed_cert()
                .map_err(|e| crate::Error::Config(format!("SSL cert generation: {e}")))?;

            // Persist to resume_data_dir if available
            if let Some(ref dir) = settings.resume_data_dir {
                let cert_path = dir.join("ssl_cert.pem");
                let key_path = dir.join("ssl_key.pem");
                // Best-effort persistence — don't fail if dir doesn't exist yet
                let _ = std::fs::create_dir_all(dir);
                let _ = std::fs::write(&cert_path, &cert);
                let _ = std::fs::write(&key_path, &key);
            }

            (cert, key)
        };

        Ok(Self {
            our_cert_pem: cert_pem,
            our_key_pem: key_pem,
        })
    }

    /// Build an `SslConfig` for a specific torrent (combines our cert with the torrent's CA).
    pub fn config_for_torrent(&self, ca_cert_pem: &[u8]) -> SslConfig {
        SslConfig {
            ca_cert_pem: ca_cert_pem.to_vec(),
            our_cert_pem: self.our_cert_pem.clone(),
            our_key_pem: self.our_key_pem.clone(),
        }
    }

    /// Build a rustls ClientConfig for outbound TLS connections to an SSL torrent.
    pub fn client_config(
        &self,
        ca_cert_pem: &[u8],
    ) -> Result<Arc<rustls::ClientConfig>, ferrite_wire::Error> {
        let config = self.config_for_torrent(ca_cert_pem);
        ssl::build_client_config(&config)
    }

    /// Build a rustls ServerConfig for inbound TLS connections to an SSL torrent.
    pub fn server_config(
        &self,
        ca_cert_pem: &[u8],
    ) -> Result<Arc<rustls::ServerConfig>, ferrite_wire::Error> {
        let config = self.config_for_torrent(ca_cert_pem);
        ssl::build_server_config(&config)
    }
}
```

### 3e. Integrate SSL into peer connection flow

**File:** `crates/ferrite-session/src/torrent.rs`

Add to `TorrentActor` struct:

```rust
/// SSL CA cert for this torrent (None = not an SSL torrent).
ssl_cert: Option<Vec<u8>>,
/// Cached rustls ClientConfig for outbound SSL connections.
ssl_client_config: Option<Arc<rustls::ClientConfig>>,
/// Cached rustls ServerConfig for inbound SSL connections.
ssl_server_config: Option<Arc<rustls::ServerConfig>>,
```

**Initialize in `TorrentHandle::from_torrent()`:**

When the torrent has an `ssl_cert`, build and cache the TLS configs:

```rust
let (ssl_cert, ssl_client_config, ssl_server_config) = if let Some(ref cert) = meta.ssl_cert {
    // Build TLS configs using the SslManager from session
    let ssl_config = ssl_manager.config_for_torrent(cert);
    let client_cfg = ferrite_wire::ssl::build_client_config(&ssl_config)
        .map_err(|e| crate::Error::Config(format!("SSL client config: {e}")))?;
    let server_cfg = ferrite_wire::ssl::build_server_config(&ssl_config)
        .map_err(|e| crate::Error::Config(format!("SSL server config: {e}")))?;
    (Some(cert.clone()), Some(client_cfg), Some(server_cfg))
} else {
    (None, None, None)
};
```

Pass `ssl_manager: Option<Arc<SslManager>>` from `SessionActor` to `TorrentHandle::from_torrent()`.

**Modify `try_connect_peers()`** to wrap TCP streams with TLS for SSL torrents:

```rust
// In the TCP connection branch, after successful connect:
let tcp_stream = match tcp_result {
    Ok(stream) => stream,
    Err(e) => { /* existing error handling */ }
};

// SSL torrent: wrap with TLS before passing to run_peer
if let Some(ref client_config) = ssl_client_config {
    match ferrite_wire::ssl::connect_tls(tcp_stream, info_hash, Arc::clone(client_config)).await {
        Ok(tls_stream) => {
            let _ = run_peer(
                addr, tls_stream, info_hash, peer_id, bitfield,
                num_pieces, event_tx, cmd_rx, enable_dht, enable_fast,
                encryption_mode, true, anonymous_mode, info_bytes, plugins,
            ).await;
        }
        Err(e) => {
            warn!(%addr, error = %e, "SSL handshake failed");
            let _ = event_tx.send(PeerEvent::Disconnected {
                peer_addr: addr,
                reason: Some(format!("SSL handshake: {e}")),
            }).await;
        }
    }
} else {
    // Non-SSL: existing run_peer call
    let _ = run_peer(
        addr, tcp_stream, info_hash, peer_id, bitfield,
        num_pieces, event_tx, cmd_rx, enable_dht, enable_fast,
        encryption_mode, true, anonymous_mode, info_bytes, plugins,
    ).await;
}
```

Note: For SSL torrents, MSE/PE encryption is **skipped** — TLS provides the transport security. The `encryption_mode` passed to `run_peer` should be `EncryptionMode::Disabled` for SSL torrents since the TLS layer already provides encryption.

**Modify `spawn_peer_from_stream()`** for inbound SSL connections:

When a connection arrives on the SSL listen port, it has already been TLS-accepted by the session-level listener (see 3f). The resulting `TlsStream` is passed directly to `spawn_peer_from_stream`, which handles it transparently since `TlsStream` implements `AsyncRead + AsyncWrite`.

### 3f. SSL listener in SessionActor

**File:** `crates/ferrite-session/src/session.rs`

Add to `SessionActor` struct:

```rust
/// SSL manager for SSL torrent certificate handling.
ssl_manager: Option<Arc<crate::ssl_manager::SslManager>>,
/// SSL/TLS TCP listener (separate port from the main listener).
ssl_listener: Option<tokio::net::TcpListener>,
```

In `SessionHandle::start_with_plugins()`, after existing listener setup:

```rust
// SSL manager (always create if ssl_listen_port != 0 or any SSL torrent might be added)
let ssl_manager = if settings.ssl_listen_port != 0
    || settings.ssl_cert_path.is_some()
{
    Some(Arc::new(crate::ssl_manager::SslManager::new(&settings)?))
} else {
    None
};

// SSL listener
let ssl_listener = if settings.ssl_listen_port != 0 {
    match TcpListener::bind(("0.0.0.0", settings.ssl_listen_port)).await {
        Ok(l) => {
            info!(port = settings.ssl_listen_port, "SSL listener started");
            Some(l)
        }
        Err(e) => {
            warn!(port = settings.ssl_listen_port, error = %e, "SSL listener bind failed");
            None
        }
    }
} else {
    None
};
```

In the `SessionActor::run()` event loop, add an arm for SSL listener accepts:

```rust
// Accept SSL connections
result = async {
    if let Some(ref listener) = self.ssl_listener {
        listener.accept().await
    } else {
        std::future::pending().await
    }
} => {
    if let Ok((stream, addr)) = result {
        self.handle_ssl_incoming(stream, addr).await;
    }
}
```

The `handle_ssl_incoming` method:

```rust
async fn handle_ssl_incoming(
    &mut self,
    stream: tokio::net::TcpStream,
    addr: std::net::SocketAddr,
) {
    // We need to accept the TLS connection first to read the SNI,
    // which tells us which torrent this connection is for.
    // However, rustls's TlsAcceptor doesn't expose SNI before accept().
    //
    // Strategy: Use rustls's lazy accept to peek at the ClientHello SNI,
    // then look up the torrent by info hash, get its server config, and
    // complete the handshake.

    let acceptor = tokio_rustls::LazyConfigAcceptor::new(
        rustls::server::Acceptor::default(),
        stream,
    );

    let start_handshake = match acceptor.await {
        Ok(sh) => sh,
        Err(e) => {
            debug!(%addr, error = %e, "SSL ClientHello read failed");
            return;
        }
    };

    // Extract SNI from ClientHello
    let client_hello = start_handshake.client_hello();
    let sni = match client_hello.server_name() {
        Some(name) => name.to_string(),
        None => {
            debug!(%addr, "SSL connection missing SNI");
            return;
        }
    };

    // SNI is hex-encoded info hash (40 chars)
    let info_hash = match Id20::from_hex(&sni) {
        Ok(h) => h,
        Err(_) => {
            debug!(%addr, sni = %sni, "SSL SNI is not a valid info hash");
            return;
        }
    };

    // Look up the torrent and get its server config
    let torrent = match self.torrents.get(&info_hash) {
        Some(t) => t,
        None => {
            debug!(%addr, %info_hash, "SSL connection for unknown torrent");
            return;
        }
    };

    // Get the SSL CA cert from the torrent's metadata
    let ssl_cert = match torrent.meta.as_ref().and_then(|m| m.ssl_cert.as_ref()) {
        Some(cert) => cert.clone(),
        None => {
            debug!(%addr, %info_hash, "SSL connection for non-SSL torrent");
            return;
        }
    };

    // Build server config using the torrent's CA cert
    let server_config = match self.ssl_manager.as_ref() {
        Some(mgr) => match mgr.server_config(&ssl_cert) {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!(%addr, %info_hash, error = %e, "failed to build SSL server config");
                return;
            }
        },
        None => {
            debug!(%addr, "SSL manager not initialized");
            return;
        }
    };

    // Complete the TLS handshake
    let tls_stream = match start_handshake.into_stream(server_config).await {
        Ok(s) => s,
        Err(e) => {
            warn!(%addr, %info_hash, error = %e, "SSL handshake failed");
            post_alert(
                &self.alert_tx,
                &self.alert_mask,
                AlertKind::SslTorrentError {
                    info_hash,
                    message: format!("inbound TLS handshake from {addr}: {e}"),
                },
            );
            return;
        }
    };

    // Route to the torrent actor
    // Send a command to the torrent to spawn a peer from this TLS stream
    let _ = torrent.handle.spawn_ssl_peer(addr, tls_stream).await;
}
```

The `TorrentHandle` needs a new command variant for SSL peers. Add to `TorrentCommand`:

```rust
SpawnSslPeer {
    addr: SocketAddr,
    // The TLS stream is boxed to erase the type since TorrentCommand
    // is sent over a channel
    stream: Box<dyn tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send>,
},
```

And handle it in the TorrentActor event loop:

```rust
TorrentCommand::SpawnSslPeer { addr, stream } => {
    self.spawn_peer_from_stream_with_mode(
        addr,
        stream,
        Some(ferrite_wire::mse::EncryptionMode::Disabled), // TLS provides encryption
    );
}
```

### 3g. Pass SSL state through TorrentHandle construction

The `TorrentHandle::from_torrent()` and `TorrentHandle::from_magnet()` signatures need an additional `ssl_manager: Option<Arc<SslManager>>` parameter. The session actor passes this when creating torrent handles.

For magnet links: SSL state is unknown until metadata is received. After metadata fetch completes and `ssl_cert` is found, the TorrentActor must lazily initialize its SSL configs:

```rust
// In the metadata-received handler:
if let Some(ref cert) = meta.ssl_cert {
    if let Some(ref mgr) = self.ssl_manager {
        match mgr.client_config(cert) {
            Ok(cfg) => self.ssl_client_config = Some(cfg),
            Err(e) => warn!("failed to build SSL client config: {e}"),
        }
        match mgr.server_config(cert) {
            Ok(cfg) => self.ssl_server_config = Some(cfg),
            Err(e) => warn!("failed to build SSL server config: {e}"),
        }
        self.ssl_cert = Some(cert.clone());
    }
}
```

### 3h. Update facade `ClientBuilder`

**File:** `crates/ferrite/src/client.rs`

```rust
/// Set the SSL listen port for SSL torrent connections.
///
/// When set to a non-zero port, the session binds a TLS listener for
/// torrents with `ssl-cert` in their info dict. Peers connect using TLS
/// with the info hash as SNI. Default: 0 (disabled).
pub fn ssl_listen_port(mut self, port: u16) -> Self {
    self.settings.ssl_listen_port = port;
    self
}

/// Set the path to the PEM-encoded certificate for SSL torrent connections.
///
/// Must be paired with `ssl_key_path`. If neither is set, a self-signed
/// certificate is auto-generated on first use.
pub fn ssl_cert_path(mut self, path: impl Into<PathBuf>) -> Self {
    self.settings.ssl_cert_path = Some(path.into());
    self
}

/// Set the path to the PEM-encoded private key for SSL torrent connections.
pub fn ssl_key_path(mut self, path: impl Into<PathBuf>) -> Self {
    self.settings.ssl_key_path = Some(path.into());
    self
}
```

---

## Phase 4: Integration Tests

### 4a. ferrite-session integration test

**File:** `crates/ferrite-session/src/ssl_manager.rs` (tests at bottom)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;

    #[test]
    fn ssl_manager_auto_generates_cert() {
        let settings = Settings::default();
        let mgr = SslManager::new(&settings).unwrap();
        // Should have generated a cert and key
        assert!(!mgr.our_cert_pem.is_empty());
        assert!(!mgr.our_key_pem.is_empty());
    }

    #[test]
    fn ssl_manager_config_for_torrent() {
        let settings = Settings::default();
        let mgr = SslManager::new(&settings).unwrap();

        let ca_cert = ferrite_wire::ssl::generate_self_signed_cert().unwrap().0;
        let config = mgr.config_for_torrent(&ca_cert);
        assert_eq!(config.ca_cert_pem, ca_cert);
        assert_eq!(config.our_cert_pem, mgr.our_cert_pem);
    }

    #[test]
    fn ssl_manager_persists_to_resume_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.resume_data_dir = Some(dir.path().to_path_buf());

        let _mgr = SslManager::new(&settings).unwrap();
        assert!(dir.path().join("ssl_cert.pem").exists());
        assert!(dir.path().join("ssl_key.pem").exists());
    }

    #[test]
    fn ssl_manager_loads_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = ferrite_wire::ssl::generate_self_signed_cert().unwrap();

        let cert_path = dir.path().join("test_cert.pem");
        let key_path = dir.path().join("test_key.pem");
        std::fs::write(&cert_path, &cert).unwrap();
        std::fs::write(&key_path, &key).unwrap();

        let mut settings = Settings::default();
        settings.ssl_cert_path = Some(cert_path);
        settings.ssl_key_path = Some(key_path);

        let mgr = SslManager::new(&settings).unwrap();
        assert_eq!(mgr.our_cert_pem, cert);
        assert_eq!(mgr.our_key_pem, key);
    }
}
```

### 4b. Settings validation tests

**File:** `crates/ferrite-session/src/settings.rs` (append to tests)

```rust
#[test]
fn ssl_settings_defaults() {
    let s = Settings::default();
    assert_eq!(s.ssl_listen_port, 0);
    assert!(s.ssl_cert_path.is_none());
    assert!(s.ssl_key_path.is_none());
}

#[test]
fn ssl_cert_key_must_pair() {
    let mut s = Settings::default();
    s.ssl_cert_path = Some("/tmp/cert.pem".into());
    // key_path is None — should fail validation
    let err = s.validate().unwrap_err();
    assert!(err.to_string().contains("ssl_cert_path"));
}

#[test]
fn ssl_settings_json_round_trip() {
    let mut s = Settings::default();
    s.ssl_listen_port = 4433;
    s.ssl_cert_path = Some("/tmp/cert.pem".into());
    s.ssl_key_path = Some("/tmp/key.pem".into());
    let json = serde_json::to_string(&s).unwrap();
    let decoded: Settings = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.ssl_listen_port, 4433);
    assert_eq!(decoded.ssl_cert_path, Some(std::path::PathBuf::from("/tmp/cert.pem")));
    assert_eq!(decoded.ssl_key_path, Some(std::path::PathBuf::from("/tmp/key.pem")));
}
```

### 4c. Alert test

**File:** `crates/ferrite-session/src/alert.rs` (append to tests)

```rust
#[test]
fn ssl_torrent_error_alert_has_error_category() {
    let alert = Alert::new(AlertKind::SslTorrentError {
        info_hash: Id20::from([0u8; 20]),
        message: "handshake failed".into(),
    });
    assert!(alert.category().contains(AlertCategory::ERROR));
}
```

### 4d. Facade tests

**File:** `crates/ferrite/src/lib.rs` (append to tests)

```rust
#[test]
fn client_builder_ssl_config() {
    let config = crate::ClientBuilder::new()
        .ssl_listen_port(4433)
        .ssl_cert_path("/tmp/cert.pem")
        .ssl_key_path("/tmp/key.pem")
        .into_settings();
    assert_eq!(config.ssl_listen_port, 4433);
    assert_eq!(
        config.ssl_cert_path,
        Some(std::path::PathBuf::from("/tmp/cert.pem"))
    );
    assert_eq!(
        config.ssl_key_path,
        Some(std::path::PathBuf::from("/tmp/key.pem"))
    );
}
```

---

## Implementation Order (TDD Steps)

Each step follows red-green-refactor: write test, implement code, verify `cargo test --workspace`.

| Step | Crate | What | Tests |
|------|-------|------|-------|
| 1 | ferrite-core | Add `ssl_cert` field to `InfoDict` (serde), `TorrentMetaV1`, `RawTorrent` | `ssl_cert_parsed_from_info_dict`, `ssl_cert_absent_by_default` |
| 2 | ferrite-core | Add `ssl_cert()` / `is_ssl()` to `TorrentMeta` | `torrent_meta_ssl_accessors` |
| 3 | ferrite-core | Add `ssl_cert` to `TorrentMetaV2` / `InfoDictV2` | (parse test with v2 torrent) |
| 4 | ferrite-core | Add `set_ssl_cert()` to `CreateTorrent` | `create_torrent_with_ssl_cert` |
| 5 | ferrite-wire | Add `Ssl(String)` variant to `Error` | (compile check) |
| 6 | ferrite-wire | Add `Cargo.toml` deps: rustls, tokio-rustls, rustls-pemfile, rcgen | (compile check) |
| 7 | ferrite-wire | Implement `ssl.rs`: `SslConfig`, PEM parsing, config builders | `generate_self_signed_cert_produces_valid_pem`, `parse_pem_*`, `build_*_config` |
| 8 | ferrite-wire | Implement `connect_tls()` / `accept_tls()` | `tls_handshake_client_server_round_trip` |
| 9 | ferrite-wire | Test CA trust chain enforcement | `tls_handshake_rejects_untrusted_cert` |
| 10 | ferrite-session | Add SSL settings to `Settings` | `ssl_settings_defaults`, `ssl_cert_key_must_pair`, `ssl_settings_json_round_trip` |
| 11 | ferrite-session | Add `SslTorrentError` alert | `ssl_torrent_error_alert_has_error_category` |
| 12 | ferrite-session | Implement `ssl_manager.rs` | `ssl_manager_auto_generates_cert`, `ssl_manager_config_for_torrent`, `ssl_manager_persists_to_resume_dir`, `ssl_manager_loads_from_disk` |
| 13 | ferrite-session | Add SSL fields to `TorrentActor`, modify `from_torrent()` | (existing tests must pass) |
| 14 | ferrite-session | Modify `try_connect_peers()` for outbound TLS | (integration) |
| 15 | ferrite-session | Add `SpawnSslPeer` command, handler in TorrentActor | (integration) |
| 16 | ferrite-session | Add SSL listener to `SessionActor`, `handle_ssl_incoming()` with lazy accept | (integration) |
| 17 | ferrite-session | Pass `ssl_manager` to `from_magnet()`, lazy init on metadata received | (integration) |
| 18 | ferrite (facade) | Add `ssl_listen_port()`, `ssl_cert_path()`, `ssl_key_path()` to `ClientBuilder` | `client_builder_ssl_config` |
| 19 | workspace | `cargo test --workspace && cargo clippy --workspace -- -D warnings` | All 895 + ~16 new tests pass |

---

## Dependency Changes

### `crates/ferrite-wire/Cargo.toml` (new deps)

```toml
rustls = "0.23"
tokio-rustls = "0.26"
rustls-pemfile = "2"
rcgen = "0.13"
```

### `crates/ferrite-session/Cargo.toml` (new deps)

```toml
tokio-rustls = "0.26"
rustls = "0.23"
```

### `Cargo.toml` (workspace-level, optional)

Consider promoting `rustls` and `tokio-rustls` to workspace deps if reused across crates. For M42 it is only ferrite-wire and ferrite-session, so crate-local deps are fine.

---

## New Files

| File | Purpose |
|------|---------|
| `crates/ferrite-wire/src/ssl.rs` | TLS transport: config builders, connect/accept, cert generation |
| `crates/ferrite-session/src/ssl_manager.rs` | Certificate lifecycle management, per-torrent TLS config factory |

---

## Modified Files Summary

| File | Changes |
|------|---------|
| `crates/ferrite-core/src/metainfo.rs` | Add `ssl_cert` to `InfoDict` and `TorrentMetaV1`, propagate in parsing |
| `crates/ferrite-core/src/metainfo_v2.rs` | Add `ssl_cert` to `InfoDictV2` and `TorrentMetaV2` |
| `crates/ferrite-core/src/detect.rs` | Add `ssl_cert()` / `is_ssl()` to `TorrentMeta` |
| `crates/ferrite-core/src/create.rs` | Add `set_ssl_cert()` builder method |
| `crates/ferrite-wire/src/lib.rs` | Export `pub mod ssl` |
| `crates/ferrite-wire/src/error.rs` | Add `Ssl(String)` variant |
| `crates/ferrite-wire/Cargo.toml` | Add rustls, tokio-rustls, rustls-pemfile, rcgen |
| `crates/ferrite-session/Cargo.toml` | Add tokio-rustls, rustls |
| `crates/ferrite-session/src/settings.rs` | Add `ssl_listen_port`, `ssl_cert_path`, `ssl_key_path` + validation |
| `crates/ferrite-session/src/alert.rs` | Add `SslTorrentError` variant |
| `crates/ferrite-session/src/session.rs` | Add `ssl_manager`, `ssl_listener`, `handle_ssl_incoming()` |
| `crates/ferrite-session/src/torrent.rs` | Add SSL fields, modify `try_connect_peers()`, `from_torrent()`, `from_magnet()` |
| `crates/ferrite-session/src/types.rs` | Add `SpawnSslPeer` to `TorrentCommand` |
| `crates/ferrite-session/src/lib.rs` | Export `ssl_manager` module |
| `crates/ferrite/src/client.rs` | Add `ssl_listen_port()`, `ssl_cert_path()`, `ssl_key_path()` |
| `crates/ferrite/src/lib.rs` | Tests for SSL facade methods |

---

## Design Decisions

### 1. SNI-based torrent routing

The SSL listen port multiplexes all SSL torrents on a single port. The info hash is encoded as a 40-char hex string in the TLS SNI extension. The `SessionActor` uses `tokio_rustls::LazyConfigAcceptor` to peek at the ClientHello before completing the handshake, enabling per-torrent server configs with different CA trust chains.

### 2. TLS replaces MSE for SSL torrents

SSL torrents bypass MSE/PE encryption entirely. When an SSL torrent connects to a peer, the connection flow is: TCP connect -> TLS handshake -> BitTorrent handshake -> message loop. The `EncryptionMode::Disabled` override is passed to `run_peer()` so the MSE negotiation phase is skipped.

### 3. Certificate auto-generation

If no cert/key paths are configured, a self-signed ECDSA certificate is generated via `rcgen` on first use. This cert is used as the client certificate in TLS mutual authentication. The cert is persisted to `resume_data_dir` if available, so it survives session restarts.

### 4. Lazy SSL initialization for magnets

For magnet links, the `ssl_cert` field is unknown until metadata is received via BEP 9. When metadata arrives and contains `ssl-cert`, the TorrentActor lazily initializes its TLS configs. Any peers already connected (via plain TCP for metadata exchange) are disconnected and reconnected via TLS.

### 5. Error handling via alerts

SSL handshake failures are reported via `SslTorrentError` alerts (category: `ERROR`) and logged at `warn` level. Failed connections are treated like any other peer disconnect — the peer is marked disconnected and may be retried.

---

## Version

Bump workspace version from `0.41.0` to `0.42.0` in root `Cargo.toml` upon completion:

```toml
[workspace.package]
version = "0.42.0"
```

Commit format: `feat: SSL torrent support with TLS peer authentication (M42)`

---

## Test Count

~16 new tests:

- ferrite-core: 3 (ssl_cert parsing, absent, creation round-trip)
- ferrite-wire: 7 (cert generation, PEM parsing x2, config building x2, TLS round-trip, trust rejection)
- ferrite-session: 5 (settings defaults, cert/key pairing validation, json round-trip, alert category, ssl manager x4 = 4 tests)
- ferrite (facade): 1 (builder config)

**Projected total:** 895 + 16 = ~911 tests
