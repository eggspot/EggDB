//! Records over a byte stream: reading and opening one record at a time, sealing and writing
//! records, reassembling handshake messages, and sending alerts.

use std::io::{Read, Write};

use crate::alert::{Alert, AlertDescription};
use crate::cipher_suite::TranscriptHash;
use crate::error::TlsError;
use crate::handshake::HandshakeBuffer;
use crate::record::{
    MAX_PLAINTEXT_BYTES, MAX_TLS12_CIPHERTEXT_BYTES, MAX_TLS13_CIPHERTEXT_BYTES,
    RECORD_HEADER_BYTES, RecordHeader, Tls12RecordProtection, Tls13RecordProtection,
    content_type, plaintext_record,
};

/// How records in one direction are protected.
#[derive(Debug)]
pub(crate) enum Protection {
    Plaintext,
    Tls13(Tls13RecordProtection),
    Tls12(Tls12RecordProtection),
}

/// A record layer over a stream.
#[derive(Debug)]
pub(crate) struct RecordLayer<S> {
    stream: S,
    read: Protection,
    write: Protection,
    handshake: HandshakeBuffer,
    incoming: Vec<u8>,
    pending_output: Vec<u8>,
    record_version: u16,
    pub(crate) ignore_change_cipher_spec: bool,
}

impl<S: Read + Write> RecordLayer<S> {
    pub(crate) fn new(stream: S, record_version: u16) -> Self {
        Self {
            stream,
            read: Protection::Plaintext,
            write: Protection::Plaintext,
            handshake: HandshakeBuffer::new(),
            incoming: Vec::new(),
            pending_output: Vec::new(),
            record_version,
            ignore_change_cipher_spec: false,
        }
    }

    pub(crate) fn stream(&self) -> &S {
        &self.stream
    }

    pub(crate) fn stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    pub(crate) fn into_stream(self) -> S {
        self.stream
    }

    pub(crate) fn set_record_version(&mut self, version: u16) {
        self.record_version = version;
    }

    pub(crate) fn set_read_protection(&mut self, protection: Protection) -> Result<(), TlsError> {
        if !self.handshake.is_empty() {
            return Err(TlsError::protocol(
                AlertDescription::UnexpectedMessage,
                "a handshake message was split across a key change",
            ));
        }
        self.read = protection;
        Ok(())
    }

    pub(crate) fn set_write_protection(&mut self, protection: Protection) {
        self.write = protection;
    }

    pub(crate) fn handshake_buffer_is_empty(&self) -> bool {
        self.handshake.is_empty()
    }

