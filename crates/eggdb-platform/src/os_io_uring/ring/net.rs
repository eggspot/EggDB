use super::{Completion, EINTR, IORING_ENTER_GETEVENTS, RawRing, Submission, transfer_len};
use crate::error::{Error, ErrorKind, Result};
use crate::net::{NetCompletion, NetOp, NetOutput, answered_at_once};
use crate::os_net::{OsListener, OsStream};
use crate::os_net_readiness::{Connecting, connect_without_waiting, finished_connecting};
use std::collections::VecDeque;
use std::ffi::{c_uint, c_void};
use std::io;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;
use std::time::{Duration, Instant};

const IORING_OP_POLL_ADD: u8 = 6;
const IORING_OP_ACCEPT: u8 = 13;
const IORING_OP_ASYNC_CANCEL: u8 = 14;
const IORING_OP_SEND: u8 = 26;
const IORING_OP_RECV: u8 = 27;
const IORING_FEAT_NODROP: u32 = 1 << 1;
const IORING_FEAT_FAST_POLL: u32 = 1 << 5;
const IORING_FEAT_EXT_ARG: u32 = 1 << 8;
const IORING_ENTER_EXT_ARG: c_uint = 1 << 3;

const POLLIN: u32 = 0x1;
const POLLOUT: u32 = 0x4;
const SOCK_NONBLOCK: u32 = 0o4000;
const SOCK_CLOEXEC: u32 = 0o2_000_000;
const MSG_NOSIGNAL: u32 = 0x4000;

const EAGAIN: i32 = 11;
const ETIME: i32 = 62;
const ECONNABORTED: i32 = 103;
const ECANCELED: i32 = 125;

const CANCEL_REQUEST: u64 = u64::MAX;

type Op = NetOp<OsListener, OsStream>;
type NetDone = NetCompletion<OsListener, OsStream>;

#[repr(C)]
#[derive(Debug, Default)]
struct WaitArgument {
    sigmask: u64,
    sigmask_size: u32,
    min_wait_usec: u32,
    timeout: u64,
}

#[repr(C)]
#[derive(Debug, Default)]
struct KernelTimespec {
    seconds: i64,
    nanoseconds: i64,
}

const _: () = assert!(size_of::<WaitArgument>() == 24);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Operating,
    WaitingForReadiness,
}

#[derive(Debug)]
struct InFlight {
    op: Op,
    user_data: u64,
    key: u64,
    stage: Stage,
    connecting: Option<TcpStream>,
    canceling: bool,
}

/// One io_uring instance running a network queue's accepts, connects, receives and sends as
/// completions: each operation is one submission, the submissions of a batch reach the kernel
/// in one `io_uring_enter` when the queue reaps, and nothing is retried in user space unless the
/// kernel answers that the socket would block.
#[derive(Debug)]
pub(crate) struct NetRing {
    raw: RawRing,
    slots: Vec<Option<InFlight>>,
    generations: Vec<u32>,
    free_slots: Vec<usize>,
    in_kernel: usize,
    ready: VecDeque<NetDone>,
    taken: Vec<Completion>,
}

impl NetRing {
    pub(crate) fn new(capacity: usize) -> Result<Self> {
        let raw = RawRing::new(
            capacity.saturating_mul(2),
            IORING_FEAT_NODROP | IORING_FEAT_FAST_POLL | IORING_FEAT_EXT_ARG,
        )?;
        Ok(Self {
            raw,
            slots: Vec::new(),
            generations: Vec::new(),
            free_slots: Vec::new(),
            in_kernel: 0,
            ready: VecDeque::new(),
            taken: Vec::new(),
        })
    }

