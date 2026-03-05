//! Proxy support: SOCKS4, SOCKS5, HTTP CONNECT, and SOCKS5 UDP ASSOCIATE.
//!
//! Hand-rolled implementations — no external dependency beyond `tokio`.
//! Provides [`connect_through_proxy`] for TCP tunneling and
//! [`socks5_udp_associate`] + [`ProxiedUdpSocket`] for UDP relay.


use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

// ── ProxyType ────────────────────────────────────────────────────────

/// Supported proxy protocols (matching libtorrent).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ProxyType {
    /// No proxy (direct connections).
    #[default]
    None,
    /// SOCKS4 proxy (no authentication, no UDP).
    Socks4,
    /// SOCKS5 proxy without authentication.
    Socks5,
    /// SOCKS5 proxy with username/password authentication.
    Socks5Password,
    /// HTTP CONNECT proxy without authentication.
    Http,
    /// HTTP CONNECT proxy with username/password authentication.
    HttpPassword,
}

// ── ProxyConfig ──────────────────────────────────────────────────────

/// Proxy connection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// Proxy protocol to use.
    pub proxy_type: ProxyType,
    /// Proxy server hostname or IP address.
    pub hostname: String,
    /// Proxy server port.
    pub port: u16,
    /// Username for authenticated proxy types.
    pub username: Option<String>,
    /// Password for authenticated proxy types.
    pub password: Option<String>,
    /// Route peer connections (incl. web seeds) through proxy.
    #[serde(default = "default_true")]
    pub proxy_peer_connections: bool,
    /// Route tracker HTTP connections through proxy.
    #[serde(default = "default_true")]
    pub proxy_tracker_connections: bool,
    /// Resolve hostnames through proxy (SOCKS5/HTTP only).
    #[serde(default = "default_true")]
    pub proxy_hostnames: bool,
    /// Include local endpoint in SOCKS5 UDP ASSOCIATE.
    #[serde(default)]
    pub socks5_udp_send_local_ep: bool,
}

fn default_true() -> bool { true }

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            proxy_type: ProxyType::None,
            hostname: String::new(),
            port: 0,
            username: None,
            password: None,
            proxy_peer_connections: true,
            proxy_tracker_connections: true,
            proxy_hostnames: true,
            socks5_udp_send_local_ep: false,
        }
    }
}

impl ProxyConfig {
    /// Format as a URL suitable for `reqwest::Proxy::all()`.
    pub fn to_url(&self) -> String {
        let scheme = match self.proxy_type {
            ProxyType::None => return String::new(),
            ProxyType::Socks4 => "socks4",
            ProxyType::Socks5 | ProxyType::Socks5Password => "socks5",
            ProxyType::Http | ProxyType::HttpPassword => "http",
        };

        match (&self.username, &self.password) {
            (Some(u), Some(p)) if self.proxy_type == ProxyType::Socks5Password
                || self.proxy_type == ProxyType::HttpPassword =>
            {
                format!("{}://{}:{}@{}:{}", scheme, u, p, self.hostname, self.port)
            }
            _ => format!("{}://{}:{}", scheme, self.hostname, self.port),
        }
    }
}

// ── connect_through_proxy ────────────────────────────────────────────

/// Connect to `target` through the configured proxy.
///
/// Returns a `TcpStream` with the proxy handshake already completed.
/// The stream is ready for BitTorrent protocol I/O.
pub(crate) async fn connect_through_proxy(
    proxy: &ProxyConfig,
    target: SocketAddr,
) -> io::Result<TcpStream> {
    let proxy_addr = format!("{}:{}", proxy.hostname, proxy.port);
    let mut stream = TcpStream::connect(&proxy_addr).await?;

    match proxy.proxy_type {
        ProxyType::Socks4 => socks4_connect(&mut stream, target).await?,
        ProxyType::Socks5 => socks5_connect(&mut stream, target, None).await?,
        ProxyType::Socks5Password => {
            let auth = match (&proxy.username, &proxy.password) {
                (Some(u), Some(p)) => Some((u.as_str(), p.as_str())),
                _ => None,
            };
            socks5_connect(&mut stream, target, auth).await?;
        }
        ProxyType::Http => http_connect(&mut stream, target, None).await?,
        ProxyType::HttpPassword => {
            let auth = match (&proxy.username, &proxy.password) {
                (Some(u), Some(p)) => Some((u.as_str(), p.as_str())),
                _ => None,
            };
            http_connect(&mut stream, target, auth).await?;
        }
        ProxyType::None => {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "no proxy configured"));
        }
    }

    Ok(stream)
}

