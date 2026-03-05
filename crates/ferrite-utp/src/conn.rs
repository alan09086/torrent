use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use tracing::{debug, trace};

use crate::congestion::LedbatController;
use crate::packet::{ExtensionType, Header, Packet, PacketType, HEADER_SIZE};
use crate::seq::SeqNr;

/// Maximum payload per packet (conservative, fits in most MTUs).
const MAX_PAYLOAD: usize = 1400 - HEADER_SIZE;

/// Duplicate ACK threshold before treating as loss.
const DUP_ACK_THRESHOLD: u32 = 3;

/// Maximum number of consecutive timeouts before giving up.
const MAX_TIMEOUTS: u32 = 5;

/// Connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    /// SYN sent, awaiting SYN-ACK.
    SynSent,
    /// SYN received, awaiting first data or ACK.
    SynRecv,
    /// Connection established.
    Connected,
    /// FIN sent, awaiting peer FIN.
    FinSent,
    /// Connection fully closed.
    Closed,
    /// Connection reset by peer.
    Reset,
}

/// Key for identifying a connection: (remote_addr, local recv_id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionKey {
    /// Remote peer address.
    pub addr: SocketAddr,
    /// Local receive connection ID.
    pub recv_id: u16,
}

/// Actions the connection wants the socket actor to perform.
#[derive(Debug)]
pub enum ConnAction {
    /// Send this packet to the peer.
    Send(Bytes),
    /// Deliver this data to the application (in-order bytes).
    Deliver(Bytes),
    /// Connection is fully closed.
    Closed,
}

/// An in-flight packet awaiting acknowledgement.
struct InFlightPacket {
    seq_nr: SeqNr,
    payload_len: u32,
    data: Bytes,
    sent_at: Instant,
    retransmits: u32,
}

/// uTP connection state machine.
///
/// Pure logic — never touches I/O directly. Produces `Vec<ConnAction>`
/// that the socket actor executes.
pub struct Connection {
    state: ConnState,
    /// Our connection ID for receiving (peer sends to this).
    recv_id: u16,
    /// Peer's connection ID for receiving (we send to this).
    send_id: u16,
    /// Next sequence number we'll send.
    seq_nr: SeqNr,
    /// Last sequence number ACKed by peer.
    ack_nr: SeqNr,
    /// Next in-order sequence number we expect to receive.
    recv_nr: SeqNr,
    /// Last ACK number we sent (highest in-order seq we received).
    last_ack_sent: SeqNr,
    /// Peer's advertised receive window.
    peer_wnd_size: u32,
    /// Our advertised receive window.
    our_wnd_size: u32,
    /// Congestion controller.
    congestion: LedbatController,
    /// Packets sent but not yet ACKed, keyed by seq_nr.
    in_flight: BTreeMap<SeqNr, InFlightPacket>,
    /// Out-of-order received data, keyed by seq_nr.
    recv_buf: BTreeMap<SeqNr, Bytes>,
    /// If we received a FIN, its sequence number.
    fin_seq_nr: Option<SeqNr>,
    /// Our last sent timestamp (for timestamp_diff calculation).
    last_recv_timestamp_us: u32,
    /// Duplicate ACK counter.
    dup_ack_count: u32,
    /// Consecutive timeout counter.
    timeout_count: u32,
    /// Whether we initiated the close.
    fin_sent: bool,
    /// Unsent data waiting for congestion window to open.
    send_buf: Vec<u8>,
    /// Remote address.
    pub remote_addr: SocketAddr,
}

impl Connection {
    /// Create a new outbound connection (we are initiating).
    ///
    /// Returns the connection and the SYN packet to send.
    pub fn new_outbound(remote_addr: SocketAddr, recv_id: u16) -> (Self, Bytes) {
        let send_id = recv_id.wrapping_add(1);
        let seq_nr = SeqNr(1);

        let conn = Self {
            state: ConnState::SynSent,
            recv_id,
            send_id,
            seq_nr: seq_nr.wrapping_add(1), // SYN consumes seq 1
            ack_nr: SeqNr(0),
            recv_nr: SeqNr(0),
            last_ack_sent: SeqNr(0),
            peer_wnd_size: 0,
            our_wnd_size: 1_048_576, // 1 MiB
            congestion: LedbatController::new(),
            in_flight: BTreeMap::new(),
            recv_buf: BTreeMap::new(),
            fin_seq_nr: None,
            last_recv_timestamp_us: 0,
            dup_ack_count: 0,
            timeout_count: 0,
            fin_sent: false,
            send_buf: Vec::new(),
            remote_addr,
        };

        let syn = conn.build_packet(PacketType::Syn, seq_nr, Bytes::new());
        (conn, syn)
    }

