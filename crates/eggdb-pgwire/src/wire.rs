use crate::messages::{MAX_STARTUP_PACKET_BYTES, Outbox};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(200);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const READ_CHUNK_BYTES: usize = 64 * 1024;
const FLUSH_ABOVE_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub(crate) enum WireError {
    Closed,
    Stopping,
    TimedOut,
    TooLarge(usize),
    Io,
}

pub(crate) struct Wire<'a> {
    stream: TcpStream,
    stop: &'a (dyn Fn() -> bool + Sync),
    pub(crate) deadline: Option<Instant>,
    pub(crate) outbox: Outbox,
}

impl<'a> Wire<'a> {
    pub(crate) fn new(
        stream: TcpStream,
        stop: &'a (dyn Fn() -> bool + Sync),
        deadline: Option<Instant>,
    ) -> io::Result<Self> {
        stream.set_nonblocking(false)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(POLL_INTERVAL))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        Ok(Self {
            stream,
            stop,
            deadline,
            outbox: Outbox::new(),
        })
    }

    fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), WireError> {
        let mut filled = 0;
        while filled < buffer.len() {
            match self.stream.read(&mut buffer[filled..]) {
                Ok(0) => return Err(WireError::Closed),
                Ok(count) => filled += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if (self.stop)() {
                        return Err(WireError::Stopping);
                    }
                    if self
                        .deadline
                        .is_some_and(|deadline| Instant::now() >= deadline)
                    {
                        return Err(WireError::TimedOut);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Err(WireError::Io),
            }
        }
        Ok(())
    }

    fn read_body(&mut self, length: usize) -> Result<Vec<u8>, WireError> {
        let mut body = Vec::new();
        while body.len() < length {
            let start = body.len();
            let chunk = (length - start).min(READ_CHUNK_BYTES);
            body.resize(start + chunk, 0);
            self.read_exact(&mut body[start..])?;
        }
        Ok(body)
    }

    pub(crate) fn read_startup_packet(&mut self) -> Result<Vec<u8>, WireError> {
        let mut length = [0u8; 4];
        self.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        if !(8..=MAX_STARTUP_PACKET_BYTES).contains(&length) {
            return Err(WireError::TooLarge(length));
        }
        self.read_body(length - 4)
    }

    pub(crate) fn read_message(&mut self, limit: usize) -> Result<(u8, Vec<u8>), WireError> {
        let mut header = [0u8; 5];
        self.read_exact(&mut header)?;
        let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        if length < 4 || length - 4 > limit {
            return Err(WireError::TooLarge(length));
        }
        let body = self.read_body(length - 4)?;
        Ok((header[0], body))
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        if !self.outbox.is_empty() {
            let result = self.stream.write_all(self.outbox.bytes());
            self.outbox.clear();
            result?;
        }
        self.stream.flush()
    }

    pub(crate) fn flush_if_large(&mut self) -> io::Result<()> {
        if self.outbox.bytes().len() >= FLUSH_ABOVE_BYTES {
            self.flush()?;
        }
        Ok(())
    }

    pub(crate) fn peer(&self) -> String {
        self.stream
            .peer_addr()
            .map_or_else(|_| "an unknown address".to_owned(), |peer| peer.to_string())
    }
}
