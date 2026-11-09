use crate::error::{Error, ErrorKind, Result};
use crate::io::IoBuffer;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Where a listener listens or a connection goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Address {
    /// A TCP address: an IP address and a port.
    Tcp(SocketAddr),
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(address) => write!(f, "tcp://{address}"),
        }
    }
}

/// A host's network: listening for connections, a queue for the operations on them, and UDP
/// sockets.
pub trait Network: Send + Sync + fmt::Debug {
    /// The listener type [`Network::listen`] returns.
    type Listener: NetListener;
    /// The connection type accepts and connects complete with.
    type Stream: NetStream;
    /// The queue type for operations on this network.
    type Queue: NetQueue<Listener = Self::Listener, Stream = Self::Stream>;
    /// The UDP socket type [`Network::bind_datagram`] returns.
    type Datagram: DatagramSocket;

    /// Listens at `address`; port 0 picks a free port, which
    /// [`NetListener::local_address`] then reports. Fails with [`ErrorKind::AddrInUse`] when
    /// something already listens there, and [`ErrorKind::AddrNotAvailable`] when the address
    /// is not this host's.
    fn listen(&self, address: Address) -> Result<Self::Listener>;

    /// Creates a queue for accepts, connects, receives and sends.
    fn net_queue(&self, options: NetQueueOptions) -> Result<Self::Queue>;

    /// Opens a UDP socket bound to `address`; port 0 picks a free port, which
    /// [`DatagramSocket::local_address`] then reports. UDP ports are apart from TCP's, so a
    /// datagram socket and a listener may share a port number. Fails with
    /// [`ErrorKind::AddrInUse`] when another datagram socket is bound there, and
    /// [`ErrorKind::AddrNotAvailable`] when the address is not this host's.
    fn bind_datagram(&self, address: SocketAddr) -> Result<Self::Datagram>;
}

/// The largest datagram a [`DatagramSocket`] sends: the most a UDP datagram over IPv4 carries
/// (65,535 bytes less the IP and UDP headers).
pub const MAX_DATAGRAM_BYTES: usize = 65_507;

/// A UDP socket: datagrams to and from any address, each one sent whole or not at all, which
/// may arrive out of order or never, as over UDP.
///
/// Its calls block the calling thread, a receive for at most the wait it is given, so a server
/// reads one with a thread of its own and checks its stop flag between receives.
pub trait DatagramSocket: Send + Sync + fmt::Debug {
    /// The address it is bound to.
    fn local_address(&self) -> Result<SocketAddr>;

    /// Sends `datagram` to `to` and says how many bytes went, always all of them. Fails with
    /// [`ErrorKind::InvalidInput`] when it is longer than [`MAX_DATAGRAM_BYTES`]. A datagram
    /// sent where nothing is bound is lost without an error.
    fn send_to(&self, datagram: &[u8], to: SocketAddr) -> Result<usize>;

    /// Waits up to `wait` for the next datagram, copies it into `buffer` and says how many
    /// bytes it copied and who sent it; fails with [`ErrorKind::TimedOut`] when none came in
    /// time, and refuses an empty `buffer` with [`ErrorKind::InvalidInput`]. A datagram longer
    /// than `buffer` is cut to the buffer's length on Unix systems and in the simulation, and
    /// fails with [`ErrorKind::InvalidInput`] on Windows, so a caller that refuses datagrams
    /// longer than some limit receives into a buffer one byte longer and refuses those that
    /// fill it.
    fn receive_from(&self, buffer: &mut [u8], wait: Duration) -> Result<(usize, SocketAddr)>;
}

/// A socket that listens for connections.
pub trait NetListener: Send + Sync + fmt::Debug {
    /// The address it listens at.
    fn local_address(&self) -> Result<Address>;
}

/// One connection: an ordered stream of bytes in each direction.
pub trait NetStream: Send + Sync + fmt::Debug {
    /// This end's address.
    fn local_address(&self) -> Result<Address>;

    /// The other end's address.
    fn peer_address(&self) -> Result<Address>;

    /// Ends this end's sending: once the peer has received everything sent before, its
    /// receives transfer 0 bytes. Receiving still works.
    fn shutdown_write(&self) -> Result<()>;

