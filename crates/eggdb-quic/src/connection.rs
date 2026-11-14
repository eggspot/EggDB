//! A QUIC server connection without I/O: datagrams go in with [`ServerConnection::receive`] and
//! come out of [`ServerConnection::poll_transmit`], timers through [`ServerConnection::timeout`]
//! and [`ServerConnection::on_timeout`]. It runs the TLS handshake over CRYPTO frames at each
//! encryption level, acknowledges what it receives, retransmits what is lost, keeps NewReno's
//! congestion window, and carries client-opened bidirectional streams with flow control.

use crate::QuicCodecError;
use crate::frame::{AckRange, Frame};
use crate::packet::{Header, LongPacketKind, packet_number_length, write_long_header, write_short_header};
use crate::protection::{PacketKeys, initial_secrets};
use crate::recovery::{CongestionControl, MAX_DATAGRAM_BYTES, PACKET_THRESHOLD, RttEstimate};
use crate::transport_parameters::TransportParameters;
use eggdb_crypto::AEAD_TAG_BYTES;
use eggdb_tls::ServerConfig;
use eggdb_tls::quic::{EncryptionLevel, QuicHandshakeEvent, QuicServerHandshake};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// The length of the connection IDs this server issues.
pub const LOCAL_CONNECTION_ID_BYTES: usize = 8;
const LEVELS: [EncryptionLevel; 3] = [EncryptionLevel::Initial, EncryptionLevel::Handshake, EncryptionLevel::Application];
const MOST_RANGES_REMEMBERED: usize = 64;
const MOST_CRYPTO_GAP_BYTES: u64 = 64 * 1024;
const STREAM_WINDOW_BYTES: u64 = 1 << 20;
const CONNECTION_WINDOW_BYTES: u64 = 16 << 20;
const MOST_STREAMS: u64 = 100;
const MOST_UNIDIRECTIONAL_STREAMS: u64 = 3;
const UNIDIRECTIONAL_WINDOW_BYTES: u64 = 64 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ACK_DELAY: Duration = Duration::from_millis(25);
const MINIMUM_PAYLOAD_BYTES: usize = 4;
const LONG_HEADER_BUDGET_BYTES: usize = 1 + 4 + 1 + 20 + 1 + 20 + 1 + 2 + 4;

mod transport_error {
    pub(super) const FLOW_CONTROL: u64 = 0x03;
    pub(super) const STREAM_LIMIT: u64 = 0x04;
    pub(super) const STREAM_STATE: u64 = 0x05;
    pub(super) const FINAL_SIZE: u64 = 0x06;
    pub(super) const FRAME_ENCODING: u64 = 0x07;
    pub(super) const TRANSPORT_PARAMETER: u64 = 0x08;
    pub(super) const PROTOCOL_VIOLATION: u64 = 0x0a;
    pub(super) const CRYPTO_BUFFER_EXCEEDED: u64 = 0x0d;
    pub(super) const CRYPTO_ERROR: u64 = 0x0100;
}

/// Why a connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionClosed {
    /// The QUIC error code: a transport error, `0x100` plus a TLS alert, or an application's.
    pub error_code: u64,
    /// Whether the peer closed it.
    pub by_peer: bool,
    /// The reason given.
    pub reason: String,
}

#[derive(Default)]
struct SentPacket {
    time: Option<Instant>,
    bytes: usize,
    ack_eliciting: bool,
    frames: Vec<Frame>,
}

#[derive(Default)]
struct Space {
    keys_in: Option<PacketKeys>,
    keys_out: Option<PacketKeys>,
    next_packet_number: u64,
    largest_received: Option<u64>,
    received: Vec<AckRange>,
    ack_pending: bool,
    largest_acknowledged: Option<u64>,
    sent: BTreeMap<u64, SentPacket>,
    crypto_send_offset: u64,
    crypto_to_send: VecDeque<(u64, Vec<u8>)>,
    crypto_received_offset: u64,
    crypto_out_of_order: BTreeMap<u64, Vec<u8>>,
    retransmit: VecDeque<Frame>,
}

impl Space {
    fn record_received(&mut self, number: u64) -> bool {
        if self.received.iter().any(|range| range.smallest <= number && number <= range.largest) {
            return false;
        }
        self.received.push(AckRange { smallest: number, largest: number });
        self.received.sort_by(|left, right| right.largest.cmp(&left.largest));
        let mut merged: Vec<AckRange> = Vec::with_capacity(self.received.len());
        for range in self.received.drain(..) {
            match merged.last_mut() {
                Some(last) if range.largest + 1 >= last.smallest => last.smallest = last.smallest.min(range.smallest),
                _ => merged.push(range),
            }
        }
        merged.truncate(MOST_RANGES_REMEMBERED);
        self.received = merged;
        self.largest_received = Some(self.largest_received.map_or(number, |largest| largest.max(number)));
        true
    }

