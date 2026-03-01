//! KRPC message encoding and decoding (BEP 5).
//!
//! KRPC messages are bencoded dictionaries with keys:
//! - `t` — transaction ID (binary string, 2 bytes)
//! - `y` — message type: `q` (query), `r` (response), `e` (error)
//! - `q` — query method name (for queries only)
//! - `a` — query arguments dict (for queries only)
//! - `r` — response values dict (for responses only)
//! - `e` — error list `[code, message]` (for errors only)

use std::collections::BTreeMap;

use ferrite_bencode::{self as bencode, BencodeValue};
use ferrite_core::Id20;

use crate::compact::{
    encode_compact_nodes, parse_compact_nodes, CompactNodeInfo,
    encode_compact_nodes6, parse_compact_nodes6, CompactNodeInfo6,
};
use crate::error::{Error, Result};

/// 2-byte transaction ID for matching requests to responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransactionId(pub [u8; 2]);

impl TransactionId {
    pub fn from_u16(val: u16) -> Self {
        TransactionId(val.to_be_bytes())
    }

    pub fn as_u16(&self) -> u16 {
        u16::from_be_bytes(self.0)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 2 {
            // Some implementations use 1-byte transaction IDs; pad with zero
            let mut buf = [0u8; 2];
            buf[..bytes.len()].copy_from_slice(bytes);
            return Ok(TransactionId(buf));
        }
        Ok(TransactionId([bytes[0], bytes[1]]))
    }
}

/// A parsed KRPC message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KrpcMessage {
    pub transaction_id: TransactionId,
    pub body: KrpcBody,
    /// BEP 42: Compact IP+port of the message recipient, included in responses.
    pub sender_ip: Option<std::net::SocketAddr>,
}

/// The body of a KRPC message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KrpcBody {
    Query(KrpcQuery),
    Response(KrpcResponse),
    Error { code: i64, message: String },
}

/// KRPC query types (BEP 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KrpcQuery {
    Ping {
        id: Id20,
    },
    FindNode {
        id: Id20,
        target: Id20,
    },
    GetPeers {
        id: Id20,
        info_hash: Id20,
    },
    AnnouncePeer {
        id: Id20,
        info_hash: Id20,
        port: u16,
        implied_port: bool,
        token: Vec<u8>,
    },
}

/// KRPC response types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KrpcResponse {
    /// Response to ping or announce_peer — just the node ID.
    NodeId { id: Id20 },
    /// Response to find_node.
    FindNode {
        id: Id20,
        nodes: Vec<CompactNodeInfo>,
        /// IPv6 nodes (BEP 24): 38-byte compact format.
        nodes6: Vec<CompactNodeInfo6>,
    },
    /// Response to get_peers — either peers or closer nodes.
    GetPeers(GetPeersResponse),
}

/// get_peers response can return peers, closer nodes, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetPeersResponse {
    pub id: Id20,
    pub token: Option<Vec<u8>>,
    /// Direct peer addresses (compact: 6 bytes each for IPv4, 18 bytes for IPv6).
    pub peers: Vec<std::net::SocketAddr>,
    /// Closer nodes (compact: 26 bytes each).
    pub nodes: Vec<CompactNodeInfo>,
    /// Closer IPv6 nodes (BEP 24, compact: 38 bytes each).
    pub nodes6: Vec<CompactNodeInfo6>,
}

impl KrpcQuery {
    /// The query method name as used in the `q` field.
    pub fn method_name(&self) -> &'static str {
        match self {
            KrpcQuery::Ping { .. } => "ping",
            KrpcQuery::FindNode { .. } => "find_node",
            KrpcQuery::GetPeers { .. } => "get_peers",
            KrpcQuery::AnnouncePeer { .. } => "announce_peer",
        }
    }

    /// The querying node's ID.
    pub fn sender_id(&self) -> &Id20 {
        match self {
            KrpcQuery::Ping { id }
            | KrpcQuery::FindNode { id, .. }
            | KrpcQuery::GetPeers { id, .. }
            | KrpcQuery::AnnouncePeer { id, .. } => id,
        }
    }
}

impl KrpcResponse {
    /// The responding node's ID.
    pub fn sender_id(&self) -> &Id20 {
        match self {
            KrpcResponse::NodeId { id } => id,
            KrpcResponse::FindNode { id, .. } => id,
            KrpcResponse::GetPeers(gp) => &gp.id,
        }
    }
}

