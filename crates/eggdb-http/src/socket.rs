//! A served connection's socket: plain TCP, or TCP under TLS (D1076). The TLS session is
//! shared by the connection's reading and writing halves, which take turns on one thread.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) enum Socket {
    Plain(TcpStream),
    Tls {
        raw: TcpStream,
        session: Arc<Mutex<eggdb_tls::TlsStream<TcpStream>>>,
    },
}

impl Socket {
    /// Runs the TLS handshake on `stream` with `config`, waiting at most ten seconds for the
    /// client, then leaves the socket's read timeout at `read_poll`.
    pub(crate) fn secure(stream: TcpStream, config: &eggdb_tls::ServerConfig, read_poll: Duration) -> io::Result<Self> {
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let raw = stream.try_clone()?;
        let session = eggdb_tls::TlsStream::accept(stream, config)
            .map_err(|error| io::Error::new(io::ErrorKind::ConnectionAborted, format!("TLS handshake: {error}")))?;
        raw.set_read_timeout(Some(read_poll))?;
        Ok(Socket::Tls { raw, session: Arc::new(Mutex::new(session)) })
    }

    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        Ok(match self {
            Socket::Plain(stream) => Socket::Plain(stream.try_clone()?),
            Socket::Tls { raw, session } => Socket::Tls { raw: raw.try_clone()?, session: Arc::clone(session) },
        })
    }

    pub(crate) fn shutdown_write(&self) -> io::Result<()> {
        match self {
            Socket::Plain(stream) => stream.shutdown(Shutdown::Write),
            Socket::Tls { raw, .. } => raw.shutdown(Shutdown::Write),
        }
    }
}

impl Read for Socket {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Socket::Plain(stream) => stream.read(buffer),
            Socket::Tls { session, .. } => session.lock().unwrap_or_else(PoisonError::into_inner).read(buffer),
        }
    }
}

impl Write for Socket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Socket::Plain(stream) => stream.write(bytes),
            Socket::Tls { session, .. } => session.lock().unwrap_or_else(PoisonError::into_inner).write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Socket::Plain(stream) => stream.flush(),
            Socket::Tls { session, .. } => session.lock().unwrap_or_else(PoisonError::into_inner).flush(),
        }
    }
}