    fn has_data_to_send(&self) -> bool {
        self.keys_out.is_some() && (self.ack_pending || !self.crypto_to_send.is_empty() || !self.retransmit.is_empty())
    }
}

#[derive(Default)]
struct Stream {
    received: BTreeMap<u64, Vec<u8>>,
    received_contiguous: u64,
    readable: Vec<u8>,
    final_size: Option<u64>,
    read_offset: u64,
    receive_limit: u64,
    advertised_limit: u64,
    to_send: VecDeque<u8>,
    send_offset: u64,
    send_limit: u64,
    fin_queued: bool,
    fin_sent: bool,
}

/// A QUIC server connection.
pub struct ServerConnection<'c> {
    tls: QuicServerHandshake<'c>,
    spaces: [Space; 3],
    local_connection_id: Vec<u8>,
    peer_connection_id: Vec<u8>,
    peer_parameters: Option<TransportParameters>,
    application_protocol: Option<Vec<u8>>,
    complete: bool,
    handshake_done_to_send: bool,
    streams: BTreeMap<u64, Stream>,
    streams_opened: u64,
    stream_limit: u64,
    unidirectional_opened: u64,
    server_unidirectional_opened: u64,
    connection_receive_limit: u64,
    connection_received: u64,
    connection_read: u64,
    connection_advertised_limit: u64,
    connection_send_limit: u64,
    connection_sent: u64,
    rtt: RttEstimate,
    congestion: CongestionControl,
    probes: u32,
    probe_pending: bool,
    application_close: bool,
    last_ack_eliciting_sent: Option<Instant>,
    last_activity: Instant,
    closing: Option<ConnectionClosed>,
    close_sent: bool,
}

impl std::fmt::Debug for ServerConnection<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerConnection")
            .field("complete", &self.complete)
            .field("closing", &self.closing)
            .finish_non_exhaustive()
    }
}

fn level_index(level: EncryptionLevel) -> usize {
    match level {
        EncryptionLevel::Initial => 0,
        EncryptionLevel::Handshake => 1,
        EncryptionLevel::Application => 2,
    }
}

fn is_retransmittable(frame: &Frame) -> bool {
    !matches!(frame, Frame::Ack { .. } | Frame::Padding(_) | Frame::Ping | Frame::ConnectionClose { .. })
}

impl<'c> ServerConnection<'c> {
    /// Accepts the client's first datagram: it must be at least 1200 bytes and start with an
    /// Initial packet. `local_connection_id` is the ID this server picks for the connection
    /// ([`LOCAL_CONNECTION_ID_BYTES`] long, random). `config` names the ALPN protocols served.
    pub fn accept(
        config: &'c ServerConfig,
        datagram: &[u8],
        local_connection_id: Vec<u8>,
        now: Instant,
    ) -> Result<Self, QuicCodecError> {
        if datagram.len() < MAX_DATAGRAM_BYTES || local_connection_id.len() != LOCAL_CONNECTION_ID_BYTES {
            return Err(QuicCodecError::InvalidValue);
        }
        let Header::Long { kind: LongPacketKind::Initial, version: crate::QUIC_VERSION_1, destination, source, .. } =
            Header::parse(datagram, LOCAL_CONNECTION_ID_BYTES)?
        else {
            return Err(QuicCodecError::InvalidValue);
        };
        if destination.len() < 8 {
            return Err(QuicCodecError::InvalidValue);
        }
        let parameters = TransportParameters {
            original_destination_connection_id: Some(destination.clone()),
            max_idle_timeout_milliseconds: IDLE_TIMEOUT.as_millis() as u64,
            initial_max_data: CONNECTION_WINDOW_BYTES,
            initial_max_stream_data_bidi_remote: STREAM_WINDOW_BYTES,
            initial_max_stream_data_bidi_local: STREAM_WINDOW_BYTES,
            initial_max_streams_bidi: MOST_STREAMS,
            initial_max_streams_uni: MOST_UNIDIRECTIONAL_STREAMS,
            initial_max_stream_data_uni: UNIDIRECTIONAL_WINDOW_BYTES,
            max_ack_delay_milliseconds: MAX_ACK_DELAY.as_millis() as u64,
            disable_active_migration: true,
            initial_source_connection_id: Some(local_connection_id.clone()),
            ..TransportParameters::default()
        };
        let (client_secret, server_secret) = initial_secrets(&destination);
        let mut spaces: [Space; 3] = Default::default();
        spaces[0].keys_in = Some(PacketKeys::initial(&client_secret)?);
        spaces[0].keys_out = Some(PacketKeys::initial(&server_secret)?);
        let mut connection = Self {
            tls: QuicServerHandshake::new(config, parameters.encode()?),
            spaces,
            local_connection_id,
            peer_connection_id: source,
            peer_parameters: None,
            application_protocol: None,
            complete: false,
            handshake_done_to_send: false,
            streams: BTreeMap::new(),
            streams_opened: 0,
            stream_limit: MOST_STREAMS,
            unidirectional_opened: 0,
            server_unidirectional_opened: 0,
            connection_receive_limit: CONNECTION_WINDOW_BYTES,
            connection_received: 0,
            connection_read: 0,
            connection_advertised_limit: CONNECTION_WINDOW_BYTES,
            connection_send_limit: 0,
            connection_sent: 0,
            rtt: RttEstimate::default(),
            congestion: CongestionControl::default(),
            probes: 0,
            probe_pending: false,
            application_close: false,
            last_ack_eliciting_sent: None,
            last_activity: now,
            closing: None,
            close_sent: false,
        };
        connection.receive(datagram, now);
        Ok(connection)
    }