// ---- Encoding ----

impl KrpcMessage {
    /// Encode this message to bencode bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut dict = BTreeMap::<Vec<u8>, BencodeValue>::new();
        dict.insert(b"t".to_vec(), BencodeValue::Bytes(self.transaction_id.0.to_vec()));

        if let Some(addr) = &self.sender_ip {
            let ip_bytes = encode_compact_addr(addr);
            dict.insert(b"ip".to_vec(), BencodeValue::Bytes(ip_bytes));
        }

        match &self.body {
            KrpcBody::Query(query) => {
                dict.insert(b"y".to_vec(), BencodeValue::Bytes(b"q".to_vec()));
                dict.insert(
                    b"q".to_vec(),
                    BencodeValue::Bytes(query.method_name().as_bytes().to_vec()),
                );
                dict.insert(b"a".to_vec(), encode_query_args(query));
            }
            KrpcBody::Response(resp) => {
                dict.insert(b"y".to_vec(), BencodeValue::Bytes(b"r".to_vec()));
                dict.insert(b"r".to_vec(), encode_response_values(resp));
            }
            KrpcBody::Error { code, message } => {
                dict.insert(b"y".to_vec(), BencodeValue::Bytes(b"e".to_vec()));
                dict.insert(
                    b"e".to_vec(),
                    BencodeValue::List(vec![
                        BencodeValue::Integer(*code),
                        BencodeValue::Bytes(message.as_bytes().to_vec()),
                    ]),
                );
            }
        }

        bencode::to_bytes(&BencodeValue::Dict(dict)).map_err(Error::from)
    }

    /// Decode from bencode bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let value: BencodeValue = bencode::from_bytes(data)?;
        let dict = value
            .as_dict()
            .ok_or_else(|| Error::InvalidMessage("top-level value is not a dict".into()))?;

        let txn_bytes = dict_bytes(dict, b"t")?;
        let transaction_id = TransactionId::from_bytes(txn_bytes)?;

        let msg_type = dict_str(dict, b"y")?;
        let body = match msg_type {
            b"q" => {
                let method = dict_str(dict, b"q")?;
                let args = dict_dict(dict, b"a")?;
                KrpcBody::Query(decode_query(method, args)?)
            }
            b"r" => {
                let values = dict_dict(dict, b"r")?;
                KrpcBody::Response(decode_response(values)?)
            }
            b"e" => {
                let err_list = dict
                    .get(&b"e"[..])
                    .and_then(|v| v.as_list())
                    .ok_or_else(|| Error::InvalidMessage("missing 'e' list".into()))?;
                if err_list.len() < 2 {
                    return Err(Error::InvalidMessage("error list too short".into()));
                }
                let code = err_list[0]
                    .as_int()
                    .ok_or_else(|| Error::InvalidMessage("error code not integer".into()))?;
                let message = err_list[1]
                    .as_bytes_raw()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .ok_or_else(|| Error::InvalidMessage("error message not string".into()))?;
                KrpcBody::Error { code, message }
            }
            other => {
                return Err(Error::InvalidMessage(format!(
                    "unknown message type: {}",
                    String::from_utf8_lossy(other)
                )));
            }
        };

        let sender_ip = dict
            .get(&b"ip"[..])
            .and_then(|v| v.as_bytes_raw())
            .and_then(decode_compact_addr);

        Ok(KrpcMessage {
            transaction_id,
            body,
            sender_ip,
        })
    }
}

// ---- Internal encoding helpers ----

fn encode_query_args(query: &KrpcQuery) -> BencodeValue {
    let mut args = BTreeMap::<Vec<u8>, BencodeValue>::new();
    match query {
        KrpcQuery::Ping { id } => {
            args.insert(b"id".to_vec(), BencodeValue::Bytes(id.0.to_vec()));
        }
        KrpcQuery::FindNode { id, target } => {
            args.insert(b"id".to_vec(), BencodeValue::Bytes(id.0.to_vec()));
            args.insert(b"target".to_vec(), BencodeValue::Bytes(target.0.to_vec()));
        }
        KrpcQuery::GetPeers { id, info_hash } => {
            args.insert(b"id".to_vec(), BencodeValue::Bytes(id.0.to_vec()));
            args.insert(b"info_hash".to_vec(), BencodeValue::Bytes(info_hash.0.to_vec()));
        }
        KrpcQuery::AnnouncePeer {
            id,
            info_hash,
            port,
            implied_port,
            token,
        } => {
            args.insert(b"id".to_vec(), BencodeValue::Bytes(id.0.to_vec()));
            if *implied_port {
                args.insert(b"implied_port".to_vec(), BencodeValue::Integer(1));
            }
            args.insert(b"info_hash".to_vec(), BencodeValue::Bytes(info_hash.0.to_vec()));
            args.insert(b"port".to_vec(), BencodeValue::Integer(i64::from(*port)));
            args.insert(b"token".to_vec(), BencodeValue::Bytes(token.clone()));
        }
    }
    BencodeValue::Dict(args)
}

