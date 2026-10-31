//! What can end a TLS connection, and the alert each failure sends to the peer.

use crate::alert::{Alert, AlertDescription};
use crate::codec::CodecError;
use crate::record::RecordError;
use crate::verify::CertificateVerificationError;

/// Why a TLS handshake or connection failed.
#[derive(Debug)]
pub enum TlsError {
    /// The underlying stream failed or closed early.
    Io(std::io::Error),
    /// The peer sent an alert other than `close_notify`.
    PeerAlert(Alert),
    /// The peer broke the protocol; `alert` was sent to it.
    Protocol {
        /// The alert sent to the peer.
        alert: AlertDescription,
        /// What went wrong, for people and logs.
        reason: &'static str,
    },
    /// The peer's certificate was refused.
    Certificate(CertificateVerificationError),
}

impl TlsError {
    /// A protocol failure that sends `alert`.
    pub fn protocol(alert: AlertDescription, reason: &'static str) -> Self {
        Self::Protocol { alert, reason }
    }

    /// The alert to send the peer for this failure, if any.
    pub fn alert_to_send(&self) -> Option<AlertDescription> {
        match self {
            Self::Io(_) | Self::PeerAlert(_) => None,
            Self::Protocol { alert, .. } => Some(*alert),
            Self::Certificate(error) => Some(certificate_alert(*error)),
        }
    }
}

fn certificate_alert(error: CertificateVerificationError) -> AlertDescription {
    match error {
        CertificateVerificationError::NotYetValid | CertificateVerificationError::Expired => {
            AlertDescription::CertificateExpired
        }
        CertificateVerificationError::UnknownIssuer => AlertDescription::UnknownCa,
        CertificateVerificationError::WeakSignatureAlgorithm
        | CertificateVerificationError::UnhandledCriticalExtension => {
            AlertDescription::UnsupportedCertificate
        }
        _ => AlertDescription::BadCertificate,
    }
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "TLS connection failed: {error}"),
            Self::PeerAlert(alert) => write!(formatter, "the peer ended the TLS connection with {alert}"),
            Self::Protocol { reason, .. } => write!(formatter, "TLS handshake failed: {reason}"),
            Self::Certificate(error) => write!(formatter, "the TLS certificate was refused: {error}"),
        }
    }
}

impl std::error::Error for TlsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Certificate(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TlsError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<CodecError> for TlsError {
    fn from(error: CodecError) -> Self {
        match error {
            CodecError::IllegalValue => Self::protocol(AlertDescription::IllegalParameter, "a handshake field has a value TLS forbids"),
            _ => Self::protocol(AlertDescription::DecodeError, "a handshake message could not be decoded"),
        }
    }
}

impl From<RecordError> for TlsError {
    fn from(error: RecordError) -> Self {
        match error {
            RecordError::Overflow => Self::protocol(AlertDescription::RecordOverflow, "a record is longer than TLS allows"),
            RecordError::BadRecordMac => Self::protocol(AlertDescription::BadRecordMac, "a record failed its integrity check"),
            RecordError::UnexpectedMessage => Self::protocol(AlertDescription::UnexpectedMessage, "a record has an unexpected content type"),
            RecordError::MalformedHeader => Self::protocol(AlertDescription::DecodeError, "the peer is not speaking TLS"),
            RecordError::SequenceExhausted => Self::protocol(AlertDescription::InternalError, "too many records under one key"),
        }
    }
}

impl From<CertificateVerificationError> for TlsError {
    fn from(error: CertificateVerificationError) -> Self {
        Self::Certificate(error)
    }
}

impl From<TlsError> for std::io::Error {
    fn from(error: TlsError) -> Self {
        match error {
            TlsError::Io(error) => error,
            other => std::io::Error::new(std::io::ErrorKind::InvalidData, other),
        }
    }
}