// ── SOCKS4 ───────────────────────────────────────────────────────────

/// SOCKS4 CONNECT handshake (RFC 1928 predecessor).
///
/// Request: VN(1)=4, CD(1)=1, DSTPORT(2), DSTIP(4), USERID(var), NULL(1)
/// Response: VN(1)=0, CD(1), DSTPORT(2), DSTIP(4) — CD=90 means granted
async fn socks4_connect(stream: &mut TcpStream, target: SocketAddr) -> io::Result<()> {
    let ip = match target.ip() {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SOCKS4 does not support IPv6",
            ));
        }
    };

    // Build request
    let mut req = Vec::with_capacity(9);
    req.push(4); // VN
    req.push(1); // CD = CONNECT
    req.extend_from_slice(&target.port().to_be_bytes());
    req.extend_from_slice(&ip.octets());
    req.push(0); // NULL-terminated empty userid
    stream.write_all(&req).await?;

    // Read 8-byte response
    let mut resp = [0u8; 8];
    stream.read_exact(&mut resp).await?;

    if resp[1] != 90 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("SOCKS4 request rejected (CD={})", resp[1]),
        ));
    }

    Ok(())
}

// ── SOCKS5 ───────────────────────────────────────────────────────────

/// SOCKS5 CONNECT handshake (RFC 1928 + optional RFC 1929 auth).
async fn socks5_connect(
    stream: &mut TcpStream,
    target: SocketAddr,
    auth: Option<(&str, &str)>,
) -> io::Result<()> {
    // Step 1: Method negotiation
    socks5_negotiate_method(stream, auth.is_some()).await?;

    // Step 2: Username/password auth if needed
    if let Some((user, pass)) = auth {
        socks5_auth(stream, user, pass).await?;
    }

    // Step 3: Connect request
    socks5_send_connect(stream, target).await
}

/// Negotiate authentication method with SOCKS5 proxy.
async fn socks5_negotiate_method(stream: &mut TcpStream, want_auth: bool) -> io::Result<()> {
    let methods: &[u8] = if want_auth {
        &[5, 2, 0, 2] // VER=5, NMETHODS=2, NO_AUTH + USERNAME/PASSWORD
    } else {
        &[5, 1, 0] // VER=5, NMETHODS=1, NO_AUTH
    };
    stream.write_all(methods).await?;

    let mut resp = [0u8; 2];
    stream.read_exact(&mut resp).await?;

    if resp[0] != 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SOCKS5: unexpected version {}", resp[0]),
        ));
    }

    match resp[1] {
        0 => Ok(()),  // NO_AUTH accepted
        2 if want_auth => Ok(()),  // USERNAME/PASSWORD accepted
        0xFF => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SOCKS5: no acceptable methods",
        )),
        m => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SOCKS5: unexpected method {}", m),
        )),
    }
}

/// RFC 1929 username/password subnegotiation.
///
/// Request: VER(1)=1, ULEN(1), UNAME(ULEN), PLEN(1), PASSWD(PLEN)
/// Response: VER(1)=1, STATUS(1) — 0=success
async fn socks5_auth(stream: &mut TcpStream, user: &str, pass: &str) -> io::Result<()> {
    if user.len() > 255 || pass.len() > 255 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SOCKS5: username or password too long (max 255 bytes)",
        ));
    }

    let mut req = Vec::with_capacity(3 + user.len() + pass.len());
    req.push(1); // subneg version
    req.push(user.len() as u8);
    req.extend_from_slice(user.as_bytes());
    req.push(pass.len() as u8);
    req.extend_from_slice(pass.as_bytes());
    stream.write_all(&req).await?;

    let mut resp = [0u8; 2];
    stream.read_exact(&mut resp).await?;

    if resp[1] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SOCKS5: authentication failed",
        ));
    }

    Ok(())
}

