use std::net::SocketAddr;
use std::os::fd::{AsFd, AsRawFd};
use std::time::Duration;

use tokio::net::UdpSocket;

use ferrite_core::Id20;

use crate::compact::{parse_compact_peers, parse_compact_peers6};
use crate::error::{Error, Result};

/// Apply DSCP/ToS marking to a UDP socket. No-op if dscp == 0.
fn apply_dscp_udp(socket: &UdpSocket, dscp: u8, is_ipv6: bool) {
    if dscp == 0 {
        return;
    }
    let tos = (dscp as u32) << 2;
    let fd = socket.as_fd().as_raw_fd();
    let result = unsafe {
        if is_ipv6 {
            libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_TCLASS,
                &(tos as libc::c_int) as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        } else {
            libc::setsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_TOS,
                &(tos as libc::c_int) as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    };
    if result != 0 {
        tracing::debug!(
            dscp,
            "failed to set DSCP on UDP tracker socket: {}",
            std::io::Error::last_os_error()
        );
    }
}
use crate::{AnnounceRequest, AnnounceResponse, ScrapeInfo};

/// Magic connection ID for UDP tracker connect (BEP 15).
const CONNECT_MAGIC: u64 = 0x0417_2710_1980;
const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_SCRAPE: u32 = 2;

/// Default timeout for UDP tracker requests.
const UDP_TIMEOUT: Duration = Duration::from_secs(15);

/// UDP tracker client (BEP 15).
pub struct UdpTracker {
    timeout: Duration,
    dscp: u8,
}

/// UDP announce response.
#[derive(Debug, Clone)]
pub struct UdpAnnounceResponse {
    /// Common announce response data (interval, peers, etc.).
    pub response: AnnounceResponse,
    /// Transaction ID echoed from the request.
    pub transaction_id: u32,
}

/// UDP scrape response.
#[derive(Debug, Clone)]
pub struct UdpScrapeResponse {
    /// Per-torrent scrape statistics (same order as requested hashes).
    pub results: Vec<ScrapeInfo>,
    /// Transaction ID echoed from the request.
    pub transaction_id: u32,
}

impl UdpTracker {
    /// Creates a new UDP tracker client with default settings.
    pub fn new() -> Self {
        UdpTracker {
            timeout: UDP_TIMEOUT,
            dscp: 0,
        }
    }

    /// Sets the timeout duration for UDP tracker requests.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the DSCP/ToS value for outbound UDP packets.
    pub fn with_dscp(mut self, dscp: u8) -> Self {
        self.dscp = dscp;
        self
    }

    /// Build a UDP connect request packet (BEP 15).
    pub fn build_connect_request(transaction_id: u32) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..8].copy_from_slice(&CONNECT_MAGIC.to_be_bytes());
        buf[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
        buf[12..16].copy_from_slice(&transaction_id.to_be_bytes());
        buf
    }

    /// Parse a UDP connect response, returning the connection_id.
    pub fn parse_connect_response(
        data: &[u8],
        expected_transaction_id: u32,
    ) -> Result<u64> {
        if data.len() < 16 {
            return Err(Error::UdpProtocol(format!(
                "connect response too short: {} bytes",
                data.len()
            )));
        }

        let action = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        if action != ACTION_CONNECT {
            return Err(Error::UdpProtocol(format!(
                "expected action 0 (connect), got {action}"
            )));
        }

        let transaction_id = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if transaction_id != expected_transaction_id {
            return Err(Error::UdpProtocol(format!(
                "transaction ID mismatch: expected {expected_transaction_id}, got {transaction_id}"
            )));
        }

        let connection_id = u64::from_be_bytes([
            data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
        ]);

        Ok(connection_id)
    }

    /// Build a UDP announce request packet (BEP 15).
    pub fn build_announce_request(
        connection_id: u64,
        transaction_id: u32,
        req: &AnnounceRequest,
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(98);
        buf.extend_from_slice(&connection_id.to_be_bytes());
        buf.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
        buf.extend_from_slice(&transaction_id.to_be_bytes());
        buf.extend_from_slice(req.info_hash.as_bytes());
        buf.extend_from_slice(req.peer_id.as_bytes());
        buf.extend_from_slice(&req.downloaded.to_be_bytes());
        buf.extend_from_slice(&req.left.to_be_bytes());
        buf.extend_from_slice(&req.uploaded.to_be_bytes());
        buf.extend_from_slice(&(req.event as u32).to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // IP address (0 = default)
        buf.extend_from_slice(&0u32.to_be_bytes()); // key (random)
        buf.extend_from_slice(&req.num_want.unwrap_or(-1i32).to_be_bytes());
        buf.extend_from_slice(&req.port.to_be_bytes());
        buf
    }

    /// Parse a UDP announce response.
    pub fn parse_announce_response(
        data: &[u8],
        expected_transaction_id: u32,
    ) -> Result<UdpAnnounceResponse> {
        if data.len() < 20 {
            return Err(Error::UdpProtocol(format!(
                "announce response too short: {} bytes",
                data.len()
            )));
        }

        let action = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        if action != ACTION_ANNOUNCE {
            // Check for error action (3)
            if action == 3 && data.len() > 8 {
                let msg = String::from_utf8_lossy(&data[8..]);
                return Err(Error::TrackerError(msg.into_owned()));
            }
            return Err(Error::UdpProtocol(format!(
                "expected action 1 (announce), got {action}"
            )));
        }

        let transaction_id = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if transaction_id != expected_transaction_id {
            return Err(Error::UdpProtocol(format!(
                "transaction ID mismatch: expected {expected_transaction_id}, got {transaction_id}"
            )));
        }

        let interval = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        let leechers = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);
        let seeders = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);

        let peers = parse_compact_peers(&data[20..])?;

        Ok(UdpAnnounceResponse {
            response: AnnounceResponse {
                interval,
                seeders: Some(seeders),
                leechers: Some(leechers),
                peers,
            },
            transaction_id,
        })
    }

    /// Parse a UDP announce response where the tracker address is IPv6.
    ///
    /// Per BEP 15, the compact peer format matches the address family of the
    /// tracker endpoint: 18-byte entries (16 IP + 2 port) for IPv6 trackers.
    pub fn parse_announce_response_v6(
        data: &[u8],
        expected_transaction_id: u32,
    ) -> Result<UdpAnnounceResponse> {
        if data.len() < 20 {
            return Err(Error::UdpProtocol(format!(
                "announce response too short: {} bytes",
                data.len()
            )));
        }

        let action = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        if action != ACTION_ANNOUNCE {
            if action == 3 && data.len() > 8 {
                let msg = String::from_utf8_lossy(&data[8..]);
                return Err(Error::TrackerError(msg.into_owned()));
            }
            return Err(Error::UdpProtocol(format!(
                "expected action 1 (announce), got {action}"
            )));
        }

        let transaction_id = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if transaction_id != expected_transaction_id {
            return Err(Error::UdpProtocol(format!(
                "transaction ID mismatch: expected {expected_transaction_id}, got {transaction_id}"
            )));
        }

        let interval = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        let leechers = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);
        let seeders = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);

        let peers = parse_compact_peers6(&data[20..])?;

        Ok(UdpAnnounceResponse {
            response: AnnounceResponse {
                interval,
                seeders: Some(seeders),
                leechers: Some(leechers),
                peers,
            },
            transaction_id,
        })
    }

    /// Perform a full UDP announce (connect + announce).
    pub async fn announce(
        &self,
        tracker_addr: &str,
        req: &AnnounceRequest,
    ) -> Result<UdpAnnounceResponse> {
        let addr: SocketAddr = tracker_addr
            .parse()
            .map_err(|_| Error::InvalidUrl(format!("invalid socket address: {tracker_addr}")))?;

        let bind_addr = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
        let socket = UdpSocket::bind(bind_addr).await?;
        apply_dscp_udp(&socket, self.dscp, addr.is_ipv6());
        socket.connect(addr).await?;

        // Step 1: Connect
        let txn_id = generate_transaction_id();
        let connect_req = Self::build_connect_request(txn_id);
        socket.send(&connect_req).await?;

        let mut buf = [0u8; 2048];
        let n = tokio::time::timeout(self.timeout, socket.recv(&mut buf))
            .await
            .map_err(|_| Error::Timeout)?
            ?;

        let connection_id = Self::parse_connect_response(&buf[..n], txn_id)?;

        // Step 2: Announce
        let txn_id2 = generate_transaction_id();
        let announce_req = Self::build_announce_request(connection_id, txn_id2, req);
        socket.send(&announce_req).await?;

        let n = tokio::time::timeout(self.timeout, socket.recv(&mut buf))
            .await
            .map_err(|_| Error::Timeout)?
            ?;

        if addr.is_ipv6() {
            Self::parse_announce_response_v6(&buf[..n], txn_id2)
        } else {
            Self::parse_announce_response(&buf[..n], txn_id2)
        }
    }

    /// Build a UDP scrape request packet (BEP 15).
    ///
    /// Format: connection_id(8) + action(4, value=2) + transaction_id(4) + info_hash(20)*N
    pub fn build_scrape_request(
        connection_id: u64,
        transaction_id: u32,
        info_hashes: &[Id20],
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16 + 20 * info_hashes.len());
        buf.extend_from_slice(&connection_id.to_be_bytes());
        buf.extend_from_slice(&ACTION_SCRAPE.to_be_bytes());
        buf.extend_from_slice(&transaction_id.to_be_bytes());
        for hash in info_hashes {
            buf.extend_from_slice(hash.as_bytes());
        }
        buf
    }

    /// Parse a UDP scrape response.
    ///
    /// Format: action(4) + transaction_id(4) + [seeders(4) + completed(4) + leechers(4)]*N
    pub fn parse_scrape_response(
        data: &[u8],
        expected_transaction_id: u32,
    ) -> Result<UdpScrapeResponse> {
        if data.len() < 8 {
            return Err(Error::UdpProtocol(format!(
                "scrape response too short: {} bytes",
                data.len()
            )));
        }

        let action = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        if action == 3 && data.len() > 8 {
            let msg = String::from_utf8_lossy(&data[8..]);
            return Err(Error::TrackerError(msg.into_owned()));
        }
        if action != ACTION_SCRAPE {
            return Err(Error::UdpProtocol(format!(
                "expected action 2 (scrape), got {action}"
            )));
        }

        let transaction_id = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if transaction_id != expected_transaction_id {
            return Err(Error::UdpProtocol(format!(
                "transaction ID mismatch: expected {expected_transaction_id}, got {transaction_id}"
            )));
        }

        let payload = &data[8..];
        if !payload.len().is_multiple_of(12) {
            return Err(Error::UdpProtocol(format!(
                "scrape payload not divisible by 12: {} bytes",
                payload.len()
            )));
        }

        let mut results = Vec::with_capacity(payload.len() / 12);
        for chunk in payload.chunks_exact(12) {
            let complete = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let downloaded = u32::from_be_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            let incomplete = u32::from_be_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]);
            results.push(ScrapeInfo { complete, incomplete, downloaded });
        }

        Ok(UdpScrapeResponse {
            results,
            transaction_id,
        })
    }

    /// Perform a full UDP scrape (connect + scrape).
    pub async fn scrape(
        &self,
        tracker_addr: &str,
        info_hashes: &[Id20],
    ) -> Result<UdpScrapeResponse> {
        let addr: SocketAddr = tracker_addr
            .parse()
            .map_err(|_| Error::InvalidUrl(format!("invalid socket address: {tracker_addr}")))?;

        let bind_addr = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
        let socket = UdpSocket::bind(bind_addr).await?;
        apply_dscp_udp(&socket, self.dscp, addr.is_ipv6());
        socket.connect(addr).await?;

        // Step 1: Connect
        let txn_id = generate_transaction_id();
        let connect_req = Self::build_connect_request(txn_id);
        socket.send(&connect_req).await?;

        let mut buf = [0u8; 2048];
        let n = tokio::time::timeout(self.timeout, socket.recv(&mut buf))
            .await
            .map_err(|_| Error::Timeout)?
            ?;

        let connection_id = Self::parse_connect_response(&buf[..n], txn_id)?;

        // Step 2: Scrape
        let txn_id2 = generate_transaction_id();
        let scrape_req = Self::build_scrape_request(connection_id, txn_id2, info_hashes);
        socket.send(&scrape_req).await?;

        let n = tokio::time::timeout(self.timeout, socket.recv(&mut buf))
            .await
            .map_err(|_| Error::Timeout)?
            ?;

        Self::parse_scrape_response(&buf[..n], txn_id2)
    }
}

