//! SAM v3.1 protocol client.
//!
//! The SAM bridge provides a simple interface to the I2P router via TCP.
//! Protocol overview:
//!   1. Connect to SAM bridge (default 127.0.0.1:7656)
//!   2. HELLO VERSION MIN=3.1 MAX=3.1
//!   3. SESSION CREATE STYLE=STREAM ID=<id> DESTINATION=TRANSIENT [tunnel opts]
//!   4. STREAM CONNECT ID=<id> DESTINATION=<base64> [SILENT=false]
//!   5. STREAM ACCEPT ID=<id> [SILENT=false]
//!
//! Each command and reply is a single line terminated by `\n`.

use std::collections::HashMap;
use std::fmt;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, info};

use super::destination::I2pDestination;

/// SAM protocol errors.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SamError {
    #[error("SAM connection failed: {0}")]
    ConnectionFailed(String),

    #[error("SAM handshake failed: {0}")]
    HandshakeFailed(String),

    #[error("SAM session creation failed: {0}")]
    SessionCreateFailed(String),

    #[error("SAM stream connect failed: {0}")]
    StreamConnectFailed(String),

    #[error("SAM stream accept failed: {0}")]
    StreamAcceptFailed(String),

    #[error("SAM naming lookup failed: {0}")]
    NamingLookupFailed(String),

    #[error("SAM protocol error: {0}")]
    ProtocolError(String),

    #[error("SAM I/O error: {0}")]
    IoError(String),

    #[error("SAM invalid destination: {0}")]
    InvalidDestination(String),
}

impl From<std::io::Error> for SamError {
    fn from(e: std::io::Error) -> Self {
        SamError::IoError(e.to_string())
    }
}

/// Parsed SAM reply: the major and minor tokens, plus key=value pairs.
///
/// Example reply: `HELLO REPLY RESULT=OK VERSION=3.1`
/// -> major="HELLO", minor="REPLY", pairs={"RESULT": "OK", "VERSION": "3.1"}
#[derive(Debug, Clone)]
pub(crate) struct SamReply {
    pub major: String,
    pub minor: String,
    pub pairs: HashMap<String, String>,
}

impl SamReply {
    /// Parse a SAM reply line.
    ///
    /// Format: `MAJOR MINOR KEY=VALUE KEY=VALUE ...`
    /// Values may be quoted with double quotes for spaces.
    pub fn parse(line: &str) -> Result<Self, SamError> {
        let line = line.trim();
        let mut parts = line.splitn(3, ' ');

        let major = parts
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| SamError::ProtocolError("empty reply".into()))?
            .to_string();
        let minor = parts
            .next()
            .ok_or_else(|| SamError::ProtocolError(format!("missing minor token in: {line}")))?
            .to_string();

        let mut pairs = HashMap::new();
        if let Some(rest) = parts.next() {
            parse_key_value_pairs(rest, &mut pairs);
        }

        Ok(SamReply { major, minor, pairs })
    }

    /// Check if RESULT=OK.
    pub fn is_ok(&self) -> bool {
        self.pairs.get("RESULT").is_some_and(|v| v == "OK")
    }

    /// Get the RESULT value, defaulting to "UNKNOWN".
    pub fn result(&self) -> &str {
        self.pairs.get("RESULT").map(|s| s.as_str()).unwrap_or("UNKNOWN")
    }

    /// Get the MESSAGE value if present.
    pub fn message(&self) -> Option<&str> {
        self.pairs.get("MESSAGE").map(|s| s.as_str())
    }
}

/// Parse SAM key=value pairs from a string.
///
/// Handles quoted values (e.g., `MESSAGE="some error message"`).
fn parse_key_value_pairs(s: &str, pairs: &mut HashMap<String, String>) {
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Skip whitespace
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        // Find '='
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b' ' {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            // No '=', skip this token
            while i < bytes.len() && bytes[i] != b' ' {
                i += 1;
            }
            continue;
        }
        let key = String::from_utf8_lossy(&bytes[key_start..i]).to_string();
        i += 1; // skip '='

        // Parse value (possibly quoted)
        let value = if i < bytes.len() && bytes[i] == b'"' {
            i += 1; // skip opening quote
            let val_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let val = String::from_utf8_lossy(&bytes[val_start..i]).to_string();
            if i < bytes.len() {
                i += 1; // skip closing quote
            }
            val
        } else {
            let val_start = i;
            while i < bytes.len() && bytes[i] != b' ' {
                i += 1;
            }
            String::from_utf8_lossy(&bytes[val_start..i]).to_string()
        };

        pairs.insert(key, value);
    }
}