    /// The connection ID this server issued, which short headers carry.
    pub fn local_connection_id(&self) -> &[u8] {
        &self.local_connection_id
    }

    /// Whether the handshake completed.
    pub fn is_established(&self) -> bool {
        self.complete
    }

    /// The application protocol agreed with ALPN, once the handshake completed.
    pub fn application_protocol(&self) -> Option<&[u8]> {
        self.application_protocol.as_deref()
    }

    /// Why the connection ended, once it did.
    pub fn closed(&self) -> Option<&ConnectionClosed> {
        self.closing.as_ref()
    }

    fn close(&mut self, error_code: u64, reason: &str) {
        if self.closing.is_none() {
            self.closing = Some(ConnectionClosed { error_code, by_peer: false, reason: reason.to_owned() });
        }
    }

    /// Takes one datagram from the peer; every coalesced packet in it is processed.
    pub fn receive(&mut self, datagram: &[u8], now: Instant) {
        if self.closing.is_some() {
            return;
        }
        let mut rest = datagram;
        while !rest.is_empty() {
            let Ok(header) = Header::parse(rest, LOCAL_CONNECTION_ID_BYTES) else {
                return;
            };
            let (level, packet_length, offset) = match header {
                Header::Long { kind: LongPacketKind::Initial, packet_number_offset, remaining_length, .. } => {
                    (EncryptionLevel::Initial, packet_number_offset + remaining_length, packet_number_offset)
                }
                Header::Long { kind: LongPacketKind::Handshake, packet_number_offset, remaining_length, .. } => {
                    (EncryptionLevel::Handshake, packet_number_offset + remaining_length, packet_number_offset)
                }
                Header::Long { kind: LongPacketKind::ZeroRtt, packet_number_offset, remaining_length, .. } => {
                    let skipped = packet_number_offset + remaining_length;
                    rest = rest.get(skipped..).unwrap_or_default();
                    continue;
                }
                Header::Short { packet_number_offset, .. } => (EncryptionLevel::Application, rest.len(), packet_number_offset),
                _ => return,
            };
            let Some(packet) = rest.get(..packet_length) else {
                return;
            };
            self.receive_packet(level, packet, offset, now);
            rest = rest.get(packet_length..).unwrap_or_default();
        }
    }

    fn receive_packet(&mut self, level: EncryptionLevel, packet: &[u8], offset: usize, now: Instant) {
        let index = level_index(level);
        let space = &mut self.spaces[index];
        let Some(keys) = &space.keys_in else {
            return;
        };
        let mut bytes = packet.to_vec();
        let Ok((number, payload)) = keys.unprotect(&mut bytes, offset, space.largest_received) else {
            return;
        };
        if !space.record_received(number) {
            return;
        }
        self.last_activity = now;
        if level == EncryptionLevel::Handshake {
            self.discard_keys(EncryptionLevel::Initial);
        }
        let frames = match Frame::parse_all(&bytes[payload]) {
            Ok(frames) => frames,
            Err(_) => return self.close(transport_error::FRAME_ENCODING, "a frame could not be read"),
        };
        if frames.iter().any(Frame::is_ack_eliciting) {
            self.spaces[index].ack_pending = true;
        }
        for frame in frames {
            if self.closing.is_some() {
                return;
            }
            self.receive_frame(level, frame, now);
        }
    }

    fn discard_keys(&mut self, level: EncryptionLevel) {
        let space = &mut self.spaces[level_index(level)];
        if space.keys_out.is_none() && space.keys_in.is_none() {
            return;
        }
        space.keys_in = None;
        space.keys_out = None;
        space.crypto_to_send.clear();
        space.retransmit.clear();
        space.ack_pending = false;
        let discarded: usize = space.sent.values().filter(|sent| sent.ack_eliciting).map(|sent| sent.bytes).sum();
        space.sent.clear();
        self.congestion.on_discarded(discarded);
    }

