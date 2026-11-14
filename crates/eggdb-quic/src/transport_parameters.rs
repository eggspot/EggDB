//! QUIC transport parameters (RFC 9000 section 18), sent in TLS's `quic_transport_parameters`
//! extension: each an identifier, a length and a value, most of them variable-length integers.

use crate::QuicCodecError;
use crate::packet::MAX_CONNECTION_ID_BYTES;
use crate::varint::{Cursor, MAX_VARINT, varint_length, write_varint};

mod id {
    pub(super) const ORIGINAL_DESTINATION_CONNECTION_ID: u64 = 0x00;
    pub(super) const MAX_IDLE_TIMEOUT: u64 = 0x01;
    pub(super) const STATELESS_RESET_TOKEN: u64 = 0x02;
    pub(super) const MAX_UDP_PAYLOAD_SIZE: u64 = 0x03;
    pub(super) const INITIAL_MAX_DATA: u64 = 0x04;
    pub(super) const INITIAL_MAX_STREAM_DATA_BIDI_LOCAL: u64 = 0x05;
    pub(super) const INITIAL_MAX_STREAM_DATA_BIDI_REMOTE: u64 = 0x06;
    pub(super) const INITIAL_MAX_STREAM_DATA_UNI: u64 = 0x07;
    pub(super) const INITIAL_MAX_STREAMS_BIDI: u64 = 0x08;
    pub(super) const INITIAL_MAX_STREAMS_UNI: u64 = 0x09;
    pub(super) const ACK_DELAY_EXPONENT: u64 = 0x0a;
    pub(super) const MAX_ACK_DELAY: u64 = 0x0b;
    pub(super) const DISABLE_ACTIVE_MIGRATION: u64 = 0x0c;
    pub(super) const ACTIVE_CONNECTION_ID_LIMIT: u64 = 0x0e;
    pub(super) const INITIAL_SOURCE_CONNECTION_ID: u64 = 0x0f;
    pub(super) const RETRY_SOURCE_CONNECTION_ID: u64 = 0x10;
}

const MOST_STREAMS: u64 = 1 << 60;
const SMALLEST_UDP_PAYLOAD: u64 = 1200;
const LARGEST_ACK_DELAY_EXPONENT: u64 = 20;
const LONGEST_MAX_ACK_DELAY_MILLISECONDS: u64 = 1 << 14;

/// One endpoint's transport parameters, with RFC 9000's defaults for those not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportParameters {
    /// The destination connection ID of the client's first Initial (server only).
    pub original_destination_connection_id: Option<Vec<u8>>,
    /// The idle timeout in milliseconds; 0 for none.
    pub max_idle_timeout_milliseconds: u64,
    /// The stateless reset token (server only).
    pub stateless_reset_token: Option<[u8; 16]>,
    /// The largest UDP payload the endpoint receives.
    pub max_udp_payload_size: u64,
    /// The connection's initial flow control limit.
    pub initial_max_data: u64,
    /// The initial limit of bidirectional streams this endpoint opens.
    pub initial_max_stream_data_bidi_local: u64,
    /// The initial limit of bidirectional streams the peer opens.
    pub initial_max_stream_data_bidi_remote: u64,
    /// The initial limit of unidirectional streams.
    pub initial_max_stream_data_uni: u64,
    /// How many bidirectional streams the peer may open.
    pub initial_max_streams_bidi: u64,
    /// How many unidirectional streams the peer may open.
    pub initial_max_streams_uni: u64,
    /// The exponent ACK delays are scaled by.
    pub ack_delay_exponent: u64,
    /// The longest the endpoint delays an acknowledgement, in milliseconds.
    pub max_ack_delay_milliseconds: u64,
    /// Whether the endpoint refuses connection migration.
    pub disable_active_migration: bool,
    /// How many connection IDs the endpoint keeps from its peer.
    pub active_connection_id_limit: u64,
    /// The source connection ID of the endpoint's first Initial.
    pub initial_source_connection_id: Option<Vec<u8>>,
    /// The source connection ID of a Retry the server sent (server only).
    pub retry_source_connection_id: Option<Vec<u8>>,
}

impl Default for TransportParameters {
    fn default() -> Self {
        Self {
            original_destination_connection_id: None,
            max_idle_timeout_milliseconds: 0,
            stateless_reset_token: None,
            max_udp_payload_size: 65_527,
            initial_max_data: 0,
            initial_max_stream_data_bidi_local: 0,
            initial_max_stream_data_bidi_remote: 0,
            initial_max_stream_data_uni: 0,
            initial_max_streams_bidi: 0,
            initial_max_streams_uni: 0,
            ack_delay_exponent: 3,
            max_ack_delay_milliseconds: 25,
            disable_active_migration: false,
            active_connection_id_limit: 2,
            initial_source_connection_id: None,
            retry_source_connection_id: None,
        }
    }
}

