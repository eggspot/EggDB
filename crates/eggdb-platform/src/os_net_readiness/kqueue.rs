use super::queue::{Connecting, Interest, Readiness};
use crate::net::NetBackend;
use crate::os_net::OsStream;
use std::collections::HashMap;
use std::ffi::{c_int, c_long, c_void};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;
use std::time::Duration;

const LONGEST_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

pub(super) const BACKEND: NetBackend = NetBackend::Kqueue;

const EVFILT_READ: i16 = -1;
const EVFILT_WRITE: i16 = -2;
const EV_ADD: u16 = 0x1;
const EV_DELETE: u16 = 0x2;
const EV_ONESHOT: u16 = 0x10;
const EV_ERROR: u16 = 0x4000;

const AF_INET: c_int = 2;
const AF_INET6: c_int = 30;
const SOCK_STREAM: c_int = 1;
const SOL_SOCKET: c_int = 0xffff;
const SO_NOSIGPIPE: c_int = 0x1022;
const IPPROTO_TCP: c_int = 6;
const TCP_NODELAY: c_int = 1;
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
    #[cfg(test)]
    fn getsockopt(
        fd: c_int,
        level: c_int,
        name: c_int,
        value: *mut c_void,
        length: *mut u32,
    ) -> c_int;
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
    armed: HashMap<RawFd, Interest>,
    pending_adds: Vec<Kevent>,
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
            armed: HashMap::new(),
            pending_adds: Vec::new(),
        })
    }

    pub(super) fn want(&mut self, fd: RawFd, wanted: Interest) -> io::Result<()> {
        let mut armed = self.armed.remove(&fd).unwrap_or(Interest::NONE);
        let mut outcome = Ok(());
        for (filter, now, wanted) in [
            (EVFILT_READ, &mut armed.read, wanted.read),
            (EVFILT_WRITE, &mut armed.write, wanted.write),
        ] {
            if *now == wanted {
                continue;
            }
            if wanted {
                self.pending_adds.push(add_once(fd, filter));
                *now = true;
            } else if self.drop_pending_add(fd, filter) {
                *now = false;
            } else {
                outcome = outcome.and(self.delete_filter(fd, filter));
                *now = false;
            }
        }
        if armed != Interest::NONE {
            self.armed.insert(fd, armed);
        }
        outcome
    }

    fn drop_pending_add(&mut self, fd: RawFd, filter: i16) -> bool {
        let ident = fd.unsigned_abs() as usize;
        let before = self.pending_adds.len();
        self.pending_adds
            .retain(|change| change.ident != ident || change.filter != filter);
        self.pending_adds.len() != before
    }

    fn delete_filter(&self, fd: RawFd, filter: i16) -> io::Result<()> {
        let change = Kevent {
            ident: fd.unsigned_abs() as usize,
            filter,
            flags: EV_DELETE,
            ..Kevent::default()
        };
        self.apply_alone(&change)
    }

    fn apply_alone(&self, change: &Kevent) -> io::Result<()> {
        // SAFETY: change is a valid kevent read during the call; no events are asked for, so
        // the call returns at once without writing through a null pointer.
        let result = unsafe {
            kevent(
                self.kqueue.as_raw_fd(),
                change,
                1,
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

    fn apply_pending_one_by_one(&mut self, ready: &mut Vec<Readiness>) {
        let pending = std::mem::take(&mut self.pending_adds);
        for change in &pending {
            if let Err(error) = self.apply_alone(change) {
                let fd = RawFd::try_from(change.ident).unwrap_or(RawFd::MAX);
                self.forget_armed(fd, change.filter);
                ready.push(Readiness {
                    fd,
                    read: change.filter == EVFILT_READ,
                    write: change.filter == EVFILT_WRITE,
                    change_error: Some(error.raw_os_error().unwrap_or(0)),
                });
            }
        }
        self.pending_adds = pending;
        self.pending_adds.clear();
    }

    fn forget_armed(&mut self, fd: RawFd, filter: i16) {
        if let Some(armed) = self.armed.get_mut(&fd) {
            if filter == EVFILT_READ {
                armed.read = false;
            } else {
                armed.write = false;
            }
            if *armed == Interest::NONE {
                self.armed.remove(&fd);
            }
        }
    }

    pub(super) fn wait(
        &mut self,
        timeout: Option<Duration>,
        ready: &mut Vec<Readiness>,
    ) -> io::Result<()> {
        let room = c_int::try_from(self.events.len()).unwrap_or(c_int::MAX);
        let changes = c_int::try_from(self.pending_adds.len()).unwrap_or(c_int::MAX);
        if changes == c_int::MAX {
            self.apply_pending_one_by_one(ready);
            return Ok(());
        }
        let timespec = timeout.map(|timeout| {
            let timeout = timeout.min(LONGEST_WAIT);
            Timespec {
                seconds: c_long::try_from(timeout.as_secs()).unwrap_or(c_long::MAX),
                nanoseconds: c_long::from(timeout.subsec_nanos()),
            }
        });
        let limit: *const Timespec = timespec.as_ref().map_or(ptr::null(), ptr::from_ref);
        let change_list: *const Kevent = if changes == 0 {
            ptr::null()
        } else {
            self.pending_adds.as_ptr()
        };
        // SAFETY: change_list is null or points at changes initialised kevents the kernel only
        // reads, events holds room initialised kevents the kernel may overwrite, the two do not
        // overlap, and limit is null or points at timespec, which outlives the call.
        let count = unsafe {
            kevent(
                self.kqueue.as_raw_fd(),
                change_list,
                changes,
                self.events.as_mut_ptr(),
                room,
                limit,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(EINTR) {
                self.pending_adds.clear();
                return Ok(());
            }
            if changes > 0 {
                self.apply_pending_one_by_one(ready);
                return Ok(());
            }
            return Err(error);
        }
        self.pending_adds.clear();
        let count = count.unsigned_abs() as usize;
        for at in 0..count {
            let event = self.events[at];
            let fd = RawFd::try_from(event.ident).unwrap_or(RawFd::MAX);
            let change_error = (event.flags & EV_ERROR != 0)
                .then(|| i32::try_from(event.data).unwrap_or(i32::MAX))
                .filter(|&code| code != 0);
            if event.flags & EV_ERROR != 0 && change_error.is_none() {
                continue;
            }
            self.forget_armed(fd, event.filter);
            ready.push(Readiness {
                fd,
                read: event.filter == EVFILT_READ,
                write: event.filter == EVFILT_WRITE,
                change_error,
            });
        }
        Ok(())
    }
}

fn add_once(fd: RawFd, filter: i16) -> Kevent {
    Kevent {
        ident: fd.unsigned_abs() as usize,
        filter,
        flags: EV_ADD | EV_ONESHOT,
        ..Kevent::default()
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
    turn_on(socket.as_raw_fd(), SOL_SOCKET, SO_NOSIGPIPE)
}

pub(crate) fn prepare_listener(listener: &TcpListener) -> io::Result<()> {
    turn_on(listener.as_raw_fd(), SOL_SOCKET, SO_NOSIGPIPE)?;
    let _ = turn_on(listener.as_raw_fd(), IPPROTO_TCP, TCP_NODELAY);
    Ok(())
}

pub(crate) fn accept(listener: &TcpListener) -> io::Result<OsStream> {
    listener
        .accept()
        .map(|(stream, _)| OsStream::inherited_from_listener(stream))
}

#[cfg(test)]
pub(crate) fn sigpipe_suppressed(socket: &TcpStream) -> io::Result<bool> {
    let mut value: c_int = 0;
    let mut length = size_of::<c_int>() as u32;
    // SAFETY: value and length are an int and its size that outlive the call, which writes at
    // most length bytes into value.
    let result = unsafe {
        getsockopt(
            socket.as_raw_fd(),
            SOL_SOCKET,
            SO_NOSIGPIPE,
            (&mut value as *mut c_int).cast(),
            &mut length,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(value != 0)
}

fn turn_on(fd: RawFd, level: c_int, option: c_int) -> io::Result<()> {
    let on: c_int = 1;
    // SAFETY: on is an int that outlives the call, which only reads it.
    let result = unsafe {
        setsockopt(
            fd,
            level,
            option,
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
