//! A mail connection's stream, plain or under TLS after `STARTTLS` (RFC 3207, RFC 2595).

use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

pub(crate) enum Transport {
    Plain(TcpStream),
    Tls(Box<eggdb_tls::TlsStream<TcpStream>>),
    Closed,
}

impl Read for Transport {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Transport::Plain(stream) => stream.read(buffer),
            Transport::Tls(stream) => stream.read(buffer),
            Transport::Closed => Ok(0),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Transport::Plain(stream) => stream.write(bytes),
            Transport::Tls(stream) => stream.write(bytes),
            Transport::Closed => Err(io::Error::new(io::ErrorKind::NotConnected, "the connection is closed")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Transport::Plain(stream) => stream.flush(),
            Transport::Tls(stream) => stream.flush(),
            Transport::Closed => Ok(()),
        }
    }
}

impl Transport {
    pub(crate) fn secured(&self) -> bool {
        matches!(self, Transport::Tls(_))
    }
}

/// Runs the TLS handshake on the connection under `reader`, after the server's go-ahead was
/// sent. Bytes the client sent before the handshake would be read as plain text after the
/// command that secured the connection, so they end it (RFC 3207 §4.2).
pub(crate) fn start_tls(reader: &mut BufReader<Transport>, config: &Arc<eggdb_tls::ServerConfig>) -> io::Result<()> {
    if !reader.buffer().is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "the client sent data before the TLS handshake"));
    }
    let plain = match std::mem::replace(reader, BufReader::new(Transport::Closed)).into_inner() {
        Transport::Plain(stream) => stream,
        other => {
            *reader = BufReader::new(other);
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the connection already runs TLS"));
        }
    };
    let secured = eggdb_tls::TlsStream::accept(plain, config).map_err(|error| io::Error::new(io::ErrorKind::ConnectionAborted, format!("TLS handshake: {error}")))?;
    *reader = BufReader::new(Transport::Tls(Box::new(secured)));
    Ok(())
}
