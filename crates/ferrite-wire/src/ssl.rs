//! TLS transport for SSL torrents.
//!
//! Wraps a TCP stream with rustls TLS, using the CA certificate from the
//! .torrent file as the trust anchor. SNI is set to the hex-encoded info hash.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::server::TlsStream as ServerTlsStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

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

/// A server cert verifier that validates the certificate chain against the
/// torrent's embedded CA but skips hostname (SNI) verification.
///
/// In SSL torrents, trust is established by the CA certificate embedded in the
/// .torrent file's info dict. The SNI is set to the hex-encoded info hash purely
/// for routing purposes, not for hostname validation. Peers use self-signed or
/// CA-issued certs without matching DNS names.
#[derive(Debug)]
struct SslTorrentServerVerifier {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
}

impl SslTorrentServerVerifier {
    fn new(root_store: Arc<rustls::RootCertStore>) -> Result<Self> {
        let inner = rustls::client::WebPkiServerVerifier::builder(root_store)
            .build()
            .map_err(|e| Error::Ssl(format!("server verifier error: {e}")))?;
        Ok(Self { inner })
    }
}

impl ServerCertVerifier for SslTorrentServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        // Validate the certificate chain against the CA, but use a dummy server
        // name to bypass hostname verification. We only care that the cert chains
        // to the torrent's embedded CA.
        //
        // We delegate to the inner WebPki verifier but catch the specific
        // "name mismatch" error and treat it as success, since SSL torrents do
        // not use hostname-based identity.
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            _server_name,
            _ocsp_response,
            now,
        ) {
            Ok(verified) => Ok(verified),
            Err(rustls::Error::InvalidCertificate(ref cert_err))
                if matches!(
                    cert_err,
                    rustls::CertificateError::NotValidForName
                        | rustls::CertificateError::NotValidForNameContext { .. }
                ) =>
            {
                // The cert chains to our CA but doesn't match the SNI name.
                // This is expected for SSL torrents — trust is based on the
                // embedded CA, not hostname matching.
                Ok(ServerCertVerified::assertion())
            }
            Err(e) => Err(e),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Build a rustls `ClientConfig` that trusts only the torrent's embedded CA.
///
/// Hostname (SNI) verification is intentionally skipped. In SSL torrents, the
/// trust model is based on the CA certificate from the .torrent info dict, not
/// DNS name matching. The SNI is set to the hex info hash for routing only.
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

    let verifier = SslTorrentServerVerifier::new(Arc::new(root_store))?;

    let client_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
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

    let key_pair =
        KeyPair::generate().map_err(|e| Error::Ssl(format!("key generation failed: {e}")))?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn generate_self_signed_cert_produces_valid_pem() {
        let (cert_pem, key_pem) = generate_self_signed_cert().unwrap();
        assert!(cert_pem.starts_with(b"-----BEGIN CERTIFICATE-----"));
        assert!(
            key_pem.starts_with(b"-----BEGIN PRIVATE KEY-----")
                || key_pem.starts_with(b"-----BEGIN RSA PRIVATE KEY-----")
                || key_pem.starts_with(b"-----BEGIN EC PRIVATE KEY-----")
        );

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
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

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
        let mut leaf_params =
            CertificateParams::new(vec!["ferrite-peer".to_string()]).unwrap();
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

        let info_hash =
            Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
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
        // Generate two separate CAs -- client trusts CA1 but server uses cert from CA2
        let (ca1_pem, leaf1_cert_pem, leaf1_key_pem) = generate_ca_and_leaf();
        let (ca2_pem, leaf2_cert_pem, leaf2_key_pem) = generate_ca_and_leaf();

        let client_config_data = SslConfig {
            ca_cert_pem: ca1_pem,  // client trusts CA1
            our_cert_pem: leaf1_cert_pem,
            our_key_pem: leaf1_key_pem,
        };
        let server_config_data = SslConfig {
            ca_cert_pem: ca2_pem,  // server has CA2 cert
            our_cert_pem: leaf2_cert_pem,
            our_key_pem: leaf2_key_pem,
        };

        let client_tls_config = build_client_config(&client_config_data).unwrap();
        let server_tls_config = build_server_config(&server_config_data).unwrap();

        let info_hash =
            Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
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
