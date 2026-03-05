//! UPnP IGD port mapping — SSDP discovery + SOAP control.

pub mod ssdp;
pub mod soap;

use crate::error::{Error, Result};

/// Common UPnP description paths to probe on the gateway.
const GATEWAY_DESC_PATHS: &[&str] = &[
    "/rootDesc.xml",
    "/gatedesc.xml",
    "/description.xml",
    "/IGD.xml",
    "/upnp/IGD.xml",
];

/// Common UPnP ports on routers.
const GATEWAY_PORTS: &[u16] = &[49152, 5000, 5431, 2869, 49153];

/// Service types to search for in device descriptions (v1 and v2).
const SERVICE_TYPES: &[&str] = &[
    ssdp::WAN_IP_SERVICE,
    ssdp::WAN_PPP_SERVICE,
    ssdp::WAN_IP_SERVICE_V2,
];

/// Discover the UPnP IGD control URL.
///
/// 1. Send SSDP M-SEARCH (v1 + v2) to find the gateway device
/// 2. If SSDP fails, probe the default gateway directly on common UPnP ports
/// 3. Fetch the device description XML from the LOCATION URL
/// 4. Parse out the `controlURL` for WANIPConnection or WANPPPConnection
pub async fn discover_control_url(timeout: std::time::Duration) -> Result<(String, String)> {
    // Try SSDP first.
    match ssdp::discover_igd(timeout).await {
        Ok(location) => {
            tracing::debug!(%location, "SSDP discovered IGD");
            return discover_from_location(&location).await;
        }
        Err(e) => {
            tracing::debug!("SSDP discovery failed ({e}), trying gateway probe");
        }
    }

    // Fallback: probe the default gateway directly.
    if let Some(gw) = crate::gateway::default_gateway() {
        tracing::debug!(%gw, "probing gateway for UPnP description");
        if let Some(result) = probe_gateway_upnp(gw).await {
            return Ok(result);
        }
    }

    Err(Error::UpnpDiscovery(
        "SSDP and gateway probe both failed".into(),
    ))
}

/// Extract control URL and service type from a device description LOCATION.
async fn discover_from_location(location: &str) -> Result<(String, String)> {
    let description = soap::fetch_text(location).await?;
    let base_url = extract_base_url(location).unwrap_or_default();
    resolve_control_url(&description, &base_url)
}

/// Parse the description XML and find the control URL for a supported service.
fn resolve_control_url(description: &str, base_url: &str) -> Result<(String, String)> {
    for service_type in SERVICE_TYPES {
        if let Some(control_path) = find_control_url(description, service_type) {
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

/// Probe the default gateway on common UPnP ports and paths for a device
/// description XML. Returns the control URL and service type if found.
async fn probe_gateway_upnp(gw: std::net::Ipv4Addr) -> Option<(String, String)> {
    for &port in GATEWAY_PORTS {
        for &path in GATEWAY_DESC_PATHS {
            let url = format!("http://{gw}:{port}{path}");
            let base_url = format!("http://{gw}:{port}");

            // Quick timeout per probe — don't spend long on dead ports.
            let fetch = async { soap::fetch_text(&url).await };
            match tokio::time::timeout(std::time::Duration::from_secs(2), fetch).await {
                Ok(Ok(description)) => {
                    if description.contains("InternetGatewayDevice")
                        || description.contains("WANIPConnection")
                        || description.contains("WANPPPConnection")
                    {
                        tracing::debug!(%url, "gateway probe found UPnP description");
                        if let Ok(result) = resolve_control_url(&description, &base_url) {
                            return Some(result);
                        }
                    }
                }
                _ => continue,
            }
        }
    }

    None
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
