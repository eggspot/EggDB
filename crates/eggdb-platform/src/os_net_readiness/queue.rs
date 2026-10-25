use super::native::{self, connect_without_waiting};
use super::selector::Selector;
use crate::error::{Error, ErrorKind, Result};
use crate::net::{NetBackend, NetCompletion, NetOp, NetOutput};
use crate::os_net::{OsListener, OsStream, attempt};
use std::collections::HashMap;
use std::io;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;
use std::time::Instant;

pub(crate) const NATIVE: Option<NetBackend> = Some(native::BACKEND);

pub(crate) fn supports(backend: NetBackend) -> bool {
    backend == NetBackend::Poll || Some(backend) == NATIVE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Interest {
    pub(super) read: bool,
    pub(super) write: bool,
}

impl Interest {
    pub(super) const NONE: Self = Self {
        read: false,
        write: false,
    };
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Readiness {
    pub(super) fd: RawFd,
    pub(super) read: bool,
    pub(super) write: bool,
}

pub(super) enum Connecting {
    Connected(TcpStream),
    #[cfg_attr(
        any(
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "illumos",
            target_os = "solaris"
        ),
        expect(dead_code)
    )]
    Pending(TcpStream),
}

enum Step {
    Done(Result<NetOutput<OsStream>>),
    Wait(RawFd, Interest),
}

type Op = NetOp<OsListener, OsStream>;
type Completion = NetCompletion<OsListener, OsStream>;

#[derive(Debug)]
struct Waiting {
    op: Op,
    user_data: u64,
    connecting: Option<TcpStream>,
}

#[derive(Debug, Default)]
struct Waiters {
    readers: Vec<usize>,
    writers: Vec<usize>,
    registered: Interest,
}