fn encode_response_values(resp: &KrpcResponse) -> BencodeValue {
    let mut values = BTreeMap::<Vec<u8>, BencodeValue>::new();
    match resp {
        KrpcResponse::NodeId { id } => {
            values.insert(b"id".to_vec(), BencodeValue::Bytes(id.0.to_vec()));
        }
        KrpcResponse::FindNode { id, nodes, nodes6 } => {
            values.insert(b"id".to_vec(), BencodeValue::Bytes(id.0.to_vec()));
            values.insert(
                b"nodes".to_vec(),
                BencodeValue::Bytes(encode_compact_nodes(nodes)),
            );
            if !nodes6.is_empty() {
                values.insert(
                    b"nodes6".to_vec(),
                    BencodeValue::Bytes(encode_compact_nodes6(nodes6)),
                );
            }
        }
        KrpcResponse::GetPeers(gp) => {
            values.insert(b"id".to_vec(), BencodeValue::Bytes(gp.id.0.to_vec()));
            if let Some(token) = &gp.token {
                values.insert(b"token".to_vec(), BencodeValue::Bytes(token.clone()));
            }
            if !gp.peers.is_empty() {
                let peer_list: Vec<BencodeValue> = gp
                    .peers
                    .iter()
                    .map(|addr| match addr {
                        SocketAddr::V4(v4) => {
                            let mut buf = [0u8; 6];
                            buf[..4].copy_from_slice(&v4.ip().octets());
                            buf[4..6].copy_from_slice(&v4.port().to_be_bytes());
                            BencodeValue::Bytes(buf.to_vec())
                        }
                        SocketAddr::V6(v6) => {
                            let mut buf = [0u8; 18];
                            buf[..16].copy_from_slice(&v6.ip().octets());
                            buf[16..18].copy_from_slice(&v6.port().to_be_bytes());
                            BencodeValue::Bytes(buf.to_vec())
                        }
                    })
                    .collect();
                values.insert(b"values".to_vec(), BencodeValue::List(peer_list));
            }
            if !gp.nodes.is_empty() {
                values.insert(
                    b"nodes".to_vec(),
                    BencodeValue::Bytes(encode_compact_nodes(&gp.nodes)),
                );
            }
            if !gp.nodes6.is_empty() {
                values.insert(
                    b"nodes6".to_vec(),
                    BencodeValue::Bytes(encode_compact_nodes6(&gp.nodes6)),
                );
            }
        }
    }
    BencodeValue::Dict(values)
}

// ---- Compact address helpers (BEP 42 `ip` field) ----

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

/// Encode a socket address to compact binary format (BEP 42 `ip` field).
fn encode_compact_addr(addr: &SocketAddr) -> Vec<u8> {
    match addr {
        SocketAddr::V4(v4) => {
            let mut buf = Vec::with_capacity(6);
            buf.extend_from_slice(&v4.ip().octets());
            buf.extend_from_slice(&v4.port().to_be_bytes());
            buf
        }
        SocketAddr::V6(v6) => {
            let mut buf = Vec::with_capacity(18);
            buf.extend_from_slice(&v6.ip().octets());
            buf.extend_from_slice(&v6.port().to_be_bytes());
            buf
        }
    }
}

/// Decode a compact binary socket address (BEP 42 `ip` field).
fn decode_compact_addr(data: &[u8]) -> Option<SocketAddr> {
    match data.len() {
        6 => {
            let ip = Ipv4Addr::new(data[0], data[1], data[2], data[3]);
            let port = u16::from_be_bytes([data[4], data[5]]);
            Some(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        18 => {
            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&data[..16]).unwrap());
            let port = u16::from_be_bytes([data[16], data[17]]);
            Some(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)))
        }
        _ => None,
    }
}