    pub(crate) fn submit(&mut self, op: Op, user_data: u64) {
        if let Some(result) = answered_at_once(&op) {
            self.ready.push_back(NetCompletion {
                user_data,
                result,
                op,
            });
            return;
        }
        let mut connecting = None;
        let mut stage = Stage::Operating;
        if let NetOp::Connect { address } = &op {
            let crate::net::Address::Tcp(address) = *address;
            match connect_without_waiting(address) {
                Ok(Connecting::Connected(socket)) => {
                    self.ready.push_back(NetCompletion {
                        user_data,
                        result: Ok(NetOutput::Connected(OsStream::set_up_non_blocking(socket))),
                        op,
                    });
                    return;
                }
                Ok(Connecting::Pending(socket)) => {
                    connecting = Some(socket);
                    stage = Stage::WaitingForReadiness;
                }
                Err(error) => {
                    self.ready.push_back(NetCompletion {
                        user_data,
                        result: Err(error.into()),
                        op,
                    });
                    return;
                }
            }
        }
        let slot = self.free_slots.pop().unwrap_or(self.slots.len());
        if slot == self.slots.len() {
            self.slots.push(None);
            self.generations.push(0);
        }
        self.generations[slot] = self.generations[slot].wrapping_add(1);
        let key = (u64::from(self.generations[slot]) << 32) | slot as u64;
        let mut in_flight = InFlight {
            op,
            user_data,
            key,
            stage,
            connecting,
            canceling: false,
        };
        match self.push(submission_for(&mut in_flight)) {
            Ok(()) => {
                self.slots[slot] = Some(in_flight);
                self.in_kernel += 1;
            }
            Err(error) => {
                self.free_slots.push(slot);
                self.ready.push_back(NetCompletion {
                    user_data,
                    result: Err(error),
                    op: in_flight.op,
                });
            }
        }
    }

    pub(crate) fn reap(
        &mut self,
        completions: &mut Vec<NetDone>,
        min: usize,
        deadline: Option<Instant>,
    ) -> Result<()> {
        let before = completions.len();
        completions.extend(self.ready.drain(..));
        self.drain(completions);
        let mut waited = false;
        loop {
            let reaped = completions.len() - before;
            let unsubmitted = self.raw.unsubmitted();
            if reaped >= min {
                if unsubmitted > 0 {
                    self.raw.enter(unsubmitted, 0, 0)?;
                    self.drain(completions);
                }
                return Ok(());
            }
            let timeout = match deadline {
                None => None,
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if waited && left.is_zero() {
                        if unsubmitted > 0 {
                            self.raw.enter(unsubmitted, 0, 0)?;
                            self.drain(completions);
                        }
                        return Ok(());
                    }
                    Some(left)
                }
            };
            let wanted = c_uint::try_from(min - reaped).unwrap_or(c_uint::MAX);
            self.wait(unsubmitted, wanted, timeout)?;
            waited = true;
            self.drain(completions);
        }
    }

    pub(crate) fn cancel(&mut self, user_data: u64) -> bool {
        let mut found = self
            .ready
            .iter()
            .any(|completion| completion.user_data == user_data);
        let mut targets = Vec::new();
        let tagged = self.slots.iter_mut().flatten();
        for in_flight in tagged.filter(|in_flight| in_flight.user_data == user_data) {
            found = true;
            if !in_flight.canceling {
                in_flight.canceling = true;
                targets.push(in_flight.key);
            }
        }
        for key in targets {
            if self.push(cancel_submission(key)).is_err() {
                self.canceling_failed(key);
            }
        }
        found
    }

    fn leak(&mut self) {
        std::mem::forget(std::mem::take(&mut self.slots));
        self.raw.leak();
    }

    fn canceling_failed(&mut self, key: u64) {
        let slot = slot_of(key);
        if let Some(Some(in_flight)) = self.slots.get_mut(slot) {
            in_flight.canceling = false;
        }
    }

    fn push(&mut self, submission: Submission) -> Result<()> {
        if self.raw.is_full() {
            let unsubmitted = self.raw.unsubmitted();
            self.raw.enter(unsubmitted, 0, 0)?;
            if self.raw.is_full() {
                return Err(Error::new(ErrorKind::QueueFull));
            }
        }
        self.raw.push(submission);
        Ok(())
    }

    fn wait(&mut self, to_submit: c_uint, wanted: c_uint, timeout: Option<Duration>) -> Result<()> {
        let timespec = timeout.map(|timeout| KernelTimespec {
            seconds: i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX),
            nanoseconds: i64::from(timeout.subsec_nanos()),
        });
        let argument = WaitArgument {
            timeout: timespec
                .as_ref()
                .map_or(0, |timespec| ptr::from_ref(timespec).addr() as u64),
            ..WaitArgument::default()
        };
        let entered = self.raw.enter_once(
            to_submit,
            wanted,
            IORING_ENTER_GETEVENTS | IORING_ENTER_EXT_ARG,
            ptr::from_ref(&argument).cast::<c_void>(),
            size_of::<WaitArgument>(),
        );
        match entered {
            Ok(_) => Ok(()),
            Err(error) if matches!(error.raw_os_error(), Some(EINTR | ETIME)) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn drain(&mut self, completions: &mut Vec<NetDone>) {
        let mut taken = std::mem::take(&mut self.taken);
        self.raw.take_completions(&mut taken);
        for completion in taken.drain(..) {
            self.finish(completion, completions);
        }
        self.taken = taken;
    }

    fn finish(&mut self, completion: Completion, completions: &mut Vec<NetDone>) {
        if completion.user_data == CANCEL_REQUEST {
            return;
        }
        let slot = slot_of(completion.user_data);
        let Some(entry) = self.slots.get_mut(slot) else {
            return;
        };
        if entry
            .as_ref()
            .is_none_or(|in_flight| in_flight.key != completion.user_data)
        {
            return;
        }
        let Some(mut in_flight) = entry.take() else {
            return;
        };
        let result = match next_step(&mut in_flight, completion.res) {
            Next::Done(result) => result,
            Next::Again(stage) => {
                in_flight.stage = stage;
                match self.push(submission_for(&mut in_flight)) {
                    Ok(()) => {
                        self.slots[slot] = Some(in_flight);
                        return;
                    }
                    Err(error) => Err(error),
                }
            }
        };
        self.free_slots.push(slot);
        self.in_kernel -= 1;
        completions.push(NetCompletion {
            user_data: in_flight.user_data,
            result,
            op: in_flight.op,
        });
    }
}