    fn receive_frame(&mut self, level: EncryptionLevel, frame: Frame, now: Instant) {
        let index = level_index(level);
        match frame {
            Frame::Padding(_) | Frame::Ping => {}
            Frame::Ack { ranges, delay, .. } => self.on_ack(index, &ranges, delay, now),
            Frame::Crypto { offset, data } => self.on_crypto(level, offset, data),
            Frame::ConnectionClose { error_code, reason, .. } => {
                self.closing = Some(ConnectionClosed {
                    error_code,
                    by_peer: true,
                    reason: String::from_utf8_lossy(&reason).into_owned(),
                });
                self.close_sent = true;
            }
            _ if level != EncryptionLevel::Application => {
                self.close(transport_error::PROTOCOL_VIOLATION, "a frame not allowed before 1-RTT");
            }
            Frame::Stream { stream_id, offset, data, fin } => self.on_stream(stream_id, offset, data, fin),
            Frame::MaxData(maximum) => self.connection_send_limit = self.connection_send_limit.max(maximum),
            Frame::MaxStreamData { stream_id, maximum } => {
                if let Some(stream) = self.streams.get_mut(&stream_id) {
                    stream.send_limit = stream.send_limit.max(maximum);
                }
            }
            Frame::ResetStream { stream_id, final_size, .. } => {
                if let Some(stream) = self.streams.get_mut(&stream_id) {
                    stream.final_size = Some(final_size);
                    stream.received.clear();
                }
            }
            Frame::StopSending { stream_id, .. } => {
                if let Some(stream) = self.streams.get_mut(&stream_id) {
                    stream.to_send.clear();
                    stream.fin_queued = true;
                }
            }
            Frame::NewToken(_) | Frame::HandshakeDone => {
                self.close(transport_error::PROTOCOL_VIOLATION, "a frame only a server sends");
            }
            Frame::PathChallenge(data) => self.spaces[2].retransmit.push_back(Frame::PathResponse(data)),
            Frame::MaxStreams { .. }
            | Frame::DataBlocked(_)
            | Frame::StreamDataBlocked { .. }
            | Frame::StreamsBlocked { .. }
            | Frame::NewConnectionId { .. }
            | Frame::RetireConnectionId(_)
            | Frame::PathResponse(_) => {}
        }
    }

    fn on_ack(&mut self, index: usize, ranges: &[AckRange], delay: u64, now: Instant) {
        let exponent = self.peer_parameters.as_ref().map_or(3, |parameters| parameters.ack_delay_exponent);
        let ack_delay = Duration::from_micros(delay.saturating_mul(1 << exponent.min(20)));
        let Some(largest) = ranges.first().map(|range| range.largest) else {
            return;
        };
        if largest >= self.spaces[index].next_packet_number {
            return self.close(transport_error::PROTOCOL_VIOLATION, "an ACK of a packet never sent");
        }
        let space = &mut self.spaces[index];
        let acknowledged: Vec<u64> = space
            .sent
            .keys()
            .copied()
            .filter(|number| ranges.iter().any(|range| range.smallest <= *number && *number <= range.largest))
            .collect();
        let newly_largest = acknowledged.last().copied();
        let mut largest_sent_at = None;
        for number in &acknowledged {
            if let Some(sent) = space.sent.remove(number) {
                if Some(*number) == newly_largest {
                    largest_sent_at = sent.time;
                }
                if sent.ack_eliciting
                    && let Some(time) = sent.time
                {
                    self.congestion.on_acknowledged(sent.bytes, time);
                }
            }
        }
        space.largest_acknowledged = Some(space.largest_acknowledged.map_or(largest, |known| known.max(largest)));
        if newly_largest == Some(largest)
            && let Some(sent_at) = largest_sent_at
        {
            self.rtt.update(now.saturating_duration_since(sent_at), ack_delay, MAX_ACK_DELAY);
        }
        if !acknowledged.is_empty() {
            self.probes = 0;
        }
        self.detect_losses(index, now);
    }

    fn detect_losses(&mut self, index: usize, now: Instant) {
        let loss_delay = self.rtt.loss_delay();
        let space = &mut self.spaces[index];
        let Some(largest) = space.largest_acknowledged else {
            return;
        };
        let lost: Vec<u64> = space
            .sent
            .iter()
            .filter(|(number, sent)| {
                **number < largest
                    && (largest - **number >= PACKET_THRESHOLD
                        || sent.time.is_some_and(|time| now.saturating_duration_since(time) >= loss_delay))
            })
            .map(|(number, _)| *number)
            .collect();
        let mut lost_bytes = 0;
        let mut latest_lost = None;
        for number in lost {
            if let Some(sent) = space.sent.remove(&number) {
                if sent.ack_eliciting {
                    lost_bytes += sent.bytes;
                    latest_lost = sent.time.max(latest_lost);
                }
                space.retransmit.extend(sent.frames);
            }
        }
        if let Some(latest) = latest_lost {
            self.congestion.on_lost(lost_bytes, latest, now);
        }
    }

