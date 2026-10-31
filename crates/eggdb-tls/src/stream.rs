//! A TLS connection over any byte stream, read and written like the stream itself.

use std::io::{Read, Write};

use crate::alert::{Alert, AlertDescription};
use crate::cipher_suite::version;
use crate::config::{ClientConfig, ServerConfig};
use crate::error::TlsError;
use crate::handshake::{
    HandshakeBuffer, decode_key_update, handshake_type, key_update_message, message_body,
};
use crate::key_schedule::{next_traffic_secret, traffic_keys};
use crate::private_key::FillRandom;
use crate::record::{MAX_PLAINTEXT_BYTES, Tls13RecordProtection, content_type};
use crate::record_layer::{Protection, RecordLayer, peer_alert};
use crate::session::{SessionInfo, Traffic};

const MAX_WRITE_BYTES: usize = 16 * MAX_PLAINTEXT_BYTES;

/// A TLS connection over `S`. Reads return the peer's application data and `Ok(0)` after its
/// `close_notify`; writes are sealed into records.
#[derive(Debug)]
pub struct TlsStream<S> {
    layer: RecordLayer<S>,
    info: SessionInfo,
    traffic: Traffic,
    is_client: bool,
    post_handshake: HandshakeBuffer,
    received: Vec<u8>,
    received_offset: usize,
    peer_closed: bool,
    sent_close: bool,
    failed: bool,
}

impl<S: Read + Write> TlsStream<S> {
    /// Runs the server side of the handshake on `stream`.
    pub fn accept(stream: S, config: &ServerConfig) -> Result<Self, TlsError> {
        Self::accept_with_random(stream, config, &mut eggdb_platform::secure_random::fill)
    }

    /// [`TlsStream::accept`] with the randomness taken from `fill_random`.
    pub fn accept_with_random(
        stream: S,
        config: &ServerConfig,
        fill_random: FillRandom<'_>,
    ) -> Result<Self, TlsError> {
        let mut layer = RecordLayer::new(stream, version::TLS12);
        match crate::server::handshake(&mut layer, config, fill_random) {
            Ok(established) => Ok(Self::established(layer, established, false)),
            Err(error) => Err(fail(&mut layer, error)),
        }
    }

    /// Runs the client side of the handshake on `stream`.
    pub fn connect(stream: S, config: &ClientConfig) -> Result<Self, TlsError> {
        Self::connect_with_random(stream, config, &mut eggdb_platform::secure_random::fill)
    }

    /// [`TlsStream::connect`] with the randomness taken from `fill_random`.
    pub fn connect_with_random(
        stream: S,
        config: &ClientConfig,
        fill_random: FillRandom<'_>,
    ) -> Result<Self, TlsError> {
        let mut layer = RecordLayer::new(stream, version::TLS12);
        match crate::client::handshake(&mut layer, config, fill_random) {
            Ok(established) => Ok(Self::established(layer, established, true)),
            Err(error) => Err(fail(&mut layer, error)),
        }
    }

    fn established(layer: RecordLayer<S>, established: crate::session::Established, is_client: bool) -> Self {
        Self {
            layer,
            info: established.info,
            traffic: established.traffic,
            is_client,
            post_handshake: HandshakeBuffer::new(),
            received: Vec::new(),
            received_offset: 0,
            peer_closed: false,
            sent_close: false,
            failed: false,
        }
    }

    /// What the handshake agreed on.
    pub fn info(&self) -> &SessionInfo {
        &self.info
    }

    /// The underlying stream.
    pub fn get_ref(&self) -> &S {
        self.layer.stream()
    }

    /// The underlying stream, mutably (for socket options; reading or writing it directly breaks
    /// the connection).
    pub fn get_mut(&mut self) -> &mut S {
        self.layer.stream_mut()
    }

    /// The underlying stream, without sending `close_notify`.
    pub fn into_inner(self) -> S {
        self.layer.into_stream()
    }

    /// Keying material exported for `label` and `context` (RFC 8446 section 7.5, RFC 5705).
    pub fn export_keying_material(&self, label: &[u8], context: Option<&[u8]>, length: usize) -> Vec<u8> {
        self.traffic.export(label, context, length)
    }

    /// Sends `close_notify`; the peer reads end-of-stream after the data sent before it.
    pub fn close(&mut self) -> std::io::Result<()> {
        if self.sent_close || self.failed {
            return Ok(());
        }
        self.sent_close = true;
        self.layer.queue(content_type::ALERT, &AlertDescription::CloseNotify.encode())?;
        self.layer.flush()?;
        Ok(())
    }

    /// Changes this side's TLS 1.3 write keys, and asks the peer to change its own when
    /// `request_peer_update` is set. TLS 1.2 has no key update; it is a no-op there.
    pub fn update_keys(&mut self, request_peer_update: bool) -> Result<(), TlsError> {
        let Traffic::Tls13(traffic) = &mut self.traffic else {
            return Ok(());
        };
        self.layer.queue(content_type::HANDSHAKE, &key_update_message(request_peer_update))?;
        traffic.write_secret = next_traffic_secret(traffic.hash, &traffic.write_secret);
        let keys = traffic_keys(traffic.hash, traffic.aead, &traffic.write_secret);
        let protection = Tls13RecordProtection::new(traffic.aead, &keys)
            .ok_or(TlsError::protocol(AlertDescription::InternalError, "traffic key of the wrong length"))?;
        self.layer.set_write_protection(Protection::Tls13(protection));
        self.layer.flush()
    }

