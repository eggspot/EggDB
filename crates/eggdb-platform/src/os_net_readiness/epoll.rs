use super::queue::{Connecting, Interest, Readiness};
use crate::net::NetBackend;
use std::ffi::{c_int, c_void};
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub(super) const BACKEND: NetBackend = NetBackend::Epoll;

const EPOLL_CLOEXEC: c_int = 0x8_0000;
const EPOLL_CTL_ADD: c_int = 1;
const EPOLL_CTL_DEL: c_int = 2;
const EPOLL_CTL_MOD: c_int = 3;
const EPOLLIN: u32 = 0x1;
const EPOLLOUT: u32 = 0x4;
const EPOLLERR: u32 = 0x8;
const EPOLLHUP: u32 = 0x10;

const AF_INET: c_int = 2;
const AF_INET6: c_int = 10;
const SOCK_STREAM: c_int = 1;
const SOCK_CLOEXEC: c_int = 0x8_0000;
const EINPROGRESS: i32 = 115;
const EINTR: i32 = 4;

const MOST_EVENTS: usize = 1024;

unsafe extern "C" {
    fn epoll_create1(flags: c_int) -> c_int;
    fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut EpollEvent) -> c_int;
    fn epoll_wait(epfd: c_int, events: *mut EpollEvent, maxevents: c_int, timeout: c_int) -> c_int;
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn connect(fd: c_int, address: *const c_void, length: u32) -> c_int;
}

#[cfg_attr(target_arch = "x86_64", repr(C, packed))]
#[cfg_attr(not(target_arch = "x86_64"), repr(C))]
#[derive(Debug, Clone, Copy, Default)]
struct EpollEvent {
    events: u32,
    data: u64,
}

const _: () = assert!(
    size_of::<EpollEvent>()
        == if cfg!(any(target_arch = "x86_64", target_arch = "x86")) {
            12
        } else {
            16
        }
);

#[derive(Debug)]
pub(super) struct Selector {
    epoll: OwnedFd,
    events: Vec<EpollEvent>,
}

impl Selector {
    pub(super) fn new(capacity: usize) -> io::Result<Self> {
        // SAFETY: epoll_create1 takes no pointers; a negative result is reported as an error.
        let fd = unsafe { epoll_create1(EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a new epoll descriptor that nothing else owns.
        let epoll = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self {
            epoll,
            events: vec![EpollEvent::default(); capacity.clamp(1, MOST_EVENTS)],
        })
    }

    pub(super) fn change(&self, fd: RawFd, from: Interest, to: Interest) -> io::Result<()> {
        if from == to {
            return Ok(());
        }
        let op = if from == Interest::NONE {
            EPOLL_CTL_ADD
        } else if to == Interest::NONE {
            EPOLL_CTL_DEL
        } else {
            EPOLL_CTL_MOD
        };
        let mut event = EpollEvent {
            events: if to.read { EPOLLIN } else { 0 } | if to.write { EPOLLOUT } else { 0 },
            data: u64::from(fd.unsigned_abs()),
        };
        // SAFETY: event is a valid epoll_event that outlives the call, which only reads it.
        let result = unsafe { epoll_ctl(self.epoll.as_raw_fd(), op, fd, &mut event) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn wait(&mut self, block: bool, ready: &mut Vec<Readiness>) -> io::Result<()> {
        let room = c_int::try_from(self.events.len()).unwrap_or(c_int::MAX);
        let count = loop {
            // SAFETY: events holds room initialised epoll_events the kernel may overwrite.
            let count = unsafe {
                epoll_wait(
                    self.epoll.as_raw_fd(),
                    self.events.as_mut_ptr(),
                    room,
                    if block { -1 } else { 0 },
                )
            };
            if count >= 0 {
                break count.unsigned_abs() as usize;
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(EINTR) {
                return Err(error);
            }
        };
        ready.extend(self.events.iter().take(count).map(|event| {
            let EpollEvent { events, data } = *event;
            let failed = events & (EPOLLERR | EPOLLHUP) != 0;
            Readiness {
                fd: RawFd::try_from(data).unwrap_or(RawFd::MAX),
                read: failed || events & EPOLLIN != 0,
                write: failed || events & EPOLLOUT != 0,
            }
        }));
        Ok(())
    }
}

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
}

const _: () = assert!(size_of::<SockaddrIn>() == 16 && size_of::<SockaddrIn6>() == 28);

pub(super) fn connect_without_waiting(address: SocketAddr) -> io::Result<Connecting> {
    let domain = if address.is_ipv4() { AF_INET } else { AF_INET6 };
    // SAFETY: socket takes no pointers; a negative result is reported as an error.
    let fd = unsafe { socket(domain, SOCK_STREAM | SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a new socket that nothing else owns.
    let socket = TcpStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
    socket.set_nonblocking(true)?;
    let result = match address {
        SocketAddr::V4(address) => {
            let raw = SockaddrIn {
                family: AF_INET as u16,
                port: address.port().to_be(),
                address: address.ip().octets(),
                zero: [0; 8],
            };
            // SAFETY: raw is a sockaddr_in of the length given, read only during the call.
            unsafe { connect(fd, (&raw as *const SockaddrIn).cast(), 16) }
        }
        SocketAddr::V6(address) => {
            let raw = SockaddrIn6 {
                family: AF_INET6 as u16,
                port: address.port().to_be(),
                flow_info: address.flowinfo(),
                address: address.ip().octets(),
                scope_id: address.scope_id(),
            };
            // SAFETY: raw is a sockaddr_in6 of the length given, read only during the call.
            unsafe { connect(fd, (&raw as *const SockaddrIn6).cast(), 28) }
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
