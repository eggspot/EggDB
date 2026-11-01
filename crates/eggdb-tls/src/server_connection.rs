//! A server's TCP connection that starts in plaintext and is upgraded to TLS when the client asks:
//! PostgreSQL's `SSLRequest`, MySQL's `CLIENT_SSL` and the TDS PRELOGIN encryption all work this
//! way.

use eggdb_platform::local_socket::LocalStream;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

use crate::config::ServerConfig;
use crate::error::TlsError;
use crate::session::SessionInfo;
use crate::stream::{PrefixedStream, TlsStream};

/// A server's TCP connection, plain or upgraded to TLS.
#[derive(Debug)]
pub enum ServerConnection {
    /// Plaintext TCP.
    Plain(TcpStream),
    /// A Unix domain socket, local to this machine; TLS is never used on it.
    Local(LocalStream),
    /// TLS over TCP.
    Tls(Box<TlsStream<PrefixedStream<TcpStream>>>),
    /// An upgrade failed; every read and write fails.
    Broken,
}

impl ServerConnection {
    /// The TCP socket underneath, for timeouts, addresses and shutdown.
    pub fn tcp(&self) -> Option<&TcpStream> {
        match self {
            Self::Plain(stream) => Some(stream),
            Self::Tls(stream) => Some(stream.get_ref().get_ref()),
            Self::Local(_) | Self::Broken => None,
        }
    }

    /// What the TLS handshake agreed on, when the connection uses TLS.
    pub fn tls(&self) -> Option<&SessionInfo> {
        match self {
            Self::Tls(stream) => Some(stream.info()),
            _ => None,
        }
    }

    /// Whether the peer connects from this machine: over loopback TCP or a local socket.
    pub fn peer_is_loopback(&self) -> bool {
        matches!(self, Self::Local(_))
            || self
                .tcp()
                .and_then(|stream| stream.peer_addr().ok())
                .is_some_and(|peer| peer.ip().is_loopback())
    }

    /// Whether the connection came over a local socket.
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// The peer, for logs: its address, or `local socket`.
    pub fn peer_text(&self) -> String {
        match self {
            Self::Local(_) => "local socket".to_owned(),
            _ => self
                .tcp()
                .and_then(|stream| stream.peer_addr().ok())
                .map_or_else(|| "an unknown address".to_owned(), |peer| peer.to_string()),
        }
    }

    /// Sets how long a read waits.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Local(stream) => stream.set_read_timeout(timeout),
            _ => self.tcp().ok_or_else(broken)?.set_read_timeout(timeout),
        }
    }

    /// Sets how long a write waits.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Local(stream) => stream.set_write_timeout(timeout),
            _ => self.tcp().ok_or_else(broken)?.set_write_timeout(timeout),
        }
    }

    /// Makes reads and writes block (accepted sockets may inherit non-blocking mode).
    pub fn set_blocking(&self) -> std::io::Result<()> {
        match self {
            Self::Local(stream) => stream.set_nonblocking(false),
            _ => self.tcp().ok_or_else(broken)?.set_nonblocking(false),
        }
    }

    /// Turns off Nagle's algorithm on TCP; local sockets have none.
    pub fn set_nodelay(&self) -> std::io::Result<()> {
        match self.tcp() {
            Some(stream) => stream.set_nodelay(true),
            None => Ok(()),
        }
    }

    /// Ends the sending direction.
    pub fn shutdown_write(&self) {
        match self {
            Self::Local(stream) => {
                let _ = stream.shutdown(Shutdown::Write);
            }
            _ => {
                if let Some(stream) = self.tcp() {
                    let _ = stream.shutdown(Shutdown::Write);
                }
            }
        }
    }

    /// Reads raw bytes, whatever TLS state the connection is in, to drain it before closing.
    pub fn drain(&self, buffer: &mut [u8], timeout: Duration) -> std::io::Result<usize> {
        match self {
            Self::Local(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                let mut reader: &LocalStream = stream;
                reader.read(buffer)
            }
            _ => {
                let stream = self.tcp().ok_or_else(broken)?;
                stream.set_read_timeout(Some(timeout))?;
                let mut reader: &TcpStream = stream;
                reader.read(buffer)
            }
        }
    }

    /// Runs the server side of a TLS handshake on the plaintext connection, with `prefix` the
    /// bytes of it already read and `handshake_timeout` the longest wait for each read. The
    /// socket's read timeout is set to `read_timeout` afterwards. A failed handshake leaves the
    /// connection [`ServerConnection::Broken`].
    pub fn upgrade(
        &mut self,
        config: &ServerConfig,
        prefix: Vec<u8>,
        handshake_timeout: Duration,
        read_timeout: Option<Duration>,
    ) -> Result<(), TlsError> {
        let refusal = || {
            TlsError::protocol(
                crate::alert::AlertDescription::UnexpectedMessage,
                "TLS was requested on a connection that already uses it or is a local socket",
            )
        };
        if !matches!(self, Self::Plain(_)) {
            return Err(refusal());
        }
        let Self::Plain(stream) = std::mem::replace(self, Self::Broken) else {
            return Err(refusal());
        };
        stream.set_read_timeout(Some(handshake_timeout))?;
        let tls = TlsStream::accept(PrefixedStream::new(prefix, stream), config)?;
        tls.get_ref().get_ref().set_read_timeout(read_timeout)?;
        *self = Self::Tls(Box::new(tls));
        Ok(())
    }

    /// Sends TLS `close_notify` when the connection uses TLS.
    pub fn close_tls(&mut self) {
        if let Self::Tls(stream) = self {
            let _ = stream.close();
        }
    }
}

fn broken() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotConnected, "the TLS handshake failed")
}

impl Read for ServerConnection {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buffer),
            Self::Local(stream) => stream.read(buffer),
            Self::Tls(stream) => stream.read(buffer),
            Self::Broken => Err(broken()),
        }
    }
}

impl Write for ServerConnection {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buffer),
            Self::Local(stream) => stream.write(buffer),
            Self::Tls(stream) => stream.write(buffer),
            Self::Broken => Err(broken()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Local(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
            Self::Broken => Err(broken()),
        }
    }
}
