use super::queue::{Connecting, Interest, Readiness};
use crate::net::NetBackend;
use std::ffi::{c_int, c_long, c_void};
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;

pub(super) const BACKEND: NetBackend = NetBackend::Kqueue;

const EVFILT_READ: i16 = -1;
const EVFILT_WRITE: i16 = -2;
const EV_ADD: u16 = 0x1;
const EV_DELETE: u16 = 0x2;

const AF_INET: c_int = 2;
const AF_INET6: c_int = 30;
const SOCK_STREAM: c_int = 1;
const SOL_SOCKET: c_int = 0xffff;
const SO_NOSIGPIPE: c_int = 0x1022;
const F_SETFD: c_int = 2;
const FD_CLOEXEC: c_int = 1;
const EINPROGRESS: i32 = 36;
const EINTR: i32 = 4;

const MOST_EVENTS: usize = 1024;

unsafe extern "C" {
    fn kqueue() -> c_int;
    fn kevent(
        kq: c_int,
        changes: *const Kevent,
        change_count: c_int,
        events: *mut Kevent,
        event_count: c_int,
        timeout: *const Timespec,
    ) -> c_int;
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn connect(fd: c_int, address: *const c_void, length: u32) -> c_int;
    fn setsockopt(fd: c_int, level: c_int, name: c_int, value: *const c_void, length: u32)
    -> c_int;
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Kevent {
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: isize,
    udata: usize,
}

#[repr(C)]
struct Timespec {
    seconds: c_long,
    nanoseconds: c_long,
}

const _: () = assert!(size_of::<Kevent>() == 3 * size_of::<usize>() + 8);

#[derive(Debug)]
pub(super) struct Selector {
    kqueue: OwnedFd,
    events: Vec<Kevent>,
}

impl Selector {
    pub(super) fn new(capacity: usize) -> io::Result<Self> {
        // SAFETY: kqueue takes no arguments; a negative result is reported as an error.
        let fd = unsafe { kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a new kqueue that nothing else owns.
        let kqueue = unsafe { OwnedFd::from_raw_fd(fd) };
        close_on_exec(fd)?;
        Ok(Self {
            kqueue,
            events: vec![Kevent::default(); capacity.clamp(1, MOST_EVENTS)],
        })
    }

    pub(super) fn change(&self, fd: RawFd, from: Interest, to: Interest) -> io::Result<()> {
        let mut changes = [Kevent::default(); 2];
        let mut count: c_int = 0;
        for (filter, was, wanted) in [
            (EVFILT_READ, from.read, to.read),
            (EVFILT_WRITE, from.write, to.write),
        ] {
            if was != wanted
                && let Some(change) = changes.get_mut(count.unsigned_abs() as usize)
            {
                *change = Kevent {
                    ident: fd.unsigned_abs() as usize,
                    filter,
                    flags: if wanted { EV_ADD } else { EV_DELETE },
                    ..Kevent::default()
                };
                count += 1;
            }
        }
        if count == 0 {
            return Ok(());
        }
        // SAFETY: changes holds count valid kevents read during the call; no events are
        // asked for, so the call returns at once without writing through a null pointer.
        let result = unsafe {
            kevent(
                self.kqueue.as_raw_fd(),
                changes.as_ptr(),
                count,
                ptr::null_mut(),
                0,
                ptr::null(),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn wait(&mut self, block: bool, ready: &mut Vec<Readiness>) -> io::Result<()> {
        let room = c_int::try_from(self.events.len()).unwrap_or(c_int::MAX);
        let at_once = Timespec {
            seconds: 0,
            nanoseconds: 0,
        };
        let timeout: *const Timespec = if block { ptr::null() } else { &at_once };
        let count = loop {
            // SAFETY: events holds room initialised kevents the kernel may overwrite, and
            // timeout is null or points at at_once, which outlives the call.
            let count = unsafe {
                kevent(
                    self.kqueue.as_raw_fd(),
                    ptr::null(),
                    0,
                    self.events.as_mut_ptr(),
                    room,
                    timeout,
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
        ready.extend(self.events.iter().take(count).map(|event| Readiness {
            fd: RawFd::try_from(event.ident).unwrap_or(RawFd::MAX),
            read: event.filter == EVFILT_READ,
            write: event.filter == EVFILT_WRITE,
        }));
        Ok(())
    }
}

fn close_on_exec(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_SETFD takes one int argument and no pointers.
    let result = unsafe { fcntl(fd, F_SETFD, FD_CLOEXEC) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn suppress_sigpipe(socket: &TcpStream) -> io::Result<()> {
    let on: c_int = 1;
    // SAFETY: on is an int that outlives the call, which only reads it.
    let result = unsafe {
        setsockopt(
            socket.as_raw_fd(),
            SOL_SOCKET,
            SO_NOSIGPIPE,
            (&on as *const c_int).cast(),
            size_of::<c_int>() as u32,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

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

pub(super) fn connect_without_waiting(address: SocketAddr) -> io::Result<Connecting> {
    let domain = if address.is_ipv4() { AF_INET } else { AF_INET6 };
    // SAFETY: socket takes no pointers; a negative result is reported as an error.
    let fd = unsafe { socket(domain, SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a new socket that nothing else owns.
    let socket = TcpStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
    close_on_exec(fd)?;
    suppress_sigpipe(&socket)?;
    socket.set_nonblocking(true)?;
    let result = match address {
        SocketAddr::V4(address) => {
            let raw = SockaddrIn {
                length: 16,
                family: AF_INET as u8,
                port: address.port().to_be(),
                address: address.ip().octets(),
                zero: [0; 8],
            };
            // SAFETY: raw is a sockaddr_in of the length given, read only during the call.
            unsafe { connect(fd, (&raw as *const SockaddrIn).cast(), 16) }
        }
        SocketAddr::V6(address) => {
            let raw = SockaddrIn6 {
                length: 28,
                family: AF_INET6 as u8,
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
