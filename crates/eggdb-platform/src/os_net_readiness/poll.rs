use super::queue::{Interest, Readiness};
use std::collections::HashMap;
use std::ffi::{c_int, c_short};
use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

const POLLIN: c_short = 0x1;
const POLLOUT: c_short = 0x4;
const POLLERR: c_short = 0x8;
const POLLHUP: c_short = 0x10;
const POLLNVAL: c_short = 0x20;
const EINTR: i32 = 4;

#[cfg(any(target_os = "linux", target_os = "illumos", target_os = "solaris"))]
type Nfds = std::ffi::c_ulong;

#[cfg(not(any(target_os = "linux", target_os = "illumos", target_os = "solaris")))]
type Nfds = std::ffi::c_uint;

unsafe extern "C" {
    fn poll(fds: *mut PollFd, count: Nfds, timeout: c_int) -> c_int;
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

const _: () = assert!(size_of::<PollFd>() == 8);

#[derive(Debug, Default)]
pub(super) struct Selector {
    fds: Vec<PollFd>,
    slots: HashMap<RawFd, usize>,
}

impl Selector {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn change(&mut self, fd: RawFd, to: Interest) -> io::Result<()> {
        let events = if to.read { POLLIN } else { 0 } | if to.write { POLLOUT } else { 0 };
        match (self.slots.get(&fd).copied(), events) {
            (Some(slot), 0) => {
                self.fds.swap_remove(slot);
                self.slots.remove(&fd);
                if let Some(moved) = self.fds.get(slot) {
                    self.slots.insert(moved.fd, slot);
                }
            }
            (Some(slot), events) => {
                if let Some(entry) = self.fds.get_mut(slot) {
                    entry.events = events;
                }
            }
            (None, 0) => {}
            (None, events) => {
                self.slots.insert(fd, self.fds.len());
                self.fds.push(PollFd {
                    fd,
                    events,
                    revents: 0,
                });
            }
        }
        Ok(())
    }

    pub(super) fn wait(
        &mut self,
        timeout: Option<Duration>,
        ready: &mut Vec<Readiness>,
    ) -> io::Result<()> {
        let count = Nfds::try_from(self.fds.len())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let milliseconds = timeout.map_or(-1, |timeout| {
            c_int::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(c_int::MAX)
        });
        // SAFETY: fds holds count initialised pollfds the kernel may update in place.
        let result = unsafe { poll(self.fds.as_mut_ptr(), count, milliseconds) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(EINTR) {
                return Ok(());
            }
            return Err(error);
        }
        let failed = POLLERR | POLLHUP | POLLNVAL;
        ready.extend(
            self.fds
                .iter()
                .filter(|entry| entry.revents != 0)
                .map(|entry| Readiness {
                    fd: entry.fd,
                    read: entry.revents & (POLLIN | failed) != 0,
                    write: entry.revents & (POLLOUT | failed) != 0,
                }),
        );
        Ok(())
    }
}
