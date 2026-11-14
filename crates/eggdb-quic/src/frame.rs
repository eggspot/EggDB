//! QUIC frames (RFC 9000 section 19), parsed from and written to a packet's payload.

use crate::QuicCodecError;
use crate::varint::{Cursor, write_varint};

const MAX_CONNECTION_ID_BYTES: usize = 20;
const STATELESS_RESET_TOKEN_BYTES: usize = 16;
const PATH_DATA_BYTES: usize = 8;

/// One acknowledged range of packet numbers, inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckRange {
    /// The smallest packet number of the range.
    pub smallest: u64,
    /// The largest packet number of the range.
    pub largest: u64,
}

/// The ECN counts an ACK frame of type 0x03 carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EcnCounts {
    /// Packets received with ECT(0).
    pub ect0: u64,
    /// Packets received with ECT(1).
    pub ect1: u64,
    /// Packets received with ECN-CE.
    pub ce: u64,
}

/// A QUIC frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// PADDING: one zero byte (consecutive ones are read as one frame with a count).
    Padding(usize),
    /// PING.
    Ping,
    /// ACK: the ranges acknowledged, largest first, and the ACK delay in the sender's units.
    Ack {
        /// The acknowledged ranges, the one with the largest packet number first.
        ranges: Vec<AckRange>,
        /// The encoded ACK delay.
        delay: u64,
        /// The ECN counts, for type 0x03.
        ecn: Option<EcnCounts>,
    },
    /// RESET_STREAM.
    ResetStream {
        /// The stream.
        stream_id: u64,
        /// The application error code.
        error_code: u64,
        /// The stream's final size.
        final_size: u64,
    },
    /// STOP_SENDING.
    StopSending {
        /// The stream.
        stream_id: u64,
        /// The application error code.
        error_code: u64,
    },
    /// CRYPTO: TLS handshake bytes at an offset.
    Crypto {
        /// Where the data starts in the crypto stream.
        offset: u64,
        /// The data.
        data: Vec<u8>,
    },
    /// NEW_TOKEN.
    NewToken(Vec<u8>),
    /// STREAM.
    Stream {
        /// The stream.
        stream_id: u64,
        /// Where the data starts in the stream.
        offset: u64,
        /// The data.
        data: Vec<u8>,
        /// Whether the data ends the stream.
        fin: bool,
    },
    /// MAX_DATA.
    MaxData(u64),
    /// MAX_STREAM_DATA.
    MaxStreamData {
        /// The stream.
        stream_id: u64,
        /// The new limit.
        maximum: u64,
    },
    /// MAX_STREAMS, for bidirectional streams when `bidirectional`.
    MaxStreams {
        /// Whether the limit is on bidirectional streams.
        bidirectional: bool,
        /// The new limit.
        maximum: u64,
    },
    /// DATA_BLOCKED.
    DataBlocked(u64),
    /// STREAM_DATA_BLOCKED.
    StreamDataBlocked {
        /// The stream.
        stream_id: u64,
        /// The limit the sender is blocked at.
        limit: u64,
    },
    /// STREAMS_BLOCKED.
    StreamsBlocked {
        /// Whether the limit is on bidirectional streams.
        bidirectional: bool,
        /// The limit the sender is blocked at.
        limit: u64,
    },
    /// NEW_CONNECTION_ID.
    NewConnectionId {
        /// Its sequence number.
        sequence: u64,
        /// The sequence numbers below which the peer retires its IDs.
        retire_prior_to: u64,
        /// The connection ID.
        connection_id: Vec<u8>,
        /// Its stateless reset token.
        reset_token: [u8; STATELESS_RESET_TOKEN_BYTES],
    },
    /// RETIRE_CONNECTION_ID.
    RetireConnectionId(u64),
    /// PATH_CHALLENGE.
    PathChallenge([u8; PATH_DATA_BYTES]),
    /// PATH_RESPONSE.
    PathResponse([u8; PATH_DATA_BYTES]),
    /// CONNECTION_CLOSE: of the transport (with the frame type that caused it) or of the
    /// application (type 0x1d, no frame type).
    ConnectionClose {
        /// The error code.
        error_code: u64,
        /// The frame type that caused a transport error; `None` for an application close.
        frame_type: Option<u64>,
        /// The reason, UTF-8 by convention.
        reason: Vec<u8>,
    },
    /// HANDSHAKE_DONE.
    HandshakeDone,
}

fn array<const N: usize>(cursor: &mut Cursor<'_>) -> Result<[u8; N], QuicCodecError> {
    let mut out = [0u8; N];
    out.copy_from_slice(cursor.take(N)?);
    Ok(out)
}