    /// Turns TCP keep-alives on with `keep_alive`'s timing, or off with `None`: an end that
    /// has heard nothing from its peer for the idle time probes it, and fails its next
    /// operation with [`ErrorKind::TimedOut`] once enough probes in a row go unanswered, so a
    /// connection whose peer vanished without a word does not wait forever. Fails with
    /// [`ErrorKind::Unsupported`] where the network has no keep-alives, which is what a network
    /// that does not implement this method says, and with [`ErrorKind::NotConnected`] after the
    /// connection was reset or timed out.
    fn set_keep_alive(&self, keep_alive: Option<TcpKeepAlive>) -> Result<()> {
        let _ = keep_alive;
        Err(Error::new(ErrorKind::Unsupported))
    }
}

/// When TCP keep-alives probe an idle connection: after [`TcpKeepAlive::idle`] without hearing
/// from the peer, then every [`TcpKeepAlive::interval`] while no probe is answered, giving up
/// with [`ErrorKind::TimedOut`] after [`TcpKeepAlive::count`] unanswered probes, so a peer that
/// vanished is noticed [`TcpKeepAlive::gives_up_after`] the last time it was heard at most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TcpKeepAlive {
    idle: Duration,
    interval: Duration,
    count: u32,
}

impl TcpKeepAlive {
    /// The longest idle time and interval, in seconds: the most Linux takes, and the most this
    /// type allows everywhere so a setting means the same on every system.
    pub const MAX_SECONDS: u64 = 32_767;

    /// The most unanswered probes before giving up: the most Linux takes.
    pub const MAX_COUNT: u32 = 127;

    /// Keep-alives that probe after `idle`, then every `interval`, giving up after `count`
    /// unanswered probes. Each time is whole seconds from 1 to [`TcpKeepAlive::MAX_SECONDS`],
    /// and `count` is from 1 to [`TcpKeepAlive::MAX_COUNT`], since operating systems count in
    /// seconds; anything else is refused with [`ErrorKind::InvalidInput`].
    pub fn new(idle: Duration, interval: Duration, count: u32) -> Result<Self> {
        let whole_seconds = |time: Duration| {
            time.subsec_nanos() == 0 && (1..=Self::MAX_SECONDS).contains(&time.as_secs())
        };
        let counted = (1..=Self::MAX_COUNT).contains(&count);
        if !whole_seconds(idle) || !whole_seconds(interval) || !counted {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        Ok(Self {
            idle,
            interval,
            count,
        })
    }

    /// Keep-alives that probe after `idle`, then every third of it (one second at least),
    /// giving up after three unanswered probes, as Redis's `tcp-keepalive` does, so a vanished
    /// peer is noticed within twice `idle`. Refuses an `idle` that [`TcpKeepAlive::new`]
    /// refuses.
    pub fn after_idle(idle: Duration) -> Result<Self> {
        let interval = Duration::from_secs((idle.as_secs() / 3).max(1));
        Self::new(idle, interval, 3)
    }

    /// How long a connection stays silent before the first probe.
    pub fn idle(self) -> Duration {
        self.idle
    }

    /// How long between unanswered probes.
    pub fn interval(self) -> Duration {
        self.interval
    }

    /// How many probes in a row go unanswered before the connection fails.
    pub fn count(self) -> u32 {
        self.count
    }

    /// How long after the peer was last heard an end gives up when no probe is answered:
    /// the idle time and every interval.
    pub fn gives_up_after(self) -> Duration {
        let probing = self.interval.saturating_mul(self.count);
        self.idle.saturating_add(probing)
    }
}

/// One asynchronous operation on a network, owning what it uses until it completes.
///
/// Receives and sends transfer at most the buffer's length and may transfer less; the
/// completion says how much. A receive that transfers 0 bytes means the peer ended its
/// sending, so a receive into an empty buffer is refused with [`ErrorKind::InvalidInput`].
#[derive(Debug)]
#[non_exhaustive]
pub enum NetOp<L, S> {
    /// Waits for the next connection to `listener` and completes with
    /// [`NetOutput::Connected`].
    Accept {
        /// The listener to accept from.
        listener: Arc<L>,
    },
    /// Connects to `address` and completes with [`NetOutput::Connected`]. Fails with
    /// [`ErrorKind::ConnectionRefused`] when nothing listens there. [`OsNetwork`]'s poller
    /// waits for the connection inside `reap`.
    ///
    /// [`OsNetwork`]: crate::OsNetwork
    Connect {
        /// Where to connect.
        address: Address,
    },
    /// Receives into `buffer`.
    Receive {
        /// The connection to receive from.
        stream: Arc<S>,
        /// The bytes to fill.
        buffer: IoBuffer,
    },
    /// Sends `buffer`.
    Send {
        /// The connection to send on.
        stream: Arc<S>,
        /// The bytes to send.
        buffer: IoBuffer,
    },
}

impl<L, S> NetOp<L, S> {
    /// The buffer of a receive or a send.
    pub fn buffer(&self) -> Option<&IoBuffer> {
        match self {
            Self::Receive { buffer, .. } | Self::Send { buffer, .. } => Some(buffer),
            Self::Accept { .. } | Self::Connect { .. } => None,
        }
    }

