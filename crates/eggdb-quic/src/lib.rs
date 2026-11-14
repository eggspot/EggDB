//! EggDB's own QUIC version 1 (RFC 9000, RFC 9001, RFC 9002), written from the RFCs with no
//! third-party code (D1, D34). This crate is the wire layer the transport is built on:
//!
//! - [`varint`]: QUIC's variable-length integers;
//! - [`frame`]: every frame of RFC 9000 section 19, parsed and written;
//! - [`packet`]: long and short packet headers, version negotiation, and packet number encoding;
//! - [`protection`]: the initial secrets, packet protection with the AEAD of the TLS 1.3 cipher
//!   suite, and header protection with AES or ChaCha20 (RFC 9001 section 5);
//! - [`transport_parameters`]: the transport parameters of RFC 9000 section 18.
//!
//! The TLS 1.3 handshake QUIC carries is `eggdb_tls::quic`, which takes and gives CRYPTO data per
//! encryption level.
//!
//! Every length a peer sends is checked against what is left: hostile input gets an error value,
//! never a panic (D9).

pub mod frame;
pub mod packet;
pub mod protection;
pub mod transport_parameters;
pub mod varint;

/// The version number of QUIC version 1.
pub const QUIC_VERSION_1: u32 = 0x0000_0001;

/// Why bytes could not be read as QUIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuicCodecError {
    /// The input ends inside a field.
    Truncated,
    /// A value outside what the field allows, such as a connection ID longer than 20 bytes.
    InvalidValue,
    /// A frame type this version does not define.
    UnknownFrame(u64),
    /// Packet or header protection did not verify or could not be removed.
    Protection,
}

impl std::fmt::Display for QuicCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => f.write_str("the QUIC data ends inside a field"),
            Self::InvalidValue => f.write_str("a QUIC field holds a value it may not"),
            Self::UnknownFrame(kind) => write!(f, "unknown QUIC frame type 0x{kind:x}"),
            Self::Protection => f.write_str("the QUIC packet's protection does not verify"),
        }
    }
}

impl std::error::Error for QuicCodecError {}
