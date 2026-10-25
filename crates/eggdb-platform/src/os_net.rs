use crate::error::{Error, ErrorKind, Result};
use crate::net::{
    Address, NetCompletion, NetListener, NetOp, NetOutput, NetQueue, NetQueueOptions, NetStream,
    Network, RejectedNetOp, answered_at_once, net_queue_full,
};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::time::Duration;

/// The operating system's network.
///
/// Its queue is a portable poller over non-blocking sockets: `reap` retries the operations
/// that would block, backing off between rounds, and a connect waits for the operating system
/// inside `reap`. It works wherever std has sockets; epoll, kqueue, io_uring and I/O
/// completion ports follow behind the same queue. Connections have Nagle's algorithm turned
/// off, as database protocols send small requests and answers.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsNetwork;

impl OsNetwork {
    /// Creates a handle to the operating system's network.
    pub fn new() -> Self {
        Self
    }
}

impl Network for OsNetwork {
    type Listener = OsListener;
    type Stream = OsStream;
    type Queue = OsNetQueue;

    fn listen(&self, address: Address) -> Result<OsListener> {
        let Address::Tcp(address) = address;
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        Ok(OsListener { listener })
    }

    fn net_queue(&self, options: NetQueueOptions) -> Result<OsNetQueue> {
        Ok(OsNetQueue {
            capacity: options.validated()?,
            in_flight: 0,
            waiting: Vec::new(),
        })
    }
}

/// A listener made by [`OsNetwork`].
#[derive(Debug)]
pub struct OsListener {
    listener: TcpListener,
}

impl NetListener for OsListener {
    fn local_address(&self) -> Result<Address> {
        Ok(Address::Tcp(self.listener.local_addr()?))
    }
}

/// A connection made by [`OsNetwork`].
#[derive(Debug)]
pub struct OsStream {
    stream: TcpStream,
}

impl OsStream {
    fn new(stream: TcpStream) -> Result<Self> {
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        Ok(Self { stream })
    }
}

impl NetStream for OsStream {
    fn local_address(&self) -> Result<Address> {
        Ok(Address::Tcp(self.stream.local_addr()?))
    }

    fn peer_address(&self) -> Result<Address> {
        Ok(Address::Tcp(self.stream.peer_addr()?))
    }

    fn shutdown_write(&self) -> Result<()> {
        Ok(self.stream.shutdown(Shutdown::Write)?)
    }
}

type Op = NetOp<OsListener, OsStream>;
type Completion = NetCompletion<OsListener, OsStream>;

#[derive(Debug)]
struct Waiting {
    op: Op,
    user_data: u64,
}

/// The queue of [`OsNetwork`].
#[derive(Debug)]
pub struct OsNetQueue {
    capacity: usize,
    in_flight: usize,
    waiting: Vec<Waiting>,
}

impl OsNetQueue {
    const FIRST_BACKOFF: Duration = Duration::from_micros(20);
    const LONGEST_BACKOFF: Duration = Duration::from_millis(2);

    fn poll(&mut self, completions: &mut Vec<Completion>) -> usize {
        let before = completions.len();
        let mut index = 0;
        while index < self.waiting.len() {
            match attempt(&mut self.waiting[index].op) {
                Some(result) => {
                    let Waiting { op, user_data } = self.waiting.swap_remove(index);
                    completions.push(NetCompletion {
                        user_data,
                        result,
                        op,
                    });
                }
                None => index += 1,
            }
        }
        completions.len() - before
    }
}

impl NetQueue for OsNetQueue {
    type Listener = OsListener;
    type Stream = OsStream;

    fn submit(
        &mut self,
        op: Op,
        user_data: u64,
    ) -> core::result::Result<(), RejectedNetOp<OsListener, OsStream>> {
        if self.in_flight >= self.capacity {
            return Err(net_queue_full(op));
        }
        self.waiting.push(Waiting { op, user_data });
        self.in_flight += 1;
        Ok(())
    }

    fn reap(&mut self, completions: &mut Vec<Completion>, min: usize) -> Result<usize> {
        if min > self.in_flight {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        let mut reaped = self.poll(completions);
        let mut backoff = Self::FIRST_BACKOFF;
        while reaped < min {
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Self::LONGEST_BACKOFF);
            reaped += self.poll(completions);
        }
        self.in_flight -= reaped;
        Ok(reaped)
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn capacity(&self) -> usize {
        self.capacity
    }
}

fn attempt(op: &mut Op) -> Option<Result<NetOutput<OsStream>>> {
    if let Some(answer) = answered_at_once(op) {
        return Some(answer);
    }
    match op {
        NetOp::Accept { listener } => finished(listener.listener.accept()).map(|accepted| {
            accepted
                .map_err(Error::from)
                .and_then(|(stream, _)| OsStream::new(stream))
                .map(NetOutput::Connected)
        }),
        NetOp::Connect { address } => {
            let Address::Tcp(address) = *address;
            finished(TcpStream::connect(address)).map(|connected| {
                connected
                    .map_err(Error::from)
                    .and_then(OsStream::new)
                    .map(NetOutput::Connected)
            })
        }
        NetOp::Receive { stream, buffer } => finished((&stream.stream).read(buffer))
            .map(|received| received.map(NetOutput::Transferred).map_err(Error::from)),
        NetOp::Send { stream, buffer } => finished((&stream.stream).write(buffer))
            .map(|sent| sent.map(NetOutput::Transferred).map_err(Error::from)),
    }
}

fn finished<T>(outcome: io::Result<T>) -> Option<io::Result<T>> {
    match outcome {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            None
        }
        outcome => Some(outcome),
    }
}
