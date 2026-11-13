//! The part of EggDB's platform layer that needs no operating system (D1081): the error every
//! platform call returns, and the traits the storage engine reads and writes through, a
//! [`File`] of bytes and a [`Clock`]. It builds with `#![no_std]` and `alloc` when its default
//! `std` feature is off, so the engine can run on a microcontroller over a flash driver;
//! `eggdb-platform` re-exports everything here under its old names.

#![cfg_attr(not(feature = "std"), no_std)]

use core::fmt;
use core::time::Duration;

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
    /// A short description of the kind, as error messages show it.
    pub fn describe(self) -> &'static str {
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
    #[cfg(feature = "std")]
    os_error: Option<std::io::Error>,
}

impl Error {
    /// Creates an error of the given kind that did not come from the operating system.
    pub fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            #[cfg(feature = "std")]
            os_error: None,
        }
    }

    /// The same error, reported as `kind`.
    pub fn with_kind(self, kind: ErrorKind) -> Self {
        Self { kind, ..self }
    }

    /// The kind of failure.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

#[cfg(feature = "std")]
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.os_error {
            Some(os_error) => write!(f, "{}: {os_error}", self.kind.describe()),
            None => f.write_str(self.kind.describe()),
        }
    }
}

#[cfg(not(feature = "std"))]
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.describe())
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.os_error
            .as_ref()
            .map(|os_error| os_error as &(dyn std::error::Error + 'static))
    }
}

#[cfg(feature = "std")]
impl From<std::io::Error> for Error {
    fn from(os_error: std::io::Error) -> Self {
        use std::io;
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
            io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted | io::ErrorKind::BrokenPipe => ErrorKind::ConnectionReset,
            io::ErrorKind::TimedOut => ErrorKind::TimedOut,
            io::ErrorKind::AddrInUse => ErrorKind::AddrInUse,
            io::ErrorKind::AddrNotAvailable => ErrorKind::AddrNotAvailable,
            io::ErrorKind::NotConnected => ErrorKind::NotConnected,
            io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable => ErrorKind::HostUnreachable,
            _ => ErrorKind::Other,
        };
        Self { kind, os_error: Some(os_error) }
    }
}

/// A source of time.
pub trait Clock: Send + Sync {
    /// Time elapsed since a fixed, arbitrary origin. Never goes backwards; use it for timeouts
    /// and durations.
    fn monotonic(&self) -> Duration;

    /// Nanoseconds since the Unix epoch by the wall clock, or 0 if the wall clock is set before
    /// the epoch. May jump in either direction when the clock is adjusted; hybrid logical clocks
    /// (D20) build on it without trusting it to be monotonic.
    fn unix_time_nanos(&self) -> u64;
}

/// The lock [`File::try_lock`] takes on a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileLock {
    /// Shared with other shared locks, refused while an exclusive lock is held.
    Shared,
    /// Held by one handle only, refused while any other lock is held.
    Exclusive,
}

/// An open file with positional reads and writes.
///
/// Every read and write names its offset, so one handle can be shared between threads without a
/// cursor. Written bytes become durable only after [`File::sync_data`] or [`File::sync_all`]
/// returns successfully.
pub trait File: Send + Sync + core::fmt::Debug {
    /// Reads into `buf` starting at `offset` and returns how many bytes were read. Returns 0 at
    /// or after the end of the file.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize>;

    /// Writes from `buf` starting at `offset` and returns how many bytes were written. Writing
    /// past the end extends the file, filling any gap with zeros; an empty `buf` changes
    /// nothing. Fails with [`ErrorKind::PermissionDenied`] through a handle opened with
    /// `OpenMode::Read`, as [`File::set_size`] does.
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize>;

    /// The size of the file in bytes.
    fn size(&self) -> Result<u64>;

    /// Truncates or extends the file to `size` bytes, filling any extension with zeros. Fails
    /// with [`ErrorKind::PermissionDenied`] through a read-only handle.
    fn set_size(&self, size: u64) -> Result<()>;

    /// Whether the handle was opened with `FileSystem::open_uncached` and so requires
    /// aligned buffers, offsets and lengths.
    fn is_uncached(&self) -> bool;

    /// Locks the file for this handle without waiting, failing with [`ErrorKind::Locked`] when
    /// another handle, in this process or another, holds a lock that conflicts with `lock`.
    ///
    /// The lock is advisory: it stops other locks, not reads or writes. It is held until the
    /// handle is dropped, or until a crash in a simulation, and the operating system releases
    /// it when the process ends, so it never outlives its owner. Dropping the handle releases it
    /// at once, even while a child process started meanwhile still has a copy of the handle
    /// because it has not yet started its own program. A handle takes one lock for
    /// its life; a second call fails with [`ErrorKind::InvalidInput`]. Fails with
    /// [`ErrorKind::Unsupported`] on targets without file locks.
    fn try_lock(&self, lock: FileLock) -> Result<()>;

    /// Makes the file's contents and size durable. Fails with
    /// [`ErrorKind::PermissionDenied`] through a handle opened with `OpenMode::Read` on every
    /// operating system, as Windows cannot flush a file it may not write.
    fn sync_data(&self) -> Result<()>;

    /// Makes the file's contents, size and other metadata durable. Fails like
    /// [`File::sync_data`] through a read-only handle.
    fn sync_all(&self) -> Result<()>;

    /// Fills `buf` from `offset`, failing with [`ErrorKind::UnexpectedEof`] if the file ends
    /// first.
    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> Result<()> {
        while !buf.is_empty() {
            let read = self.read_at(buf, offset)?;
            if read == 0 {
                return Err(Error::new(ErrorKind::UnexpectedEof));
            }
            buf = &mut buf[read..];
            offset = advance(offset, read)?;
        }
        Ok(())
    }

    /// Writes all of `buf` from `offset`.
    fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> Result<()> {
        while !buf.is_empty() {
            let written = self.write_at(buf, offset)?;
            if written == 0 {
                return Err(Error::new(ErrorKind::WriteZero));
            }
            buf = &buf[written..];
            offset = advance(offset, written)?;
        }
        Ok(())
    }
}

fn advance(offset: u64, by: usize) -> Result<u64> {
    u64::try_from(by)
        .ok()
        .and_then(|by| offset.checked_add(by))
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput))
}