    /// Create a new inbound connection from a received SYN.
    ///
    /// Returns the connection and the SYN-ACK (ST_STATE) packet to send.
    pub fn new_inbound(remote_addr: SocketAddr, syn: &Packet) -> (Self, Bytes) {
        let recv_id = syn.header.connection_id.wrapping_add(1);
        let send_id = syn.header.connection_id;
        let seq_nr = SeqNr(1);

        let conn = Self {
            state: ConnState::SynRecv,
            recv_id,
            send_id,
            seq_nr: seq_nr.wrapping_add(1), // SYN-ACK consumes seq 1
            ack_nr: syn.header.seq_nr,
            recv_nr: syn.header.seq_nr.wrapping_add(1),
            last_ack_sent: syn.header.seq_nr,
            peer_wnd_size: syn.header.wnd_size,
            our_wnd_size: 1_048_576,
            congestion: LedbatController::new(),
            in_flight: BTreeMap::new(),
            recv_buf: BTreeMap::new(),
            fin_seq_nr: None,
            last_recv_timestamp_us: syn.header.timestamp_us,
            dup_ack_count: 0,
            timeout_count: 0,
            fin_sent: false,
            send_buf: Vec::new(),
            remote_addr,
        };

        let syn_ack = conn.build_packet(PacketType::State, seq_nr, Bytes::new());
        (conn, syn_ack)
    }

    /// Current connection state.
    pub fn state(&self) -> ConnState {
        self.state
    }

    /// Our recv_id (used for connection key lookup).
    pub fn recv_id(&self) -> u16 {
        self.recv_id
    }

    /// Our send_id.
    pub fn send_id(&self) -> u16 {
        self.send_id
    }

    /// Process an incoming packet and return actions.
    pub fn on_packet(&mut self, packet: &Packet, now: Instant) -> Vec<ConnAction> {
        let mut actions = Vec::new();

        // Update peer window and timestamp
        self.peer_wnd_size = packet.header.wnd_size;
        let timestamp_diff = self.compute_timestamp_diff(packet.header.timestamp_us);
        self.last_recv_timestamp_us = packet.header.timestamp_us;

        match packet.header.packet_type {
            PacketType::Reset => {
                debug!("received ST_RESET from {}", self.remote_addr);
                self.state = ConnState::Reset;
                actions.push(ConnAction::Closed);
                return actions;
            }
            PacketType::Syn => {
                // Duplicate SYN — resend our SYN-ACK
                if self.state == ConnState::SynRecv {
                    let ack = self.build_ack();
                    actions.push(ConnAction::Send(ack));
                }
                return actions;
            }
            _ => {}
        }

        // Process ACK (all non-SYN, non-RESET packets carry an ACK)
        self.process_ack(packet, timestamp_diff, now, &mut actions);

        // State transitions based on packet type
        match self.state {
            ConnState::SynSent => {
                if packet.header.packet_type == PacketType::State {
                    debug!("SynSent -> Connected");
                    self.state = ConnState::Connected;
                    self.ack_nr = packet.header.seq_nr;
                    self.recv_nr = packet.header.seq_nr.wrapping_add(1);
                    self.last_ack_sent = packet.header.seq_nr;
                    self.timeout_count = 0;
                }
            }
            ConnState::SynRecv => {
                // Any valid packet (DATA or STATE) transitions to Connected
                debug!("SynRecv -> Connected");
                self.state = ConnState::Connected;
                self.timeout_count = 0;
            }
            _ => {}
        }

        // Process data payload
        if !packet.payload.is_empty()
            && (self.state == ConnState::Connected || self.state == ConnState::FinSent)
        {
            self.receive_data(packet.header.seq_nr, packet.payload.clone(), &mut actions);
        }

        // Handle FIN
        if packet.header.packet_type == PacketType::Fin {
            self.fin_seq_nr = Some(packet.header.seq_nr);
            // Insert empty entry so drain logic detects FIN position
            self.recv_buf
                .entry(packet.header.seq_nr)
                .or_default();
            self.drain_recv_buf(&mut actions);
        }

        actions
    }

    /// Queue data for sending. Buffers internally; call `flush_send_buf()`
    /// to emit Send actions when window space is available.
    pub fn send_data(&mut self, data: &[u8], now: Instant) -> Vec<ConnAction> {
        if self.state != ConnState::Connected {
            return Vec::new();
        }

        self.send_buf.extend_from_slice(data);
        self.flush_send_buf(now)
    }