// ---- Internal decoding helpers ----

fn decode_query(
    method: &[u8],
    args: &BTreeMap<Vec<u8>, BencodeValue>,
) -> Result<KrpcQuery> {
    let id = args_id20(args, b"id")?;
    match method {
        b"ping" => Ok(KrpcQuery::Ping { id }),
        b"find_node" => {
            let target = args_id20(args, b"target")?;
            Ok(KrpcQuery::FindNode { id, target })
        }
        b"get_peers" => {
            let info_hash = args_id20(args, b"info_hash")?;
            Ok(KrpcQuery::GetPeers { id, info_hash })
        }
        b"announce_peer" => {
            let info_hash = args_id20(args, b"info_hash")?;
            let port = args_int(args, b"port")? as u16;
            let implied_port = args
                .get(&b"implied_port"[..])
                .and_then(|v| v.as_int())
                .unwrap_or(0)
                != 0;
            let token = args
                .get(&b"token"[..])
                .and_then(|v| v.as_bytes_raw())
                .map(|b| b.to_vec())
                .ok_or_else(|| Error::InvalidMessage("missing 'token' in announce_peer".into()))?;
            Ok(KrpcQuery::AnnouncePeer {
                id,
                info_hash,
                port,
                implied_port,
                token,
            })
        }
        _ => Err(Error::InvalidMessage(format!(
            "unknown query method: {}",
            String::from_utf8_lossy(method)
        ))),
    }
}

fn decode_response(
    values: &BTreeMap<Vec<u8>, BencodeValue>,
) -> Result<KrpcResponse> {
    let id = args_id20(values, b"id")?;

    // get_peers response: has "values" (peers) or "nodes" (closer nodes) + optional "token"
    let has_values = values.contains_key(&b"values"[..]);
    let has_token = values.contains_key(&b"token"[..]);

    if has_values || has_token {
        let token = values
            .get(&b"token"[..])
            .and_then(|v| v.as_bytes_raw())
            .map(|b| b.to_vec());

        let mut peers = Vec::new();
        if let Some(BencodeValue::List(peer_list)) = values.get(&b"values"[..]) {
            for item in peer_list {
                if let Some(data) = item.as_bytes_raw() {
                    match data.len() {
                        6 => {
                            let ip = Ipv4Addr::new(data[0], data[1], data[2], data[3]);
                            let port = u16::from_be_bytes([data[4], data[5]]);
                            peers.push(SocketAddr::V4(SocketAddrV4::new(ip, port)));
                        }
                        18 => {
                            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&data[..16]).unwrap());
                            let port = u16::from_be_bytes([data[16], data[17]]);
                            peers.push(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)));
                        }
                        _ => {} // skip unknown sizes
                    }
                }
            }
        }

        let nodes = if let Some(nodes_bytes) = values.get(&b"nodes"[..]).and_then(|v| v.as_bytes_raw()) {
            parse_compact_nodes(nodes_bytes)?
        } else {
            Vec::new()
        };

        let nodes6 = if let Some(nodes6_bytes) = values.get(&b"nodes6"[..]).and_then(|v| v.as_bytes_raw()) {
            parse_compact_nodes6(nodes6_bytes)?
        } else {
            Vec::new()
        };

        return Ok(KrpcResponse::GetPeers(GetPeersResponse {
            id,
            token,
            peers,
            nodes,
            nodes6,
        }));
    }

    // find_node response: has "nodes" or "nodes6"
    let has_nodes = values.contains_key(&b"nodes"[..]);
    let has_nodes6 = values.contains_key(&b"nodes6"[..]);

    if has_nodes || has_nodes6 {
        let nodes = if let Some(nodes_bytes) = values.get(&b"nodes"[..]).and_then(|v| v.as_bytes_raw()) {
            parse_compact_nodes(nodes_bytes)?
        } else {
            Vec::new()
        };

        let nodes6 = if let Some(nodes6_bytes) = values.get(&b"nodes6"[..]).and_then(|v| v.as_bytes_raw()) {
            parse_compact_nodes6(nodes6_bytes)?
        } else {
            Vec::new()
        };

        return Ok(KrpcResponse::FindNode { id, nodes, nodes6 });
    }

    // Plain ID response (ping, announce_peer)
    Ok(KrpcResponse::NodeId { id })
}