/// Configuration for the SAM session tunnel parameters.
#[derive(Debug, Clone)]
pub struct SamTunnelConfig {
    /// Number of inbound tunnels (1-16, default: 3).
    pub inbound_quantity: u8,
    /// Number of outbound tunnels (1-16, default: 3).
    pub outbound_quantity: u8,
    /// Number of inbound hops (0-7, default: 3).
    pub inbound_length: u8,
    /// Number of outbound hops (0-7, default: 3).
    pub outbound_length: u8,
}

impl Default for SamTunnelConfig {
    fn default() -> Self {
        Self {
            inbound_quantity: 3,
            outbound_quantity: 3,
            inbound_length: 3,
            outbound_length: 3,
        }
    }
}

impl SamTunnelConfig {
    /// Format as SAM tunnel option string.
    fn to_sam_options(&self) -> String {
        format!(
            "inbound.quantity={} outbound.quantity={} inbound.length={} outbound.length={}",
            self.inbound_quantity, self.outbound_quantity,
            self.inbound_length, self.outbound_length,
        )
    }
}

/// A SAM session connected to the I2P router.
///
/// Owns the control socket and the session's ephemeral destination.
/// Creating a session requires a HELLO handshake followed by SESSION CREATE.
///
/// The control socket **must** stay open for the session's lifetime. If it is
/// dropped, the I2P router destroys the session and all associated tunnels.
pub struct SamSession {
    /// SAM bridge host.
    sam_host: String,
    /// SAM bridge port.
    sam_port: u16,
    /// Our ephemeral I2P destination for this session.
    destination: I2pDestination,
    /// The session nickname (used for STREAM CONNECT/ACCEPT).
    session_id: String,
    /// Control socket — must stay open for session lifetime.
    ///
    /// The SAM specification requires the original control connection (used for
    /// HELLO and SESSION CREATE) to remain open. If this socket is dropped, the
    /// I2P router destroys the session immediately.
    _control_stream: TcpStream,
    /// Tunnel configuration (retained for potential session re-creation).
    #[allow(dead_code)]
    tunnel_config: SamTunnelConfig,
}

impl fmt::Debug for SamSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SamSession")
            .field("session_id", &self.session_id)
            .field("destination", &self.destination)
            .finish()
    }
}

/// A connected I2P stream (the TCP socket carries data after SAM handshake).
pub struct SamStream {
    /// The underlying TCP connection (SAM bridge proxies I2P traffic over this).
    stream: TcpStream,
    /// The remote I2P destination we connected to (or that connected to us).
    remote_destination: I2pDestination,
}

impl SamStream {
    /// Get the remote I2P destination.
    pub fn remote_destination(&self) -> &I2pDestination {
        &self.remote_destination
    }

    /// Consume the SamStream and return the underlying TcpStream.
    ///
    /// After the SAM handshake, the TCP stream carries raw BitTorrent protocol
    /// data, so it can be passed directly to `run_peer()`.
    pub fn into_inner(self) -> TcpStream {
        self.stream
    }

    /// Borrow the underlying TcpStream.
    pub fn inner(&self) -> &TcpStream {
        &self.stream
    }

    /// Mutably borrow the underlying TcpStream.
    pub fn inner_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }
}

impl SamSession {
    /// Create a new SAM session.
    ///
    /// 1. Connect to the SAM bridge at `host:port`
    /// 2. Perform HELLO VERSION handshake (v3.1)
    /// 3. Create a STREAM session with TRANSIENT destination
    ///
    /// The control socket is kept alive for the session's lifetime. Dropping
    /// the `SamSession` closes the control socket and destroys the session.
    /// Each STREAM CONNECT/ACCEPT opens a new TCP connection to the bridge.
    pub async fn create(
        host: &str,
        port: u16,
        session_id: &str,
        tunnel_config: SamTunnelConfig,
    ) -> Result<Self, SamError> {
        let addr = format!("{host}:{port}");
        let stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| SamError::ConnectionFailed(format!("{addr}: {e}")))?;

        let mut reader = BufReader::new(stream);