    fn receive_key_update(&mut self, body: &[u8]) -> Result<(), TlsError> {
        let update_requested = decode_key_update(body)?;
        if !self.post_handshake.is_empty() {
            return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "a KeyUpdate was followed by part of a message"));
        }
        let Traffic::Tls13(traffic) = &mut self.traffic else {
            return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "KeyUpdate on a TLS 1.2 connection"));
        };
        traffic.read_secret = next_traffic_secret(traffic.hash, &traffic.read_secret);
        let keys = traffic_keys(traffic.hash, traffic.aead, &traffic.read_secret);
        let protection = Tls13RecordProtection::new(traffic.aead, &keys)
            .ok_or(TlsError::protocol(AlertDescription::InternalError, "traffic key of the wrong length"))?;
        self.layer.set_read_protection(Protection::Tls13(protection))?;
        if update_requested && !self.sent_close {
            self.update_keys(false)?;
        }
        Ok(())
    }

    fn receive_handshake(&mut self, fragment: &[u8]) -> Result<(), TlsError> {
        self.post_handshake.push(fragment)?;
        while let Some((message_type, message)) = self.post_handshake.next_message() {
            let tls13 = matches!(self.traffic, Traffic::Tls13(_));
            match message_type {
                handshake_type::NEW_SESSION_TICKET if tls13 && self.is_client => {}
                handshake_type::KEY_UPDATE if tls13 => self.receive_key_update(message_body(&message))?,
                0 if !tls13 && self.is_client && message_body(&message).is_empty() => {}
                _ => {
                    return Err(TlsError::protocol(
                        AlertDescription::UnexpectedMessage,
                        "a handshake message arrived after the handshake",
                    ));
                }
            }
        }
        Ok(())
    }

    fn fill_received(&mut self) -> Result<bool, TlsError> {
        loop {
            let Some((content, payload)) = self.layer.read_record()? else {
                return Err(TlsError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the peer closed the connection without close_notify",
                )));
            };
            match content {
                content_type::APPLICATION_DATA => {
                    if !self.post_handshake.is_empty() {
                        return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "application data inside a handshake message"));
                    }
                    if payload.is_empty() {
                        continue;
                    }
                    self.received = payload;
                    self.received_offset = 0;
                    return Ok(true);
                }
                content_type::ALERT => {
                    let alert = Alert::decode(&payload).map_err(|_| peer_alert(&payload))?;
                    if alert.is_close_notify() {
                        self.peer_closed = true;
                        return Ok(false);
                    }
                    if alert.level == crate::alert::level::WARNING && !matches!(self.traffic, Traffic::Tls13(_)) {
                        continue;
                    }
                    return Err(TlsError::PeerAlert(alert));
                }
                content_type::HANDSHAKE => self.receive_handshake(&payload)?,
                _ => {
                    return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "an unexpected record after the handshake"));
                }
            }
        }
    }

    fn fail_with(&mut self, error: TlsError) -> std::io::Error {
        self.failed = true;
        fail(&mut self.layer, error).into()
    }
}

fn fail<S: Read + Write>(layer: &mut RecordLayer<S>, error: TlsError) -> TlsError {
    if let Some(alert) = error.alert_to_send() {
        layer.send_alert(alert);
    }
    error
}

impl<S: Read + Write> Read for TlsStream<S> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.failed {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the TLS connection has failed"));
        }
        while self.received_offset >= self.received.len() {
            if self.peer_closed {
                return Ok(0);
            }
            match self.fill_received() {
                Ok(true) => {}
                Ok(false) => return Ok(0),
                Err(error) => return Err(self.fail_with(error)),
            }
        }
        let available = &self.received[self.received_offset..];
        let count = available.len().min(buffer.len());
        buffer[..count].copy_from_slice(&available[..count]);
        self.received_offset += count;
        Ok(count)
    }
}

impl<S: Read + Write> Write for TlsStream<S> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self.sent_close || self.failed {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the TLS connection is closed"));
        }
        if buffer.is_empty() {
            return Ok(0);
        }
        let count = buffer.len().min(MAX_WRITE_BYTES);
        if let Err(error) = self.layer.queue(content_type::APPLICATION_DATA, &buffer[..count]).and_then(|()| self.layer.flush()) {
            return Err(self.fail_with(error));
        }
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.layer.flush().map_err(std::io::Error::from)
    }
}

/// A stream whose first bytes were already read by the caller (for example the first byte of a
/// PostgreSQL direct TLS connection), handed back before the rest of `inner`.
#[derive(Debug)]
pub struct PrefixedStream<S> {
    prefix: Vec<u8>,
    offset: usize,
    inner: S,
}

impl<S> PrefixedStream<S> {
    /// `inner` with `prefix` read first.
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self { prefix, offset: 0, inner }
    }

    /// The wrapped stream.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// The wrapped stream, mutably.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.inner
    }
}

impl<S: Read> Read for PrefixedStream<S> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.offset < self.prefix.len() {
            let available = &self.prefix[self.offset..];
            let count = available.len().min(buffer.len());
            buffer[..count].copy_from_slice(&available[..count]);
            self.offset += count;
            return Ok(count);
        }
        self.inner.read(buffer)
    }
}

impl<S: Write> Write for PrefixedStream<S> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