fn read_ack(cursor: &mut Cursor<'_>, with_ecn: bool) -> Result<Frame, QuicCodecError> {
    let largest = cursor.varint()?;
    let delay = cursor.varint()?;
    let range_count = cursor.varint()?;
    let first_range = cursor.varint()?;
    let mut smallest = largest.checked_sub(first_range).ok_or(QuicCodecError::InvalidValue)?;
    let mut ranges = vec![AckRange { smallest, largest }];
    for _ in 0..range_count {
        let gap = cursor.varint()?;
        let length = cursor.varint()?;
        let next_largest = smallest
            .checked_sub(gap)
            .and_then(|value| value.checked_sub(2))
            .ok_or(QuicCodecError::InvalidValue)?;
        smallest = next_largest.checked_sub(length).ok_or(QuicCodecError::InvalidValue)?;
        ranges.push(AckRange {
            smallest,
            largest: next_largest,
        });
    }
    let ecn = if with_ecn {
        Some(EcnCounts {
            ect0: cursor.varint()?,
            ect1: cursor.varint()?,
            ce: cursor.varint()?,
        })
    } else {
        None
    };
    Ok(Frame::Ack { ranges, delay, ecn })
}

impl Frame {
    /// Reads every frame of a decrypted payload, in order.
    pub fn parse_all(payload: &[u8]) -> Result<Vec<Self>, QuicCodecError> {
        let mut cursor = Cursor::new(payload);
        let mut frames = Vec::new();
        while !cursor.is_empty() {
            frames.push(Self::parse(&mut cursor)?);
        }
        Ok(frames)
    }

    /// Reads one frame.
    pub fn parse(cursor: &mut Cursor<'_>) -> Result<Self, QuicCodecError> {
        let kind = cursor.varint()?;
        Ok(match kind {
            0x00 => {
                let mut count = 1;
                while cursor.remaining().first() == Some(&0) {
                    cursor.take(1)?;
                    count += 1;
                }
                Self::Padding(count)
            }
            0x01 => Self::Ping,
            0x02 | 0x03 => read_ack(cursor, kind == 0x03)?,
            0x04 => Self::ResetStream {
                stream_id: cursor.varint()?,
                error_code: cursor.varint()?,
                final_size: cursor.varint()?,
            },
            0x05 => Self::StopSending {
                stream_id: cursor.varint()?,
                error_code: cursor.varint()?,
            },
            0x06 => {
                let offset = cursor.varint()?;
                let length = cursor.length()?;
                Self::Crypto {
                    offset,
                    data: cursor.take(length)?.to_vec(),
                }
            }
            0x07 => {
                let length = cursor.length()?;
                if length == 0 {
                    return Err(QuicCodecError::InvalidValue);
                }
                Self::NewToken(cursor.take(length)?.to_vec())
            }
            0x08..=0x0f => {
                let stream_id = cursor.varint()?;
                let offset = if kind & 0x04 != 0 { cursor.varint()? } else { 0 };
                let data = if kind & 0x02 != 0 {
                    let length = cursor.length()?;
                    cursor.take(length)?.to_vec()
                } else {
                    cursor.take(cursor.remaining().len())?.to_vec()
                };
                Self::Stream {
                    stream_id,
                    offset,
                    data,
                    fin: kind & 0x01 != 0,
                }
            }
            0x10 => Self::MaxData(cursor.varint()?),
            0x11 => Self::MaxStreamData {
                stream_id: cursor.varint()?,
                maximum: cursor.varint()?,
            },
            0x12 | 0x13 => Self::MaxStreams {
                bidirectional: kind == 0x12,
                maximum: cursor.varint()?,
            },
            0x14 => Self::DataBlocked(cursor.varint()?),
            0x15 => Self::StreamDataBlocked {
                stream_id: cursor.varint()?,
                limit: cursor.varint()?,
            },
            0x16 | 0x17 => Self::StreamsBlocked {
                bidirectional: kind == 0x16,
                limit: cursor.varint()?,
            },
            0x18 => {
                let sequence = cursor.varint()?;
                let retire_prior_to = cursor.varint()?;
                let length = usize::from(cursor.u8()?);
                if length == 0 || length > MAX_CONNECTION_ID_BYTES || retire_prior_to > sequence {
                    return Err(QuicCodecError::InvalidValue);
                }
                Self::NewConnectionId {
                    sequence,
                    retire_prior_to,
                    connection_id: cursor.take(length)?.to_vec(),
                    reset_token: array(cursor)?,
                }
            }
            0x19 => Self::RetireConnectionId(cursor.varint()?),
            0x1a => Self::PathChallenge(array(cursor)?),
            0x1b => Self::PathResponse(array(cursor)?),
            0x1c | 0x1d => {
                let error_code = cursor.varint()?;
                let frame_type = if kind == 0x1c { Some(cursor.varint()?) } else { None };
                let length = cursor.length()?;
                Self::ConnectionClose {
                    error_code,
                    frame_type,
                    reason: cursor.take(length)?.to_vec(),
                }
            }
            0x1e => Self::HandshakeDone,
            other => return Err(QuicCodecError::UnknownFrame(other)),
        })
    }

