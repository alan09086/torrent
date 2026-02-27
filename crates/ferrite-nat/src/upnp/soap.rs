//! SOAP XML formatting and raw HTTP for UPnP IGD control.
//!
//! Formats SOAP envelopes for AddPortMapping/DeletePortMapping/GetExternalIPAddress
//! and sends them via raw TCP (no HTTP library needed).

use crate::error::{Error, Result};

/// Format a SOAP AddPortMapping request body.
pub fn format_add_port_mapping(
    external_port: u16,
    protocol: &str,
    internal_port: u16,
    internal_client: &str,
    description: &str,
    lease_duration: u32,
) -> String {
    format!(
        r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:AddPortMapping xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1">
<NewRemoteHost></NewRemoteHost>
<NewExternalPort>{external_port}</NewExternalPort>
<NewProtocol>{protocol}</NewProtocol>
<NewInternalPort>{internal_port}</NewInternalPort>
<NewInternalClient>{internal_client}</NewInternalClient>
<NewEnabled>1</NewEnabled>
<NewPortMappingDescription>{description}</NewPortMappingDescription>
<NewLeaseDuration>{lease_duration}</NewLeaseDuration>
</u:AddPortMapping>
</s:Body>
</s:Envelope>"#
    )
}

/// Format a SOAP DeletePortMapping request body.
pub fn format_delete_port_mapping(external_port: u16, protocol: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:DeletePortMapping xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1">
<NewRemoteHost></NewRemoteHost>
<NewExternalPort>{external_port}</NewExternalPort>
<NewProtocol>{protocol}</NewProtocol>
</u:DeletePortMapping>
</s:Body>
</s:Envelope>"#
    )
}

/// Format a SOAP GetExternalIPAddress request body.
pub fn format_get_external_ip() -> String {
    r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:GetExternalIPAddress xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1">
</u:GetExternalIPAddress>
</s:Body>
</s:Envelope>"#
        .to_string()
}

/// Parse a simple HTTP URL into `(host, port, path)`.
///
/// Only handles `http://host:port/path` — no HTTPS, no auth, no query.
pub fn parse_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) => {
            let port = authority[i + 1..].parse::<u16>().ok()?;
            (&authority[..i], port)
        }
        None => (authority, 80),
    };
    Some((host.to_string(), port, path.to_string()))
}

/// Extract the text content of an XML element by tag name.
///
/// Simple string search — no full XML parser. Finds `<tag>content</tag>`.
pub fn extract_xml_value(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

/// Send a SOAP request via raw TCP and return the response body.
pub async fn soap_request(
    control_url: &str,
    service_type: &str,
    action: &str,
    body: &str,
) -> Result<String> {
    let (host, port, path) =
        parse_url(control_url).ok_or_else(|| Error::UpnpControl("invalid control URL".into()))?;

    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Content-Type: text/xml; charset=\"utf-8\"\r\n\
         Content-Length: {}\r\n\
         SOAPAction: \"{service_type}#{action}\"\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::TcpStream::connect(format!("{host}:{port}")),
    )
    .await
    .map_err(|_| Error::Timeout)?
    .map_err(Error::Io)?;

    stream.write_all(request.as_bytes()).await?;

    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_string(&mut response),
    )
    .await
    .map_err(|_| Error::Timeout)?
    .map_err(Error::Io)?;

    Ok(response)
}

/// Fetch text content from an HTTP URL via raw TCP GET.
pub async fn fetch_text(url: &str) -> Result<String> {
    let (host, port, path) =
        parse_url(url).ok_or_else(|| Error::UpnpControl("invalid URL".into()))?;

    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Connection: close\r\n\
         \r\n"
    );

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::TcpStream::connect(format!("{host}:{port}")),
    )
    .await
    .map_err(|_| Error::Timeout)?
    .map_err(Error::Io)?;

    stream.write_all(request.as_bytes()).await?;

    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_string(&mut response),
    )
    .await
    .map_err(|_| Error::Timeout)?
    .map_err(Error::Io)?;

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn format_add_port_mapping_contains_fields() {
        let body =
            format_add_port_mapping(6881, "TCP", 6881, "192.168.1.100", "ferrite", 3600);
        assert!(body.contains("<NewExternalPort>6881</NewExternalPort>"));
        assert!(body.contains("<NewProtocol>TCP</NewProtocol>"));
        assert!(body.contains("<NewInternalPort>6881</NewInternalPort>"));
        assert!(body.contains("<NewInternalClient>192.168.1.100</NewInternalClient>"));
        assert!(body.contains("<NewPortMappingDescription>ferrite</NewPortMappingDescription>"));
        assert!(body.contains("<NewLeaseDuration>3600</NewLeaseDuration>"));
    }

    #[test]
    fn parse_url_with_port_and_path() {
        let (host, port, path) =
            parse_url("http://192.168.1.1:5000/ctl/IPConn").unwrap();
        assert_eq!(host, "192.168.1.1");
        assert_eq!(port, 5000);
        assert_eq!(path, "/ctl/IPConn");
    }

    #[test]
    fn extract_xml_value_simple() {
        let xml = "<root><NewExternalIPAddress>203.0.113.5</NewExternalIPAddress></root>";
        let ip = extract_xml_value(xml, "NewExternalIPAddress").unwrap();
        assert_eq!(ip, "203.0.113.5");
    }
}
