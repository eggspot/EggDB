use crate::error::{Error, ErrorKind, Result};
use crate::net::{NetBackend, NetCompletion, NetOp};
use crate::os_net::{OsListener, OsStream};

pub(crate) const NATIVE: Option<NetBackend> = None;

pub(crate) fn supports(_: NetBackend) -> bool {
    false
}

#[derive(Debug)]
pub(crate) enum ReadinessQueue {}

impl ReadinessQueue {
    pub(crate) fn new(_: usize, _: NetBackend) -> Result<Self> {
        Err(Error::new(ErrorKind::Unsupported))
    }

    pub(crate) fn backend(&self) -> NetBackend {
        match *self {}
    }

    pub(crate) fn submit(&mut self, _: NetOp<OsListener, OsStream>, _: u64) {
        match *self {}
    }

    pub(crate) fn reap(
        &mut self,
        _: &mut Vec<NetCompletion<OsListener, OsStream>>,
        _: usize,
        _: Option<std::time::Instant>,
    ) -> Result<()> {
        match *self {}
    }

    pub(crate) fn cancel(&mut self, _: u64, _: &mut Vec<NetCompletion<OsListener, OsStream>>) {
        match *self {}
    }
}
