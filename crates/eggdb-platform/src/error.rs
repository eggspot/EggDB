use std::fmt;
use std::io;

/// The result of a platform operation.
pub type Result<T> = core::result::Result<T, Error>;

/// What kind of failure a platform operation hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The file or directory does not exist.
    NotFound,
    /// The file already exists and the operation required a new one.
    AlreadyExists,
    /// The operation is not allowed, for example a write through a read-only handle.
    PermissionDenied,
    /// The file ended before the requested number of bytes was read.
    UnexpectedEof,
    /// A write made no progress although bytes remained to be written.
    WriteZero,
    /// An argument was invalid, for example an offset beyond the largest file size.
    InvalidInput,
    /// The platform or file system cannot do what was asked, for example bypass its cache.
    Unsupported,
    /// Memory for the operation could not be allocated.
    OutOfMemory,
    /// The handle was opened before a simulated crash and no longer refers to a live file.
    StaleHandle,
    /// The simulation injected this failure on purpose.
    InjectedFault,
    /// A queue already holds as many operations as it may.
    QueueFull,
    /// Nothing listens at the address a connection went to.
    ConnectionRefused,
    /// The connection was reset, aborted or closed by the peer while data was still owed.
    ConnectionReset,
    /// The operation gave up waiting.
    TimedOut,
    /// The operation was canceled before it finished.
    Canceled,
    /// Something already listens at the address.
    AddrInUse,
    /// The address does not belong to this host, or no local port is left for it.
    AddrNotAvailable,
    /// The socket is not connected.
    NotConnected,
    /// No route leads to the host or its network.
    HostUnreachable,
    /// In a simulation, nothing could complete the operations a queue waits for unless another
    /// queue acts first.
    Stalled,
    /// Another handle holds a lock on the file that conflicts with the one asked for.
    Locked,
    /// Any other failure reported by the operating system.
    Other,
}

impl ErrorKind {
    fn describe(self) -> &'static str {
        match self {
            Self::NotFound => "not found",
            Self::AlreadyExists => "already exists",
            Self::PermissionDenied => "permission denied",
            Self::UnexpectedEof => "unexpected end of file",
            Self::WriteZero => "write made no progress",
            Self::InvalidInput => "invalid input",
            Self::Unsupported => "not supported here",
            Self::OutOfMemory => "out of memory",
            Self::StaleHandle => "handle opened before a simulated crash",
            Self::InjectedFault => "injected fault",
            Self::QueueFull => "queue full",
            Self::ConnectionRefused => "connection refused",
            Self::ConnectionReset => "connection reset",
            Self::TimedOut => "timed out",
            Self::Canceled => "canceled",
            Self::AddrInUse => "address in use",
            Self::AddrNotAvailable => "address not available",
            Self::NotConnected => "not connected",
            Self::HostUnreachable => "host unreachable",
            Self::Stalled => "simulation stalled",
            Self::Locked => "locked by another handle",
            Self::Other => "operating system error",
        }
    }
}

/// A failed platform operation, with the operating system's error when there is one.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    os_error: Option<io::Error>,
}

impl Error {
    /// Creates an error of the given kind that did not come from the operating system.
    pub fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            os_error: None,
        }
    }

    pub(crate) fn with_kind(self, kind: ErrorKind) -> Self {
        Self { kind, ..self }
    }

    /// The kind of failure.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.os_error {
            Some(os_error) => write!(f, "{}: {os_error}", self.kind.describe()),
            None => f.write_str(self.kind.describe()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.os_error
            .as_ref()
            .map(|os_error| os_error as &(dyn std::error::Error + 'static))
    }
}

impl From<io::Error> for Error {
    fn from(os_error: io::Error) -> Self {
        let kind = match os_error.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => ErrorKind::NotFound,
            io::ErrorKind::IsADirectory => ErrorKind::InvalidInput,
            io::ErrorKind::AlreadyExists => ErrorKind::AlreadyExists,
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            io::ErrorKind::UnexpectedEof => ErrorKind::UnexpectedEof,
            io::ErrorKind::WriteZero => ErrorKind::WriteZero,
            io::ErrorKind::InvalidInput => ErrorKind::InvalidInput,
            io::ErrorKind::OutOfMemory => ErrorKind::OutOfMemory,
            io::ErrorKind::Unsupported => ErrorKind::Unsupported,
            io::ErrorKind::ConnectionRefused => ErrorKind::ConnectionRefused,
            io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe => ErrorKind::ConnectionReset,
            io::ErrorKind::TimedOut => ErrorKind::TimedOut,
            io::ErrorKind::AddrInUse => ErrorKind::AddrInUse,
            io::ErrorKind::AddrNotAvailable => ErrorKind::AddrNotAvailable,
            io::ErrorKind::NotConnected => ErrorKind::NotConnected,
            io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable => {
                ErrorKind::HostUnreachable
            }
            _ => ErrorKind::Other,
        };
        Self {
            kind,
            os_error: Some(os_error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, ErrorKind};
    use std::error::Error as _;
    use std::io;

    #[test]
    fn os_errors_keep_their_kind_and_source() {
        let error = Error::from(io::Error::new(io::ErrorKind::NotFound, "no such file"));
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(error.source().is_some());
        assert_eq!(error.to_string(), "not found: no such file");
    }

    #[test]
    fn unknown_os_errors_become_other() {
        let error = Error::from(io::Error::other("disk on fire"));
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn network_errors_map_to_one_set_of_kinds() {
        for (os, kind) in [
            (
                io::ErrorKind::ConnectionRefused,
                ErrorKind::ConnectionRefused,
            ),
            (io::ErrorKind::ConnectionReset, ErrorKind::ConnectionReset),
            (io::ErrorKind::ConnectionAborted, ErrorKind::ConnectionReset),
            (io::ErrorKind::BrokenPipe, ErrorKind::ConnectionReset),
            (io::ErrorKind::TimedOut, ErrorKind::TimedOut),
            (io::ErrorKind::AddrInUse, ErrorKind::AddrInUse),
            (io::ErrorKind::AddrNotAvailable, ErrorKind::AddrNotAvailable),
            (io::ErrorKind::NotConnected, ErrorKind::NotConnected),
            (io::ErrorKind::HostUnreachable, ErrorKind::HostUnreachable),
            (
                io::ErrorKind::NetworkUnreachable,
                ErrorKind::HostUnreachable,
            ),
        ] {
            assert_eq!(Error::from(io::Error::from(os)).kind(), kind, "{os:?}");
        }
    }

    #[test]
    fn errors_without_an_os_cause_describe_their_kind() {
        let error = Error::new(ErrorKind::InjectedFault);
        assert!(error.source().is_none());
        assert_eq!(error.to_string(), "injected fault");
    }
}
