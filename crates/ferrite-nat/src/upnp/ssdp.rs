//! SSDP M-SEARCH discovery for UPnP Internet Gateway Devices.
//!
//! Sends a multicast M-SEARCH to `239.255.255.250:1900` and collects
//! the first response containing a valid LOCATION header.

use crate::error::{Error, Result};

/// SSDP multicast address.
pub const SSDP_MULTICAST: std::net::Ipv4Addr = std::net::Ipv4Addr::new(239, 255, 255, 250);

/// SSDP multicast port.
pub const SSDP_PORT: u16 = 1900;

/// UPnP WANIPConnection service type.
pub const WAN_IP_SERVICE: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";

/// UPnP WANPPPConnection service type (for PPPoE gateways).
pub const WAN_PPP_SERVICE: &str = "urn:schemas-upnp-org:service:WANPPPConnection:1";

/// UPnP Internet Gateway Device type.
const IGD_DEVICE: &str = "urn:schemas-upnp-org:device:InternetGatewayDevice:1";

/// Format an SSDP M-SEARCH request.
pub fn format_msearch(search_target: &str) -> Vec<u8> {
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: 3\r\n\
         ST: {search_target}\r\n\
         \r\n"
    )
    .into_bytes()
}

/// Parsed SSDP M-SEARCH response.
#[derive(Debug, Clone)]
pub struct SsdpResponse {
    /// URL from the LOCATION header.
    pub location: String,
    /// The search target that matched.
    pub search_target: String,
}

/// Parse an SSDP M-SEARCH response, extracting the LOCATION header.
///
/// Returns `None` if the response is not HTTP 200 OK or lacks a LOCATION.
pub fn parse_msearch_response(data: &[u8]) -> Option<SsdpResponse> {
    let text = std::str::from_utf8(data).ok()?;

    // Must be HTTP 200 OK
    let first_line = text.lines().next()?;
    if !first_line.contains("200") {
        return None;
    }

    let mut location = None;
    let mut st = None;

    for line in text.lines().skip(1) {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("location:") {
            location = Some(line[9..].trim().to_string());
        } else if lower.starts_with("st:") {
            st = Some(line[3..].trim().to_string());
        }
    }

    Some(SsdpResponse {
        location: location?,
        search_target: st.unwrap_or_default(),
    })
}

/// Discover an Internet Gateway Device via SSDP multicast.
///
/// Sends M-SEARCH for IGD and returns the first valid LOCATION URL.
pub async fn discover_igd(timeout: std::time::Duration) -> Result<String> {
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;

    let target = std::net::SocketAddr::from((SSDP_MULTICAST, SSDP_PORT));
    let search = format_msearch(IGD_DEVICE);
    socket.send_to(&search, target).await?;

    let mut buf = vec![0u8; 2048];

    match tokio::time::timeout(timeout, socket.recv_from(&mut buf)).await {
        Ok(Ok((n, _addr))) => {
            if let Some(resp) = parse_msearch_response(&buf[..n]) {
                Ok(resp.location)
            } else {
                Err(Error::UpnpDiscovery("no valid SSDP response".into()))
            }
        }
        Ok(Err(e)) => Err(Error::Io(e)),
        Err(_) => Err(Error::Timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn format_msearch_contains_required_headers() {
        let msg = format_msearch(IGD_DEVICE);
        let text = String::from_utf8(msg).unwrap();
        assert!(text.starts_with("M-SEARCH * HTTP/1.1\r\n"));
        assert!(text.contains("HOST: 239.255.255.250:1900\r\n"));
        assert!(text.contains("MAN: \"ssdp:discover\"\r\n"));
        assert!(text.contains("MX: 3\r\n"));
        assert!(text.contains(&format!("ST: {IGD_DEVICE}\r\n")));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn parse_msearch_response_valid() {
        let response = b"HTTP/1.1 200 OK\r\n\
            LOCATION: http://192.168.1.1:5000/rootDesc.xml\r\n\
            ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\
            USN: uuid:test-device\r\n\
            \r\n";
        let parsed = parse_msearch_response(response).unwrap();
        assert_eq!(
            parsed.location,
            "http://192.168.1.1:5000/rootDesc.xml"
        );
        assert_eq!(
            parsed.search_target,
            "urn:schemas-upnp-org:device:InternetGatewayDevice:1"
        );
    }

    #[test]
    fn parse_msearch_response_not_200() {
        let response = b"HTTP/1.1 404 Not Found\r\n\r\n";
        assert!(parse_msearch_response(response).is_none());
    }
}
