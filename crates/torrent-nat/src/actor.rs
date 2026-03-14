//! NatActor / NatHandle — background port mapping lifecycle.
//!
//! Tries PCP first (newest), falls back to NAT-PMP, then UPnP IGD.
//! Automatically renews mappings before they expire.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::gateway;

/// Configuration for the NAT port mapper.
#[derive(Debug, Clone)]
pub struct NatConfig {
    /// Enable UPnP IGD port mapping.
    pub enable_upnp: bool,
    /// Enable NAT-PMP / PCP port mapping.
    pub enable_natpmp: bool,
    /// UPnP lease duration in seconds.
    pub upnp_lease_duration: u32,
    /// NAT-PMP / PCP lifetime in seconds.
    pub natpmp_lifetime: u32,
}

impl Default for NatConfig {
    fn default() -> Self {
        Self {
            enable_upnp: true,
            enable_natpmp: true,
            upnp_lease_duration: 3600,
            natpmp_lifetime: 7200,
        }
    }
}

/// Commands sent to the NatActor.
enum NatCommand {
    /// Request port mappings.
    MapPorts {
        tcp_port: u16,
        udp_port: Option<u16>,
    },
    /// Shut down the actor, cleaning up mappings.
    Shutdown,
}

/// Events emitted by the NatActor.
#[derive(Debug, Clone)]
pub enum NatEvent {
    /// A port mapping was successfully created.
    MappingSucceeded {
        /// The mapped port.
        port: u16,
        /// Protocol name ("TCP" or "UDP").
        protocol: String,
    },
    /// A port mapping attempt failed.
    MappingFailed {
        /// The port that failed to map.
        port: u16,
        /// Description of the failure.
        message: String,
    },
    /// External IP address discovered via NAT traversal (PCP/NAT-PMP/UPnP).
    ExternalIpDiscovered {
        /// The discovered external IP address.
        ip: IpAddr,
    },
}

/// Cloneable handle to the NAT port mapping actor.
#[derive(Clone)]
pub struct NatHandle {
    cmd_tx: mpsc::Sender<NatCommand>,
}

impl NatHandle {
    /// Spawn a NatActor and return a handle + event receiver.
    pub fn start(config: NatConfig) -> (Self, mpsc::Receiver<NatEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let (event_tx, event_rx) = mpsc::channel(64);

        let actor = NatActor {
            config,
            cmd_rx,
            event_tx,
            gateway: None,
            upnp_control: None,
            active_tcp_port: None,
            active_udp_port: None,
            granted_lifetime: None,
        };

        tokio::spawn(actor.run());

        (Self { cmd_tx }, event_rx)
    }

    /// Request port mappings for the given TCP (and optionally UDP) port.
    pub async fn map_ports(&self, tcp_port: u16, udp_port: Option<u16>) {
        let _ = self
            .cmd_tx
            .send(NatCommand::MapPorts { tcp_port, udp_port })
            .await;
    }

    /// Shut down the actor, cleaning up any active mappings.
    pub async fn shutdown(&self) {
        let _ = self.cmd_tx.send(NatCommand::Shutdown).await;
    }
}

/// Background actor that manages port mapping lifecycle.
struct NatActor {
    config: NatConfig,
    cmd_rx: mpsc::Receiver<NatCommand>,
    event_tx: mpsc::Sender<NatEvent>,
    gateway: Option<Ipv4Addr>,
    /// Cached UPnP control URL and service type.
    upnp_control: Option<(String, String)>,
    active_tcp_port: Option<u16>,
    active_udp_port: Option<u16>,
    /// Shortest granted lifetime (for renewal scheduling).
    granted_lifetime: Option<u32>,
}

