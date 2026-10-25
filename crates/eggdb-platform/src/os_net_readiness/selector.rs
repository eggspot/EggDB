use super::queue::{Interest, Readiness};
use super::{native, poll};
use crate::net::NetBackend;
use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

#[derive(Debug)]
pub(super) enum Selector {
    Native(native::Selector),
    Poll(poll::Selector),
}

impl Selector {
    pub(super) fn new(backend: NetBackend, capacity: usize) -> io::Result<Self> {
        if backend == NetBackend::Poll {
            Ok(Self::Poll(poll::Selector::new()))
        } else {
            native::Selector::new(capacity).map(Self::Native)
        }
    }

    pub(super) fn backend(&self) -> NetBackend {
        match self {
            Self::Native(_) => native::BACKEND,
            Self::Poll(_) => NetBackend::Poll,
        }
    }

    pub(super) fn want(&mut self, fd: RawFd, wanted: Interest) -> io::Result<()> {
        match self {
            Self::Native(selector) => selector.want(fd, wanted),
            Self::Poll(selector) => selector.want(fd, wanted),
        }
    }

    pub(super) fn wait(
        &mut self,
        timeout: Option<Duration>,
        ready: &mut Vec<Readiness>,
    ) -> io::Result<()> {
        match self {
            Self::Native(selector) => selector.wait(timeout, ready),
            Self::Poll(selector) => selector.wait(timeout, ready),
        }
    }
}