// ---- Dict access helpers ----

fn dict_bytes<'a>(
    dict: &'a BTreeMap<Vec<u8>, BencodeValue>,
    key: &[u8],
) -> Result<&'a [u8]> {
    dict.get(key)
        .and_then(|v| v.as_bytes_raw())
        .ok_or_else(|| {
            Error::InvalidMessage(format!(
                "missing or invalid key '{}'",
                String::from_utf8_lossy(key)
            ))
        })
}

fn dict_str<'a>(
    dict: &'a BTreeMap<Vec<u8>, BencodeValue>,
    key: &[u8],
) -> Result<&'a [u8]> {
    dict_bytes(dict, key)
}

fn dict_dict<'a>(
    dict: &'a BTreeMap<Vec<u8>, BencodeValue>,
    key: &[u8],
) -> Result<&'a BTreeMap<Vec<u8>, BencodeValue>> {
    dict.get(key)
        .and_then(|v| v.as_dict())
        .ok_or_else(|| {
            Error::InvalidMessage(format!(
                "missing or invalid dict key '{}'",
                String::from_utf8_lossy(key)
            ))
        })
}

fn args_id20(
    args: &BTreeMap<Vec<u8>, BencodeValue>,
    key: &[u8],
) -> Result<Id20> {
    let bytes = args
        .get(key)
        .and_then(|v| v.as_bytes_raw())
        .ok_or_else(|| {
            Error::InvalidMessage(format!(
                "missing '{}' in args",
                String::from_utf8_lossy(key)
            ))
        })?;
    Id20::from_bytes(bytes).map_err(|e| Error::InvalidMessage(e.to_string()))
}