    /// Try to send buffered data within the congestion window.
    pub fn flush_send_buf(&mut self, now: Instant) -> Vec<ConnAction> {
        let mut actions = Vec::new();

        if self.state != ConnState::Connected || self.send_buf.is_empty() {
            return actions;
        }

        while !self.send_buf.is_empty() {
            let available = self.congestion.available_window() as usize;
            let chunk_size = self.send_buf.len().min(MAX_PAYLOAD);
            if available < chunk_size {
                break; // Window full
            }

            let chunk_data: Vec<u8> = self.send_buf.drain(..chunk_size).collect();
            let chunk = Bytes::from(chunk_data);
            let seq = self.seq_nr;
            let packet_data = self.build_packet(PacketType::Data, seq, chunk.clone());
            let payload_len = chunk.len() as u32;

            self.in_flight.insert(
                seq,
                InFlightPacket {
                    seq_nr: seq,
                    payload_len,
                    data: packet_data.clone(),
                    sent_at: now,
                    retransmits: 0,
                },
            );

            self.congestion.on_send(payload_len);
            self.seq_nr = self.seq_nr.wrapping_add(1);
            actions.push(ConnAction::Send(packet_data));
        }

        actions
    }

    /// Check for timeouts and return retransmission actions.
    pub fn check_timeouts(&mut self, now: Instant) -> Vec<ConnAction> {
        let mut actions = Vec::new();
        let rto = std::time::Duration::from_millis(self.congestion.rto_ms());

        // Check if connection timed out completely
        if (self.state == ConnState::SynSent || self.state == ConnState::SynRecv)
            && self.timeout_count >= MAX_TIMEOUTS
        {
            self.state = ConnState::Closed;
            actions.push(ConnAction::Closed);
            return actions;
        }

        // Find oldest unACKed packet
        let oldest = self.in_flight.values().next().map(|p| (p.sent_at, p.seq_nr));

        if let Some((sent_at, seq)) = oldest
            && now.duration_since(sent_at) >= rto
        {
            trace!("timeout on seq {seq}, rto={rto:?}");
            self.congestion.on_timeout();
            self.timeout_count += 1;

            if self.timeout_count >= MAX_TIMEOUTS {
                debug!("max timeouts reached, closing connection");
                self.state = ConnState::Closed;
                actions.push(ConnAction::Closed);
                return actions;
            }

            // Retransmit
            if let Some(pkt) = self.in_flight.get_mut(&seq) {
                pkt.sent_at = now;
                pkt.retransmits += 1;
                actions.push(ConnAction::Send(pkt.data.clone()));
            }
        }

        actions
    }

    /// Initiate a graceful close by sending FIN.
    pub fn initiate_close(&mut self) -> Vec<ConnAction> {
        let mut actions = Vec::new();

        if self.state == ConnState::Connected && !self.fin_sent {
            let seq = self.seq_nr;
            let fin = self.build_packet(PacketType::Fin, seq, Bytes::new());
            self.seq_nr = self.seq_nr.wrapping_add(1);
            self.state = ConnState::FinSent;
            self.fin_sent = true;
            actions.push(ConnAction::Send(fin));
        }

        actions
    }

    /// Build a SACK bitmask from gaps in recv_buf.
    ///
    /// Bit 0 of byte 0 = recv_nr+2 received, bit 1 = recv_nr+3, etc.
    pub fn build_sack(&self) -> Option<Bytes> {
        if self.recv_buf.is_empty() {
            return None;
        }

        let mut bitmask = [0u8; 4]; // 32 bits = 32 future packets
        let mut any_set = false;

        for i in 0u16..32 {
            let seq = self.recv_nr.wrapping_add(i + 2);
            if self.recv_buf.contains_key(&seq) {
                let byte_idx = (i / 8) as usize;
                let bit_idx = i % 8;
                bitmask[byte_idx] |= 1 << bit_idx;
                any_set = true;
            }
        }

        if any_set {
            Some(Bytes::copy_from_slice(&bitmask))
        } else {
            None
        }
    }

    // --- Private helpers ---

