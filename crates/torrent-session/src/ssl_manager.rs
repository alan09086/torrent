//! SSL certificate management for SSL torrents.
//!
//! Handles loading or auto-generating client certificates and building
//! per-torrent TLS configurations from the CA cert embedded in .torrent files.

use std::sync::Arc;

use torrent_wire::ssl::{self, SslConfig};

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
                crate::Error::Config(format!(
                    "failed to read SSL cert {}: {e}",
                    cert_path.display()
                ))
            })?;
            let key = std::fs::read(key_path).map_err(|e| {
                crate::Error::Config(format!(
                    "failed to read SSL key {}: {e}",
                    key_path.display()
                ))
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
                // Best-effort persistence -- don't fail if dir doesn't exist yet
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

    /// Create an SSL manager from existing PEM bytes.
    pub fn from_pem(cert_pem: Vec<u8>, key_pem: Vec<u8>) -> Self {
        Self {
            our_cert_pem: cert_pem,
            our_key_pem: key_pem,
        }
    }

    /// Build an [`SslConfig`] for a specific torrent (combines our cert with the torrent's CA).
    pub fn config_for_torrent(&self, ca_cert_pem: &[u8]) -> SslConfig {
        SslConfig {
            ca_cert_pem: ca_cert_pem.to_vec(),
            our_cert_pem: self.our_cert_pem.clone(),
            our_key_pem: self.our_key_pem.clone(),
        }
    }

    /// Build a rustls `ClientConfig` for outbound TLS connections to an SSL torrent.
    pub fn client_config(
        &self,
        ca_cert_pem: &[u8],
    ) -> Result<Arc<rustls::ClientConfig>, torrent_wire::Error> {
        let config = self.config_for_torrent(ca_cert_pem);
        ssl::build_client_config(&config)
    }

    /// Build a rustls `ServerConfig` for inbound TLS connections to an SSL torrent.
    pub fn server_config(
        &self,
        ca_cert_pem: &[u8],
    ) -> Result<Arc<rustls::ServerConfig>, torrent_wire::Error> {
        let config = self.config_for_torrent(ca_cert_pem);
        ssl::build_server_config(&config)
    }

    /// Our client certificate in PEM format.
    pub fn cert_pem(&self) -> &[u8] {
        &self.our_cert_pem
    }

    /// Our private key in PEM format.
    pub fn key_pem(&self) -> &[u8] {
        &self.our_key_pem
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssl_manager_generates_cert() {
        let settings = Settings::default();
        let mgr = SslManager::new(&settings).unwrap();

        // Cert should be valid PEM
        assert!(!mgr.cert_pem().is_empty());
        assert!(!mgr.key_pem().is_empty());
        assert!(mgr.cert_pem().starts_with(b"-----BEGIN CERTIFICATE-----"));
        assert!(
            mgr.key_pem().starts_with(b"-----BEGIN PRIVATE KEY-----")
                || mgr
                    .key_pem()
                    .starts_with(b"-----BEGIN RSA PRIVATE KEY-----")
                || mgr
                    .key_pem()
                    .starts_with(b"-----BEGIN EC PRIVATE KEY-----")
        );
    }

    #[test]
    fn ssl_manager_builds_config() {
        let settings = Settings::default();
        let mgr = SslManager::new(&settings).unwrap();

        // Use the manager's own cert as the CA cert (self-signed acts as its own CA)
        let ca_cert = mgr.cert_pem().to_vec();

        // Both client and server configs should build successfully
        let client_cfg = mgr.client_config(&ca_cert).unwrap();
        assert!(Arc::strong_count(&client_cfg) >= 1);

        let server_cfg = mgr.server_config(&ca_cert).unwrap();
        assert!(Arc::strong_count(&server_cfg) >= 1);

        // config_for_torrent should produce a valid SslConfig
        let ssl_config = mgr.config_for_torrent(&ca_cert);
        assert_eq!(ssl_config.ca_cert_pem, ca_cert);
        assert_eq!(ssl_config.our_cert_pem, mgr.cert_pem());
        assert_eq!(ssl_config.our_key_pem, mgr.key_pem());
    }

    #[test]
    fn ssl_manager_from_pem_round_trip() {
        // Generate a cert, then reconstruct via from_pem
        let settings = Settings::default();
        let original = SslManager::new(&settings).unwrap();

        let restored = SslManager::from_pem(
            original.cert_pem().to_vec(),
            original.key_pem().to_vec(),
        );

        assert_eq!(restored.cert_pem(), original.cert_pem());
        assert_eq!(restored.key_pem(), original.key_pem());

        // The restored manager should also build valid configs
        let ca_cert = restored.cert_pem().to_vec();
        restored.client_config(&ca_cert).unwrap();
        restored.server_config(&ca_cert).unwrap();
    }
}
