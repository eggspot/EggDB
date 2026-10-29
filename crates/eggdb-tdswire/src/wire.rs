use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant};

pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(200);
const SHORTEST_WAIT: Duration = Duration::from_millis(1);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const READ_CHUNK_BYTES: usize = 64 * 1024;
const FLUSH_ABOVE_BYTES: usize = 256 * 1024;
const REFUSAL_LINGER: Duration = Duration::from_millis(200);
const REFUSAL_LINGER_BYTES: usize = 256 * 1024;
pub(crate) const HEADER_BYTES: usize = 8;
pub(crate) const DEFAULT_PACKET_SIZE: usize = 4096;
const END_OF_MESSAGE: u8 = 0x01;
const TLS_HANDSHAKE: u8 = 0x16;

#[derive(Debug)]
pub(crate) enum WireError {
    Closed,
    Stopping,
    TimedOut,
    TooLarge,
    Malformed(&'static str),
    Tls,
    Io,
}

pub(crate) struct Message {
    pub(crate) kind: u8,
    pub(crate) status: u8,
    pub(crate) body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Packet {
    pub(crate) kind: u8,
    pub(crate) status: u8,
    pub(crate) last: bool,
}

pub(crate) struct Wire<'a> {
    stream: TcpStream,
    stop: &'a (dyn Fn() -> bool + Sync),
    pub(crate) deadline: Option<Instant>,
    outbox: Vec<u8>,
    pending: Vec<u8>,
    pending_kind: u8,
    next_packet_id: u8,
    read_timeout: Duration,
    pub(crate) packet_size: usize,
    pub(crate) spid: u16,
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
            pending: Vec::new(),
            pending_kind: 0,
            next_packet_id: 1,
            read_timeout: POLL_INTERVAL,
            packet_size: DEFAULT_PACKET_SIZE,
            spid: 0,
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

    pub(crate) fn read_message(&mut self, limit: usize) -> Result<Message, WireError> {
        let mut body = Vec::new();
        let read = self.read_message_into(&mut body, limit);
        if read.is_err() {
            body.fill(0);
            std::hint::black_box(&body);
        }
        read.map(|(kind, status)| Message { kind, status, body })
    }

    fn read_message_into(
        &mut self,
        body: &mut Vec<u8>,
        limit: usize,
    ) -> Result<(u8, u8), WireError> {
        let first = self.read_packet(body, limit, true)?;
        self.read_rest_of_message(&first, body, limit)?;
        Ok((first.kind, first.status))
    }

    pub(crate) fn read_first_packet(
        &mut self,
        body: &mut Vec<u8>,
        limit: usize,
    ) -> Result<Packet, WireError> {
        let read = self.read_packet(body, limit, true);
        if read.is_err() {
            body.fill(0);
            std::hint::black_box(&*body);
        }
        read
    }

    pub(crate) fn read_rest_of_message(
        &mut self,
        first: &Packet,
        body: &mut Vec<u8>,
        limit: usize,
    ) -> Result<(), WireError> {
        let mut last = first.last;
        while !last {
            let packet = self.read_packet(body, limit, false)?;
            if packet.kind != first.kind {
                return Err(WireError::Malformed(
                    "the packets of one message have different types",
                ));
            }
            last = packet.last;
        }
        Ok(())
    }

    pub(crate) fn read_packet(
        &mut self,
        body: &mut Vec<u8>,
        limit: usize,
        first: bool,
    ) -> Result<Packet, WireError> {
        let mut header = [0u8; HEADER_BYTES];
        self.read_exact(&mut header[..1])?;
        if first && header[0] == TLS_HANDSHAKE {
            return Err(WireError::Tls);
        }
        self.read_exact(&mut header[1..])?;
        let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
        if length < HEADER_BYTES {
            return Err(WireError::Malformed("a packet is shorter than its header"));
        }
        let payload = length - HEADER_BYTES;
        if body.len().saturating_add(payload) > limit {
            return Err(WireError::TooLarge);
        }
        let mut read = 0;
        while read < payload {
            let chunk = (payload - read).min(READ_CHUNK_BYTES);
            let start = body.len();
            body.resize(start + chunk, 0);
            self.read_exact(&mut body[start..])?;
            read += chunk;
        }
        Ok(Packet {
            kind: header[0],
            status: header[1],
            last: header[1] & END_OF_MESSAGE != 0,
        })
    }

    pub(crate) fn begin_message(&mut self, kind: u8) {
        self.pending.clear();
        self.pending_kind = kind;
        self.next_packet_id = 1;
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.pending.extend_from_slice(bytes);
        let payload = self.packet_size.saturating_sub(HEADER_BYTES).max(1);
        if self.pending.len() > payload {
            let whole = (self.pending.len() - 1) / payload * payload;
            let mut start = 0;
            while start < whole {
                self.push_packet(start, start + payload, false);
                start += payload;
            }
            self.pending.drain(..whole);
        }
        if self.outbox.len() >= FLUSH_ABOVE_BYTES {
            self.flush()?;
        }
        Ok(())
    }

    fn push_packet(&mut self, start: usize, end: usize, last: bool) {
        let length = (end - start + HEADER_BYTES) as u16;
        self.outbox.push(self.pending_kind);
        self.outbox.push(if last { END_OF_MESSAGE } else { 0 });
        self.outbox.extend_from_slice(&length.to_be_bytes());
        self.outbox.extend_from_slice(&self.spid.to_be_bytes());
        self.outbox.push(self.next_packet_id);
        self.outbox.push(0);
        self.next_packet_id = self.next_packet_id.wrapping_add(1);
        let (outbox, pending) = (&mut self.outbox, &self.pending);
        outbox.extend_from_slice(&pending[start..end]);
    }

    pub(crate) fn end_message(&mut self) -> io::Result<()> {
        let end = self.pending.len();
        self.push_packet(0, end, true);
        self.pending.clear();
        self.flush()
    }

    pub(crate) fn send_message(&mut self, kind: u8, body: &[u8]) -> io::Result<()> {
        self.begin_message(kind);
        self.write(body)?;
        self.end_message()
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        if !self.outbox.is_empty() {
            let result = self.stream.write_all(&self.outbox);
            self.outbox.clear();
            result?;
        }
        self.stream.flush()
    }

    pub(crate) fn close_after_refusal(&mut self) {
        let _ = self.flush();
        let _ = self.stream.shutdown(Shutdown::Write);
        let deadline = Instant::now() + REFUSAL_LINGER;
        let mut buffer = [0u8; 8 * 1024];
        let mut discarded = 0;
        while discarded < REFUSAL_LINGER_BYTES {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || self.stream.set_read_timeout(Some(left)).is_err() {
                return;
            }
            match self.stream.read(&mut buffer) {
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
        self.stream
            .peer_addr()
            .map_or_else(|_| "an unknown address".to_owned(), |peer| peer.to_string())
    }
}