    /// Gives back the buffer of a receive or a send.
    pub fn into_buffer(self) -> Option<IoBuffer> {
        match self {
            Self::Receive { buffer, .. } | Self::Send { buffer, .. } => Some(buffer),
            Self::Accept { .. } | Self::Connect { .. } => None,
        }
    }
}

/// What a successful network operation produced.
#[derive(Debug)]
#[non_exhaustive]
pub enum NetOutput<S> {
    /// How many bytes a receive or a send transferred.
    Transferred(usize),
    /// The new connection of an accept or a connect.
    Connected(S),
}

impl<S> NetOutput<S> {
    /// The byte count of a receive or a send.
    pub fn transferred(&self) -> Option<usize> {
        match self {
            Self::Transferred(count) => Some(*count),
            Self::Connected(_) => None,
        }
    }

    /// The connection of an accept or a connect.
    pub fn into_stream(self) -> Option<S> {
        match self {
            Self::Connected(stream) => Some(stream),
            Self::Transferred(_) => None,
        }
    }
}

/// A finished network operation: the caller's tag, the outcome and the operation.
#[derive(Debug)]
pub struct NetCompletion<L, S> {
    /// The tag given to [`NetQueue::submit`].
    pub user_data: u64,
    /// What the operation produced, or why it failed.
    pub result: Result<NetOutput<S>>,
    /// The operation, handing back its listener, connection and buffer.
    pub op: NetOp<L, S>,
}

/// A network operation [`NetQueue::submit`] did not accept, handed back with the reason.
#[derive(Debug)]
pub struct RejectedNetOp<L, S> {
    /// Why the operation was not accepted.
    pub error: Error,
    /// The operation, untouched.
    pub op: NetOp<L, S>,
}

impl<L, S> fmt::Display for RejectedNetOp<L, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "network operation not submitted: {}", self.error)
    }
}

impl<L: fmt::Debug, S: fmt::Debug> std::error::Error for RejectedNetOp<L, S> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// A queue of asynchronous network operations, owned by one thread: operations are submitted
/// with a tag and their completions are reaped later, in any order, as with
/// [`IoQueue`](crate::IoQueue).
///
/// Bytes on one connection arrive in the order they were sent, but operations on different
/// connections complete in any order, and two sends on one connection in flight at once may
/// interleave, so a connection has at most one send and one receive in flight. Dropping a
/// queue abandons its operations in flight: a send may or may not have happened.
///
/// Timeouts are built above the queue: [`NetQueue::reap_timeout`] bounds a wait, and
/// [`NetQueue::cancel`] gives up an operation whose time ran out and hands back its buffer.
pub trait NetQueue: Send + fmt::Debug {
    /// The listener type accepts use.
    type Listener: NetListener;
    /// The connection type.
    type Stream: NetStream;

    /// Starts `op`, tagged with `user_data` for its completion. Fails with
    /// [`ErrorKind::QueueFull`] when [`NetQueue::in_flight`] has reached
    /// [`NetQueue::capacity`], handing the operation back.
    fn submit(
        &mut self,
        op: NetOp<Self::Listener, Self::Stream>,
        user_data: u64,
    ) -> core::result::Result<(), RejectedNetOp<Self::Listener, Self::Stream>>;