    /// Process the ACK field and SACK extension from an incoming packet.
    fn process_ack(
        &mut self,
        packet: &Packet,
        timestamp_diff: u32,
        now: Instant,
        actions: &mut Vec<ConnAction>,
    ) {
        let ack_nr = packet.header.ack_nr;

        // Count newly ACKed packets
        let mut bytes_acked = 0u32;
        let mut newly_acked = Vec::new();

        for (&seq, pkt) in &self.in_flight {
            // ACKed if seq <= ack_nr (in wrapping sense)
            if seq == ack_nr || seq.precedes(ack_nr) {
                newly_acked.push(seq);
                bytes_acked += pkt.payload_len;

                // Update RTT from the first non-retransmitted packet
                if pkt.retransmits == 0 {
                    let rtt_us = now
                        .duration_since(pkt.sent_at)
                        .as_micros()
                        .min(u32::MAX as u128) as u32;
                    self.congestion.update_rtt(rtt_us);
                }
            }
        }

        for seq in &newly_acked {
            self.in_flight.remove(seq);
        }

        if bytes_acked > 0 {
            self.congestion.on_ack(timestamp_diff, bytes_acked, now);
            self.dup_ack_count = 0;
            self.timeout_count = 0;
        } else if !self.in_flight.is_empty() {
            // Duplicate ACK
            self.dup_ack_count += 1;
            if self.dup_ack_count >= DUP_ACK_THRESHOLD {
                trace!("triple dup ack, treating as loss");
                self.congestion.on_loss();
                self.dup_ack_count = 0;

                // Fast retransmit oldest
                if let Some(pkt) = self.in_flight.values().next() {
                    actions.push(ConnAction::Send(pkt.data.clone()));
                }
            }
        }

        // Process SACK extension: mark selectively ACKed packets
        if let Some(sack) = &packet.sack {
            for (i, &byte) in sack.iter().enumerate() {
                for bit in 0..8 {
                    if byte & (1 << bit) != 0 {
                        let sacked_seq = ack_nr.wrapping_add((i as u16) * 8 + bit + 2);
                        if let Some(pkt) = self.in_flight.remove(&sacked_seq) {
                            self.congestion.on_bytes_acked(pkt.payload_len);
                        }
                    }
                }
            }
        }
    }

    /// Insert received data into recv_buf and drain in-order.
    fn receive_data(&mut self, seq_nr: SeqNr, data: Bytes, actions: &mut Vec<ConnAction>) {
        if seq_nr == self.recv_nr || self.recv_nr.precedes(seq_nr) {
            self.recv_buf.insert(seq_nr, data);
            self.drain_recv_buf(actions);
        }
        // else: already received or too old, ignore

        // Always ACK (possibly with SACK)
        let ack = self.build_ack();
        actions.push(ConnAction::Send(ack));
    }

    /// Drain contiguous in-order data from recv_buf.
    fn drain_recv_buf(&mut self, actions: &mut Vec<ConnAction>) {
        while let Some(data) = self.recv_buf.remove(&self.recv_nr) {
            // Check if this is the FIN position
            if Some(self.recv_nr) == self.fin_seq_nr {
                debug!("received FIN at seq {}", self.recv_nr);
                self.recv_nr = self.recv_nr.wrapping_add(1);
                if self.state == ConnState::FinSent {
                    self.state = ConnState::Closed;
                }
                actions.push(ConnAction::Closed);
                // Send final ACK
                let ack = self.build_ack();
                actions.push(ConnAction::Send(ack));
                break;
            }

            if !data.is_empty() {
                actions.push(ConnAction::Deliver(data));
            }
            self.recv_nr = self.recv_nr.wrapping_add(1);
        }
    }

    /// Build an ACK (ST_STATE) packet, optionally with SACK.
    fn build_ack(&mut self) -> Bytes {
        let sack = self.build_sack();
        let ack_nr = self.recv_nr.wrapping_sub(1);
        self.last_ack_sent = ack_nr;

        let now_us = timestamp_us();
        let diff_us = now_us.wrapping_sub(self.last_recv_timestamp_us);

        let header = Header {
            packet_type: PacketType::State,
            extension: if sack.is_some() {
                ExtensionType::Sack
            } else {
                ExtensionType::None
            },
            connection_id: self.send_id,
            timestamp_us: now_us,
            timestamp_diff_us: diff_us,
            wnd_size: self.our_wnd_size,
            seq_nr: self.seq_nr,
            ack_nr,
        };

        let packet = Packet {
            header,
            sack,
            close_reason: None,
            payload: Bytes::new(),
        };
        packet.encode()
    }

    /// Build a packet with the given type, sequence number, and payload.
    fn build_packet(&self, ptype: PacketType, seq: SeqNr, payload: Bytes) -> Bytes {
        let now_us = timestamp_us();
        let diff_us = now_us.wrapping_sub(self.last_recv_timestamp_us);

        let header = Header {
            packet_type: ptype,
            extension: ExtensionType::None,
            connection_id: if ptype == PacketType::Syn {
                self.recv_id
            } else {
                self.send_id
            },
            timestamp_us: now_us,
            timestamp_diff_us: diff_us,
            wnd_size: self.our_wnd_size,
            seq_nr: seq,
            ack_nr: if self.recv_nr.0 > 0 {
                self.recv_nr.wrapping_sub(1)
            } else {
                SeqNr(0)
            },
        };

        let packet = Packet {
            header,
            sack: None,
            close_reason: None,
            payload,
        };
        packet.encode()
    }