        // Step 1: HELLO VERSION
        let hello_cmd = "HELLO VERSION MIN=3.1 MAX=3.1\n";
        reader.get_mut().write_all(hello_cmd.as_bytes()).await?;

        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let reply = SamReply::parse(&line)?;

        if reply.major != "HELLO" || reply.minor != "REPLY" || !reply.is_ok() {
            return Err(SamError::HandshakeFailed(format!(
                "unexpected reply: {} ({})",
                reply.result(),
                reply.message().unwrap_or("no message"),
            )));
        }

        let version = reply.pairs.get("VERSION").cloned().unwrap_or_default();
        debug!("SAM handshake OK, version {version}");

        // Step 2: SESSION CREATE
        let tunnel_opts = tunnel_config.to_sam_options();
        let session_cmd = format!(
            "SESSION CREATE STYLE=STREAM ID={session_id} DESTINATION=TRANSIENT {tunnel_opts}\n"
        );
        reader.get_mut().write_all(session_cmd.as_bytes()).await?;

        line.clear();
        reader.read_line(&mut line).await?;
        let reply = SamReply::parse(&line)?;

        if reply.major != "SESSION" || reply.minor != "STATUS" || !reply.is_ok() {
            return Err(SamError::SessionCreateFailed(format!(
                "{} ({})",
                reply.result(),
                reply.message().unwrap_or("no message"),
            )));
        }

        // Extract our destination from the reply
        let dest_b64 = reply.pairs.get("DESTINATION").ok_or_else(|| {
            SamError::SessionCreateFailed("missing DESTINATION in reply".into())
        })?;

        let destination = I2pDestination::from_base64(dest_b64).map_err(|e| {
            SamError::InvalidDestination(format!("bad destination in SESSION STATUS: {e}"))
        })?;

        info!(
            session_id = session_id,
            dest_len = destination.len(),
            "SAM session created"
        );

        // Keep the control socket alive — the I2P router destroys the session
        // if this connection is closed.
        let control_stream = reader.into_inner();