impl Drop for NetRing {
    fn drop(&mut self) {
        let keys: Vec<u64> = self
            .slots
            .iter_mut()
            .flatten()
            .filter(|in_flight| !in_flight.canceling)
            .map(|in_flight| {
                in_flight.canceling = true;
                in_flight.key
            })
            .collect();
        let mut finished = Vec::new();
        for key in keys {
            if self.push(cancel_submission(key)).is_err() {
                self.leak();
                return;
            }
        }
        while self.in_kernel > 0 {
            let unsubmitted = self.raw.unsubmitted();
            if self
                .raw
                .enter(unsubmitted, 1, IORING_ENTER_GETEVENTS)
                .is_err()
            {
                self.leak();
                return;
            }
            self.drain(&mut finished);
            finished.clear();
        }
    }
}

enum Next {
    Done(Result<NetOutput<OsStream>>),
    Again(Stage),
}

fn next_step(in_flight: &mut InFlight, res: i32) -> Next {
    let canceled = || Next::Done(Err(Error::new(ErrorKind::Canceled)));
    if res == -ECANCELED {
        return canceled();
    }
    let retry = res == -EAGAIN
        || (res == -ECONNABORTED && matches!(in_flight.op, NetOp::Accept { .. }));
    if in_flight.canceling && (retry || in_flight.stage == Stage::WaitingForReadiness) {
        return canceled();
    }
    if retry {
        return Next::Again(Stage::WaitingForReadiness);
    }
    if res < 0 {
        return Next::Done(Err(io::Error::from_raw_os_error(res.saturating_neg()).into()));
    }
    match (&in_flight.op, in_flight.stage) {
        (NetOp::Connect { .. }, _) => {
            let Some(socket) = in_flight.connecting.take() else {
                return Next::Done(Err(Error::new(ErrorKind::Other)));
            };
            match finished_connecting(&socket) {
                Ok(true) => Next::Done(Ok(NetOutput::Connected(OsStream::set_up_non_blocking(
                    socket,
                )))),
                Ok(false) => {
                    in_flight.connecting = Some(socket);
                    Next::Again(Stage::WaitingForReadiness)
                }
                Err(error) => Next::Done(Err(error.into())),
            }
        }
        (_, Stage::WaitingForReadiness) => Next::Again(Stage::Operating),
        (NetOp::Accept { .. }, Stage::Operating) => {
            // SAFETY: a successful accept returns a new connection's descriptor that nothing
            // else owns.
            let fd = unsafe { OwnedFd::from_raw_fd(res) };
            Next::Done(Ok(NetOutput::Connected(OsStream::set_up_non_blocking(
                TcpStream::from(fd),
            ))))
        }
        (NetOp::Receive { .. } | NetOp::Send { .. }, Stage::Operating) => {
            Next::Done(Ok(NetOutput::Transferred(res.unsigned_abs() as usize)))
        }
    }
}