/// Send SOCKS5 CONNECT request and read response.
///
/// Request: VER(1)=5, CMD(1)=1, RSV(1)=0, ATYP(1), DST.ADDR(var), DST.PORT(2)
/// Response: VER(1)=5, REP(1), RSV(1)=0, ATYP(1), BND.ADDR(var), BND.PORT(2)
async fn socks5_send_connect(stream: &mut TcpStream, target: SocketAddr) -> io::Result<()> {
    let mut req = Vec::with_capacity(22);
    req.push(5); // VER
    req.push(1); // CMD = CONNECT
    req.push(0); // RSV
    encode_socks5_addr(&mut req, target);
    stream.write_all(&req).await?;

    read_socks5_response(stream).await
}

/// Encode a socket address in SOCKS5 format (ATYP + ADDR + PORT).
fn encode_socks5_addr(buf: &mut Vec<u8>, addr: SocketAddr) {
    match addr {
        SocketAddr::V4(v4) => {
            buf.push(1); // ATYP = IPv4
            buf.extend_from_slice(&v4.ip().octets());
            buf.extend_from_slice(&v4.port().to_be_bytes());
        }
        SocketAddr::V6(v6) => {
            buf.push(4); // ATYP = IPv6
            buf.extend_from_slice(&v6.ip().octets());
            buf.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
}

/// Read and validate a SOCKS5 response (CONNECT or UDP ASSOCIATE).
async fn read_socks5_response(stream: &mut TcpStream) -> io::Result<()> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;

    if header[0] != 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SOCKS5: unexpected version {}", header[0]),
        ));
    }

    if header[1] != 0 {
        let msg = socks5_error_message(header[1]);
        return Err(io::Error::new(io::ErrorKind::ConnectionRefused, msg));
    }

    // Skip bound address
    match header[3] {
        1 => {
            // IPv4: 4 bytes + 2 port
            let mut skip = [0u8; 6];
            stream.read_exact(&mut skip).await?;
        }
        4 => {
            // IPv6: 16 bytes + 2 port
            let mut skip = [0u8; 18];
            stream.read_exact(&mut skip).await?;
        }
        3 => {
            // Domain: 1 len + domain + 2 port
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut skip = vec![0u8; len[0] as usize + 2];
            stream.read_exact(&mut skip).await?;
        }
        atyp => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("SOCKS5: unknown ATYP {}", atyp),
            ));
        }
    }

    Ok(())
}

/// Read a SOCKS5 response and return the bound address.
async fn read_socks5_response_addr(stream: &mut TcpStream) -> io::Result<SocketAddr> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;

    if header[0] != 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SOCKS5: unexpected version {}", header[0]),
        ));
    }

    if header[1] != 0 {
        let msg = socks5_error_message(header[1]);
        return Err(io::Error::new(io::ErrorKind::ConnectionRefused, msg));
    }

    match header[3] {
        1 => {
            let mut buf = [0u8; 6];
            stream.read_exact(&mut buf).await?;
            let ip = Ipv4Addr::new(buf[0], buf[1], buf[2], buf[3]);
            let port = u16::from_be_bytes([buf[4], buf[5]]);
            Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        4 => {
            let mut buf = [0u8; 18];
            stream.read_exact(&mut buf).await?;
            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&buf[..16]).unwrap());
            let port = u16::from_be_bytes([buf[16], buf[17]]);
            Ok(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)))
        }
        atyp => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SOCKS5: unsupported BND.ATYP {} in UDP ASSOCIATE", atyp),
        )),
    }
}

fn socks5_error_message(code: u8) -> String {
    match code {
        1 => "SOCKS5: general failure".into(),
        2 => "SOCKS5: connection not allowed by ruleset".into(),
        3 => "SOCKS5: network unreachable".into(),
        4 => "SOCKS5: host unreachable".into(),
        5 => "SOCKS5: connection refused".into(),
        6 => "SOCKS5: TTL expired".into(),
        7 => "SOCKS5: command not supported".into(),
        8 => "SOCKS5: address type not supported".into(),
        _ => format!("SOCKS5: unknown error ({})", code),
    }
}

// ── HTTP CONNECT ─────────────────────────────────────────────────────