    /// Appends the frame. A STREAM frame is written with its offset and length fields.
    pub fn write(&self, out: &mut Vec<u8>) -> Result<(), QuicCodecError> {
        match self {
            Self::Padding(count) => out.extend(std::iter::repeat_n(0u8, *count)),
            Self::Ping => out.push(0x01),
            Self::Ack { ranges, delay, ecn } => {
                let Some(first) = ranges.first() else {
                    return Err(QuicCodecError::InvalidValue);
                };
                write_varint(out, if ecn.is_some() { 0x03 } else { 0x02 })?;
                write_varint(out, first.largest)?;
                write_varint(out, *delay)?;
                write_varint(out, (ranges.len() - 1) as u64)?;
                write_varint(
                    out,
                    first.largest.checked_sub(first.smallest).ok_or(QuicCodecError::InvalidValue)?,
                )?;
                let mut previous_smallest = first.smallest;
                for range in &ranges[1..] {
                    let gap = previous_smallest
                        .checked_sub(range.largest)
                        .and_then(|value| value.checked_sub(2))
                        .ok_or(QuicCodecError::InvalidValue)?;
                    write_varint(out, gap)?;
                    write_varint(
                        out,
                        range.largest.checked_sub(range.smallest).ok_or(QuicCodecError::InvalidValue)?,
                    )?;
                    previous_smallest = range.smallest;
                }
                if let Some(counts) = ecn {
                    write_varint(out, counts.ect0)?;
                    write_varint(out, counts.ect1)?;
                    write_varint(out, counts.ce)?;
                }
            }
            Self::ResetStream {
                stream_id,
                error_code,
                final_size,
            } => {
                out.push(0x04);
                write_varint(out, *stream_id)?;
                write_varint(out, *error_code)?;
                write_varint(out, *final_size)?;
            }
            Self::StopSending { stream_id, error_code } => {
                out.push(0x05);
                write_varint(out, *stream_id)?;
                write_varint(out, *error_code)?;
            }
            Self::Crypto { offset, data } => {
                out.push(0x06);
                write_varint(out, *offset)?;
                write_varint(out, data.len() as u64)?;
                out.extend_from_slice(data);
            }
            Self::NewToken(token) => {
                out.push(0x07);
                write_varint(out, token.len() as u64)?;
                out.extend_from_slice(token);
            }
            Self::Stream {
                stream_id,
                offset,
                data,
                fin,
            } => {
                out.push(0x08 | 0x04 | 0x02 | u8::from(*fin));
                write_varint(out, *stream_id)?;
                write_varint(out, *offset)?;
                write_varint(out, data.len() as u64)?;
                out.extend_from_slice(data);
            }
            Self::MaxData(maximum) => {
                out.push(0x10);
                write_varint(out, *maximum)?;
            }
            Self::MaxStreamData { stream_id, maximum } => {
                out.push(0x11);
                write_varint(out, *stream_id)?;
                write_varint(out, *maximum)?;
            }
            Self::MaxStreams { bidirectional, maximum } => {
                out.push(if *bidirectional { 0x12 } else { 0x13 });
                write_varint(out, *maximum)?;
            }
            Self::DataBlocked(limit) => {
                out.push(0x14);
                write_varint(out, *limit)?;
            }
            Self::StreamDataBlocked { stream_id, limit } => {
                out.push(0x15);
                write_varint(out, *stream_id)?;
                write_varint(out, *limit)?;
            }
            Self::StreamsBlocked { bidirectional, limit } => {
                out.push(if *bidirectional { 0x16 } else { 0x17 });
                write_varint(out, *limit)?;
            }
            Self::NewConnectionId {
                sequence,
                retire_prior_to,
                connection_id,
                reset_token,
            } => {
                if connection_id.is_empty() || connection_id.len() > MAX_CONNECTION_ID_BYTES {
                    return Err(QuicCodecError::InvalidValue);
                }
                out.push(0x18);
                write_varint(out, *sequence)?;
                write_varint(out, *retire_prior_to)?;
                out.push(connection_id.len() as u8);
                out.extend_from_slice(connection_id);
                out.extend_from_slice(reset_token);
            }
            Self::RetireConnectionId(sequence) => {
                out.push(0x19);
                write_varint(out, *sequence)?;
            }
            Self::PathChallenge(data) => {
                out.push(0x1a);
                out.extend_from_slice(data);
            }
            Self::PathResponse(data) => {
                out.push(0x1b);
                out.extend_from_slice(data);
            }
            Self::ConnectionClose {
                error_code,
                frame_type,
                reason,
            } => {
                out.push(if frame_type.is_some() { 0x1c } else { 0x1d });
                write_varint(out, *error_code)?;
                if let Some(frame_type) = frame_type {
                    write_varint(out, *frame_type)?;
                }
                write_varint(out, reason.len() as u64)?;
                out.extend_from_slice(reason);
            }
            Self::HandshakeDone => out.push(0x1e),
        }
        Ok(())
    }

