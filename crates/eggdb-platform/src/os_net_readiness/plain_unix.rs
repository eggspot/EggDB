use super::queue::{Connecting, Interest, Readiness};
use crate::net::NetBackend;
use std::ffi::{c_int, c_void};
use std::io;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6, TcpStream};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

pub(super) const BACKEND: NetBackend = NetBackend::Poll;

#[derive(Debug)]
pub(super) enum Selector {}

impl Selector {
    pub(super) fn new(_: usize) -> io::Result<Self> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    pub(super) fn want(&mut self, _: RawFd, _: Interest) -> io::Result<()> {
        match *self {}
    }

    pub(super) fn wait(&mut self, _: Option<Duration>, _: &mut Vec<Readiness>) -> io::Result<()> {
        match *self {}
    }
}

const AF_INET: c_int = 2;
const F_SETFD: c_int = 2;
const FD_CLOEXEC: c_int = 1;
const EINTR: i32 = 4;

cfg_select! {
    any(target_os = "illumos", target_os = "solaris") => {
        const AF_INET6: c_int = 26;
        const SOCK_STREAM: c_int = 2;
        const EINPROGRESS: i32 = 150;
    }
    any(target_os = "netbsd", target_os = "openbsd") => {
        const AF_INET6: c_int = 24;
        const SOCK_STREAM: c_int = 1;
        const EINPROGRESS: i32 = 36;
    }
    _ => {
        const AF_INET6: c_int = 28;
        const SOCK_STREAM: c_int = 1;
        const EINPROGRESS: i32 = 36;
    }
}

unsafe extern "C" {
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn connect(fd: c_int, address: *const c_void, length: u32) -> c_int;
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
}

cfg_select! {
    any(target_os = "illumos", target_os = "solaris") => {
        #[repr(C)]
        struct SockaddrIn {
            family: u16,
            port: u16,
            address: [u8; 4],
            zero: [u8; 8],
        }

        #[repr(C)]
        struct SockaddrIn6 {
            family: u16,
            port: u16,
            flow_info: u32,
            address: [u8; 16],
            scope_id: u32,
            source_id: u32,
        }

        const _: () = assert!(size_of::<SockaddrIn>() == 16 && size_of::<SockaddrIn6>() == 32);

        fn ipv4(address: &SocketAddrV4) -> SockaddrIn {
            SockaddrIn {
                family: AF_INET as u16,
                port: address.port().to_be(),
                address: address.ip().octets(),
                zero: [0; 8],
            }
        }

        fn ipv6(address: &SocketAddrV6) -> SockaddrIn6 {
            SockaddrIn6 {
                family: AF_INET6 as u16,
                port: address.port().to_be(),
                flow_info: address.flowinfo(),
                address: address.ip().octets(),
                scope_id: address.scope_id(),
                source_id: 0,
            }
        }
    }
    _ => {
        #[repr(C)]
        struct SockaddrIn {
            length: u8,
            family: u8,
            port: u16,
            address: [u8; 4],
            zero: [u8; 8],
        }

        #[repr(C)]
        struct SockaddrIn6 {
            length: u8,
            family: u8,
            port: u16,
            flow_info: u32,
            address: [u8; 16],
            scope_id: u32,
        }

        const _: () = assert!(size_of::<SockaddrIn>() == 16 && size_of::<SockaddrIn6>() == 28);

        fn ipv4(address: &SocketAddrV4) -> SockaddrIn {
            SockaddrIn {
                length: size_of::<SockaddrIn>() as u8,
                family: AF_INET as u8,
                port: address.port().to_be(),
                address: address.ip().octets(),
                zero: [0; 8],
            }
        }

        fn ipv6(address: &SocketAddrV6) -> SockaddrIn6 {
            SockaddrIn6 {
                length: size_of::<SockaddrIn6>() as u8,
                family: AF_INET6 as u8,
                port: address.port().to_be(),
                flow_info: address.flowinfo(),
                address: address.ip().octets(),
                scope_id: address.scope_id(),
            }
        }
    }
}

pub(super) fn connect_without_waiting(address: SocketAddr) -> io::Result<Connecting> {
    let domain = if address.is_ipv4() { AF_INET } else { AF_INET6 };
    // SAFETY: socket takes no pointers; a negative result is reported as an error.
    let fd = unsafe { socket(domain, SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a new socket that nothing else owns.
    let socket = TcpStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
    // SAFETY: F_SETFD takes one int argument and no pointers.
    if unsafe { fcntl(fd, F_SETFD, FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    socket.set_nonblocking(true)?;
    let result = match address {
        SocketAddr::V4(address) => {
            let raw = ipv4(&address);
            // SAFETY: raw is a sockaddr_in of the length given, read only during the call.
            unsafe {
                connect(
                    fd,
                    (&raw as *const SockaddrIn).cast(),
                    size_of::<SockaddrIn>() as u32,
                )
            }
        }
        SocketAddr::V6(address) => {
            let raw = ipv6(&address);
            // SAFETY: raw is a sockaddr_in6 of the length given, read only during the call.
            unsafe {
                connect(
                    fd,
                    (&raw as *const SockaddrIn6).cast(),
                    size_of::<SockaddrIn6>() as u32,
                )
            }
        }
    };
    if result == 0 {
        return Ok(Connecting::Connected(socket));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(EINPROGRESS | EINTR) => Ok(Connecting::Pending(socket)),
        _ => Err(error),
    }
}