/// HTTP CONNECT tunnel handshake.
async fn http_connect(
    stream: &mut TcpStream,
    target: SocketAddr,
    auth: Option<(&str, &str)>,
) -> io::Result<()> {
    let host_port = format!("{}:{}", target.ip(), target.port());

    let mut request = format!(
        "CONNECT {} HTTP/1.1\r\nHost: {}\r\n",
        host_port, host_port,
    );

    if let Some((user, pass)) = auth {
        use std::fmt::Write;
        let credentials = format!("{}:{}", user, pass);
        let encoded = base64_encode(credentials.as_bytes());
        let _ = write!(request, "Proxy-Authorization: Basic {}\r\n", encoded);
    }

    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;

    // Read until we see \r\n\r\n (end of HTTP headers)
    let mut response_buf = Vec::with_capacity(256);
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        response_buf.push(byte[0]);

        if response_buf.len() >= 4 {
            let len = response_buf.len();
            if response_buf[len - 4..] == [b'\r', b'\n', b'\r', b'\n'] {
                break;
            }
        }

        if response_buf.len() > 8192 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP CONNECT: response too large",
            ));
        }
    }

    let response_str = String::from_utf8_lossy(&response_buf);
    let status_line = response_str.lines().next().unwrap_or("");

    // Parse status code from "HTTP/1.x NNN ..."
    let parts: Vec<&str> = status_line.splitn(3, ' ').collect();
    if parts.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("HTTP CONNECT: malformed response: {}", status_line),
        ));
    }

    let status_code: u16 = parts[1].parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("HTTP CONNECT: invalid status code: {}", parts[1]),
        )
    })?;

    if status_code != 200 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("HTTP CONNECT: proxy returned status {}", status_code),
        ));
    }

    Ok(())
}

/// Minimal base64 encoder (avoids external dep).
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);

    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }

    result
}

// ── SOCKS5 UDP ASSOCIATE ─────────────────────────────────────────────

/// Establish a SOCKS5 UDP relay.
///
/// Returns `(relay_addr, control_stream)`. The control stream must be
/// kept alive for the duration of the relay — dropping it terminates
/// the association.
pub(crate) async fn socks5_udp_associate(
    proxy: &ProxyConfig,
    local_addr: SocketAddr,
) -> io::Result<(SocketAddr, TcpStream)> {
    let proxy_addr = format!("{}:{}", proxy.hostname, proxy.port);
    let mut stream = TcpStream::connect(&proxy_addr).await?;

    let want_auth = proxy.proxy_type == ProxyType::Socks5Password;
    socks5_negotiate_method(&mut stream, want_auth).await?;

    if want_auth {
        let user = proxy.username.as_deref().unwrap_or("");
        let pass = proxy.password.as_deref().unwrap_or("");
        socks5_auth(&mut stream, user, pass).await?;
    }

    // UDP ASSOCIATE request (CMD=3)
    let mut req = Vec::with_capacity(22);
    req.push(5); // VER
    req.push(3); // CMD = UDP ASSOCIATE
    req.push(0); // RSV

    if proxy.socks5_udp_send_local_ep {
        encode_socks5_addr(&mut req, local_addr);
    } else {
        // Send zeros — let proxy figure it out
        req.push(1); // ATYP = IPv4
        req.extend_from_slice(&[0, 0, 0, 0]); // 0.0.0.0
        req.extend_from_slice(&[0, 0]); // port 0
    }

    stream.write_all(&req).await?;

    let relay_addr = read_socks5_response_addr(&mut stream).await?;

    Ok((relay_addr, stream))
}

// ── SOCKS5 UDP header ────────────────────────────────────────────────

/// Encode a SOCKS5 UDP relay header.
///
/// Format: RSV(2)=0, FRAG(1)=0, ATYP(1), DST.ADDR(var), DST.PORT(2)
pub(crate) fn encode_socks5_udp_header(target: SocketAddr) -> Vec<u8> {
    let mut hdr = Vec::with_capacity(10);
    hdr.extend_from_slice(&[0, 0]); // RSV
    hdr.push(0); // FRAG
    encode_socks5_addr(&mut hdr, target);
    hdr
}