    fn fill_incoming(&mut self, wanted: usize) -> Result<bool, TlsError> {
        let mut chunk = [0u8; 4096];
        while self.incoming.len() < wanted {
            let room = (wanted - self.incoming.len()).max(RECORD_HEADER_BYTES).min(chunk.len());
            match self.stream.read(&mut chunk[..room]) {
                Ok(0) if self.incoming.is_empty() => return Ok(false),
                Ok(0) => {
                    return Err(TlsError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "the peer closed the connection inside a TLS record",
                    )));
                }
                Ok(read) => self.incoming.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(TlsError::Io(error)),
            }
        }
        Ok(true)
    }

    /// The next record's content type and plaintext, or `None` when the peer closed the stream
    /// between records. A read timeout of the stream comes back as an I/O error with the record
    /// read so far kept, so the call can be repeated.
    pub(crate) fn read_record(&mut self) -> Result<Option<(u8, Vec<u8>)>, TlsError> {
        if !self.fill_incoming(RECORD_HEADER_BYTES)? {
            return Ok(None);
        }
        let limit = match self.read {
            Protection::Plaintext => MAX_PLAINTEXT_BYTES,
            Protection::Tls13(_) => MAX_TLS13_CIPHERTEXT_BYTES,
            Protection::Tls12(_) => MAX_TLS12_CIPHERTEXT_BYTES,
        };
        let mut header_bytes = [0u8; RECORD_HEADER_BYTES];
        header_bytes.copy_from_slice(&self.incoming[..RECORD_HEADER_BYTES]);
        let header = RecordHeader::parse(&header_bytes, limit)?;
        let total = RECORD_HEADER_BYTES + header.length;
        if !self.fill_incoming(total)? {
            return Err(TlsError::Io(std::io::ErrorKind::UnexpectedEof.into()));
        }
        let mut payload: Vec<u8> = self.incoming.drain(..total).skip(RECORD_HEADER_BYTES).collect();
        let content = match &mut self.read {
            Protection::Plaintext => {
                if header.content_type == content_type::APPLICATION_DATA {
                    return Err(TlsError::protocol(
                        AlertDescription::UnexpectedMessage,
                        "application data arrived before the handshake finished",
                    ));
                }
                header.content_type
            }
            Protection::Tls13(protection) => {
                let unprotected_during_handshake = self.ignore_change_cipher_spec
                    && header.content_type == content_type::ALERT;
                if header.content_type == content_type::CHANGE_CIPHER_SPEC || unprotected_during_handshake {
                    header.content_type
                } else {
                    protection.open(&header, &mut payload)?
                }
            }
            Protection::Tls12(protection) => {
                protection.open(&header, &mut payload)?;
                header.content_type
            }
        };
        if content == content_type::HANDSHAKE && payload.is_empty() {
            return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "an empty handshake record"));
        }
        Ok(Some((content, payload)))
    }

    /// The next whole handshake message, header included, added to `transcript` when given.
    /// TLS 1.3's compatibility ChangeCipherSpec records are dropped while
    /// `ignore_change_cipher_spec` is set.
    pub(crate) fn read_handshake_message(
        &mut self,
        transcript: Option<&mut TranscriptHash>,
    ) -> Result<(u8, Vec<u8>), TlsError> {
        loop {
            if let Some((message_type, message)) = self.handshake.next_message() {
                if let Some(transcript) = transcript {
                    transcript.update(&message);
                }
                return Ok((message_type, message));
            }
            let Some((content, payload)) = self.read_record()? else {
                return Err(TlsError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the peer closed the connection during the TLS handshake",
                )));
            };
            match content {
                content_type::HANDSHAKE => self.handshake.push(&payload)?,
                content_type::CHANGE_CIPHER_SPEC if self.ignore_change_cipher_spec && payload == [1] => {}
                content_type::ALERT => return Err(peer_alert(&payload)),
                _ => {
                    return Err(TlsError::protocol(
                        AlertDescription::UnexpectedMessage,
                        "a record of the wrong type arrived during the handshake",
                    ));
                }
            }
        }
    }

    /// Reads the TLS 1.2 ChangeCipherSpec that must come next.
    pub(crate) fn read_change_cipher_spec(&mut self) -> Result<(), TlsError> {
        if !self.handshake.is_empty() {
            return Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "ChangeCipherSpec inside a handshake message"));
        }
        match self.read_record()? {
            Some((content_type::CHANGE_CIPHER_SPEC, payload)) if payload == [1] => Ok(()),
            Some((content_type::ALERT, payload)) => Err(peer_alert(&payload)),
            Some(_) => Err(TlsError::protocol(AlertDescription::UnexpectedMessage, "expected ChangeCipherSpec")),
            None => Err(TlsError::Io(std::io::ErrorKind::UnexpectedEof.into())),
        }
    }

    /// Queues `data` as records of `content`, split at the record size limit.
    pub(crate) fn queue(&mut self, content: u8, data: &[u8]) -> Result<(), TlsError> {
        if data.is_empty() {
            return self.queue_fragment(content, &[]);
        }
        for fragment in data.chunks(MAX_PLAINTEXT_BYTES) {
            self.queue_fragment(content, fragment)?;
        }
        Ok(())
    }

    fn queue_fragment(&mut self, content: u8, fragment: &[u8]) -> Result<(), TlsError> {
        let record = match &mut self.write {
            Protection::Plaintext => plaintext_record(content, self.record_version, fragment),
            Protection::Tls13(protection) => protection.seal(content, fragment)?,
            Protection::Tls12(protection) => protection.seal(content, fragment)?,
        };
        self.pending_output.extend_from_slice(&record);
        Ok(())
    }

    /// Queues a plaintext ChangeCipherSpec record, whatever the write protection.
    pub(crate) fn queue_change_cipher_spec(&mut self) {
        self.pending_output.extend_from_slice(&plaintext_record(
            content_type::CHANGE_CIPHER_SPEC,
            self.record_version,
            &[1],
        ));
    }

    /// Writes every queued record.
    pub(crate) fn flush(&mut self) -> Result<(), TlsError> {
        if !self.pending_output.is_empty() {
            self.stream.write_all(&self.pending_output)?;
            self.pending_output.clear();
        }
        self.stream.flush()?;
        Ok(())
    }

    /// Sends `alert` as well as the connection still allows; failures are ignored, since the
    /// connection is ending anyway.
    pub(crate) fn send_alert(&mut self, alert: AlertDescription) {
        self.pending_output.clear();
        if self.queue(content_type::ALERT, &alert.encode()).is_ok() {
            let _ = self.flush();
        }
    }
}

pub(crate) fn peer_alert(payload: &[u8]) -> TlsError {
    match Alert::decode(payload) {
        Ok(alert) => TlsError::PeerAlert(alert),
        Err(_) => TlsError::protocol(AlertDescription::DecodeError, "the peer sent a malformed alert"),
    }
}
