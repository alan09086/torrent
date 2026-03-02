//! MSE/PE handshake: initiator and responder state machines.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use ferrite_core::Id20;

use crate::error::{Error, Result};
use super::cipher::Rc4;
use super::crypto;
use super::dh::{DhKeypair, DH_KEY_SIZE};
use super::stream::MseStream;

/// Result of a successful MSE/PE negotiation.
pub struct NegotiationResult<S> {
    /// The encrypted (or plaintext) stream wrapper.
    pub stream: MseStream<S>,
    /// Negotiated crypto method bitmask (`CRYPTO_PLAINTEXT` or `CRYPTO_RC4`).
    pub crypto_method: u32,
}

/// Run the MSE/PE handshake as the initiator (outbound connection).
///
/// `skey` is the info_hash (20 bytes).
/// `crypto_provide` is a bitmask of methods we support (CRYPTO_PLAINTEXT | CRYPTO_RC4).
pub async fn negotiate_outbound<S>(
    mut stream: S,
    skey: &Id20,
    crypto_provide: u32,
) -> Result<NegotiationResult<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let skey_bytes = skey.as_bytes();

    // Phase 1: DH key exchange
    let dh = DhKeypair::generate();

    // Send Ya + PadA (no padding for simplicity)
    stream.write_all(&dh.public).await?;
    stream.flush().await?;

    // Receive Yb
    let mut yb = [0u8; DH_KEY_SIZE];
    stream.read_exact(&mut yb).await?;

    // Compute shared secret
    let s = dh.shared_secret(&yb);

    // Phase 2: Send crypto negotiation
    // Initialize RC4 ciphers for encrypted portion
    let ka = crypto::key_a(&s, skey_bytes);
    let kb = crypto::key_b(&s, skey_bytes);
    let mut encrypt_cipher = Rc4::new(&ka);

    // Build packet 3:
    // HASH("req1" + S) [20] + (HASH("req2" + SKEY) XOR HASH("req3" + S)) [20]
    // + ENCRYPT_A(VC[8] + crypto_provide[4] + len(PadC)[2] + PadC[0] + len(IA)[2] + IA[0])
    let sync = crypto::sync_marker(&s);
    let proof = crypto::skey_proof(skey_bytes, &s);

    let mut encrypted_part = Vec::new();
    encrypted_part.extend_from_slice(&crypto::VC); // 8 bytes
    encrypted_part.extend_from_slice(&crypto_provide.to_be_bytes()); // 4 bytes
    encrypted_part.extend_from_slice(&0u16.to_be_bytes()); // len(PadC) = 0
    // No PadC
    encrypted_part.extend_from_slice(&0u16.to_be_bytes()); // len(IA) = 0
    // No IA (initial payload)
    encrypt_cipher.apply(&mut encrypted_part);

    stream.write_all(&sync).await?;
    stream.write_all(&proof).await?;
    stream.write_all(&encrypted_part).await?;
    stream.flush().await?;

    // Phase 3: Receive responder's encrypted reply
    // Read until we find VC (8 zero bytes) in the encrypted stream
    // The responder sends: ENCRYPT_B(VC + crypto_select + len(PadD) + PadD)
    // We need to scan for VC after decrypting

    // Read up to 512 + 8 + 4 + 2 + 512 bytes looking for decrypted VC
    let mut resp_buf = Vec::new();
    let max_scan = 512 + 8 + 4 + 2 + 512; // Yb padding + VC + select + len + PadD

    // We need to scan byte by byte through encrypted data to find VC
    // Create a temporary decrypt cipher to test
    let mut scan_cipher = Rc4::new(&kb);
    let mut found_vc = false;

    for _ in 0..max_scan {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        scan_cipher.apply(&mut byte);
        resp_buf.push(byte[0]);

        if resp_buf.len() >= 8 && resp_buf[resp_buf.len() - 8..] == crypto::VC {
            found_vc = true;
            break;
        }
    }

    if !found_vc {
        return Err(Error::EncryptionHandshakeFailed(
            "VC not found in responder reply".into(),
        ));
    }

    // Read crypto_select (4 bytes) + len(PadD) (2 bytes)
    let mut select_buf = [0u8; 6];
    stream.read_exact(&mut select_buf).await?;
    scan_cipher.apply(&mut select_buf);

    let crypto_select = u32::from_be_bytes([select_buf[0], select_buf[1], select_buf[2], select_buf[3]]);
    let pad_len = u16::from_be_bytes([select_buf[4], select_buf[5]]) as usize;

    // Validate crypto_select
    if crypto_select & crypto_provide == 0 {
        return Err(Error::UnsupportedCryptoMethod);
    }

    // Read and discard PadD
    if pad_len > 0 {
        let mut pad = vec![0u8; pad_len];
        stream.read_exact(&mut pad).await?;
        scan_cipher.apply(&mut pad);
    }

    // Build the final stream based on selected crypto method
    let result_stream = if crypto_select & crypto::CRYPTO_RC4 != 0 {
        // Continue using the scan_cipher as our decrypt cipher (it's in the right state)
        MseStream::encrypted(stream, scan_cipher, encrypt_cipher)
    } else {
        // Plaintext selected -- no further encryption
        MseStream::plaintext(stream)
    };

    Ok(NegotiationResult {
        stream: result_stream,
        crypto_method: crypto_select,
    })
}