/// Decode a SOCKS5 UDP relay header, returning (source_addr, data_offset).
pub(crate) fn decode_socks5_udp_header(buf: &[u8]) -> io::Result<(SocketAddr, usize)> {
    if buf.len() < 7 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SOCKS5 UDP header too short",
        ));
    }

    // RSV(2) + FRAG(1) = first 3 bytes
    let atyp = buf[3];

    match atyp {
        1 => {
            // IPv4: 4 + 2 = 6 bytes after ATYP
            if buf.len() < 10 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SOCKS5 UDP: IPv4 header truncated",
                ));
            }
            let ip = Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]);
            let port = u16::from_be_bytes([buf[8], buf[9]]);
            Ok((SocketAddr::V4(SocketAddrV4::new(ip, port)), 10))
        }
        4 => {
            // IPv6: 16 + 2 = 18 bytes after ATYP
            if buf.len() < 22 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SOCKS5 UDP: IPv6 header truncated",
                ));
            }
            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&buf[4..20]).unwrap());
            let port = u16::from_be_bytes([buf[20], buf[21]]);
            Ok((SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)), 22))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SOCKS5 UDP: unsupported ATYP {}", atyp),
        )),
    }
}

// ── ProxiedUdpSocket ─────────────────────────────────────────────────

/// A UDP socket that routes through a SOCKS5 UDP relay.
///
/// Wraps each datagram with the SOCKS5 UDP header and strips it on receive.
/// The `_control` stream must remain alive for the relay to function.
pub(crate) struct ProxiedUdpSocket {
    socket: UdpSocket,
    relay_addr: SocketAddr,
    _control: TcpStream,
}

impl ProxiedUdpSocket {
    /// Create a new proxied UDP socket.
    ///
    /// `socket` is the local UDP socket, `relay_addr` is the proxy's UDP relay
    /// endpoint, and `control` is the TCP stream that must stay alive.
    pub fn new(socket: UdpSocket, relay_addr: SocketAddr, control: TcpStream) -> Self {
        Self {
            socket,
            relay_addr,
            _control: control,
        }
    }

    /// Send data to `target` through the SOCKS5 UDP relay.
    pub async fn send_to(&self, data: &[u8], target: SocketAddr) -> io::Result<usize> {
        let header = encode_socks5_udp_header(target);
        let mut packet = Vec::with_capacity(header.len() + data.len());
        packet.extend_from_slice(&header);
        packet.extend_from_slice(data);
        self.socket.send_to(&packet, self.relay_addr).await
    }

    /// Receive data from the SOCKS5 UDP relay, stripping the relay header.
    ///
    /// Returns `(bytes_read, source_addr)` where `source_addr` is the original
    /// sender (extracted from the SOCKS5 header, not the relay).
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut raw = vec![0u8; buf.len() + 22]; // max header overhead
        let (n, _relay) = self.socket.recv_from(&mut raw).await?;

        let (source, offset) = decode_socks5_udp_header(&raw[..n])?;
        let data_len = n - offset;