    fn on_crypto(&mut self, level: EncryptionLevel, offset: u64, data: Vec<u8>) {
        let index = level_index(level);
        let received = self.spaces[index].crypto_received_offset;
        if offset.saturating_add(data.len() as u64) <= received {
            return;
        }
        if offset > received.saturating_add(MOST_CRYPTO_GAP_BYTES) {
            return self.close(transport_error::CRYPTO_BUFFER_EXCEEDED, "CRYPTO data too far ahead");
        }
        let space = &mut self.spaces[index];
        space.crypto_out_of_order.insert(offset, data);
        let mut ready = Vec::new();
        while let Some((&start, _)) = space.crypto_out_of_order.first_key_value() {
            if start > space.crypto_received_offset {
                break;
            }
            let Some((start, piece)) = space.crypto_out_of_order.pop_first() else {
                break;
            };
            let skip = (space.crypto_received_offset - start) as usize;
            if skip < piece.len() {
                ready.extend_from_slice(&piece[skip..]);
                space.crypto_received_offset = start + piece.len() as u64;
            }
        }
        if ready.is_empty() {
            return;
        }
        let events = match self.tls.receive(level, &ready, &mut eggdb_platform::secure_random::fill) {
            Ok(events) => events,
            Err(error) => {
                let alert = error.alert_to_send().map_or(80, |alert| alert as u8);
                return self.close(transport_error::CRYPTO_ERROR + u64::from(alert), &error.to_string());
            }
        };
        for event in events {
            self.on_handshake_event(event);
        }
    }

    fn on_handshake_event(&mut self, event: QuicHandshakeEvent) {
        match event {
            QuicHandshakeEvent::Send { level, data } => {
                let space = &mut self.spaces[level_index(level)];
                let offset = space.crypto_send_offset;
                space.crypto_send_offset += data.len() as u64;
                space.crypto_to_send.push_back((offset, data));
            }
            QuicHandshakeEvent::Secrets { level, client, server } => {
                let Some(suite) = self.tls.suite() else {
                    return;
                };
                let space = &mut self.spaces[level_index(level)];
                match (
                    PacketKeys::new(suite.hash(), suite.aead(), &client),
                    PacketKeys::new(suite.hash(), suite.aead(), &server),
                ) {
                    (Ok(keys_in), Ok(keys_out)) => {
                        space.keys_in = Some(keys_in);
                        space.keys_out = Some(keys_out);
                    }
                    _ => self.close(transport_error::CRYPTO_ERROR + 80, "the packet keys could not be derived"),
                }
            }
            QuicHandshakeEvent::Complete { application_protocol, peer_transport_parameters, .. } => {
                match TransportParameters::decode(&peer_transport_parameters, false) {
                    Ok(parameters) if parameters.initial_source_connection_id.as_deref() == Some(&self.peer_connection_id[..]) => {
                        self.connection_send_limit = parameters.initial_max_data;
                        for (stream_id, stream) in &mut self.streams {
                            if stream_id & 0x03 == 0 {
                                stream.send_limit = stream.send_limit.max(parameters.initial_max_stream_data_bidi_local);
                            }
                        }
                        self.peer_parameters = Some(parameters);
                    }
                    _ => return self.close(transport_error::TRANSPORT_PARAMETER, "the client's transport parameters are not valid"),
                }
                self.application_protocol = Some(application_protocol);
                self.complete = true;
                self.handshake_done_to_send = true;
                self.discard_keys(EncryptionLevel::Handshake);
            }
        }
    }

    fn on_stream(&mut self, stream_id: u64, offset: u64, data: Vec<u8>, fin: bool) {
        let unidirectional = match stream_id & 0x03 {
            0x00 => false,
            0x02 => true,
            _ => return self.close(transport_error::STREAM_STATE, "data on a stream only this server sends on"),
        };
        if !self.streams.contains_key(&stream_id) {
            let sequence = stream_id / 4;
            let (limit, opened_so_far, window, send_limit) = if unidirectional {
                (MOST_UNIDIRECTIONAL_STREAMS, self.unidirectional_opened, UNIDIRECTIONAL_WINDOW_BYTES, 0)
            } else {
                let send_limit =
                    self.peer_parameters.as_ref().map_or(0, |parameters| parameters.initial_max_stream_data_bidi_local);
                (self.stream_limit, self.streams_opened, STREAM_WINDOW_BYTES, send_limit)
            };
            if sequence >= limit {
                return self.close(transport_error::STREAM_LIMIT, "more streams than allowed");
            }
            for opened in opened_so_far..=sequence {
                self.streams.insert(
                    opened * 4 + (stream_id & 0x03),
                    Stream {
                        receive_limit: window,
                        advertised_limit: window,
                        send_limit,
                        ..Stream::default()
                    },
                );
            }
            if unidirectional {
                self.unidirectional_opened = self.unidirectional_opened.max(sequence + 1);
            } else {
                self.streams_opened = self.streams_opened.max(sequence + 1);
            }
        }
        let Some(stream) = self.streams.get_mut(&stream_id) else {
            return;
        };
        let end = offset.saturating_add(data.len() as u64);
        if end > stream.receive_limit {
            return self.close(transport_error::FLOW_CONTROL, "a stream passed its flow control limit");
        }
        if let Some(final_size) = stream.final_size
            && (end > final_size || (fin && end != final_size))
        {
            return self.close(transport_error::FINAL_SIZE, "data past a stream's final size");
        }
        if fin {
            stream.final_size = Some(end);
        }
        let before = stream.received_contiguous;
        if end > stream.received_contiguous {
            stream.received.insert(offset, data);
        }
        while let Some((&start, _)) = stream.received.first_key_value() {
            if start > stream.received_contiguous {
                break;
            }
            let Some((start, piece)) = stream.received.pop_first() else {
                break;
            };
            let skip = (stream.received_contiguous - start) as usize;
            if skip < piece.len() {
                stream.readable.extend_from_slice(&piece[skip..]);
                stream.received_contiguous = start + piece.len() as u64;
            }
        }
        let grown = stream.received_contiguous - before;
        self.connection_received = self.connection_received.saturating_add(grown);
        if self.connection_received > self.connection_receive_limit {
            self.close(transport_error::FLOW_CONTROL, "the connection passed its flow control limit");
        }
    }

