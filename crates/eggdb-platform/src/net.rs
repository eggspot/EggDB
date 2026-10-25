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

/// A host's network: listening for connections and a queue for the operations on them.
pub trait Network: Send + Sync + fmt::Debug {
    /// The listener type [`Network::listen`] returns.
    type Listener: NetListener;
    /// The connection type accepts and connects complete with.
    type Stream: NetStream;
    /// The queue type for operations on this network.
    type Queue: NetQueue<Listener = Self::Listener, Stream = Self::Stream>;

    /// Listens at `address`; port 0 picks a free port, which
    /// [`NetListener::local_address`] then reports. Fails with [`ErrorKind::AddrInUse`] when
    /// something already listens there, and [`ErrorKind::AddrNotAvailable`] when the address
    /// is not this host's.
    fn listen(&self, address: Address) -> Result<Self::Listener>;

    /// Creates a queue for accepts, connects, receives and sends.
    fn net_queue(&self, options: NetQueueOptions) -> Result<Self::Queue>;
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
    /// operations stay in flight. A zero timeout only collects what has already finished. In a
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
    /// finished first completes with its own result instead; one that completes with
    /// [`ErrorKind::Canceled`] transferred nothing. Fails with [`ErrorKind::NotFound`] when no operation in flight has that tag.
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
    /// loongarch64), in kqueue on Apple's systems, and
    /// polls elsewhere. A machine refuses a backend it lacks with [`ErrorKind::Unsupported`];
    /// the simulation accepts every request and runs every operation its own way.
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
    use super::{Address, NetQueueOptions};
    use crate::ErrorKind;

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
    fn tcp_addresses_print_with_their_scheme() {
        let address = Address::Tcp("127.0.0.1:5432".parse().unwrap());
        assert_eq!(address.to_string(), "tcp://127.0.0.1:5432");
    }
}