fn connection_id(value: &[u8]) -> Result<Vec<u8>, QuicCodecError> {
    if value.len() > MAX_CONNECTION_ID_BYTES {
        return Err(QuicCodecError::InvalidValue);
    }
    Ok(value.to_vec())
}

fn integer(value: &[u8]) -> Result<u64, QuicCodecError> {
    let mut cursor = Cursor::new(value);
    let number = cursor.varint()?;
    if !cursor.is_empty() {
        return Err(QuicCodecError::InvalidValue);
    }
    Ok(number)
}

fn write_integer(out: &mut Vec<u8>, identifier: u64, value: u64) -> Result<(), QuicCodecError> {
    write_varint(out, identifier)?;
    write_varint(out, varint_length(value) as u64)?;
    write_varint(out, value)
}

fn write_bytes(out: &mut Vec<u8>, identifier: u64, value: &[u8]) -> Result<(), QuicCodecError> {
    write_varint(out, identifier)?;
    write_varint(out, value.len() as u64)?;
    out.extend_from_slice(value);
    Ok(())
}

impl TransportParameters {
    /// Reads the parameters a peer sent. A server's parameters only a server may send are
    /// refused from a client (`from_server` false), each parameter may come once, and every
    /// value is checked against RFC 9000's bounds. Unknown parameters are ignored, as the RFC
    /// requires.
    pub fn decode(bytes: &[u8], from_server: bool) -> Result<Self, QuicCodecError> {
        let mut parameters = Self::default();
        let mut seen = Vec::new();
        let mut cursor = Cursor::new(bytes);
        while !cursor.is_empty() {
            let identifier = cursor.varint()?;
            let length = cursor.length()?;
            let value = cursor.take(length)?;
            if seen.contains(&identifier) {
                return Err(QuicCodecError::InvalidValue);
            }
            seen.push(identifier);
            let server_only = matches!(
                identifier,
                id::ORIGINAL_DESTINATION_CONNECTION_ID | id::STATELESS_RESET_TOKEN | id::RETRY_SOURCE_CONNECTION_ID
            );
            if server_only && !from_server {
                return Err(QuicCodecError::InvalidValue);
            }
            match identifier {
                id::ORIGINAL_DESTINATION_CONNECTION_ID => {
                    parameters.original_destination_connection_id = Some(connection_id(value)?);
                }
                id::MAX_IDLE_TIMEOUT => parameters.max_idle_timeout_milliseconds = integer(value)?,
                id::STATELESS_RESET_TOKEN => {
                    let token: [u8; 16] = value.try_into().map_err(|_| QuicCodecError::InvalidValue)?;
                    parameters.stateless_reset_token = Some(token);
                }
                id::MAX_UDP_PAYLOAD_SIZE => {
                    parameters.max_udp_payload_size = integer(value)?;
                    if parameters.max_udp_payload_size < SMALLEST_UDP_PAYLOAD {
                        return Err(QuicCodecError::InvalidValue);
                    }
                }
                id::INITIAL_MAX_DATA => parameters.initial_max_data = integer(value)?,
                id::INITIAL_MAX_STREAM_DATA_BIDI_LOCAL => parameters.initial_max_stream_data_bidi_local = integer(value)?,
                id::INITIAL_MAX_STREAM_DATA_BIDI_REMOTE => {
                    parameters.initial_max_stream_data_bidi_remote = integer(value)?;
                }
                id::INITIAL_MAX_STREAM_DATA_UNI => parameters.initial_max_stream_data_uni = integer(value)?,
                id::INITIAL_MAX_STREAMS_BIDI | id::INITIAL_MAX_STREAMS_UNI => {
                    let streams = integer(value)?;
                    if streams > MOST_STREAMS {
                        return Err(QuicCodecError::InvalidValue);
                    }
                    if identifier == id::INITIAL_MAX_STREAMS_BIDI {
                        parameters.initial_max_streams_bidi = streams;
                    } else {
                        parameters.initial_max_streams_uni = streams;
                    }
                }
                id::ACK_DELAY_EXPONENT => {
                    parameters.ack_delay_exponent = integer(value)?;
                    if parameters.ack_delay_exponent > LARGEST_ACK_DELAY_EXPONENT {
                        return Err(QuicCodecError::InvalidValue);
                    }
                }
                id::MAX_ACK_DELAY => {
                    parameters.max_ack_delay_milliseconds = integer(value)?;
                    if parameters.max_ack_delay_milliseconds >= LONGEST_MAX_ACK_DELAY_MILLISECONDS {
                        return Err(QuicCodecError::InvalidValue);
                    }
                }
                id::DISABLE_ACTIVE_MIGRATION => {
                    if !value.is_empty() {
                        return Err(QuicCodecError::InvalidValue);
                    }
                    parameters.disable_active_migration = true;
                }
                id::ACTIVE_CONNECTION_ID_LIMIT => {
                    parameters.active_connection_id_limit = integer(value)?;
                    if parameters.active_connection_id_limit < 2 {
                        return Err(QuicCodecError::InvalidValue);
                    }
                }
                id::INITIAL_SOURCE_CONNECTION_ID => parameters.initial_source_connection_id = Some(connection_id(value)?),
                id::RETRY_SOURCE_CONNECTION_ID => parameters.retry_source_connection_id = Some(connection_id(value)?),
                _ => {}
            }
        }
        Ok(parameters)
    }