impl NatActor {
    async fn run(mut self) {
        // Discover gateway on startup.
        self.gateway = gateway::default_gateway();
        if let Some(gw) = self.gateway {
            debug!(%gw, "NAT actor discovered default gateway");
        } else {
            debug!("NAT actor: no default gateway found (direct connection?)");
        }

        // Renewal interval — starts at a large value, adjusted after first mapping.
        let mut renewal = tokio::time::interval(Duration::from_secs(3600));
        renewal.tick().await; // consume the immediate first tick
        let mut has_mappings = false;

        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(NatCommand::MapPorts { tcp_port, udp_port }) => {
                            self.active_tcp_port = Some(tcp_port);
                            self.active_udp_port = udp_port;
                            self.do_mapping().await;
                            has_mappings = true;

                            // Update renewal interval to half the granted lifetime.
                            if let Some(lifetime) = self.granted_lifetime {
                                let renew_secs = (lifetime / 2).max(60);
                                renewal = tokio::time::interval(Duration::from_secs(renew_secs as u64));
                                renewal.tick().await; // consume immediate tick
                            }
                        }
                        Some(NatCommand::Shutdown) => {
                            self.do_unmap().await;
                            return;
                        }
                        None => return, // channel closed
                    }
                }
                _ = renewal.tick(), if has_mappings => {
                    debug!("NAT renewal timer fired");
                    self.do_mapping().await;
                }
            }
        }
    }

    /// Try all protocols to create port mappings.
    async fn do_mapping(&mut self) {
        let tcp_port = match self.active_tcp_port {
            Some(p) => p,
            None => return,
        };

        // Try PCP / NAT-PMP first (they share the same port).
        if self.config.enable_natpmp
            && let Some(gw) = self.gateway
            && self.try_pcp_natpmp(gw, tcp_port).await
        {
            return;
        }

        // Fall back to UPnP.
        if self.config.enable_upnp {
            self.try_upnp(tcp_port).await;
        }
    }

    /// Try PCP first, then NAT-PMP fallback. Returns true if successful.
    async fn try_pcp_natpmp(&mut self, gateway: Ipv4Addr, tcp_port: u16) -> bool {
        let local_ip = gateway::local_ip_for_gateway(gateway).unwrap_or(Ipv4Addr::UNSPECIFIED);

        // Try PCP MAP first.
        let mut nonce = [0u8; 12];
        torrent_core::random_bytes(&mut nonce);

        let lifetime = self.config.natpmp_lifetime;

        let pcp_req = crate::pcp::encode_map_request(
            local_ip,
            &nonce,
            crate::pcp::PROTO_TCP,
            tcp_port,
            tcp_port,
            Ipv4Addr::UNSPECIFIED,
            lifetime,
        );

        match crate::pcp::send_pcp_request(gateway, &pcp_req).await {
            Ok(resp_bytes) => match crate::pcp::decode_map_response(&resp_bytes) {
                Ok(resp) if resp.result_code == 0 => {
                    info!(
                        port = tcp_port,
                        protocol = "TCP",
                        "PCP port mapping succeeded"
                    );
                    self.granted_lifetime = Some(resp.lifetime);
                    self.emit(NatEvent::ExternalIpDiscovered {
                        ip: resp.external_ip,
                    })
                    .await;
                    self.emit(NatEvent::MappingSucceeded {
                        port: tcp_port,
                        protocol: "TCP".into(),
                    })
                    .await;
                    // Also map UDP if requested.
                    if let Some(udp_port) = self.active_udp_port {
                        self.try_pcp_udp(gateway, local_ip, udp_port, lifetime)
                            .await;
                    }
                    return true;
                }
                Ok(resp) => {
                    debug!(
                        result_code = resp.result_code,
                        "PCP returned non-zero result, trying NAT-PMP"
                    );
                }
                Err(e) => {
                    debug!(error = %e, "PCP decode failed, trying NAT-PMP");
                }
            },
            Err(crate::error::Error::Timeout) => {
                debug!("PCP timed out, trying NAT-PMP");
            }
            Err(e) => {
                debug!(error = %e, "PCP request failed, trying NAT-PMP");
            }
        }

        // NAT-PMP fallback.
        match crate::natpmp::map_tcp(gateway, tcp_port, tcp_port, lifetime).await {
            Ok(resp) => {
                info!(
                    port = tcp_port,
                    external_port = resp.external_port,
                    "NAT-PMP TCP mapping succeeded"
                );
                self.granted_lifetime = Some(resp.lifetime);
                // Query external IP via NAT-PMP opcode 0.
                if let Ok(ext_ip) = crate::natpmp::query_external_ip(gateway).await {
                    self.emit(NatEvent::ExternalIpDiscovered {
                        ip: IpAddr::V4(ext_ip),
                    })
                    .await;
                }
                self.emit(NatEvent::MappingSucceeded {
                    port: tcp_port,
                    protocol: "TCP".into(),
                })
                .await;

                // Also map UDP if requested.
                if let Some(udp_port) = self.active_udp_port {
                    match crate::natpmp::map_udp(gateway, udp_port, udp_port, lifetime).await {
                        Ok(_) => {
                            info!(port = udp_port, "NAT-PMP UDP mapping succeeded");
                            self.emit(NatEvent::MappingSucceeded {
                                port: udp_port,
                                protocol: "UDP".into(),
                            })
                            .await;
                        }
                        Err(e) => {
                            warn!(port = udp_port, error = %e, "NAT-PMP UDP mapping failed");
                            self.emit(NatEvent::MappingFailed {
                                port: udp_port,
                                message: format!("NAT-PMP UDP: {e}"),
                            })
                            .await;
                        }
                    }
                }
                return true;
            }
            Err(e) => {
                debug!(error = %e, "NAT-PMP TCP mapping failed");
            }
        }

        false
    }

    /// Try PCP UDP mapping (called after TCP succeeds).
    async fn try_pcp_udp(
        &mut self,
        gateway: Ipv4Addr,
        local_ip: Ipv4Addr,
        udp_port: u16,
        lifetime: u32,
    ) {
        let mut nonce = [0u8; 12];
        torrent_core::random_bytes(&mut nonce);

        let req = crate::pcp::encode_map_request(
            local_ip,
            &nonce,
            crate::pcp::PROTO_UDP,
            udp_port,
            udp_port,
            Ipv4Addr::UNSPECIFIED,
            lifetime,
        );

        match crate::pcp::send_pcp_request(gateway, &req).await {
            Ok(resp_bytes) => match crate::pcp::decode_map_response(&resp_bytes) {
                Ok(resp) if resp.result_code == 0 => {
                    info!(port = udp_port, "PCP UDP mapping succeeded");
                    self.emit(NatEvent::MappingSucceeded {
                        port: udp_port,
                        protocol: "UDP".into(),
                    })
                    .await;
                }
                Ok(resp) => {
                    warn!(
                        port = udp_port,
                        result_code = resp.result_code,
                        "PCP UDP mapping refused"
                    );
                    self.emit(NatEvent::MappingFailed {
                        port: udp_port,
                        message: format!("PCP refused (code {})", resp.result_code),
                    })
                    .await;
                }
                Err(e) => {
                    warn!(port = udp_port, error = %e, "PCP UDP decode failed");
                    self.emit(NatEvent::MappingFailed {
                        port: udp_port,
                        message: format!("PCP UDP: {e}"),
                    })
                    .await;
                }
            },
            Err(e) => {
                warn!(port = udp_port, error = %e, "PCP UDP request failed");
                self.emit(NatEvent::MappingFailed {
                    port: udp_port,
                    message: format!("PCP UDP: {e}"),
                })
                .await;
            }
        }
    }

    /// Try UPnP IGD port mapping.
    async fn try_upnp(&mut self, tcp_port: u16) {
        // Discover control URL if not cached.
        if self.upnp_control.is_none() {
            match crate::upnp::discover_control_url(Duration::from_secs(5)).await {
                Ok(result) => {
                    self.upnp_control = Some(result);
                }
                Err(e) => {
                    warn!(error = %e, "UPnP IGD discovery failed");
                    self.emit(NatEvent::MappingFailed {
                        port: tcp_port,
                        message: format!("UPnP discovery: {e}"),
                    })
                    .await;
                    return;
                }
            }
        }

        let (control_url, service_type) = self.upnp_control.as_ref().unwrap();
        let local_ip = self
            .gateway
            .and_then(gateway::local_ip_for_gateway)
            .unwrap_or(Ipv4Addr::LOCALHOST);

        let body = crate::upnp::soap::format_add_port_mapping(
            tcp_port,
            "TCP",
            tcp_port,
            &local_ip.to_string(),
            "torrent",
            self.config.upnp_lease_duration,
        );

        match crate::upnp::soap::soap_request(control_url, service_type, "AddPortMapping", &body)
            .await
        {
            Ok(resp) => {
                if resp.contains("errorCode") {
                    let code = crate::upnp::soap::extract_xml_value(&resp, "errorCode")
                        .unwrap_or_default();
                    warn!(port = tcp_port, error_code = %code, "UPnP AddPortMapping failed");
                    self.emit(NatEvent::MappingFailed {
                        port: tcp_port,
                        message: format!("UPnP error {code}"),
                    })
                    .await;
                } else {
                    info!(port = tcp_port, "UPnP TCP port mapping succeeded");
                    self.granted_lifetime = Some(self.config.upnp_lease_duration);
                    // Query external IP via UPnP GetExternalIPAddress.
                    let ext_body = crate::upnp::soap::format_get_external_ip();
                    if let Ok(ext_resp) = crate::upnp::soap::soap_request(
                        control_url,
                        service_type,
                        "GetExternalIPAddress",
                        &ext_body,
                    )
                    .await
                        && let Some(ip_str) =
                            crate::upnp::soap::extract_xml_value(&ext_resp, "NewExternalIPAddress")
                        && let Ok(ip) = ip_str.parse::<IpAddr>()
                    {
                        self.emit(NatEvent::ExternalIpDiscovered { ip }).await;
                    }
                    self.emit(NatEvent::MappingSucceeded {
                        port: tcp_port,
                        protocol: "TCP".into(),
                    })
                    .await;
                }
            }
            Err(e) => {
                warn!(port = tcp_port, error = %e, "UPnP SOAP request failed");
                self.emit(NatEvent::MappingFailed {
                    port: tcp_port,
                    message: format!("UPnP: {e}"),
                })
                .await;
            }
        }

        // Also map UDP if requested.
        if let Some(udp_port) = self.active_udp_port {
            let body = crate::upnp::soap::format_add_port_mapping(
                udp_port,
                "UDP",
                udp_port,
                &local_ip.to_string(),
                "torrent",
                self.config.upnp_lease_duration,
            );

            match crate::upnp::soap::soap_request(
                control_url,
                service_type,
                "AddPortMapping",
                &body,
            )
            .await
            {
                Ok(resp) if !resp.contains("errorCode") => {
                    info!(port = udp_port, "UPnP UDP port mapping succeeded");
                    self.emit(NatEvent::MappingSucceeded {
                        port: udp_port,
                        protocol: "UDP".into(),
                    })
                    .await;
                }
                Ok(_) | Err(_) => {
                    warn!(port = udp_port, "UPnP UDP port mapping failed");
                    self.emit(NatEvent::MappingFailed {
                        port: udp_port,
                        message: "UPnP UDP failed".into(),
                    })
                    .await;
                }
            }
        }
    }

    /// Best-effort cleanup: delete mappings on shutdown.
    async fn do_unmap(&self) {
        if let Some(tcp_port) = self.active_tcp_port {
            // Try NAT-PMP/PCP deletion (lifetime = 0).
            if self.config.enable_natpmp
                && let Some(gw) = self.gateway
            {
                if let Err(e) = crate::natpmp::delete_tcp_mapping(gw, tcp_port).await {
                    debug!("failed to delete NAT-PMP TCP mapping on port {tcp_port}: {e}");
                }
                if let Some(udp_port) = self.active_udp_port
                    && let Err(e) = crate::natpmp::delete_udp_mapping(gw, udp_port).await
                {
                    debug!("failed to delete NAT-PMP UDP mapping on port {udp_port}: {e}");
                }
            }

            // Try UPnP deletion.
            if let Some((ref control_url, ref service_type)) = self.upnp_control {
                let body = crate::upnp::soap::format_delete_port_mapping(tcp_port, "TCP");
                if let Err(e) = crate::upnp::soap::soap_request(
                    control_url,
                    service_type,
                    "DeletePortMapping",
                    &body,
                )
                .await
                {
                    debug!("failed to delete UPnP TCP mapping on port {tcp_port}: {e}");
                }

                if let Some(udp_port) = self.active_udp_port {
                    let body = crate::upnp::soap::format_delete_port_mapping(udp_port, "UDP");
                    if let Err(e) = crate::upnp::soap::soap_request(
                        control_url,
                        service_type,
                        "DeletePortMapping",
                        &body,
                    )
                    .await
                    {
                        debug!("failed to delete UPnP UDP mapping on port {udp_port}: {e}");
                    }
                }
            }
        }

        debug!("NAT actor shut down, mappings cleaned up");
    }

    /// Emit an event to the session.
    async fn emit(&self, event: NatEvent) {
        let _ = self.event_tx.send(event).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nat_config_defaults() {
        let config = NatConfig::default();
        assert!(config.enable_upnp);
        assert!(config.enable_natpmp);
        assert_eq!(config.upnp_lease_duration, 3600);
        assert_eq!(config.natpmp_lifetime, 7200);
    }

    #[test]
    fn nat_event_external_ip_variant() {
        let event = NatEvent::ExternalIpDiscovered {
            ip: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)),
        };
        match event {
            NatEvent::ExternalIpDiscovered { ip } => {
                assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)));
            }
            _ => panic!("expected ExternalIpDiscovered"),
        }
    }

    #[tokio::test]
    async fn nat_handle_start_and_shutdown() {
        let config = NatConfig {
            enable_upnp: false,
            enable_natpmp: false,
            ..Default::default()
        };
        let (handle, _events_rx) = NatHandle::start(config);
        // Should shut down cleanly without attempting any network operations.
        handle.shutdown().await;
    }
}
