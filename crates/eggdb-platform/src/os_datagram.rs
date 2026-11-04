use crate::error::{Error, ErrorKind, Result};
use crate::net::{DatagramSocket, MAX_DATAGRAM_BYTES};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

const SHORTEST_WAIT: Duration = Duration::from_millis(1);

/// A UDP socket made by [`OsNetwork`](crate::OsNetwork): the operating system's own, on Unix
/// systems and Windows alike. Receives on one socket take turns, each setting the socket's read
/// timeout only when its wait differs from the last one's; a wait shorter than a millisecond
/// waits a millisecond.
#[derive(Debug)]
pub struct OsDatagramSocket {
    socket: UdpSocket,
    read_timeout: Mutex<Option<Duration>>,
}

impl OsDatagramSocket {
    pub(crate) fn bind(address: SocketAddr) -> Result<Self> {
        Ok(Self {
            socket: UdpSocket::bind(address)?,
            read_timeout: Mutex::new(None),
        })
    }
}

#[cfg(windows)]
fn ignorable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionReset
    )
}

#[cfg(not(windows))]
fn ignorable(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Interrupted
}

#[cfg(windows)]
fn too_long(error: &io::Error) -> bool {
    const WSAEMSGSIZE: i32 = 10_040;
    error.raw_os_error() == Some(WSAEMSGSIZE)
}

#[cfg(not(windows))]
fn too_long(_: &io::Error) -> bool {
    false
}

impl DatagramSocket for OsDatagramSocket {
    fn local_address(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    fn send_to(&self, datagram: &[u8], to: SocketAddr) -> Result<usize> {
        if datagram.len() > MAX_DATAGRAM_BYTES {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        loop {
            match self.socket.send_to(datagram, to) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                sent => return Ok(sent?),
            }
        }
    }

    fn receive_from(&self, buffer: &mut [u8], wait: Duration) -> Result<(usize, SocketAddr)> {
        if buffer.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        let wait = wait.max(SHORTEST_WAIT);
        let mut read_timeout = self
            .read_timeout
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *read_timeout != Some(wait) {
            self.socket.set_read_timeout(Some(wait))?;
            *read_timeout = Some(wait);
        }
        loop {
            match self.socket.recv_from(buffer) {
                Ok(received) => return Ok(received),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(Error::from(error).with_kind(ErrorKind::TimedOut));
                }
                Err(error) if too_long(&error) => {
                    return Err(Error::from(error).with_kind(ErrorKind::InvalidInput));
                }
                Err(error) if ignorable(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OsDatagramSocket;
    use crate::ErrorKind;
    use crate::net::DatagramSocket;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::time::{Duration, Instant};

    fn loopback() -> OsDatagramSocket {
        OsDatagramSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap()
    }

    #[test]
    fn a_receive_waits_about_as_long_as_asked_and_then_times_out() {
        let socket = loopback();
        let started = Instant::now();
        let error = socket
            .receive_from(&mut [0; 16], Duration::from_millis(30))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert!(started.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn a_wait_shorter_than_a_millisecond_still_receives_what_already_arrived() {
        let receiver = loopback();
        let sender = loopback();
        sender
            .send_to(b"ready", receiver.local_address().unwrap())
            .unwrap();
        let mut buffer = [0; 16];
        let mut received = None;
        for _ in 0..1000 {
            match receiver.receive_from(&mut buffer, Duration::ZERO) {
                Ok(datagram) => {
                    received = Some(datagram);
                    break;
                }
                Err(error) => assert_eq!(error.kind(), ErrorKind::TimedOut),
            }
        }
        let (length, from) = received.unwrap();
        assert_eq!(&buffer[..length], b"ready");
        assert_eq!(from, sender.local_address().unwrap());
    }
}