    /// Compute timestamp_diff from received timestamp.
    fn compute_timestamp_diff(&self, recv_timestamp_us: u32) -> u32 {
        let now = timestamp_us();
        now.wrapping_sub(recv_timestamp_us)
    }
}

/// Get current time as microseconds (wrapping u32).
pub fn timestamp_us() -> u32 {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    (dur.as_micros() & 0xFFFF_FFFF) as u32
}

/// Build a ST_RESET packet for unknown connections.
pub fn build_reset(connection_id: u16) -> Bytes {
    let header = Header {
        packet_type: PacketType::Reset,
        extension: ExtensionType::None,
        connection_id,
        timestamp_us: timestamp_us(),
        timestamp_diff_us: 0,
        wnd_size: 0,
        seq_nr: SeqNr(0),
        ack_nr: SeqNr(0),
    };
    let packet = Packet {
        header,
        sack: None,
        close_reason: None,
        payload: Bytes::new(),
    };
    packet.encode()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_state_syn_to_connected() {
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let (mut conn, syn_data) = Connection::new_outbound(addr, 100);
        assert_eq!(conn.state(), ConnState::SynSent);

        // Verify SYN packet
        let syn_pkt = Packet::decode(&syn_data).unwrap();
        assert_eq!(syn_pkt.header.packet_type, PacketType::Syn);
        assert_eq!(syn_pkt.header.connection_id, 100); // SYN uses recv_id

        // Simulate receiving SYN-ACK (ST_STATE)
        let syn_ack = Packet {
            header: Header {
                packet_type: PacketType::State,
                extension: ExtensionType::None,
                connection_id: 101, // send_id = recv_id + 1
                timestamp_us: timestamp_us(),
                timestamp_diff_us: 1000,
                wnd_size: 65536,
                seq_nr: SeqNr(1),
                ack_nr: SeqNr(1), // ACKs our SYN
            },
            sack: None,
            close_reason: None,
            payload: Bytes::new(),
        };

        let actions = conn.on_packet(&syn_ack, Instant::now());
        assert_eq!(conn.state(), ConnState::Connected);
        // No Closed action
        assert!(!actions.iter().any(|a| matches!(a, ConnAction::Closed)));
    }

    #[test]
    fn build_sack_bitmask() {
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let (mut conn, _) = Connection::new_outbound(addr, 100);
        conn.state = ConnState::Connected;
        conn.recv_nr = SeqNr(10);

        // No gaps → no SACK
        assert!(conn.build_sack().is_none());

        // Insert packets at recv_nr+2 (=12) and recv_nr+4 (=14)
        // Missing: recv_nr (10), 11, 13
        conn.recv_buf.insert(SeqNr(12), Bytes::from_static(b"a"));
        conn.recv_buf.insert(SeqNr(14), Bytes::from_static(b"b"));

        let sack = conn.build_sack().unwrap();
        // Bit 0 = seq 12 (recv_nr+2) → set
        // Bit 1 = seq 13 → not set
        // Bit 2 = seq 14 → set
        assert_eq!(sack[0] & 0b001, 1); // bit 0 set (seq 12)
        assert_eq!(sack[0] & 0b010, 0); // bit 1 clear (seq 13)
        assert_eq!(sack[0] & 0b100, 4); // bit 2 set (seq 14)
    }

    #[test]
    fn retransmit_on_timeout() {
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let (mut conn, _) = Connection::new_outbound(addr, 100);
        conn.state = ConnState::Connected;

        // Send some data
        let past = Instant::now() - std::time::Duration::from_secs(5);
        let seq = conn.seq_nr;
        let pkt_data = conn.build_packet(PacketType::Data, seq, Bytes::from_static(b"test"));
        conn.in_flight.insert(
            seq,
            InFlightPacket {
                seq_nr: seq,
                payload_len: 4,
                data: pkt_data,
                sent_at: past, // Sent 5 seconds ago
                retransmits: 0,
            },
        );
        conn.seq_nr = conn.seq_nr.wrapping_add(1);

        // Check timeouts — RTO is 1000ms, packet was sent 5s ago
        let actions = conn.check_timeouts(Instant::now());

        // Should have a retransmission
        let sends: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, ConnAction::Send(_)))
            .collect();
        assert!(!sends.is_empty(), "should retransmit timed-out packet");
    }
}
