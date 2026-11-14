//! QUIC packet headers (RFC 9000 section 17) and packet numbers (section 17.1 and appendix A):
//! the long header of Initial, 0-RTT, Handshake and Retry packets, version negotiation, and the
//! short header of 1-RTT packets.

use crate::QuicCodecError;
use crate::varint::{Cursor, write_varint, write_varint_with_length};

/// The longest connection ID QUIC version 1 allows.
pub const MAX_CONNECTION_ID_BYTES: usize = 20;
const LONG_HEADER: u8 = 0x80;
const FIXED_BIT: u8 = 0x40;
const RETRY_INTEGRITY_TAG_BYTES: usize = 16;

/// The kind of a long-header packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongPacketKind {
    /// Initial (type 0).
    Initial,
    /// 0-RTT (type 1).
    ZeroRtt,
    /// Handshake (type 2).
    Handshake,
    /// Retry (type 3).
    Retry,
}

impl LongPacketKind {
    fn bits(self) -> u8 {
        match self {
            Self::Initial => 0,
            Self::ZeroRtt => 1,
            Self::Handshake => 2,
            Self::Retry => 3,
        }
    }

    fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::Initial,
            1 => Self::ZeroRtt,
            2 => Self::Handshake,
            _ => Self::Retry,
        }
    }
}

/// A packet's header as read before its header protection is removed: what can be read without
/// keys, and where the protected packet number starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Header {
    /// A long header of version 1.
    Long {
        /// The packet kind.
        kind: LongPacketKind,
        /// The version.
        version: u32,
        /// The destination connection ID.
        destination: Vec<u8>,
        /// The source connection ID.
        source: Vec<u8>,
        /// An Initial packet's token; a Retry packet's retry token.
        token: Vec<u8>,
        /// Where the packet number starts; for a Retry packet, where its integrity tag starts.
        packet_number_offset: usize,
        /// The bytes of the packet number and payload (from the Length field); 0 for Retry.
        remaining_length: usize,
    },
    /// A version negotiation packet: the versions the server supports.
    VersionNegotiation {
        /// The destination connection ID.
        destination: Vec<u8>,
        /// The source connection ID.
        source: Vec<u8>,
        /// The versions offered.
        versions: Vec<u32>,
    },
    /// A short header (1-RTT); the destination connection ID's length is the receiver's own.
    Short {
        /// The destination connection ID.
        destination: Vec<u8>,
        /// Where the packet number starts.
        packet_number_offset: usize,
    },
}

fn connection_id(cursor: &mut Cursor<'_>) -> Result<Vec<u8>, QuicCodecError> {
    let length = usize::from(cursor.u8()?);
    if length > MAX_CONNECTION_ID_BYTES {
        return Err(QuicCodecError::InvalidValue);
    }
    Ok(cursor.take(length)?.to_vec())
}

impl Header {
    /// Reads the header at the start of `datagram`; `short_id_length` is the length of the
    /// connection IDs this endpoint issued, which short headers carry without a length.
    pub fn parse(datagram: &[u8], short_id_length: usize) -> Result<Self, QuicCodecError> {
        let mut cursor = Cursor::new(datagram);
        let first = cursor.u8()?;
        if first & LONG_HEADER == 0 {
            if first & FIXED_BIT == 0 {
                return Err(QuicCodecError::InvalidValue);
            }
            let destination = cursor.take(short_id_length)?.to_vec();
            return Ok(Self::Short {
                destination,
                packet_number_offset: cursor.position(),
            });
        }
        let version = cursor.u32()?;
        let destination = connection_id(&mut cursor)?;
        let source = connection_id(&mut cursor)?;
        if version == 0 {
            let mut versions = Vec::new();
            while !cursor.is_empty() {
                versions.push(cursor.u32()?);
            }
            return Ok(Self::VersionNegotiation {
                destination,
                source,
                versions,
            });
        }
        if first & FIXED_BIT == 0 {
            return Err(QuicCodecError::InvalidValue);
        }
        let kind = LongPacketKind::from_bits(first >> 4);
        let (token, remaining_length) = match kind {
            LongPacketKind::Initial => {
                let length = cursor.length()?;
                let token = cursor.take(length)?.to_vec();
                (token, cursor.length()?)
            }
            LongPacketKind::Retry => {
                let left = cursor.remaining().len();
                let token_length = left
                    .checked_sub(RETRY_INTEGRITY_TAG_BYTES)
                    .ok_or(QuicCodecError::Truncated)?;
                (cursor.take(token_length)?.to_vec(), 0)
            }
            LongPacketKind::ZeroRtt | LongPacketKind::Handshake => (Vec::new(), cursor.length()?),
        };
        Ok(Self::Long {
            kind,
            version,
            destination,
            source,
            token,
            packet_number_offset: cursor.position(),
            remaining_length,
        })
    }
}

/// Writes a long header up to its packet number, with a 2-byte Length field holding
/// `remaining_length` (the packet number's and the protected payload's bytes).
pub fn write_long_header(
    out: &mut Vec<u8>,
    kind: LongPacketKind,
    destination: &[u8],
    source: &[u8],
    token: &[u8],
    packet_number_length: usize,
    remaining_length: usize,
) -> Result<(), QuicCodecError> {
    if destination.len() > MAX_CONNECTION_ID_BYTES
        || source.len() > MAX_CONNECTION_ID_BYTES
        || !(1..=4).contains(&packet_number_length)
        || kind == LongPacketKind::Retry
    {
        return Err(QuicCodecError::InvalidValue);
    }
    out.push(LONG_HEADER | FIXED_BIT | (kind.bits() << 4) | (packet_number_length as u8 - 1));
    out.extend_from_slice(&crate::QUIC_VERSION_1.to_be_bytes());
    out.push(destination.len() as u8);
    out.extend_from_slice(destination);
    out.push(source.len() as u8);
    out.extend_from_slice(source);
    if kind == LongPacketKind::Initial {
        write_varint(out, token.len() as u64)?;
        out.extend_from_slice(token);
    }
    write_varint_with_length(out, remaining_length as u64, 2)
}