    /// Whether receiving the frame obliges the receiver to acknowledge its packet: every frame
    /// but ACK, PADDING and CONNECTION_CLOSE (RFC 9000 section 13.2.1).
    pub fn is_ack_eliciting(&self) -> bool {
        !matches!(self, Self::Ack { .. } | Self::Padding(_) | Self::ConnectionClose { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(frame: Frame) {
        let mut out = Vec::new();
        frame.write(&mut out).unwrap();
        assert_eq!(Frame::parse_all(&out).unwrap(), vec![frame]);
    }

    #[test]
    fn every_frame_round_trips() {
        round_trip(Frame::Ping);
        round_trip(Frame::Padding(5));
        round_trip(Frame::Ack {
            ranges: vec![
                AckRange { smallest: 90, largest: 100 },
                AckRange { smallest: 50, largest: 80 },
                AckRange { smallest: 0, largest: 10 },
            ],
            delay: 25,
            ecn: Some(EcnCounts { ect0: 1, ect1: 2, ce: 3 }),
        });
        round_trip(Frame::ResetStream { stream_id: 4, error_code: 7, final_size: 1000 });
        round_trip(Frame::StopSending { stream_id: 8, error_code: 1 });
        round_trip(Frame::Crypto { offset: 0, data: b"client hello".to_vec() });
        round_trip(Frame::NewToken(vec![1, 2, 3]));
        round_trip(Frame::Stream { stream_id: 0, offset: 77, data: b"GET /".to_vec(), fin: true });
        round_trip(Frame::MaxData(1 << 20));
        round_trip(Frame::MaxStreamData { stream_id: 4, maximum: 65_536 });
        round_trip(Frame::MaxStreams { bidirectional: false, maximum: 100 });
        round_trip(Frame::DataBlocked(9));
        round_trip(Frame::StreamDataBlocked { stream_id: 4, limit: 9 });
        round_trip(Frame::StreamsBlocked { bidirectional: true, limit: 3 });
        round_trip(Frame::NewConnectionId {
            sequence: 2,
            retire_prior_to: 1,
            connection_id: vec![9; 8],
            reset_token: [7; 16],
        });
        round_trip(Frame::RetireConnectionId(1));
        round_trip(Frame::PathChallenge([1; 8]));
        round_trip(Frame::PathResponse([2; 8]));
        round_trip(Frame::ConnectionClose { error_code: 0x0a, frame_type: Some(0x06), reason: b"bad".to_vec() });
        round_trip(Frame::ConnectionClose { error_code: 3, frame_type: None, reason: Vec::new() });
        round_trip(Frame::HandshakeDone);
    }

    #[test]
    fn a_stream_frame_without_a_length_takes_the_rest_of_the_packet() {
        let frames = Frame::parse_all(&[0x09, 0x04, b'a', b'b']).unwrap();
        assert_eq!(
            frames,
            vec![Frame::Stream { stream_id: 4, offset: 0, data: b"ab".to_vec(), fin: true }]
        );
    }

    #[test]
    fn hostile_frames_are_errors_not_panics() {
        assert_eq!(Frame::parse_all(&[0x02, 0x05, 0x00, 0x00, 0x06]), Err(QuicCodecError::InvalidValue));
        assert_eq!(Frame::parse_all(&[0x06, 0x00, 0x40, 0xff]), Err(QuicCodecError::Truncated));
        assert_eq!(Frame::parse_all(&[0x18, 0x01, 0x02, 0x08]), Err(QuicCodecError::InvalidValue));
        assert_eq!(Frame::parse_all(&[0x21]), Err(QuicCodecError::UnknownFrame(0x21)));
        assert!(!Frame::Padding(1).is_ack_eliciting());
        assert!(Frame::Ping.is_ack_eliciting());
    }
}
