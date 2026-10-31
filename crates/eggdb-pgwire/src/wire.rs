use crate::messages::{MAX_STARTUP_PACKET_BYTES, Outbox};
use eggdb_tls::{ServerConnection, SessionInfo};
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(200);
const SHORTEST_WAIT: Duration = Duration::from_millis(1);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const READ_CHUNK_BYTES: usize = 64 * 1024;
const REFUSAL_LINGER: Duration = Duration::from_millis(200);
const REFUSAL_LINGER_BYTES: usize = 256 * 1024;
const FLUSH_ABOVE_BYTES: usize = 256 * 1024;
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Wire<'a> {
    stream: ServerConnection,
    peeked: Option<u8>,
    stop: &'a (dyn Fn() -> bool + Sync),
    pub(crate) deadline: Option<Instant>,
    pub(crate) outbox: Outbox,
    read_timeout: Duration,
}

impl<'a> Wire<'a> {
    pub(crate) fn new(
        stream: ServerConnection,
        stop: &'a (dyn Fn() -> bool + Sync),
        deadline: Option<Instant>,
    ) -> io::Result<Self> {
        stream.set_blocking()?;
        stream.set_nodelay()?;
        stream.set_read_timeout(Some(POLL_INTERVAL))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        Ok(Self {
            stream,
            peeked: None,
            stop,
            deadline,
            outbox: Outbox::new(),
            read_timeout: POLL_INTERVAL,
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

    fn set_read_timeout(&mut self, wait: Duration) -> Result<(), WireError> {
        self.stream
            .set_read_timeout(Some(wait))
            .map_err(|_| WireError::Io)?;
        self.read_timeout = wait;
        Ok(())
    }

    fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), WireError> {
        let mut filled = 0;
        if let (Some(byte), Some(first)) = (self.peeked, buffer.first_mut()) {
            *first = byte;
            self.peeked = None;
            filled = 1;
        }
        while filled < buffer.len() {
            let wait = self.next_wait()?;
            if wait != self.read_timeout {
                self.set_read_timeout(wait)?;
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

    pub(crate) fn close_after_refusal(&mut self) {
        self.stream.close_tls();
        self.stream.shutdown_write();
        let deadline = Instant::now() + REFUSAL_LINGER;
        let mut buffer = [0u8; 8 * 1024];
        let mut discarded = 0;
        while discarded < REFUSAL_LINGER_BYTES {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match self.stream.drain(&mut buffer, left) {
                Ok(0) => return,
                Ok(count) => discarded += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return,
            }
        }
    }

    pub(crate) fn peer(&self) -> String {
        self.stream.peer_text()
    }

    pub(crate) fn is_local(&self) -> bool {
        self.stream.is_local()
    }

    pub(crate) fn peer_is_loopback(&self) -> bool {
        self.stream.peer_is_loopback()
    }

    pub(crate) fn peek_byte(&mut self) -> Result<u8, WireError> {
        if let Some(byte) = self.peeked {
            return Ok(byte);
        }
        let mut byte = [0u8; 1];
        self.read_exact(&mut byte)?;
        self.peeked = Some(byte[0]);
        Ok(byte[0])
    }

    pub(crate) fn start_tls(&mut self, config: &eggdb_tls::ServerConfig) -> Result<(), String> {
        let prefix: Vec<u8> = self.peeked.take().into_iter().collect();
        let timeout = self
            .deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .filter(|left| !left.is_zero())
            .unwrap_or(TLS_HANDSHAKE_TIMEOUT);
        self.stream
            .upgrade(config, prefix, timeout, Some(POLL_INTERVAL))
            .map_err(|error| error.to_string())?;
        self.read_timeout = POLL_INTERVAL;
        Ok(())
    }

    pub(crate) fn tls(&self) -> Option<&SessionInfo> {
        self.stream.tls()
    }
}
