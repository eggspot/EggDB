use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(200);
const SHORTEST_WAIT: Duration = Duration::from_millis(1);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const READ_CHUNK_BYTES: usize = 64 * 1024;
const FLUSH_ABOVE_BYTES: usize = 256 * 1024;
pub(crate) const LARGEST_FRAME: usize = 0xFF_FFFF;

#[derive(Debug)]
pub(crate) enum WireError {
    Closed,
    Stopping,
    TimedOut,
    TooLarge,
    OutOfOrder,
    Io,
}

pub(crate) struct Wire<'a> {
    stream: TcpStream,
    stop: &'a (dyn Fn() -> bool + Sync),
    pub(crate) deadline: Option<Instant>,
    outbox: Vec<u8>,
    read_timeout: Duration,
    pub(crate) sequence: u8,
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
            outbox: Vec::new(),
            read_timeout: POLL_INTERVAL,
            sequence: 0,
        })
    }

    fn next_wait(&self) -> Result<Duration, WireError> {
        if (self.stop)() {
            return Err(WireError::Stopping);
        }
        match self.deadline {
            None => Ok(POLL_INTERVAL),
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(WireError::TimedOut);
                }
                Ok(left.clamp(SHORTEST_WAIT, POLL_INTERVAL))
            }
        }
    }

    fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), WireError> {
        let mut filled = 0;
        while filled < buffer.len() {
            let wait = self.next_wait()?;
            if wait != self.read_timeout {
                self.stream
                    .set_read_timeout(Some(wait))
                    .map_err(|_| WireError::Io)?;
                self.read_timeout = wait;
            }
            match self.stream.read(&mut buffer[filled..]) {
                Ok(0) => return Err(WireError::Closed),
                Ok(count) => filled += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err(WireError::Io),
            }
        }
        Ok(())
    }

    pub(crate) fn read_payload(
        &mut self,
        expected: u8,
        limit: usize,
    ) -> Result<Vec<u8>, WireError> {
        let mut payload = Vec::new();
        let mut sequence = expected;
        loop {
            let mut header = [0u8; 4];
            self.read_exact(&mut header)?;
            let length =
                usize::from(header[0]) | usize::from(header[1]) << 8 | usize::from(header[2]) << 16;
            if header[3] != sequence {
                return Err(WireError::OutOfOrder);
            }
            if payload.len() + length > limit {
                return Err(WireError::TooLarge);
            }
            let mut read = 0;
            while read < length {
                let chunk = (length - read).min(READ_CHUNK_BYTES);
                let start = payload.len();
                payload.resize(start + chunk, 0);
                self.read_exact(&mut payload[start..])?;
                read += chunk;
            }
            sequence = sequence.wrapping_add(1);
            if length < LARGEST_FRAME {
                self.sequence = sequence;
                return Ok(payload);
            }
        }
    }

    pub(crate) fn send(&mut self, payload: &[u8]) {
        let mut rest = payload;
        loop {
            let length = rest.len().min(LARGEST_FRAME);
            self.outbox
                .extend_from_slice(&(length as u32).to_le_bytes()[..3]);
            self.outbox.push(self.sequence);
            self.sequence = self.sequence.wrapping_add(1);
            self.outbox.extend_from_slice(&rest[..length]);
            rest = &rest[length..];
            if length < LARGEST_FRAME {
                return;
            }
        }
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        if !self.outbox.is_empty() {
            let result = self.stream.write_all(&self.outbox);
            self.outbox.clear();
            result?;
        }
        self.stream.flush()
    }

    pub(crate) fn flush_if_large(&mut self) -> io::Result<()> {
        if self.outbox.len() >= FLUSH_ABOVE_BYTES {
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
