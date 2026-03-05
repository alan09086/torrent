//! Extension plugin interface for custom BEP 10 extensions.
//!
//! Plugins implement [`ExtensionPlugin`] to handle custom extension messages
//! without modifying torrent internals. Built-in extensions (ut_metadata,
//! ut_pex, lt_trackers) are hard-coded; plugins receive IDs starting at 10.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use torrent_bencode::BencodeValue;
use torrent_core::Id20;
use torrent_wire::ExtHandshake;

/// A custom BEP 10 extension message handler.
///
/// Implement this trait to add custom extension protocol support to a torrent
/// session. Plugins are registered via `ClientBuilder::add_extension()` and
/// are immutable after session start.
///
/// # Extension ID allocation
///
/// Built-in extensions occupy IDs 1-3:
/// - `ut_metadata` = 1
/// - `ut_pex` = 2
/// - `lt_trackers` = 3
///
/// Plugins are assigned IDs starting at 10, in registration order.
///
/// # Constraints
///
/// - Plugins cannot access torrent internals (piece state, peer list).
/// - Plugins cannot initiate unsolicited messages -- respond only.
/// - Plugins cannot override built-in extensions.
/// - Callbacks are synchronous -- spawn tasks internally for async work.
pub trait ExtensionPlugin: Send + Sync + 'static {
    /// Extension name for BEP 10 handshake negotiation (e.g. `"ut_comment"`).
    ///
    /// This name is advertised in the extension handshake `m` dictionary.
    fn name(&self) -> &str;

    /// Called when a peer's extension handshake arrives.
    ///
    /// Return extra key-value pairs to merge into our handshake response,
    /// or `None` to add nothing.
    fn on_handshake(
        &self,
        _info_hash: &Id20,
        _peer_addr: SocketAddr,
        _handshake: &ExtHandshake,
    ) -> Option<BTreeMap<String, BencodeValue>> {
        None
    }

    /// Called when an extension message for this plugin arrives.
    ///
    /// Return an optional response payload to send back to the peer.
    fn on_message(
        &self,
        _info_hash: &Id20,
        _peer_addr: SocketAddr,
        _payload: &[u8],
    ) -> Option<Vec<u8>> {
        None
    }

    /// Peer connected (after BT handshake, before extension handshake).
    fn on_peer_connected(&self, _info_hash: &Id20, _peer_addr: SocketAddr) {}

    /// Peer disconnected.
    fn on_peer_disconnected(&self, _info_hash: &Id20, _peer_addr: SocketAddr) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal test plugin that echoes messages back.
    struct EchoPlugin;

    impl ExtensionPlugin for EchoPlugin {
        fn name(&self) -> &str {
            "ut_echo"
        }

        fn on_message(
            &self,
            _info_hash: &Id20,
            _peer_addr: SocketAddr,
            payload: &[u8],
        ) -> Option<Vec<u8>> {
            Some(payload.to_vec())
        }
    }

    #[test]
    fn plugin_as_trait_object() {
        let plugin: Box<dyn ExtensionPlugin> = Box::new(EchoPlugin);
        assert_eq!(plugin.name(), "ut_echo");
    }

    #[test]
    fn plugin_echo_response() {
        let plugin = EchoPlugin;
        let info_hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        let response = plugin.on_message(&info_hash, addr, b"hello");
        assert_eq!(response, Some(b"hello".to_vec()));
    }

    #[test]
    fn default_lifecycle_hooks_are_noops() {
        let plugin = EchoPlugin;
        let info_hash = Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap();
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();

        // These should not panic
        plugin.on_peer_connected(&info_hash, addr);
        plugin.on_peer_disconnected(&info_hash, addr);

        // Default handshake returns None
        let hs = ExtHandshake::new();
        assert!(plugin.on_handshake(&info_hash, addr, &hs).is_none());
    }

    #[test]
    fn plugin_vec_in_arc() {
        use std::sync::Arc;
        let plugins: Arc<Vec<Box<dyn ExtensionPlugin>>> = Arc::new(vec![
            Box::new(EchoPlugin),
        ]);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name(), "ut_echo");
    }
}