/// Writes a short header up to its packet number.
pub fn write_short_header(
    out: &mut Vec<u8>,
    destination: &[u8],
    key_phase: bool,
    packet_number_length: usize,
) -> Result<(), QuicCodecError> {
    if !(1..=4).contains(&packet_number_length) {
        return Err(QuicCodecError::InvalidValue);
    }
    out.push(FIXED_BIT | (u8::from(key_phase) << 2) | (packet_number_length as u8 - 1));
    out.extend_from_slice(destination);
    Ok(())
}

/// A version negotiation packet listing `versions`, answering a client's packet whose source
/// and destination connection IDs it swaps.
pub fn version_negotiation(client_destination: &[u8], client_source: &[u8], versions: &[u32], first_byte_bits: u8) -> Vec<u8> {
    let mut out = vec![LONG_HEADER | (first_byte_bits & 0x7f)];
    out.extend_from_slice(&0u32.to_be_bytes());
    out.push(client_source.len() as u8);
    out.extend_from_slice(client_source);
    out.push(client_destination.len() as u8);
    out.extend_from_slice(client_destination);
    for version in versions {
        out.extend_from_slice(&version.to_be_bytes());
    }
    out
}

/// How many bytes to send packet number `number` in, given the largest packet number the peer
/// acknowledged (RFC 9000 appendix A.2): enough for twice the unacknowledged range.
pub fn packet_number_length(number: u64, largest_acknowledged: Option<u64>) -> usize {
    let unacknowledged = match largest_acknowledged {
        Some(largest) => number.saturating_sub(largest),
        None => number.saturating_add(1),
    };
    let unacknowledged = unacknowledged.max(1);
    let bits_needed = (64 - unacknowledged.leading_zeros()) as usize + usize::from(!unacknowledged.is_power_of_two());
    bits_needed.div_ceil(8).clamp(1, 4)
}

/// The packet number whose low `length` bytes are `truncated`, closest to the next expected
/// one after `largest_received` (RFC 9000 appendix A.3).
pub fn decode_packet_number(largest_received: Option<u64>, truncated: u64, length: usize) -> u64 {
    let expected = largest_received.map_or(0, |largest| largest + 1);
    let window = 1u64 << (length * 8);
    let half = window / 2;
    let mask = window - 1;
    let candidate = (expected & !mask) | truncated;
    if candidate.saturating_add(half) <= expected && candidate < (1u64 << 62) - window {
        candidate + window
    } else if candidate > expected.saturating_add(half) && candidate >= window {
        candidate - window
    } else {
        candidate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_9000_appendix_a_3_decodes_its_example() {
        assert_eq!(decode_packet_number(Some(0xa82f30ea), 0x9b32, 2), 0xa82f9b32);
    }

    #[test]
    fn packet_number_lengths_follow_the_unacknowledged_range() {
        assert_eq!(packet_number_length(0xac5c02, Some(0xabe8b3)), 2);
        assert_eq!(packet_number_length(0xace8fe, Some(0xabe8b3)), 3);
        assert_eq!(packet_number_length(0, None), 1);
    }

    #[test]
    fn a_long_header_round_trips_and_a_short_one_reads_its_id() {
        let mut out = Vec::new();
        write_long_header(&mut out, LongPacketKind::Initial, &[1; 8], &[2; 5], b"tok", 2, 300).unwrap();
        let length = out.len();
        out.extend_from_slice(&[0; 300]);
        match Header::parse(&out, 0).unwrap() {
            Header::Long { kind, destination, source, token, packet_number_offset, remaining_length, .. } => {
                assert_eq!(kind, LongPacketKind::Initial);
                assert_eq!(destination, vec![1; 8]);
                assert_eq!(source, vec![2; 5]);
                assert_eq!(token, b"tok");
                assert_eq!(packet_number_offset, length);
                assert_eq!(remaining_length, 300);
            }
            other => panic!("read {other:?}"),
        }
        let mut short = Vec::new();
        write_short_header(&mut short, &[3; 8], true, 1).unwrap();
        short.push(0);
        assert_eq!(
            Header::parse(&short, 8).unwrap(),
            Header::Short { destination: vec![3; 8], packet_number_offset: 9 }
        );
    }

    #[test]
    fn version_negotiation_swaps_the_ids_and_lists_versions() {
        let packet = version_negotiation(&[1, 2], &[3], &[crate::QUIC_VERSION_1], 0x2a);
        assert_eq!(
            Header::parse(&packet, 0).unwrap(),
            Header::VersionNegotiation { destination: vec![3], source: vec![1, 2], versions: vec![1] }
        );
    }

    #[test]
    fn hostile_headers_are_errors() {
        assert_eq!(Header::parse(&[0xc0, 0, 0, 0, 1, 21], 0), Err(QuicCodecError::InvalidValue));
        assert_eq!(Header::parse(&[0x00, 1], 1), Err(QuicCodecError::InvalidValue));
        assert_eq!(Header::parse(&[0xc0, 0, 0, 0, 1, 0, 0, 0x05], 0), Err(QuicCodecError::Truncated));
    }
}
