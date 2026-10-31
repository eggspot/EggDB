//! A server's TCP connection that starts in plaintext and is upgraded to TLS when the client asks:
//! PostgreSQL's `SSLRequest`, MySQL's `CLIENT_SSL` and the TDS PRELOGIN encryption all work this
//! way.

use std::io::{Read, Write};
use std::net::TcpStream;
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
            Self::Broken => None,
        }
    }

    /// What the TLS handshake agreed on, when the connection uses TLS.
    pub fn tls(&self) -> Option<&SessionInfo> {
        match self {
            Self::Tls(stream) => Some(stream.info()),
            _ => None,
        }
    }

    /// Whether the peer connects from this machine.
    pub fn peer_is_loopback(&self) -> bool {
        self.tcp()
            .and_then(|stream| stream.peer_addr().ok())
            .is_some_and(|peer| peer.ip().is_loopback())
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
        let Self::Plain(stream) = std::mem::replace(self, Self::Broken) else {
            return Err(TlsError::protocol(
                crate::alert::AlertDescription::UnexpectedMessage,
                "TLS was requested on a connection that already uses it",
            ));
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
            Self::Tls(stream) => stream.read(buffer),
            Self::Broken => Err(broken()),
        }
    }
}

impl Write for ServerConnection {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buffer),
            Self::Tls(stream) => stream.write(buffer),
            Self::Broken => Err(broken()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
            Self::Broken => Err(broken()),
        }
    }
}