impl Default for UdpTracker {
    fn default() -> Self {
        Self::new()
    }
}

fn generate_transaction_id() -> u32 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AnnounceEvent;
    use ferrite_core::Id20;

    fn test_request() -> AnnounceRequest {
        AnnounceRequest {
            info_hash: Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap(),
            peer_id: Id20::from_hex("0102030405060708091011121314151617181920").unwrap(),
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 1000000,
            event: AnnounceEvent::Started,
            num_want: Some(50),
            compact: true,
        }
    }

    #[test]
    fn connect_request_format() {
        let req = UdpTracker::build_connect_request(12345);
        assert_eq!(req.len(), 16);
        // Magic connection ID
        assert_eq!(
            u64::from_be_bytes(req[0..8].try_into().unwrap()),
            CONNECT_MAGIC
        );
        // Action = 0
        assert_eq!(u32::from_be_bytes(req[8..12].try_into().unwrap()), 0);
        // Transaction ID
        assert_eq!(u32::from_be_bytes(req[12..16].try_into().unwrap()), 12345);
    }

    #[test]
    fn connect_response_parse() {
        let mut resp = [0u8; 16];
        resp[0..4].copy_from_slice(&0u32.to_be_bytes()); // action = connect
        resp[4..8].copy_from_slice(&12345u32.to_be_bytes()); // txn id
        resp[8..16].copy_from_slice(&99999u64.to_be_bytes()); // connection id

        let conn_id = UdpTracker::parse_connect_response(&resp, 12345).unwrap();
        assert_eq!(conn_id, 99999);
    }

    #[test]
    fn connect_response_wrong_txn() {
        let mut resp = [0u8; 16];
        resp[0..4].copy_from_slice(&0u32.to_be_bytes());
        resp[4..8].copy_from_slice(&12345u32.to_be_bytes());
        resp[8..16].copy_from_slice(&99999u64.to_be_bytes());

        assert!(UdpTracker::parse_connect_response(&resp, 99999).is_err());
    }

    #[test]
    fn announce_request_format() {
        let req = test_request();
        let data = UdpTracker::build_announce_request(42, 100, &req);
        assert_eq!(data.len(), 98);

        // Connection ID
        assert_eq!(u64::from_be_bytes(data[0..8].try_into().unwrap()), 42);
        // Action = announce
        assert_eq!(u32::from_be_bytes(data[8..12].try_into().unwrap()), 1);
        // Transaction ID
        assert_eq!(u32::from_be_bytes(data[12..16].try_into().unwrap()), 100);
        // Port at the end
        assert_eq!(u16::from_be_bytes(data[96..98].try_into().unwrap()), 6881);
    }

    #[test]
    fn announce_response_parse() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&1u32.to_be_bytes()); // action = announce
        resp.extend_from_slice(&42u32.to_be_bytes()); // txn id
        resp.extend_from_slice(&1800u32.to_be_bytes()); // interval
        resp.extend_from_slice(&5u32.to_be_bytes()); // leechers
        resp.extend_from_slice(&10u32.to_be_bytes()); // seeders
        // One peer: 192.168.1.1:6881
        resp.extend_from_slice(&[192, 168, 1, 1, 0x1A, 0xE1]);

        let parsed = UdpTracker::parse_announce_response(&resp, 42).unwrap();
        assert_eq!(parsed.response.interval, 1800);
        assert_eq!(parsed.response.seeders, Some(10));
        assert_eq!(parsed.response.leechers, Some(5));
        assert_eq!(parsed.response.peers.len(), 1);
        assert_eq!(parsed.response.peers[0].to_string(), "192.168.1.1:6881");
    }

    #[test]
    fn announce_response_error() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&3u32.to_be_bytes()); // action = error
        resp.extend_from_slice(&42u32.to_be_bytes()); // txn id
        resp.extend_from_slice(b"torrent not found");

        let result = UdpTracker::parse_announce_response(&resp, 42);
        assert!(result.is_err());
    }

    #[test]
    fn connect_response_too_short() {
        assert!(UdpTracker::parse_connect_response(&[0u8; 10], 0).is_err());
    }

    #[test]
    fn scrape_request_format() {
        let hash1 = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let hash2 = Id20::from_hex("0102030405060708091011121314151617181920").unwrap();
        let data = UdpTracker::build_scrape_request(42, 100, &[hash1, hash2]);
        assert_eq!(data.len(), 16 + 40); // header + 2 hashes
        assert_eq!(u64::from_be_bytes(data[0..8].try_into().unwrap()), 42);
        assert_eq!(u32::from_be_bytes(data[8..12].try_into().unwrap()), 2); // action=scrape
        assert_eq!(u32::from_be_bytes(data[12..16].try_into().unwrap()), 100);
        assert_eq!(&data[16..36], hash1.as_bytes());
        assert_eq!(&data[36..56], hash2.as_bytes());
    }

    #[test]
    fn scrape_response_parse() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&2u32.to_be_bytes()); // action = scrape
        resp.extend_from_slice(&42u32.to_be_bytes()); // txn id
        // seeders=10, completed=50, leechers=3
        resp.extend_from_slice(&10u32.to_be_bytes());
        resp.extend_from_slice(&50u32.to_be_bytes());
        resp.extend_from_slice(&3u32.to_be_bytes());

        let parsed = UdpTracker::parse_scrape_response(&resp, 42).unwrap();
        assert_eq!(parsed.results.len(), 1);
        assert_eq!(parsed.results[0].complete, 10);
        assert_eq!(parsed.results[0].downloaded, 50);
        assert_eq!(parsed.results[0].incomplete, 3);
    }

    #[test]
    fn scrape_response_multiple_hashes() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&2u32.to_be_bytes());
        resp.extend_from_slice(&42u32.to_be_bytes());
        // Hash 1: seeders=10, completed=50, leechers=3
        resp.extend_from_slice(&10u32.to_be_bytes());
        resp.extend_from_slice(&50u32.to_be_bytes());
        resp.extend_from_slice(&3u32.to_be_bytes());
        // Hash 2: seeders=20, completed=100, leechers=5
        resp.extend_from_slice(&20u32.to_be_bytes());
        resp.extend_from_slice(&100u32.to_be_bytes());
        resp.extend_from_slice(&5u32.to_be_bytes());

        let parsed = UdpTracker::parse_scrape_response(&resp, 42).unwrap();
        assert_eq!(parsed.results.len(), 2);
        assert_eq!(parsed.results[1].complete, 20);
        assert_eq!(parsed.results[1].downloaded, 100);
        assert_eq!(parsed.results[1].incomplete, 5);
    }

    #[test]
    fn scrape_response_wrong_action() {
        let mut resp = Vec::new();
        resp.extend_from_slice(&1u32.to_be_bytes()); // action = announce (wrong)
        resp.extend_from_slice(&42u32.to_be_bytes());

        let result = UdpTracker::parse_scrape_response(&resp, 42);
        assert!(result.is_err());
    }

    #[test]
    fn scrape_response_too_short() {
        let result = UdpTracker::parse_scrape_response(&[0u8; 4], 0);
        assert!(result.is_err());
    }

    #[test]
    fn announce_response_v6_parse() {
        use std::net::Ipv6Addr;
        let mut resp = Vec::new();
        resp.extend_from_slice(&1u32.to_be_bytes()); // action = announce
        resp.extend_from_slice(&42u32.to_be_bytes()); // txn id
        resp.extend_from_slice(&1800u32.to_be_bytes()); // interval
        resp.extend_from_slice(&5u32.to_be_bytes()); // leechers
        resp.extend_from_slice(&10u32.to_be_bytes()); // seeders
        // One IPv6 peer: [2001:db8::1]:6881
        let ip: Ipv6Addr = "2001:db8::1".parse().unwrap();
        resp.extend_from_slice(&ip.octets());
        resp.extend_from_slice(&6881u16.to_be_bytes());

        let parsed = UdpTracker::parse_announce_response_v6(&resp, 42).unwrap();
        assert_eq!(parsed.response.interval, 1800);
        assert_eq!(parsed.response.peers.len(), 1);
        assert_eq!(
            parsed.response.peers[0],
            "[2001:db8::1]:6881".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn udp_tracker_dscp_builder() {
        let tracker = UdpTracker::new().with_dscp(0x2E);
        assert_eq!(tracker.dscp, 0x2E);
    }

    #[test]
    fn udp_tracker_default_no_dscp() {
        let tracker = UdpTracker::new();
        assert_eq!(tracker.dscp, 0);
    }
}