    /// Appends the completions of finished operations to `completions`, first waiting until
    /// at least `min` have finished, and returns how many it appended. Fails with
    /// [`ErrorKind::InvalidInput`] when `min` is more than [`NetQueue::in_flight`]; in a
    /// simulation, with [`ErrorKind::Stalled`] when this queue's operations cannot finish
    /// unless another queue acts first. When it
    /// fails while waiting, the completions it already appended stay in `completions`.
    fn reap(
        &mut self,
        completions: &mut Vec<NetCompletion<Self::Listener, Self::Stream>>,
        min: usize,
    ) -> Result<usize>;

    /// Like [`NetQueue::reap`], but waits at most `timeout`: when it runs out first, it
    /// returns the fewer than `min` completions it appended, which may be none, and the other
    /// operations stay in flight. A zero timeout only collects what has already finished, and one
    /// too long for the clock to count, such as [`Duration::MAX`], waits as `reap` does. In a
    /// simulation, the wait is on the simulated clock, and running out of time is the answer
    /// instead of [`ErrorKind::Stalled`].
    fn reap_timeout(
        &mut self,
        completions: &mut Vec<NetCompletion<Self::Listener, Self::Stream>>,
        min: usize,
        timeout: Duration,
    ) -> Result<usize>;

    /// Gives up the operations in flight tagged `user_data`: each still completes exactly once
    /// through a later reap, with [`ErrorKind::Canceled`] and its listener, connection and
    /// buffer, and stays counted in [`NetQueue::in_flight`] until then. An operation that
    /// finished first completes with its own result instead; a receive or send that completes
    /// with [`ErrorKind::Canceled`] moved no bytes of its buffer, while a canceled connect may
    /// already have reached the peer, whose accept then sees a connection that closes at once.
    /// Fails with [`ErrorKind::NotFound`] when no operation in flight has that tag.
    fn cancel(&mut self, user_data: u64) -> Result<()>;

    /// How many operations were submitted and not yet reaped.
    fn in_flight(&self) -> usize;

    /// The most operations that may be in flight at once.
    fn capacity(&self) -> usize;
}

/// What runs the operations of an [`OsNetwork`](crate::OsNetwork) queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NetBackend {
    /// Retries the operations that would block inside [`NetQueue::reap`], sleeping between
    /// rounds, and connects with a blocking call, which a [`NetQueue::reap_timeout`] cannot cut
    /// short: correct wherever std has sockets, never fast.
    Portable,
    /// Waits for readiness in Linux's epoll, inside [`NetQueue::reap`].
    Epoll,
    /// Waits for readiness in the kqueue of Apple's systems, inside [`NetQueue::reap`].
    Kqueue,
    /// Waits for readiness in `poll(2)`, inside [`NetQueue::reap`]: the automatic choice on
    /// FreeBSD, NetBSD, illumos and Solaris, where connects still block, and available on
    /// request wherever [`NetBackend::Epoll`] or [`NetBackend::Kqueue`] is, and connects there
    /// never block. Each wait hands the kernel every socket an operation waits on, so it suits
    /// fewer connections than epoll or kqueue.
    Poll,
    /// Runs accepts, receives and sends as completions in Linux's io_uring (kernel 5.11 or
    /// later), on request only: each operation is one submission, a batch of submissions reaches
    /// the kernel in one system call when the queue reaps, and a connect waits for its socket in
    /// the same ring. A kernel without io_uring, or one that disables it, refuses the request
    /// with [`ErrorKind::Unsupported`].
    IoUring,
    /// Runs accepts, connects, receives and sends as overlapped operations on a Windows I/O
    /// completion port, on request only, waiting for their completion packets inside
    /// [`NetQueue::reap`]. A socket is tied to the first such queue that uses it: another
    /// completion-port queue refuses it with [`ErrorKind::Unsupported`], while the other
    /// backends still take it.
    Iocp,
}

/// How [`Network::net_queue`] builds a queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetQueueOptions {
    capacity: usize,
    backend: Option<NetBackend>,
}

impl NetQueueOptions {
    /// The capacity of a queue built with [`NetQueueOptions::new`].
    pub const DEFAULT_CAPACITY: usize = 128;

    /// The largest capacity a queue may have.
    pub const MAX_CAPACITY: usize = 32_768;

    /// Options for a queue of [`NetQueueOptions::DEFAULT_CAPACITY`] operations.
    pub fn new() -> Self {
        Self {
            capacity: Self::DEFAULT_CAPACITY,
            backend: None,
        }
    }