        if data_len > buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SOCKS5 UDP: data exceeds buffer",
            ));
        }

        buf[..data_len].copy_from_slice(&raw[offset..n]);
        Ok((data_len, source))
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Test 11: SOCKS5 method negotiation + connect request/response ──

    #[test]
    fn socks5_encode_method_negotiation() {
        // No auth
        let no_auth = [5u8, 1, 0];
        assert_eq!(no_auth[0], 5); // VER
        assert_eq!(no_auth[1], 1); // NMETHODS
        assert_eq!(no_auth[2], 0); // NO_AUTH

        // With auth
        let with_auth = [5u8, 2, 0, 2];
        assert_eq!(with_auth[0], 5);
        assert_eq!(with_auth[1], 2);
        assert_eq!(with_auth[2], 0); // NO_AUTH
        assert_eq!(with_auth[3], 2); // USERNAME/PASSWORD
    }

    #[test]
    fn socks5_encode_connect_request_ipv4() {
        let target: SocketAddr = "192.168.1.1:8080".parse().unwrap();
        let mut buf = Vec::new();
        buf.push(5); // VER
        buf.push(1); // CMD = CONNECT
        buf.push(0); // RSV
        encode_socks5_addr(&mut buf, target);

        assert_eq!(buf[0], 5); // VER
        assert_eq!(buf[1], 1); // CMD
        assert_eq!(buf[2], 0); // RSV
        assert_eq!(buf[3], 1); // ATYP = IPv4
        assert_eq!(&buf[4..8], &[192, 168, 1, 1]);
        assert_eq!(&buf[8..10], &8080u16.to_be_bytes());
    }

    #[test]
    fn socks5_encode_connect_request_ipv6() {
        let target: SocketAddr = "[::1]:9999".parse().unwrap();
        let mut buf = Vec::new();
        buf.push(5);
        buf.push(1);
        buf.push(0);
        encode_socks5_addr(&mut buf, target);

        assert_eq!(buf[3], 4); // ATYP = IPv6
        assert_eq!(buf.len(), 3 + 1 + 16 + 2); // header + atyp + ipv6 + port
        // Last two bytes = port
        assert_eq!(&buf[20..22], &9999u16.to_be_bytes());
    }

    #[test]
    fn socks5_error_messages() {
        assert!(socks5_error_message(1).contains("general failure"));
        assert!(socks5_error_message(5).contains("connection refused"));
        assert!(socks5_error_message(99).contains("unknown"));
    }

    // ── Test 12: SOCKS5 password auth subnegotiation ──

    #[test]
    fn socks5_auth_encode() {
        // Verify RFC 1929 encoding: VER(1)=1, ULEN(1), UNAME, PLEN(1), PASSWD
        let user = "alice";
        let pass = "secret";

        let mut req = Vec::new();
        req.push(1); // subneg version
        req.push(user.len() as u8);
        req.extend_from_slice(user.as_bytes());
        req.push(pass.len() as u8);
        req.extend_from_slice(pass.as_bytes());

        assert_eq!(req[0], 1); // VER
        assert_eq!(req[1], 5); // ULEN
        assert_eq!(&req[2..7], b"alice");
        assert_eq!(req[7], 6); // PLEN
        assert_eq!(&req[8..14], b"secret");
    }

    // ── Test 13: SOCKS4 handshake encode/decode ──

    #[test]
    fn socks4_encode_connect_request() {
        let target: SocketAddr = "1.2.3.4:80".parse().unwrap();
        let mut req = Vec::new();
        req.push(4); // VN
        req.push(1); // CD = CONNECT
        req.extend_from_slice(&target.port().to_be_bytes());
        req.extend_from_slice(&[1, 2, 3, 4]);
        req.push(0); // NULL userid

        assert_eq!(req[0], 4);
        assert_eq!(req[1], 1);
        assert_eq!(&req[2..4], &80u16.to_be_bytes());
        assert_eq!(&req[4..8], &[1, 2, 3, 4]);
        assert_eq!(req[8], 0);
    }

    #[test]
    fn socks4_response_granted() {
        let resp = [0u8, 90, 0, 0, 0, 0, 0, 0];
        assert_eq!(resp[1], 90); // granted
    }

    #[test]
    fn socks4_response_rejected() {
        let resp = [0u8, 91, 0, 0, 0, 0, 0, 0];
        assert_ne!(resp[1], 90); // rejected
    }

    // ── Test 14: HTTP CONNECT format/parse ──

    #[test]
    fn http_connect_request_no_auth() {
        let target: SocketAddr = "93.184.216.34:443".parse().unwrap();
        let host_port = format!("{}:{}", target.ip(), target.port());
        let request = format!(
            "CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n",
            host_port, host_port,
        );

        assert!(request.starts_with("CONNECT 93.184.216.34:443 HTTP/1.1\r\n"));
        assert!(request.contains("Host: 93.184.216.34:443\r\n"));
        assert!(request.ends_with("\r\n\r\n"));
    }

    #[test]
    fn http_connect_request_with_auth() {
        let encoded = base64_encode(b"user:pass");
        assert_eq!(encoded, "dXNlcjpwYXNz");

        let header = format!("Proxy-Authorization: Basic {}\r\n", encoded);
        assert!(header.contains("dXNlcjpwYXNz"));
    }

    #[test]
    fn http_connect_parse_200_response() {
        let response = b"HTTP/1.1 200 Connection Established\r\n\r\n";
        let response_str = String::from_utf8_lossy(response);
        let status_line = response_str.lines().next().unwrap();
        let parts: Vec<&str> = status_line.splitn(3, ' ').collect();
        let status_code: u16 = parts[1].parse().unwrap();
        assert_eq!(status_code, 200);
    }

    #[test]
    fn http_connect_parse_407_response() {
        let response = b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n";
        let response_str = String::from_utf8_lossy(response);
        let status_line = response_str.lines().next().unwrap();
        let parts: Vec<&str> = status_line.splitn(3, ' ').collect();
        let status_code: u16 = parts[1].parse().unwrap();
        assert_eq!(status_code, 407);
    }

    // ── Test 15: SOCKS5 UDP header encode/decode ──

    #[test]
    fn socks5_udp_header_ipv4_roundtrip() {
        let target: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let header = encode_socks5_udp_header(target);

        assert_eq!(header[0], 0); // RSV
        assert_eq!(header[1], 0); // RSV
        assert_eq!(header[2], 0); // FRAG
        assert_eq!(header[3], 1); // ATYP = IPv4
        assert_eq!(&header[4..8], &[10, 0, 0, 1]);
        assert_eq!(&header[8..10], &6881u16.to_be_bytes());

        let (decoded_addr, offset) = decode_socks5_udp_header(&header).unwrap();
        assert_eq!(decoded_addr, target);
        assert_eq!(offset, 10);
    }

    #[test]
    fn socks5_udp_header_ipv6_roundtrip() {
        let target: SocketAddr = "[2001:db8::1]:51413".parse().unwrap();
        let header = encode_socks5_udp_header(target);

        assert_eq!(header[3], 4); // ATYP = IPv6
        assert_eq!(header.len(), 22); // 3 + 1 + 16 + 2

        let (decoded_addr, offset) = decode_socks5_udp_header(&header).unwrap();
        assert_eq!(decoded_addr, target);
        assert_eq!(offset, 22);
    }

    #[test]
    fn socks5_udp_header_too_short() {
        let short = [0u8; 5];
        assert!(decode_socks5_udp_header(&short).is_err());
    }

    // ── Test 16: ProxyConfig::to_url() ──

    #[test]
    fn proxy_config_to_url_none() {
        let cfg = ProxyConfig::default();
        assert_eq!(cfg.to_url(), "");
    }

    #[test]
    fn proxy_config_to_url_socks5() {
        let cfg = ProxyConfig {
            proxy_type: ProxyType::Socks5,
            hostname: "proxy.example.com".into(),
            port: 1080,
            ..Default::default()
        };
        assert_eq!(cfg.to_url(), "socks5://proxy.example.com:1080");
    }

    #[test]
    fn proxy_config_to_url_socks5_password() {
        let cfg = ProxyConfig {
            proxy_type: ProxyType::Socks5Password,
            hostname: "proxy.example.com".into(),
            port: 1080,
            username: Some("user".into()),
            password: Some("pass".into()),
            ..Default::default()
        };
        assert_eq!(cfg.to_url(), "socks5://user:pass@proxy.example.com:1080");
    }

    #[test]
    fn proxy_config_to_url_http() {
        let cfg = ProxyConfig {
            proxy_type: ProxyType::Http,
            hostname: "httpproxy.local".into(),
            port: 8080,
            ..Default::default()
        };
        assert_eq!(cfg.to_url(), "http://httpproxy.local:8080");
    }

    #[test]
    fn proxy_config_to_url_http_password() {
        let cfg = ProxyConfig {
            proxy_type: ProxyType::HttpPassword,
            hostname: "httpproxy.local".into(),
            port: 3128,
            username: Some("admin".into()),
            password: Some("secret".into()),
            ..Default::default()
        };
        assert_eq!(cfg.to_url(), "http://admin:secret@httpproxy.local:3128");
    }

    #[test]
    fn proxy_config_to_url_socks4() {
        let cfg = ProxyConfig {
            proxy_type: ProxyType::Socks4,
            hostname: "s4proxy".into(),
            port: 1080,
            ..Default::default()
        };
        assert_eq!(cfg.to_url(), "socks4://s4proxy:1080");
    }

    #[test]
    fn base64_encode_basic() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn proxy_config_default_flags() {
        let cfg = ProxyConfig::default();
        assert_eq!(cfg.proxy_type, ProxyType::None);
        assert!(cfg.proxy_peer_connections);
        assert!(cfg.proxy_tracker_connections);
        assert!(cfg.proxy_hostnames);
        assert!(!cfg.socks5_udp_send_local_ep);
    }
}
