use crate::frame::Http2Error;

const PREFIX_BYTES: usize = 5;
const MOST_MESSAGE_BYTES: usize = 64 << 20;

/// gRPC's status codes, as its protocol documents them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrpcStatus {
    /// `OK`.
    Ok = 0,
    /// `CANCELLED`.
    Cancelled = 1,
    /// `INVALID_ARGUMENT`.
    InvalidArgument = 3,
    /// `NOT_FOUND`.
    NotFound = 5,
    /// `PERMISSION_DENIED`.
    PermissionDenied = 7,
    /// `RESOURCE_EXHAUSTED`.
    ResourceExhausted = 8,
    /// `FAILED_PRECONDITION`.
    FailedPrecondition = 9,
    /// `OUT_OF_RANGE`.
    OutOfRange = 11,
    /// `UNIMPLEMENTED`.
    Unimplemented = 12,
    /// `INTERNAL`.
    Internal = 13,
    /// `UNAUTHENTICATED`.
    Unauthenticated = 16,
}

impl GrpcStatus {
    /// The status as `grpc-status` writes it.
    pub fn code(self) -> String {
        (self as u8).to_string()
    }
}

/// The messages of a gRPC request or response body: each a compressed flag, a big-endian
/// length and that many bytes. Compressed messages are refused, as EggDB announces no
/// `grpc-accept-encoding`.
pub fn grpc_messages(body: &[u8]) -> Result<Vec<Vec<u8>>, Http2Error> {
    let mut messages = Vec::new();
    let mut at = 0;
    while at < body.len() {
        let prefix = body
            .get(at..at + PREFIX_BYTES)
            .ok_or_else(|| Http2Error::protocol("a gRPC message prefix ends early"))?;
        if prefix[0] != 0 {
            return Err(Http2Error::protocol("a compressed gRPC message"));
        }
        let length = u32::from_be_bytes([prefix[1], prefix[2], prefix[3], prefix[4]]) as usize;
        if length > MOST_MESSAGE_BYTES {
            return Err(Http2Error::protocol("a gRPC message larger than 64 MiB"));
        }
        let start = at + PREFIX_BYTES;
        let message = body
            .get(start..start + length)
            .ok_or_else(|| Http2Error::protocol("a gRPC message ends early"))?;
        messages.push(message.to_vec());
        at = start + length;
    }
    Ok(messages)
}

/// One gRPC message with its five-byte prefix.
pub fn grpc_message(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + PREFIX_BYTES);
    out.push(0);
    out.extend_from_slice(&u32::try_from(message.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(message);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let mut body = grpc_message(b"one");
        body.extend(grpc_message(b""));
        assert_eq!(grpc_messages(&body).unwrap(), [b"one".to_vec(), Vec::new()]);
        assert!(grpc_messages(&[0, 0, 0, 0, 9, 1]).is_err());
    }
}