    /// Sets the most operations in flight at once.
    pub fn capacity(mut self, operations: usize) -> Self {
        self.capacity = operations;
        self
    }

    /// Asks for one backend, given or refused: the queue never falls back to another. Without
    /// it, the queue waits in epoll on Linux and Android on the architectures whose system
    /// constants EggDB declares (x86, x86_64, arm, aarch64, riscv64, powerpc64, s390x and
    /// loongarch64), in kqueue on Apple's systems, in `poll(2)` on FreeBSD, NetBSD, illumos and
    /// Solaris, and polls elsewhere. A machine refuses a backend it lacks with
    /// [`ErrorKind::Unsupported`]; the simulation accepts every request and runs every operation
    /// its own way.
    pub fn backend(mut self, backend: NetBackend) -> Self {
        self.backend = Some(backend);
        self
    }

    pub(crate) fn requested_backend(self) -> Option<NetBackend> {
        self.backend
    }

    pub(crate) fn validated(self) -> Result<usize> {
        if self.capacity == 0 || self.capacity > Self::MAX_CAPACITY {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        Ok(self.capacity)
    }
}

impl Default for NetQueueOptions {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn answered_at_once<L, S>(op: &NetOp<L, S>) -> Option<Result<NetOutput<S>>> {
    match op {
        NetOp::Receive { buffer, .. } if buffer.is_empty() => {
            Some(Err(Error::new(ErrorKind::InvalidInput)))
        }
        NetOp::Send { buffer, .. } if buffer.is_empty() => Some(Ok(NetOutput::Transferred(0))),
        _ => None,
    }
}

pub(crate) fn net_queue_full<L, S>(op: NetOp<L, S>) -> RejectedNetOp<L, S> {
    RejectedNetOp {
        error: Error::new(ErrorKind::QueueFull),
        op,
    }
}

#[cfg(test)]
mod tests {
    use super::{Address, NetQueueOptions, TcpKeepAlive};
    use crate::ErrorKind;
    use std::time::Duration;

    #[test]
    fn options_reject_empty_and_oversized_queues() {
        for options in [
            NetQueueOptions::new().capacity(0),
            NetQueueOptions::new().capacity(NetQueueOptions::MAX_CAPACITY + 1),
        ] {
            assert_eq!(
                options.validated().unwrap_err().kind(),
                ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn keep_alive_timing_is_whole_seconds_within_what_every_system_takes() {
        let second = Duration::from_secs(1);
        let most = Duration::from_secs(TcpKeepAlive::MAX_SECONDS);
        assert!(TcpKeepAlive::new(second, second, 1).is_ok());
        assert!(TcpKeepAlive::new(most, most, TcpKeepAlive::MAX_COUNT).is_ok());
        for (idle, interval, count) in [
            (Duration::ZERO, second, 1),
            (second, Duration::ZERO, 1),
            (second, second, 0),
            (most + second, second, 1),
            (second, most + second, 1),
            (second, second, TcpKeepAlive::MAX_COUNT + 1),
            (Duration::from_millis(1500), second, 1),
        ] {
            assert_eq!(
                TcpKeepAlive::new(idle, interval, count).unwrap_err().kind(),
                ErrorKind::InvalidInput,
                "{idle:?} {interval:?} {count}"
            );
        }
    }

    #[test]
    fn keep_alives_after_an_idle_time_probe_three_times_a_third_of_it_apart() {
        let keep_alive = TcpKeepAlive::after_idle(Duration::from_secs(300)).unwrap();
        assert_eq!(keep_alive.idle(), Duration::from_secs(300));
        assert_eq!(keep_alive.interval(), Duration::from_secs(100));
        assert_eq!(keep_alive.count(), 3);
        assert_eq!(keep_alive.gives_up_after(), Duration::from_secs(600));
        let short = TcpKeepAlive::after_idle(Duration::from_secs(2)).unwrap();
        assert_eq!(short.interval(), Duration::from_secs(1));
    }

    #[test]
    fn tcp_addresses_print_with_their_scheme() {
        let address = Address::Tcp("127.0.0.1:5432".parse().unwrap());
        assert_eq!(address.to_string(), "tcp://127.0.0.1:5432");
    }
}