fn slot_of(key: u64) -> usize {
    (key & u64::from(u32::MAX)) as usize
}

fn submission_for(in_flight: &mut InFlight) -> Submission {
    let key = in_flight.key;
    if in_flight.stage == Stage::WaitingForReadiness {
        let (fd, events) = match (&in_flight.op, &in_flight.connecting) {
            (NetOp::Connect { .. }, Some(socket)) => (socket.as_raw_fd(), POLLOUT),
            (NetOp::Send { stream, .. }, _) => (stream.stream.as_raw_fd(), POLLOUT),
            (NetOp::Receive { stream, .. }, _) => (stream.stream.as_raw_fd(), POLLIN),
            (NetOp::Accept { listener }, _) => (listener.listener.as_raw_fd(), POLLIN),
            (NetOp::Connect { .. }, None) => (-1, POLLOUT),
        };
        return poll_submission(fd, events, key);
    }
    let mut submission = Submission {
        user_data: key,
        ..Submission::default()
    };
    match &mut in_flight.op {
        NetOp::Accept { listener } => {
            submission.opcode = IORING_OP_ACCEPT;
            submission.fd = listener.listener.as_raw_fd();
            submission.op_flags = SOCK_NONBLOCK | SOCK_CLOEXEC;
        }
        NetOp::Receive { stream, buffer } => {
            submission.opcode = IORING_OP_RECV;
            submission.fd = stream.stream.as_raw_fd();
            submission.addr = buffer.as_mut_ptr().addr() as u64;
            submission.len = transfer_len(buffer.len());
        }
        NetOp::Send { stream, buffer } => {
            submission.opcode = IORING_OP_SEND;
            submission.fd = stream.stream.as_raw_fd();
            submission.addr = buffer.as_ptr().addr() as u64;
            submission.len = transfer_len(buffer.len());
            submission.op_flags = MSG_NOSIGNAL;
        }
        NetOp::Connect { .. } => {
            let fd = in_flight.connecting.as_ref().map_or(-1, AsRawFd::as_raw_fd);
            return poll_submission(fd, POLLOUT, key);
        }
    }
    submission
}

fn poll_submission(fd: RawFd, events: u32, key: u64) -> Submission {
    Submission {
        opcode: IORING_OP_POLL_ADD,
        fd,
        op_flags: poll_events(events),
        user_data: key,
        ..Submission::default()
    }
}

fn poll_events(events: u32) -> u32 {
    if cfg!(target_endian = "big") {
        events.rotate_left(16)
    } else {
        events
    }
}

fn cancel_submission(key: u64) -> Submission {
    Submission {
        opcode: IORING_OP_ASYNC_CANCEL,
        fd: -1,
        addr: key,
        user_data: CANCEL_REQUEST,
        ..Submission::default()
    }
}