fn args_int(
    args: &BTreeMap<Vec<u8>, BencodeValue>,
    key: &[u8],
) -> Result<i64> {
    args.get(key)
        .and_then(|v| v.as_int())
        .ok_or_else(|| {
            Error::InvalidMessage(format!(
                "missing '{}' integer in args",
                String::from_utf8_lossy(key)
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn test_id() -> Id20 {
        Id20::from_hex("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").unwrap()
    }

    fn target_id() -> Id20 {
        Id20::from_hex("0000000000000000000000000000000000000001").unwrap()
    }

    #[test]
    fn ping_query_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(42),
            body: KrpcBody::Query(KrpcQuery::Ping { id: test_id() }),
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ping_response_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(42),
            body: KrpcBody::Response(KrpcResponse::NodeId { id: test_id() }),
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn find_node_query_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(100),
            body: KrpcBody::Query(KrpcQuery::FindNode {
                id: test_id(),
                target: target_id(),
            }),
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn find_node_response_round_trip() {
        let nodes = vec![CompactNodeInfo {
            id: target_id(),
            addr: "10.0.0.1:6881".parse().unwrap(),
        }];
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(100),
            body: KrpcBody::Response(KrpcResponse::FindNode {
                id: test_id(),
                nodes,
                nodes6: Vec::new(),
            }),
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn get_peers_query_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(200),
            body: KrpcBody::Query(KrpcQuery::GetPeers {
                id: test_id(),
                info_hash: target_id(),
            }),
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn get_peers_response_with_peers_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(200),
            body: KrpcBody::Response(KrpcResponse::GetPeers(GetPeersResponse {
                id: test_id(),
                token: Some(b"aoeusnth".to_vec()),
                peers: vec!["192.168.1.1:6881".parse().unwrap()],
                nodes: Vec::new(),
                nodes6: Vec::new(),
            })),
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn get_peers_response_with_nodes_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(200),
            body: KrpcBody::Response(KrpcResponse::GetPeers(GetPeersResponse {
                id: test_id(),
                token: Some(b"token123".to_vec()),
                peers: Vec::new(),
                nodes: vec![CompactNodeInfo {
                    id: target_id(),
                    addr: "10.0.0.1:6881".parse().unwrap(),
                }],
                nodes6: Vec::new(),
            })),
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn announce_peer_query_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(300),
            body: KrpcBody::Query(KrpcQuery::AnnouncePeer {
                id: test_id(),
                info_hash: target_id(),
                port: 6881,
                implied_port: true,
                token: b"aoeusnth".to_vec(),
            }),
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn error_message_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(500),
            body: KrpcBody::Error {
                code: 201,
                message: "A Generic Error Occurred".into(),
            },
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn decode_bep5_ping_example() {
        // BEP 5 example: ping query
        // d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe
        let data = b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe";
        let msg = KrpcMessage::from_bytes(data).unwrap();
        assert_eq!(msg.transaction_id.0, *b"aa");
        match &msg.body {
            KrpcBody::Query(KrpcQuery::Ping { id }) => {
                assert_eq!(id.as_bytes(), b"abcdefghij0123456789");
            }
            other => panic!("expected Ping query, got {other:?}"),
        }
    }

    #[test]
    fn decode_bep5_error_example() {
        // BEP 5 example: generic error (corrected length: 24 chars)
        let data = b"d1:eli201e24:A Generic Error Occurrede1:t2:aa1:y1:ee";
        let msg = KrpcMessage::from_bytes(data).unwrap();
        assert_eq!(msg.transaction_id.0, *b"aa");
        match &msg.body {
            KrpcBody::Error { code, message } => {
                assert_eq!(*code, 201);
                assert_eq!(message, "A Generic Error Occurred");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn transaction_id_from_single_byte() {
        let tid = TransactionId::from_bytes(&[0x42]).unwrap();
        assert_eq!(tid.0, [0x42, 0x00]);
    }

    #[test]
    fn query_method_names() {
        assert_eq!(
            KrpcQuery::Ping { id: Id20::ZERO }.method_name(),
            "ping"
        );
        assert_eq!(
            KrpcQuery::FindNode {
                id: Id20::ZERO,
                target: Id20::ZERO,
            }
            .method_name(),
            "find_node"
        );
    }

    // --- IPv6 KRPC tests ---

    #[test]
    fn find_node_response_with_nodes6_round_trip() {
        let nodes = vec![CompactNodeInfo {
            id: target_id(),
            addr: "10.0.0.1:6881".parse().unwrap(),
        }];
        let nodes6 = vec![CompactNodeInfo6 {
            id: target_id(),
            addr: "[2001:db8::1]:6881".parse().unwrap(),
        }];
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(100),
            body: KrpcBody::Response(KrpcResponse::FindNode {
                id: test_id(),
                nodes,
                nodes6,
            }),
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn get_peers_response_with_nodes6_round_trip() {
        let nodes6 = vec![CompactNodeInfo6 {
            id: target_id(),
            addr: "[::1]:8080".parse().unwrap(),
        }];
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(200),
            body: KrpcBody::Response(KrpcResponse::GetPeers(GetPeersResponse {
                id: test_id(),
                token: Some(b"tok".to_vec()),
                peers: Vec::new(),
                nodes: Vec::new(),
                nodes6,
            })),
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn get_peers_response_with_ipv6_peer_values() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(200),
            body: KrpcBody::Response(KrpcResponse::GetPeers(GetPeersResponse {
                id: test_id(),
                token: Some(b"tok".to_vec()),
                peers: vec![
                    "192.168.1.1:6881".parse().unwrap(),
                    "[2001:db8::1]:8080".parse().unwrap(),
                ],
                nodes: Vec::new(),
                nodes6: Vec::new(),
            })),
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    // --- BEP 42 ip field tests ---

    #[test]
    fn response_with_ip_field_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(42),
            body: KrpcBody::Response(KrpcResponse::NodeId { id: test_id() }),
            sender_ip: Some("203.0.113.5:6881".parse().unwrap()),
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.sender_ip, Some("203.0.113.5:6881".parse().unwrap()));
    }

    #[test]
    fn response_with_ipv6_ip_field_round_trip() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(42),
            body: KrpcBody::Response(KrpcResponse::NodeId { id: test_id() }),
            sender_ip: Some("[2001:db8::1]:6881".parse().unwrap()),
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.sender_ip, Some("[2001:db8::1]:6881".parse().unwrap()));
    }

    #[test]
    fn message_without_ip_field_parses_as_none() {
        let msg = KrpcMessage {
            transaction_id: TransactionId::from_u16(42),
            body: KrpcBody::Response(KrpcResponse::NodeId { id: test_id() }),
            sender_ip: None,
        };
        let bytes = msg.to_bytes().unwrap();
        let decoded = KrpcMessage::from_bytes(&bytes).unwrap();
        assert!(decoded.sender_ip.is_none());
    }
}
