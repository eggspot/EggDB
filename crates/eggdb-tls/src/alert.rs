//! TLS alerts (RFC 8446 section 6): the two-byte message that closes a connection and says why.

use crate::codec::{CodecError, Reader};

/// Alert levels. TLS 1.3 ignores the level, but TLS 1.2 peers read it.
pub mod level {
    /// `warning`, used only for `close_notify` and `user_canceled`.
    pub const WARNING: u8 = 1;
    /// `fatal`.
    pub const FATAL: u8 = 2;
}

/// The alert descriptions EggDB sends or understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AlertDescription {
    /// The sender will send nothing more.
    CloseNotify = 0,
    /// A message arrived that the handshake does not allow at this point.
    UnexpectedMessage = 10,
    /// A record failed its integrity check.
    BadRecordMac = 20,
    /// A record was longer than allowed.
    RecordOverflow = 22,
    /// No parameters both sides support.
    HandshakeFailure = 40,
    /// A certificate that could not be used.
    BadCertificate = 42,
    /// A certificate of a kind that is not supported.
    UnsupportedCertificate = 43,
    /// A certificate that has expired or is not yet valid.
    CertificateExpired = 45,
    /// Some other problem with a certificate.
    CertificateUnknown = 46,
    /// A field with a value the protocol forbids.
    IllegalParameter = 47,
    /// A certificate chain that does not lead to a trusted root.
    UnknownCa = 48,
    /// A message that could not be decoded.
    DecodeError = 50,
    /// A signature or Finished value that did not verify.
    DecryptError = 51,
    /// The peer offered only protocol versions EggDB does not speak.
    ProtocolVersion = 70,
    /// The peer offered only weaker parameters than EggDB requires.
    InsufficientSecurity = 71,
    /// A problem in the sender itself.
    InternalError = 80,
    /// A downgrade was attempted against a client that supports more.
    InappropriateFallback = 86,
    /// A required extension is missing.
    MissingExtension = 109,
    /// An extension the message does not allow.
    UnsupportedExtension = 110,
    /// The server name is not one the server serves.
    UnrecognizedName = 112,
    /// A client certificate is required.
    CertificateRequired = 116,
    /// No application protocol both sides support.
    NoApplicationProtocol = 120,
}

impl AlertDescription {
    /// The description with code `code`, if EggDB knows it.
    pub fn from_code(code: u8) -> Option<Self> {
        const ALL: [AlertDescription; 22] = [
            AlertDescription::CloseNotify,
            AlertDescription::UnexpectedMessage,
            AlertDescription::BadRecordMac,
            AlertDescription::RecordOverflow,
            AlertDescription::HandshakeFailure,
            AlertDescription::BadCertificate,
            AlertDescription::UnsupportedCertificate,
            AlertDescription::CertificateExpired,
            AlertDescription::CertificateUnknown,
            AlertDescription::IllegalParameter,
            AlertDescription::UnknownCa,
            AlertDescription::DecodeError,
            AlertDescription::DecryptError,
            AlertDescription::ProtocolVersion,
            AlertDescription::InsufficientSecurity,
            AlertDescription::InternalError,
            AlertDescription::InappropriateFallback,
            AlertDescription::MissingExtension,
            AlertDescription::UnsupportedExtension,
            AlertDescription::UnrecognizedName,
            AlertDescription::CertificateRequired,
            AlertDescription::NoApplicationProtocol,
        ];
        ALL.into_iter().find(|description| *description as u8 == code)
    }

    /// The two bytes of this alert, at the level TLS requires for it.
    pub fn encode(self) -> [u8; 2] {
        let level = if self == Self::CloseNotify { level::WARNING } else { level::FATAL };
        [level, self as u8]
    }
}

/// An alert as received: its level and its description code, known or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alert {
    /// The level byte.
    pub level: u8,
    /// The description code.
    pub code: u8,
}

impl Alert {
    /// Parses the two bytes of an alert record.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(bytes);
        let level = reader.read_u8()?;
        let code = reader.read_u8()?;
        reader.finish()?;
        if level != level::WARNING && level != level::FATAL {
            return Err(CodecError::IllegalValue);
        }
        Ok(Self { level, code })
    }

    /// The description, if EggDB knows it.
    pub fn description(&self) -> Option<AlertDescription> {
        AlertDescription::from_code(self.code)
    }

    /// Whether this is the peer's orderly `close_notify`.
    pub fn is_close_notify(&self) -> bool {
        self.code == AlertDescription::CloseNotify as u8
    }
}

impl std::fmt::Display for Alert {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.description() {
            Some(description) => write!(formatter, "TLS alert {description:?} ({})", self.code),
            None => write!(formatter, "TLS alert {}", self.code),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alerts_round_trip_with_their_level() {
        assert_eq!(AlertDescription::CloseNotify.encode(), [1, 0]);
        assert_eq!(AlertDescription::DecodeError.encode(), [2, 50]);
        let alert = Alert::decode(&[2, 48]).unwrap();
        assert_eq!(alert.description(), Some(AlertDescription::UnknownCa));
        assert!(Alert::decode(&[1, 0]).unwrap().is_close_notify());
    }

    #[test]
    fn malformed_alerts_are_refused() {
        assert_eq!(Alert::decode(&[2]), Err(CodecError::Truncated));
        assert_eq!(Alert::decode(&[2, 0, 0]), Err(CodecError::TrailingData));
        assert_eq!(Alert::decode(&[3, 0]), Err(CodecError::IllegalValue));
    }
}