        Ok(SamSession {
            destination,
            session_id: session_id.to_string(),
            sam_host: host.to_string(),
            sam_port: port,
            tunnel_config,
            _control_stream: control_stream,
        })
    }

    /// Our ephemeral I2P destination.
    pub fn destination(&self) -> &I2pDestination {
        &self.destination
    }

    /// The session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Connect to a remote I2P destination.
    ///
    /// Opens a new TCP connection to the SAM bridge and sends:
    /// `STREAM CONNECT ID=<session_id> DESTINATION=<dest> SILENT=false`
    ///
    /// On success, returns a `SamStream` whose underlying TCP socket carries
    /// raw data to/from the remote destination.
    pub async fn connect(&self, dest: &I2pDestination) -> Result<SamStream, SamError> {
        let addr = format!("{}:{}", self.sam_host, self.sam_port);
        let stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| SamError::ConnectionFailed(format!("{addr}: {e}")))?;

        let mut reader = BufReader::new(stream);

        // Must re-handshake on the new connection
        reader
            .get_mut()
            .write_all(b"HELLO VERSION MIN=3.1 MAX=3.1\n")
            .await?;
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let reply = SamReply::parse(&line)?;
        if !reply.is_ok() {
            return Err(SamError::HandshakeFailed(format!(
                "connect re-handshake: {}",
                reply.result()
            )));
        }

        // STREAM CONNECT
        let dest_b64 = dest.to_base64();
        let cmd = format!(
            "STREAM CONNECT ID={} DESTINATION={} SILENT=false\n",
            self.session_id, dest_b64,
        );
        reader.get_mut().write_all(cmd.as_bytes()).await?;

        line.clear();
        reader.read_line(&mut line).await?;
        let reply = SamReply::parse(&line)?;

        if reply.major != "STREAM" || reply.minor != "STATUS" || !reply.is_ok() {
            return Err(SamError::StreamConnectFailed(format!(
                "{} ({})",
                reply.result(),
                reply.message().unwrap_or("no message"),
            )));
        }

        debug!(dest = %dest, "SAM stream connected");

        // After successful STREAM CONNECT, the TCP socket is a raw data pipe
        let stream = reader.into_inner();
        Ok(SamStream {
            stream,
            remote_destination: dest.clone(),
        })
    }

    /// Accept an incoming I2P connection.
    ///
    /// Opens a new TCP connection to the SAM bridge and sends:
    /// `STREAM ACCEPT ID=<session_id> SILENT=false`
    ///
    /// Blocks until a remote peer connects. Returns a `SamStream` whose
    /// underlying TCP socket carries raw data from the connecting peer.
    pub async fn accept(&self) -> Result<SamStream, SamError> {
        let addr = format!("{}:{}", self.sam_host, self.sam_port);
        let stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| SamError::ConnectionFailed(format!("{addr}: {e}")))?;

        let mut reader = BufReader::new(stream);

        // Must re-handshake on the new connection
        reader
            .get_mut()
            .write_all(b"HELLO VERSION MIN=3.1 MAX=3.1\n")
            .await?;
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let reply = SamReply::parse(&line)?;
        if !reply.is_ok() {
            return Err(SamError::HandshakeFailed(format!(
                "accept re-handshake: {}",
                reply.result()
            )));
        }

        // STREAM ACCEPT
        let cmd = format!("STREAM ACCEPT ID={} SILENT=false\n", self.session_id);
        reader.get_mut().write_all(cmd.as_bytes()).await?;

        line.clear();
        reader.read_line(&mut line).await?;
        let reply = SamReply::parse(&line)?;

        if reply.major != "STREAM" || reply.minor != "STATUS" || !reply.is_ok() {
            return Err(SamError::StreamAcceptFailed(format!(
                "{} ({})",
                reply.result(),
                reply.message().unwrap_or("no message"),
            )));
        }

        // Read the incoming destination (sent as a line before data)
        line.clear();
        reader.read_line(&mut line).await?;
        let remote_dest_b64 = line.trim();

        let remote_destination =
            I2pDestination::from_base64(remote_dest_b64).map_err(|e| {
                SamError::InvalidDestination(format!("incoming destination: {e}"))
            })?;

        debug!(remote = %remote_destination, "SAM stream accepted");

        let stream = reader.into_inner();
        Ok(SamStream {
            stream,
            remote_destination,
        })
    }

    /// Look up a .b32.i2p or .i2p hostname and return the full destination.
    ///
    /// Sends: `NAMING LOOKUP NAME=<name>`
    /// Expects: `NAMING REPLY RESULT=OK NAME=<name> VALUE=<destination>`
    pub async fn naming_lookup(&self, name: &str) -> Result<I2pDestination, SamError> {
        let addr = format!("{}:{}", self.sam_host, self.sam_port);
        let stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| SamError::ConnectionFailed(format!("{addr}: {e}")))?;

        let mut reader = BufReader::new(stream);

        // Handshake
        reader
            .get_mut()
            .write_all(b"HELLO VERSION MIN=3.1 MAX=3.1\n")
            .await?;
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let reply = SamReply::parse(&line)?;
        if !reply.is_ok() {
            return Err(SamError::HandshakeFailed(format!(
                "naming re-handshake: {}",
                reply.result()
            )));
        }

        // NAMING LOOKUP
        let cmd = format!("NAMING LOOKUP NAME={name}\n");
        reader.get_mut().write_all(cmd.as_bytes()).await?;

        line.clear();
        reader.read_line(&mut line).await?;
        let reply = SamReply::parse(&line)?;

        if reply.major != "NAMING" || reply.minor != "REPLY" || !reply.is_ok() {
            return Err(SamError::NamingLookupFailed(format!(
                "{}: {} ({})",
                name,
                reply.result(),
                reply.message().unwrap_or("no message"),
            )));
        }

        let dest_b64 = reply.pairs.get("VALUE").ok_or_else(|| {
            SamError::NamingLookupFailed(format!("{name}: missing VALUE in reply"))
        })?;

        I2pDestination::from_base64(dest_b64)
            .map_err(|e| SamError::InvalidDestination(format!("{name}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sam_reply_parse_hello() {
        let reply = SamReply::parse("HELLO REPLY RESULT=OK VERSION=3.1").unwrap();
        assert_eq!(reply.major, "HELLO");
        assert_eq!(reply.minor, "REPLY");
        assert!(reply.is_ok());
        assert_eq!(reply.pairs.get("VERSION").unwrap(), "3.1");
    }

    #[test]
    fn sam_reply_parse_error() {
        let reply =
            SamReply::parse("SESSION STATUS RESULT=DUPLICATED_ID MESSAGE=\"session exists\"")
                .unwrap();
        assert_eq!(reply.major, "SESSION");
        assert_eq!(reply.minor, "STATUS");
        assert!(!reply.is_ok());
        assert_eq!(reply.result(), "DUPLICATED_ID");
        assert_eq!(reply.message(), Some("session exists"));
    }

    #[test]
    fn sam_reply_parse_session_create_ok() {
        let dest_b64 = super::super::destination::i2p_base64_encode(&[42u8; 516]);
        let line = format!("SESSION STATUS RESULT=OK DESTINATION={dest_b64}");
        let reply = SamReply::parse(&line).unwrap();
        assert!(reply.is_ok());
        assert_eq!(reply.pairs.get("DESTINATION").unwrap(), &dest_b64);
    }

    #[test]
    fn sam_reply_parse_stream_status() {
        let reply = SamReply::parse("STREAM STATUS RESULT=OK").unwrap();
        assert_eq!(reply.major, "STREAM");
        assert_eq!(reply.minor, "STATUS");
        assert!(reply.is_ok());
    }

    #[test]
    fn sam_reply_parse_naming_ok() {
        let dest_b64 = super::super::destination::i2p_base64_encode(&[7u8; 400]);
        let line = format!("NAMING REPLY RESULT=OK NAME=test.i2p VALUE={dest_b64}");
        let reply = SamReply::parse(&line).unwrap();
        assert!(reply.is_ok());
        assert_eq!(reply.pairs.get("NAME").unwrap(), "test.i2p");
        assert_eq!(reply.pairs.get("VALUE").unwrap(), &dest_b64);
    }

    #[test]
    fn sam_reply_parse_naming_error() {
        let reply =
            SamReply::parse("NAMING REPLY RESULT=KEY_NOT_FOUND NAME=unknown.i2p").unwrap();
        assert!(!reply.is_ok());
        assert_eq!(reply.result(), "KEY_NOT_FOUND");
    }

    #[test]
    fn sam_reply_parse_empty_line() {
        let err = SamReply::parse("").unwrap_err();
        assert!(matches!(err, SamError::ProtocolError(_)));
    }

    #[test]
    fn sam_reply_parse_single_token() {
        let err = SamReply::parse("HELLO").unwrap_err();
        assert!(matches!(err, SamError::ProtocolError(_)));
    }

    #[test]
    fn parse_key_value_quoted_message() {
        let mut pairs = HashMap::new();
        parse_key_value_pairs("RESULT=I2P_ERROR MESSAGE=\"tunnel build failed\"", &mut pairs);
        assert_eq!(pairs.get("RESULT").unwrap(), "I2P_ERROR");
        assert_eq!(pairs.get("MESSAGE").unwrap(), "tunnel build failed");
    }

    #[test]
    fn parse_key_value_multiple_unquoted() {
        let mut pairs = HashMap::new();
        parse_key_value_pairs("A=1 B=hello C=world", &mut pairs);
        assert_eq!(pairs.get("A").unwrap(), "1");
        assert_eq!(pairs.get("B").unwrap(), "hello");
        assert_eq!(pairs.get("C").unwrap(), "world");
    }

    #[test]
    fn tunnel_config_default() {
        let cfg = SamTunnelConfig::default();
        assert_eq!(cfg.inbound_quantity, 3);
        assert_eq!(cfg.outbound_quantity, 3);
        assert_eq!(cfg.inbound_length, 3);
        assert_eq!(cfg.outbound_length, 3);
    }

    #[test]
    fn tunnel_config_to_sam_options() {
        let cfg = SamTunnelConfig {
            inbound_quantity: 2,
            outbound_quantity: 4,
            inbound_length: 1,
            outbound_length: 2,
        };
        let opts = cfg.to_sam_options();
        assert!(opts.contains("inbound.quantity=2"));
        assert!(opts.contains("outbound.quantity=4"));
        assert!(opts.contains("inbound.length=1"));
        assert!(opts.contains("outbound.length=2"));
    }

    #[test]
    fn sam_error_display() {
        let err = SamError::HandshakeFailed("version mismatch".into());
        assert!(err.to_string().contains("handshake"));
        assert!(err.to_string().contains("version mismatch"));
    }

    #[test]
    fn sam_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let sam_err = SamError::from(io_err);
        assert!(matches!(sam_err, SamError::IoError(_)));
    }
}