    /// The client-opened streams that have data or an end to read.
    pub fn readable_streams(&self) -> Vec<u64> {
        self.streams
            .iter()
            .filter(|(_, stream)| !stream.readable.is_empty() || stream.final_size == Some(stream.read_offset))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Takes what stream `stream_id` received in order, and whether the client ended it there.
    pub fn read_stream(&mut self, stream_id: u64) -> Option<(Vec<u8>, bool)> {
        let stream = self.streams.get_mut(&stream_id)?;
        let data = std::mem::take(&mut stream.readable);
        stream.read_offset += data.len() as u64;
        self.connection_read += data.len() as u64;
        if stream.read_offset + STREAM_WINDOW_BYTES / 2 > stream.advertised_limit && stream.final_size.is_none() {
            stream.receive_limit = stream.read_offset + STREAM_WINDOW_BYTES;
            stream.advertised_limit = stream.receive_limit;
            self.spaces[2].retransmit.push_back(Frame::MaxStreamData { stream_id, maximum: stream.receive_limit });
        }
        if self.connection_read + CONNECTION_WINDOW_BYTES / 2 > self.connection_advertised_limit {
            self.connection_receive_limit = self.connection_read + CONNECTION_WINDOW_BYTES;
            self.connection_advertised_limit = self.connection_receive_limit;
            self.spaces[2].retransmit.push_back(Frame::MaxData(self.connection_receive_limit));
        }
        let ended = stream.final_size == Some(stream.read_offset);
        Some((data, ended))
    }

    /// Queues `data` on stream `stream_id`, ending it after when `fin`; sent as flow control allows.
    pub fn write_stream(&mut self, stream_id: u64, data: &[u8], fin: bool) -> bool {
        let Some(stream) = self.streams.get_mut(&stream_id) else {
            return false;
        };
        if stream.fin_queued {
            return false;
        }
        stream.to_send.extend(data);
        stream.fin_queued = fin;
        true
    }

    /// Opens a stream only this server sends on (RFC 9000 section 2.1: IDs 3, 7, 11 and so on)
    /// within the client's limit of unidirectional streams, or `None` when the limit is reached
    /// or the handshake has not completed.
    pub fn open_unidirectional_stream(&mut self) -> Option<u64> {
        let parameters = self.peer_parameters.as_ref()?;
        if self.server_unidirectional_opened >= parameters.initial_max_streams_uni {
            return None;
        }
        let stream_id = self.server_unidirectional_opened * 4 + 3;
        self.server_unidirectional_opened += 1;
        self.streams.insert(
            stream_id,
            Stream {
                send_limit: parameters.initial_max_stream_data_uni,
                ..Stream::default()
            },
        );
        Some(stream_id)
    }

    /// Closes the connection with an application error code.
    pub fn close_with(&mut self, error_code: u64, reason: &str) {
        if self.closing.is_none() {
            self.application_close = true;
        }
        self.close(error_code, reason);
    }

    fn stream_frames(&mut self, budget: usize, frames: &mut Vec<Frame>, used: &mut usize) {
        let ids: Vec<u64> = self.streams.keys().copied().collect();
        for stream_id in ids {
            let connection_room = self.connection_send_limit.saturating_sub(self.connection_sent);
            let Some(stream) = self.streams.get_mut(&stream_id) else {
                continue;
            };
            let pending_fin = stream.fin_queued && !stream.fin_sent;
            if stream.to_send.is_empty() && !pending_fin {
                continue;
            }
            let room = budget.saturating_sub(*used).saturating_sub(24);
            let stream_room = stream.send_limit.saturating_sub(stream.send_offset);
            let take = (stream.to_send.len() as u64).min(stream_room).min(connection_room).min(room as u64) as usize;
            if take == 0 && !(pending_fin && stream.to_send.is_empty()) {
                continue;
            }
            let data: Vec<u8> = stream.to_send.drain(..take).collect();
            let fin = stream.fin_queued && stream.to_send.is_empty();
            let frame = Frame::Stream { stream_id, offset: stream.send_offset, data, fin };
            stream.send_offset += take as u64;
            stream.fin_sent = fin;
            self.connection_sent += take as u64;
            *used += 24 + take;
            frames.push(frame);
        }
    }

    fn ack_frame(space: &mut Space) -> Option<Frame> {
        if !space.ack_pending || space.received.is_empty() {
            return None;
        }
        space.ack_pending = false;
        Some(Frame::Ack { ranges: space.received.clone(), delay: 0, ecn: None })
    }

    fn packet_frames(&mut self, index: usize, budget: usize) -> (Vec<Frame>, bool) {
        let mut frames = Vec::new();
        let mut used = 0usize;
        if let Some(close) = self.closing.as_ref().filter(|_| !self.close_sent) {
            let reason = close.reason.as_bytes();
            frames.push(Frame::ConnectionClose {
                error_code: close.error_code,
                frame_type: (!self.application_close).then_some(0),
                reason: reason[..reason.len().min(256)].to_vec(),
            });
            return (frames, false);
        }
        if let Some(ack) = Self::ack_frame(&mut self.spaces[index]) {
            used += 16 * ranges_count(&ack) + 16;
            frames.push(ack);
        }
        let may_elicit = self.congestion.may_send(MAX_DATAGRAM_BYTES);
        if may_elicit {
            while used + 64 < budget {
                let Some(frame) = self.spaces[index].retransmit.pop_front() else {
                    break;
                };
                used += encoded_size(&frame);
                frames.push(frame);
            }
            while used + 32 < budget {
                let Some((offset, mut data)) = self.spaces[index].crypto_to_send.pop_front() else {
                    break;
                };
                let room = budget - used - 16;
                if data.len() > room {
                    let rest = data.split_off(room);
                    self.spaces[index].crypto_to_send.push_front((offset + room as u64, rest));
                }
                used += 16 + data.len();
                frames.push(Frame::Crypto { offset, data });
            }
            if index == 2 {
                if self.handshake_done_to_send {
                    self.handshake_done_to_send = false;
                    frames.push(Frame::HandshakeDone);
                    used += 1;
                }
                self.stream_frames(budget, &mut frames, &mut used);
            }
            if self.probe_pending && !frames.iter().any(Frame::is_ack_eliciting) && self.spaces[index].keys_out.is_some() {
                frames.push(Frame::Ping);
            }
        }
        let eliciting = frames.iter().any(Frame::is_ack_eliciting);
        (frames, eliciting)
    }

    /// The next datagram to send, or `None` when nothing is due: coalesced Initial, Handshake
    /// and 1-RTT packets within 1200 bytes, padded to 1200 when it holds an ack-eliciting Initial.
    pub fn poll_transmit(&mut self, now: Instant) -> Option<Vec<u8>> {
        if self.close_sent {
            return None;
        }
        let mut planned: Vec<(usize, Vec<u8>, u64, usize, bool, Vec<Frame>)> = Vec::new();
        let mut room = MAX_DATAGRAM_BYTES;
        for level in LEVELS {
            let index = level_index(level);
            let wants = self.spaces[index].has_data_to_send()
                || (index == 2 && self.complete && (self.handshake_done_to_send || self.streams_have_data()))
                || (self.closing.is_some() && self.spaces[index].keys_out.is_some())
                || (self.probe_pending && self.spaces[index].keys_out.is_some());
            if !wants || room <= LONG_HEADER_BUDGET_BYTES + AEAD_TAG_BYTES + MINIMUM_PAYLOAD_BYTES {
                continue;
            }
            let budget = room - LONG_HEADER_BUDGET_BYTES - AEAD_TAG_BYTES;
            let (frames, eliciting) = self.packet_frames(index, budget);
            if frames.is_empty() {
                continue;
            }
            let mut payload = Vec::new();
            for frame in &frames {
                if frame.write(&mut payload).is_err() {
                    return None;
                }
            }
            let space = &mut self.spaces[index];
            let number = space.next_packet_number;
            space.next_packet_number += 1;
            let length = packet_number_length(number, space.largest_acknowledged);
            room = room.saturating_sub(LONG_HEADER_BUDGET_BYTES + payload.len() + AEAD_TAG_BYTES);
            let kept: Vec<Frame> = frames.into_iter().filter(is_retransmittable).collect();
            planned.push((index, payload, number, length, eliciting, kept));
            if self.closing.is_some() {
                break;
            }
        }
        if planned.is_empty() {
            return None;
        }
        let padded_initial = planned.iter().any(|(index, _, _, _, eliciting, _)| *index == 0 && *eliciting);
        let mut datagram = Vec::with_capacity(MAX_DATAGRAM_BYTES);
        let last = planned.len() - 1;
        for (position, (index, mut payload, number, length, eliciting, kept)) in planned.into_iter().enumerate() {
            if payload.len() + length < MINIMUM_PAYLOAD_BYTES {
                payload.resize(MINIMUM_PAYLOAD_BYTES - length, 0);
            }
            if position == last && padded_initial {
                let header_estimate = if index == 2 { 1 + self.peer_connection_id.len() } else { LONG_HEADER_BUDGET_BYTES };
                let total = datagram.len() + header_estimate + length + payload.len() + AEAD_TAG_BYTES;
                if total < MAX_DATAGRAM_BYTES {
                    payload.resize(payload.len() + MAX_DATAGRAM_BYTES - total, 0);
                }
            }
            let mut packet = Vec::new();
            let written = match index {
                0 | 1 => write_long_header(
                    &mut packet,
                    if index == 0 { LongPacketKind::Initial } else { LongPacketKind::Handshake },
                    &self.peer_connection_id,
                    &self.local_connection_id,
                    &[],
                    length,
                    length + payload.len() + AEAD_TAG_BYTES,
                ),
                _ => write_short_header(&mut packet, &self.peer_connection_id, false, length),
            };
            if written.is_err() {
                return None;
            }
            let offset = packet.len();
            packet.extend_from_slice(&number.to_be_bytes()[8 - length..]);
            packet.extend_from_slice(&payload);
            let Some(keys) = &self.spaces[index].keys_out else {
                continue;
            };
            if keys.protect(&mut packet, offset, length, number).is_err() {
                return None;
            }
            if eliciting {
                self.congestion.on_sent(packet.len());
                self.last_ack_eliciting_sent = Some(now);
            }
            self.spaces[index].sent.insert(
                number,
                SentPacket { time: Some(now), bytes: packet.len(), ack_eliciting: eliciting, frames: kept },
            );
            datagram.extend_from_slice(&packet);
        }
        if self.closing.is_some() {
            self.close_sent = true;
        }
        self.probe_pending = false;
        Some(datagram)
    }

    fn streams_have_data(&self) -> bool {
        self.streams.values().any(|stream| !stream.to_send.is_empty() || (stream.fin_queued && !stream.fin_sent))
    }

    /// When [`ServerConnection::on_timeout`] must next run: the probe timeout of packets in
    /// flight, or the idle timeout.
    pub fn timeout(&self) -> Instant {
        let idle = self.last_activity + IDLE_TIMEOUT;
        match self.last_ack_eliciting_sent {
            Some(sent) if self.congestion.in_flight() > 0 => {
                let probe = self.rtt.probe_timeout(MAX_ACK_DELAY) * 2u32.saturating_pow(self.probes.min(16));
                (sent + probe).min(idle)
            }
            _ => idle,
        }
    }

    /// Runs the timers due at `now`: an idle connection closes; otherwise unacknowledged packets
    /// get a probe and their frames are sent again.
    pub fn on_timeout(&mut self, now: Instant) {
        if now >= self.last_activity + IDLE_TIMEOUT {
            self.closing = Some(ConnectionClosed { error_code: 0, by_peer: false, reason: "idle".to_owned() });
            self.close_sent = true;
            return;
        }
        if now < self.timeout() {
            return;
        }
        self.probes = self.probes.saturating_add(1);
        self.probe_pending = true;
        for space in &mut self.spaces {
            if space.keys_out.is_none() {
                continue;
            }
            let frames: Vec<Frame> = space.sent.values_mut().flat_map(|sent| std::mem::take(&mut sent.frames)).collect();
            space.retransmit.extend(frames);
        }
    }
}

fn ranges_count(frame: &Frame) -> usize {
    match frame {
        Frame::Ack { ranges, .. } => ranges.len(),
        _ => 0,
    }
}

fn encoded_size(frame: &Frame) -> usize {
    let mut out = Vec::new();
    frame.write(&mut out).map_or(64, |()| out.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn received_packet_numbers_merge_into_ranges_and_duplicates_are_refused() {
        let mut space = Space::default();
        for number in [1, 2, 3, 7, 5, 6] {
            assert!(space.record_received(number));
        }
        assert!(!space.record_received(2));
        assert_eq!(
            space.received,
            vec![AckRange { smallest: 5, largest: 7 }, AckRange { smallest: 1, largest: 3 }]
        );
        assert_eq!(space.largest_received, Some(7));
    }

    #[test]
    fn a_datagram_too_short_for_a_client_initial_is_refused() {
        let request = eggdb_tls::self_signed::SelfSignedCertificateRequest {
            common_name: "quic.example".to_owned(),
            subject_alternative_names: vec![eggdb_tls::x509::SubjectAlternativeName::DnsName("quic.example".to_owned())],
            not_before_unix_seconds: 0,
            not_after_unix_seconds: 4_000_000_000,
        };
        let (certificate, key) =
            eggdb_tls::self_signed::generate_self_signed_certificate(&request, &mut eggdb_platform::secure_random::fill)
                .expect("certificate");
        let config = ServerConfig::new(vec![certificate], key).expect("config");
        assert!(ServerConnection::accept(&config, &[0xc0; 100], vec![1; 8], Instant::now()).is_err());
    }
}