/// Run the MSE/PE handshake as the responder (inbound connection).
///
/// `skey` is the info_hash (20 bytes).
/// `crypto_select_preference` decides which method to pick when multiple are offered.
/// Typically CRYPTO_RC4 if available, else CRYPTO_PLAINTEXT.
pub async fn negotiate_inbound<S>(
    mut stream: S,
    skey: &Id20,
    prefer_rc4: bool,
) -> Result<NegotiationResult<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let skey_bytes = skey.as_bytes();

    // Phase 1: Receive Ya, send Yb
    let mut ya = [0u8; DH_KEY_SIZE];
    stream.read_exact(&mut ya).await?;

    let dh = DhKeypair::generate();
    stream.write_all(&dh.public).await?;
    stream.flush().await?;

    // Compute shared secret
    let s = dh.shared_secret(&ya);

    // Phase 2: Find sync marker in incoming stream
    let expected_sync = crypto::sync_marker(&s);

    // Read bytes looking for the 20-byte sync marker
    // Max scan: 512 (PadA) + 20 (sync) bytes past the DH key
    let mut scan_buf = Vec::new();
    let max_scan = 512 + 20;
    let mut found_sync = false;

    for _ in 0..max_scan {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        scan_buf.push(byte[0]);

        if scan_buf.len() >= 20 && scan_buf[scan_buf.len() - 20..] == expected_sync {
            found_sync = true;
            break;
        }
    }

    if !found_sync {
        return Err(Error::EncryptionHandshakeFailed(
            "sync marker not found".into(),
        ));
    }

    // Read SKEY proof (20 bytes)
    let mut proof = [0u8; 20];
    stream.read_exact(&mut proof).await?;

    // Verify SKEY proof
    let expected_proof = crypto::skey_proof(skey_bytes, &s);
    if proof != expected_proof {
        return Err(Error::EncryptionHandshakeFailed(
            "SKEY proof mismatch".into(),
        ));
    }

    // Initialize decrypt cipher for reading initiator's encrypted data
    let ka = crypto::key_a(&s, skey_bytes);
    let kb = crypto::key_b(&s, skey_bytes);
    let mut decrypt_cipher = Rc4::new(&ka); // Initiator encrypts with keyA
    let mut encrypt_cipher = Rc4::new(&kb); // Responder encrypts with keyB

    // Read encrypted portion: VC[8] + crypto_provide[4] + len(PadC)[2]
    let mut enc_header = [0u8; 14];
    stream.read_exact(&mut enc_header).await?;
    decrypt_cipher.apply(&mut enc_header);

    // Verify VC
    if enc_header[..8] != crypto::VC {
        return Err(Error::EncryptionHandshakeFailed("VC mismatch".into()));
    }

    let crypto_provide = u32::from_be_bytes([enc_header[8], enc_header[9], enc_header[10], enc_header[11]]);
    let pad_c_len = u16::from_be_bytes([enc_header[12], enc_header[13]]) as usize;

    // Read PadC
    if pad_c_len > 0 {
        let mut pad = vec![0u8; pad_c_len];
        stream.read_exact(&mut pad).await?;
        decrypt_cipher.apply(&mut pad);
    }

    // Read len(IA)[2] + IA
    let mut ia_len_buf = [0u8; 2];
    stream.read_exact(&mut ia_len_buf).await?;
    decrypt_cipher.apply(&mut ia_len_buf);
    let ia_len = u16::from_be_bytes(ia_len_buf) as usize;

    if ia_len > 0 {
        let mut ia = vec![0u8; ia_len];
        stream.read_exact(&mut ia).await?;
        decrypt_cipher.apply(&mut ia);
        // Initial payload discarded for now (could be piggybacked BT handshake)
    }

    // Phase 3: Select crypto method and send response
    let crypto_select = if prefer_rc4 && (crypto_provide & crypto::CRYPTO_RC4 != 0) {
        crypto::CRYPTO_RC4
    } else if crypto_provide & crypto::CRYPTO_PLAINTEXT != 0 {
        crypto::CRYPTO_PLAINTEXT
    } else if crypto_provide & crypto::CRYPTO_RC4 != 0 {
        crypto::CRYPTO_RC4
    } else {
        return Err(Error::UnsupportedCryptoMethod);
    };

    // Send: ENCRYPT_B(VC + crypto_select + len(PadD) + PadD)
    let mut response = Vec::new();
    response.extend_from_slice(&crypto::VC); // 8 bytes
    response.extend_from_slice(&crypto_select.to_be_bytes()); // 4 bytes
    response.extend_from_slice(&0u16.to_be_bytes()); // len(PadD) = 0
    encrypt_cipher.apply(&mut response);

    stream.write_all(&response).await?;
    stream.flush().await?;

    // Build final stream
    let result_stream = if crypto_select & crypto::CRYPTO_RC4 != 0 {
        MseStream::encrypted(stream, decrypt_cipher, encrypt_cipher)
    } else {
        MseStream::plaintext(stream)
    };

    Ok(NegotiationResult {
        stream: result_stream,
        crypto_method: crypto_select,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::crypto;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn full_handshake_rc4() {
        let info_hash = Id20::from([0xAA; 20]);

        let (client_stream, server_stream) = tokio::io::duplex(4096);

        let client_handle = tokio::spawn(async move {
            negotiate_outbound(
                client_stream,
                &info_hash,
                crypto::CRYPTO_RC4,
            ).await
        });

        let server_handle = tokio::spawn(async move {
            negotiate_inbound(
                server_stream,
                &info_hash,
                true, // prefer RC4
            ).await
        });

        let client_result = client_handle.await.unwrap().unwrap();
        let server_result = server_handle.await.unwrap().unwrap();

        assert_eq!(client_result.crypto_method, crypto::CRYPTO_RC4);
        assert_eq!(server_result.crypto_method, crypto::CRYPTO_RC4);

        // Verify bidirectional communication works
        let mut client = client_result.stream;
        let mut server = server_result.stream;

        client.write_all(b"ping").await.unwrap();
        client.flush().await.unwrap();

        let mut buf = [0u8; 4];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        server.write_all(b"pong").await.unwrap();
        server.flush().await.unwrap();

        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn full_handshake_plaintext() {
        let info_hash = Id20::from([0xBB; 20]);

        let (client_stream, server_stream) = tokio::io::duplex(4096);

        let client_handle = tokio::spawn(async move {
            negotiate_outbound(
                client_stream,
                &info_hash,
                crypto::CRYPTO_PLAINTEXT,
            ).await
        });

        let server_handle = tokio::spawn(async move {
            negotiate_inbound(
                server_stream,
                &info_hash,
                false, // don't prefer RC4
            ).await
        });

        let client_result = client_handle.await.unwrap().unwrap();
        let server_result = server_handle.await.unwrap().unwrap();

        assert_eq!(client_result.crypto_method, crypto::CRYPTO_PLAINTEXT);
        assert_eq!(server_result.crypto_method, crypto::CRYPTO_PLAINTEXT);
    }

    #[tokio::test]
    async fn handshake_skey_mismatch_fails() {
        let client_hash = Id20::from([0xCC; 20]);
        let server_hash = Id20::from([0xDD; 20]); // Different!

        let (client_stream, server_stream) = tokio::io::duplex(4096);

        let client_handle = tokio::spawn(async move {
            negotiate_outbound(client_stream, &client_hash, crypto::CRYPTO_RC4).await
        });

        let server_handle = tokio::spawn(async move {
            negotiate_inbound(server_stream, &server_hash, true).await
        });

        // At least one side should fail
        let client_result = client_handle.await.unwrap();
        let server_result = server_handle.await.unwrap();

        assert!(client_result.is_err() || server_result.is_err());
    }
}
