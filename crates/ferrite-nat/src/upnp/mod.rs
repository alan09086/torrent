//! UPnP IGD port mapping — SSDP discovery + SOAP control.

pub mod ssdp;
pub mod soap;

use crate::error::{Error, Result};

/// Discover the UPnP IGD control URL.
///
/// 1. Send SSDP M-SEARCH to find the gateway device
/// 2. Fetch the device description XML from the LOCATION URL
/// 3. Parse out the `controlURL` for WANIPConnection or WANPPPConnection
pub async fn discover_control_url(timeout: std::time::Duration) -> Result<(String, String)> {
    let location = ssdp::discover_igd(timeout).await?;
    tracing::debug!(%location, "SSDP discovered IGD");

    let description = soap::fetch_text(&location).await?;

    // Determine base URL from LOCATION (e.g., http://192.168.1.1:5000)
    let base_url = extract_base_url(&location).unwrap_or_default();

    // Try WANIPConnection first, then WANPPPConnection
    for service_type in [ssdp::WAN_IP_SERVICE, ssdp::WAN_PPP_SERVICE] {
        if let Some(control_path) = find_control_url(&description, service_type) {
            let control_url = if control_path.starts_with("http") {
                control_path
            } else {
                format!("{base_url}{control_path}")
            };
            tracing::debug!(%control_url, %service_type, "found UPnP control URL");
            return Ok((control_url, service_type.to_string()));
        }
    }

    Err(Error::UpnpDiscovery(
        "no WANIPConnection or WANPPPConnection service found".into(),
    ))
}

/// Extract the base URL (scheme + host + port) from a full URL.
fn extract_base_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let end = rest.find('/').unwrap_or(rest.len());
    Some(format!("http://{}", &rest[..end]))
}

/// Find the `controlURL` for a given service type in device description XML.
///
/// Looks for the service type string, then finds the nearest `<controlURL>`.
fn find_control_url(xml: &str, service_type: &str) -> Option<String> {
    let st_pos = xml.find(service_type)?;
    // Search for controlURL after the service type
    let after = &xml[st_pos..];
    soap::extract_xml_value(after, "controlURL")
}
