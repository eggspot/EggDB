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

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    fn try_receive_from(&self, buffer: &mut [u8]) -> Result<Option<(usize, SocketAddr)>> {
        if buffer.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        Ok(waiting::receive_now(&self.socket, buffer)?)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn receive_batch(
        &self,
        buffers: &mut [&mut [u8]],
        received: &mut Vec<(usize, SocketAddr)>,
        wait: Duration,
    ) -> Result<usize> {
        let Some((first, rest)) = buffers.split_first_mut() else {
            return Err(Error::new(ErrorKind::InvalidInput));
        };
        received.push(self.receive_from(first, wait)?);
        let rest: Vec<&mut [u8]> = rest
            .iter_mut()
            .map(|buffer| &mut **buffer)
            .take_while(|buffer| !buffer.is_empty())
            .collect();
        let more = waiting::receive_many_now(&self.socket, rest, received).unwrap_or(0);
        Ok(1 + more)
    }
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
mod waiting {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    use std::ffi::c_uint;
    use std::ffi::{c_int, c_void};
    use std::io;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
    use std::os::fd::AsRawFd;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    const MSG_DONTWAIT: c_int = 0x40;
    #[cfg(target_vendor = "apple")]
    const MSG_DONTWAIT: c_int = 0x80;
    const AF_INET: u16 = 2;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const AF_INET6: u16 = 10;
    #[cfg(target_vendor = "apple")]
    const AF_INET6: u16 = 30;
    const STORAGE_BYTES: usize = 128;

    #[repr(C, align(8))]
    struct AddressStorage([u8; STORAGE_BYTES]);

    unsafe extern "C" {
        fn recvfrom(
            socket: c_int,
            buffer: *mut c_void,
            length: usize,
            flags: c_int,
            address: *mut c_void,
            address_length: *mut u32,
        ) -> isize;
    }

    fn family(storage: &AddressStorage) -> u16 {
        if cfg!(target_vendor = "apple") {
            u16::from(storage.0[1])
        } else {
            u16::from_ne_bytes([storage.0[0], storage.0[1]])
        }
    }

    fn address_of(storage: &AddressStorage) -> io::Result<SocketAddr> {
        let bytes = &storage.0;
        let port = u16::from_be_bytes([bytes[2], bytes[3]]);
        match family(storage) {
            AF_INET => {
                let ip = Ipv4Addr::new(bytes[4], bytes[5], bytes[6], bytes[7]);
                Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
            }
            AF_INET6 => {
                let flow = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&bytes[8..24]);
                let scope = u32::from_ne_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
                Ok(SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(octets), port, flow, scope)))
            }
            _ => Err(io::Error::from(io::ErrorKind::InvalidData)),
        }
    }

    pub(super) fn receive_now(
        socket: &UdpSocket,
        buffer: &mut [u8],
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        let mut storage = AddressStorage([0; STORAGE_BYTES]);
        loop {
            let mut length = STORAGE_BYTES as u32;
            // SAFETY: `buffer` and `storage` are writable for the lengths passed and outlive
            // the call, and `length` holds the storage's size as recvfrom requires.
            let got = unsafe {
                recvfrom(
                    socket.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    MSG_DONTWAIT,
                    storage.0.as_mut_ptr().cast(),
                    &mut length,
                )
            };
            if got >= 0 {
                let taken = usize::try_from(got).unwrap_or(0).min(buffer.len());
                return Ok(Some((taken, address_of(&storage)?)));
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock => return Ok(None),
                _ => return Err(error),
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[repr(C)]
    struct IoVector {
        base: *mut c_void,
        length: usize,
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[repr(C)]
    struct MessageHeader {
        name: *mut c_void,
        name_length: u32,
        vectors: *mut IoVector,
        vector_count: usize,
        control: *mut c_void,
        control_length: usize,
        flags: c_int,
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[repr(C)]
    struct MultipleMessageHeader {
        header: MessageHeader,
        length: c_uint,
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe extern "C" {
        fn recvmmsg(
            socket: c_int,
            messages: *mut MultipleMessageHeader,
            count: c_uint,
            flags: c_int,
            timeout: *mut c_void,
        ) -> c_int;
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    const MOST_PER_CALL: usize = 1024;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(super) fn receive_many_now(
        socket: &UdpSocket,
        mut buffers: Vec<&mut [u8]>,
        received: &mut Vec<(usize, SocketAddr)>,
    ) -> io::Result<usize> {
        buffers.truncate(MOST_PER_CALL);
        if buffers.is_empty() {
            return Ok(0);
        }
        let lengths: Vec<usize> = buffers.iter().map(|buffer| buffer.len()).collect();
        let mut storages: Vec<AddressStorage> = buffers
            .iter()
            .map(|_| AddressStorage([0; STORAGE_BYTES]))
            .collect();
        let mut vectors: Vec<IoVector> = buffers
            .iter_mut()
            .map(|buffer| IoVector {
                base: buffer.as_mut_ptr().cast(),
                length: buffer.len(),
            })
            .collect();
        let mut messages: Vec<MultipleMessageHeader> = vectors
            .iter_mut()
            .zip(storages.iter_mut())
            .map(|(vector, storage)| MultipleMessageHeader {
                header: MessageHeader {
                    name: storage.0.as_mut_ptr().cast(),
                    name_length: STORAGE_BYTES as u32,
                    vectors: vector,
                    vector_count: 1,
                    control: std::ptr::null_mut(),
                    control_length: 0,
                    flags: 0,
                },
                length: 0,
            })
            .collect();
        let count = c_uint::try_from(messages.len()).unwrap_or(0);
        let got = loop {
            // SAFETY: every header points at a live vector and address storage of the stated
            // lengths, each vector at a live buffer of its length, and all of them outlive the
            // call; a null timeout is allowed.
            let got = unsafe {
                recvmmsg(
                    socket.as_raw_fd(),
                    messages.as_mut_ptr(),
                    count,
                    MSG_DONTWAIT,
                    std::ptr::null_mut(),
                )
            };
            if got >= 0 {
                break usize::try_from(got).unwrap_or(0);
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock => return Ok(0),
                _ => return Err(error),
            }
        };
        let mut taken = 0;
        for ((message, storage), capacity) in messages.iter().zip(storages.iter()).zip(lengths).take(got) {
            let Ok(sender) = address_of(storage) else {
                break;
            };
            let length = usize::try_from(message.length).unwrap_or(0).min(capacity);
            received.push((length, sender));
            taken += 1;
        }
        Ok(taken)
    }
}

#[cfg(test)]
mod tests {
    use super::OsDatagramSocket;
    use crate::ErrorKind;
    use crate::net::DatagramSocket;
    use std::net::{Ipv4Addr, SocketAddr};
    use eggdb_platform_core::time::Instant;
    use std::time::Duration;

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