    /// The parameters as sent, leaving out those at their defaults.
    pub fn encode(&self) -> Result<Vec<u8>, QuicCodecError> {
        let defaults = Self::default();
        let mut out = Vec::new();
        if let Some(original) = &self.original_destination_connection_id {
            write_bytes(&mut out, id::ORIGINAL_DESTINATION_CONNECTION_ID, original)?;
        }
        let integers = [
            (id::MAX_IDLE_TIMEOUT, self.max_idle_timeout_milliseconds, defaults.max_idle_timeout_milliseconds),
            (id::MAX_UDP_PAYLOAD_SIZE, self.max_udp_payload_size, defaults.max_udp_payload_size),
            (id::INITIAL_MAX_DATA, self.initial_max_data, defaults.initial_max_data),
            (
                id::INITIAL_MAX_STREAM_DATA_BIDI_LOCAL,
                self.initial_max_stream_data_bidi_local,
                defaults.initial_max_stream_data_bidi_local,
            ),
            (
                id::INITIAL_MAX_STREAM_DATA_BIDI_REMOTE,
                self.initial_max_stream_data_bidi_remote,
                defaults.initial_max_stream_data_bidi_remote,
            ),
            (id::INITIAL_MAX_STREAM_DATA_UNI, self.initial_max_stream_data_uni, defaults.initial_max_stream_data_uni),
            (id::INITIAL_MAX_STREAMS_BIDI, self.initial_max_streams_bidi, defaults.initial_max_streams_bidi),
            (id::INITIAL_MAX_STREAMS_UNI, self.initial_max_streams_uni, defaults.initial_max_streams_uni),
            (id::ACK_DELAY_EXPONENT, self.ack_delay_exponent, defaults.ack_delay_exponent),
            (id::MAX_ACK_DELAY, self.max_ack_delay_milliseconds, defaults.max_ack_delay_milliseconds),
            (id::ACTIVE_CONNECTION_ID_LIMIT, self.active_connection_id_limit, defaults.active_connection_id_limit),
        ];
        for (identifier, value, default) in integers {
            if value > MAX_VARINT {
                return Err(QuicCodecError::InvalidValue);
            }
            if value != default {
                write_integer(&mut out, identifier, value)?;
            }
        }
        if let Some(token) = &self.stateless_reset_token {
            write_bytes(&mut out, id::STATELESS_RESET_TOKEN, token)?;
        }
        if self.disable_active_migration {
            write_bytes(&mut out, id::DISABLE_ACTIVE_MIGRATION, &[])?;
        }
        if let Some(source) = &self.initial_source_connection_id {
            write_bytes(&mut out, id::INITIAL_SOURCE_CONNECTION_ID, source)?;
        }
        if let Some(retry) = &self.retry_source_connection_id {
            write_bytes(&mut out, id::RETRY_SOURCE_CONNECTION_ID, retry)?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_s_parameters_round_trip() {
        let parameters = TransportParameters {
            original_destination_connection_id: Some(vec![1; 8]),
            max_idle_timeout_milliseconds: 30_000,
            stateless_reset_token: Some([9; 16]),
            initial_max_data: 1 << 20,
            initial_max_stream_data_bidi_remote: 1 << 18,
            initial_max_streams_bidi: 100,
            initial_max_streams_uni: 3,
            disable_active_migration: true,
            initial_source_connection_id: Some(vec![2; 8]),
            ..TransportParameters::default()
        };
        let encoded = parameters.encode().unwrap();
        assert_eq!(TransportParameters::decode(&encoded, true).unwrap(), parameters);
    }

    #[test]
    fn a_client_may_not_send_server_parameters_or_repeat_or_break_bounds() {
        let with_token = TransportParameters { stateless_reset_token: Some([1; 16]), ..TransportParameters::default() };
        assert!(TransportParameters::decode(&with_token.encode().unwrap(), false).is_err());
        assert!(TransportParameters::decode(&[0x01, 0x01, 0x05, 0x01, 0x01, 0x05], false).is_err());
        assert!(TransportParameters::decode(&[0x0a, 0x01, 0x15], false).is_err());
        assert!(TransportParameters::decode(&[0x03, 0x02, 0x41, 0x00], false).is_err());
        assert_eq!(
            TransportParameters::decode(&[0x40, 0x99, 0x02, 0xab, 0xcd], false).unwrap(),
            TransportParameters::default()
        );
    }
}
