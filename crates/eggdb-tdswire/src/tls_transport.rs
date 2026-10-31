//! The TCP side of TDS over TLS ([MS-TDS] 2.2.6.5, 3.2.5.2): during the handshake TLS records
//! travel inside PRELOGIN packets; afterwards they go on the socket as they are. TDS 8 strict
//! connections start TLS at once, with no packets around the handshake.

use std::io::{self, Read, Write};
use std::net::TcpStream;

use crate::messages::PRELOGIN;
use crate::wire::{DEFAULT_PACKET_SIZE, HEADER_BYTES};

const END_OF_MESSAGE: u8 = 0x01;

/// A TCP stream that frames writes in PRELOGIN packets and unframes reads while `framed` is set.
pub(crate) struct TdsTransport {
    pub(crate) tcp: TcpStream,
    prefix: Vec<u8>,
    prefix_offset: usize,
    pub(crate) framed: bool,
    left_in_packet: usize,
    pending_write: Vec<u8>,
    next_packet_id: u8,
}

impl std::fmt::Debug for TdsTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TdsTransport")
            .field("framed", &self.framed)
            .finish_non_exhaustive()
    }
}

impl TdsTransport {
    pub(crate) fn new(tcp: TcpStream, prefix: Vec<u8>, framed: bool) -> Self {
        Self {
            tcp,
            prefix,
            prefix_offset: 0,
            framed,
            left_in_packet: 0,
            pending_write: Vec::new(),
            next_packet_id: 1,
        }
    }

    fn read_header(&mut self) -> io::Result<()> {
        let mut header = [0u8; HEADER_BYTES];
        self.tcp.read_exact(&mut header)?;
        if header[0] != PRELOGIN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a TLS handshake packet is not a PRELOGIN packet",
            ));
        }
        let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
        self.left_in_packet = length.checked_sub(HEADER_BYTES).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "a packet is shorter than its header")
        })?;
        Ok(())
    }
}

impl Read for TdsTransport {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.prefix_offset < self.prefix.len() {
            let available = &self.prefix[self.prefix_offset..];
            let count = available.len().min(buffer.len());
            buffer[..count].copy_from_slice(&available[..count]);
            self.prefix_offset += count;
            return Ok(count);
        }
        if !self.framed {
            return self.tcp.read(buffer);
        }
        while self.left_in_packet == 0 {
            self.read_header()?;
        }
        let wanted = buffer.len().min(self.left_in_packet);
        let read = self.tcp.read(&mut buffer[..wanted])?;
        self.left_in_packet -= read;
        Ok(read)
    }
}

impl Write for TdsTransport {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.framed {
            self.pending_write.extend_from_slice(buffer);
            Ok(buffer.len())
        } else {
            self.tcp.write(buffer)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.framed && !self.pending_write.is_empty() {
            let payload_bytes = DEFAULT_PACKET_SIZE - HEADER_BYTES;
            let pending = std::mem::take(&mut self.pending_write);
            let packets = pending.chunks(payload_bytes).count();
            let mut framed = Vec::with_capacity(pending.len() + packets * HEADER_BYTES);
            for (index, chunk) in pending.chunks(payload_bytes).enumerate() {
                let status = if index + 1 == packets { END_OF_MESSAGE } else { 0 };
                framed.push(PRELOGIN);
                framed.push(status);
                framed.extend_from_slice(&((chunk.len() + HEADER_BYTES) as u16).to_be_bytes());
                framed.extend_from_slice(&[0, 0, self.next_packet_id, 0]);
                self.next_packet_id = self.next_packet_id.wrapping_add(1);
                framed.extend_from_slice(chunk);
            }
            self.tcp.write_all(&framed)?;
        }
        self.tcp.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn handshake_bytes_are_framed_both_ways_then_sent_raw() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let mut transport = TdsTransport::new(accepted, Vec::new(), true);
        transport.write_all(b"hello").unwrap();
        transport.flush().unwrap();
        let mut framed = [0u8; HEADER_BYTES + 5];
        peer.read_exact(&mut framed).unwrap();
        assert_eq!(framed[0], PRELOGIN);
        assert_eq!(framed[1], END_OF_MESSAGE);
        assert_eq!(u16::from_be_bytes([framed[2], framed[3]]), 13);
        assert_eq!(&framed[HEADER_BYTES..], b"hello");

        peer.write_all(&[PRELOGIN, 1, 0, 11, 0, 0, 1, 0, b'a', b'b', b'c']).unwrap();
        let mut unframed = [0u8; 3];
        transport.read_exact(&mut unframed).unwrap();
        assert_eq!(&unframed, b"abc");

        transport.framed = false;
        transport.write_all(b"raw").unwrap();
        transport.flush().unwrap();
        let mut raw = [0u8; 3];
        peer.read_exact(&mut raw).unwrap();
        assert_eq!(&raw, b"raw");
    }

    #[test]
    fn a_packet_of_another_type_during_the_handshake_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let mut transport = TdsTransport::new(accepted, Vec::new(), true);
        peer.write_all(&[0x10, 1, 0, 9, 0, 0, 1, 0, 0]).unwrap();
        let mut byte = [0u8; 1];
        assert!(transport.read(&mut byte).is_err());
    }
}