impl Waiters {
    fn wanted(&self) -> Interest {
        Interest {
            read: !self.readers.is_empty(),
            write: !self.writers.is_empty(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ReadinessQueue {
    selector: Selector,
    slots: Vec<Option<Waiting>>,
    free: Vec<usize>,
    fresh: Vec<usize>,
    waiters: HashMap<RawFd, Waiters>,
    ready: Vec<Readiness>,
    woken: Vec<usize>,
    touched: Vec<RawFd>,
    closing: Vec<TcpStream>,
}

impl ReadinessQueue {
    pub(crate) fn new(capacity: usize, backend: NetBackend) -> Result<Self> {
        Ok(Self {
            selector: Selector::new(backend, capacity)?,
            slots: Vec::new(),
            free: Vec::new(),
            fresh: Vec::new(),
            waiters: HashMap::new(),
            ready: Vec::new(),
            woken: Vec::new(),
            touched: Vec::new(),
            closing: Vec::new(),
        })
    }

    pub(crate) fn backend(&self) -> NetBackend {
        self.selector.backend()
    }

    pub(crate) fn submit(&mut self, op: Op, user_data: u64) {
        let waiting = Waiting {
            op,
            user_data,
            connecting: None,
        };
        let slot = match self.free.pop() {
            Some(slot) => {
                if let Some(free) = self.slots.get_mut(slot) {
                    *free = Some(waiting);
                }
                slot
            }
            None => {
                self.slots.push(Some(waiting));
                self.slots.len() - 1
            }
        };
        self.fresh.push(slot);
    }

    pub(crate) fn reap(
        &mut self,
        completions: &mut Vec<Completion>,
        min: usize,
        deadline: Option<Instant>,
    ) -> Result<()> {
        let before = completions.len();
        let fresh = std::mem::take(&mut self.fresh);
        for &slot in &fresh {
            self.advance(slot, completions);
        }
        self.fresh = fresh;
        self.fresh.clear();
        self.settle(completions);
        let mut polled = false;
        loop {
            let short = completions.len() - before < min;
            if self.waiters.is_empty() || (polled && !short) {
                return Ok(());
            }
            let timeout = match (short, deadline) {
                (false, _) => Some(Duration::ZERO),
                (true, None) => None,
                (true, Some(deadline)) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if polled && left.is_zero() {
                        return Ok(());
                    }
                    Some(left)
                }
            };
            self.selector.wait(timeout, &mut self.ready)?;
            polled = true;
            let ready = std::mem::take(&mut self.ready);
            for readiness in &ready {
                self.wake(*readiness, completions);
            }
            self.ready = ready;
            self.ready.clear();
            self.settle(completions);
        }
    }

    pub(crate) fn cancel(&mut self, user_data: u64, canceled: &mut Vec<Completion>) {
        let tagged: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, waiting)| waiting.as_ref().is_some_and(|w| w.user_data == user_data))
            .map(|(slot, _)| slot)
            .collect();
        for slot in tagged {
            self.fresh.retain(|&fresh| fresh != slot);
            if let Some((fd, _)) = self.slots[slot].as_ref().and_then(wait_on) {
                if let Some(waiters) = self.waiters.get_mut(&fd) {
                    waiters.readers.retain(|&reader| reader != slot);
                    waiters.writers.retain(|&writer| writer != slot);
                }
                self.touched.push(fd);
            }
            self.complete(slot, Err(Error::new(ErrorKind::Canceled)), canceled);
        }
    }

    fn wake(&mut self, readiness: Readiness, completions: &mut Vec<Completion>) {
        let Some(waiters) = self.waiters.get_mut(&readiness.fd) else {
            return;
        };
        let mut woken = std::mem::take(&mut self.woken);
        if readiness.read {
            woken.append(&mut waiters.readers);
        }
        if readiness.write {
            woken.append(&mut waiters.writers);
        }
        self.touched.push(readiness.fd);
        for &slot in &woken {
            self.advance(slot, completions);
        }
        woken.clear();
        self.woken = woken;
    }

    fn advance(&mut self, slot: usize, completions: &mut Vec<Completion>) {
        let step = match self.slots.get_mut(slot).and_then(Option::as_mut) {
            None => return,
            Some(waiting) => match try_once(waiting) {
                Some(result) => Step::Done(result),
                None => wait_on(waiting).map_or(
                    Step::Done(Err(Error::new(ErrorKind::Other))),
                    |(fd, interest)| Step::Wait(fd, interest),
                ),
            },
        };
        match step {
            Step::Wait(fd, interest) => {
                let waiters = self.waiters.entry(fd).or_default();
                if interest.read {
                    waiters.readers.push(slot);
                } else {
                    waiters.writers.push(slot);
                }
                self.touched.push(fd);
            }
            Step::Done(result) => self.complete(slot, result, completions),
        }
    }

    fn complete(
        &mut self,
        slot: usize,
        result: Result<NetOutput<OsStream>>,
        completions: &mut Vec<Completion>,
    ) {
        let Some(Waiting {
            op,
            user_data,
            connecting,
        }) = self.slots.get_mut(slot).and_then(Option::take)
        else {
            return;
        };
        self.closing.extend(connecting);
        self.free.push(slot);
        completions.push(NetCompletion {
            user_data,
            result,
            op,
        });
    }

    fn settle(&mut self, completions: &mut Vec<Completion>) {
        let mut touched = std::mem::take(&mut self.touched);
        touched.sort_unstable();
        touched.dedup();
        for &fd in &touched {
            self.settle_one(fd, completions);
        }
        touched.clear();
        self.touched = touched;
        self.closing.clear();
    }

    fn settle_one(&mut self, fd: RawFd, completions: &mut Vec<Completion>) {
        let Some(waiters) = self.waiters.get_mut(&fd) else {
            return;
        };
        let wanted = waiters.wanted();
        if wanted == waiters.registered {
            if wanted == Interest::NONE {
                self.waiters.remove(&fd);
            }
            return;
        }
        let changed = self.selector.change(fd, waiters.registered, wanted);
        if wanted == Interest::NONE {
            self.waiters.remove(&fd);
            return;
        }
        match changed {
            Ok(()) => waiters.registered = wanted,
            Err(error) => {
                let failed: Vec<usize> = waiters
                    .readers
                    .drain(..)
                    .chain(waiters.writers.drain(..))
                    .collect();
                let either = Interest {
                    read: waiters.registered.read || wanted.read,
                    write: waiters.registered.write || wanted.write,
                };
                let _ = self.selector.change(fd, either, Interest::NONE);
                self.waiters.remove(&fd);
                for slot in failed {
                    self.complete(slot, Err(copy_of(&error)), completions);
                }
            }
        }
    }
}

fn copy_of(error: &io::Error) -> Error {
    match error.raw_os_error() {
        Some(code) => Error::from(io::Error::from_raw_os_error(code)),
        None => Error::new(ErrorKind::Other),
    }
}

fn try_once(waiting: &mut Waiting) -> Option<Result<NetOutput<OsStream>>> {
    let connecting = &mut waiting.connecting;
    attempt(&mut waiting.op, &mut |address| match connecting.take() {
        None => match connect_without_waiting(address) {
            Ok(Connecting::Connected(socket)) => Some(connected(socket)),
            Ok(Connecting::Pending(socket)) => {
                *connecting = Some(socket);
                None
            }
            Err(error) => Some(Err(error.into())),
        },
        Some(socket) => match finished_connecting(&socket) {
            Ok(true) => Some(connected(socket)),
            Ok(false) => {
                *connecting = Some(socket);
                None
            }
            Err(error) => {
                *connecting = Some(socket);
                Some(Err(error.into()))
            }
        },
    })
}

fn connected(socket: TcpStream) -> Result<NetOutput<OsStream>> {
    OsStream::new(socket).map(NetOutput::Connected)
}

fn finished_connecting(socket: &TcpStream) -> io::Result<bool> {
    if let Some(error) = socket.take_error()? {
        return Err(error);
    }
    match socket.peer_addr() {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotConnected => Ok(false),
        Err(error) => Err(error),
    }
}

fn wait_on(waiting: &Waiting) -> Option<(RawFd, Interest)> {
    const READ: Interest = Interest {
        read: true,
        write: false,
    };
    const WRITE: Interest = Interest {
        read: false,
        write: true,
    };
    match &waiting.op {
        NetOp::Accept { listener } => Some((listener.listener.as_raw_fd(), READ)),
        NetOp::Receive { stream, .. } => Some((stream.stream.as_raw_fd(), READ)),
        NetOp::Send { stream, .. } => Some((stream.stream.as_raw_fd(), WRITE)),
        NetOp::Connect { .. } => waiting
            .connecting
            .as_ref()
            .map(|socket| (socket.as_raw_fd(), WRITE)),
    }
}
