use crate::error::{Error, ErrorKind, Result};
use crate::net::{
    Address, NetBackend, NetCompletion, NetListener, NetOp, NetOutput, NetQueue, NetQueueOptions,
    NetStream, Network, RejectedNetOp, answered_at_once, net_queue_full,
};
use crate::os_net_readiness::{NATIVE, ReadinessQueue, supports, suppress_sigpipe};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// The operating system's network.
///
/// Unless [`NetQueueOptions::backend`] asks for one, its queues wait for sockets to become
/// ready in epoll on Linux and Android ([`NetBackend::Epoll`]) and in kqueue on Apple's
/// systems ([`NetBackend::Kqueue`]), blocking inside `reap` until enough operations finish,
/// and connect without blocking. On FreeBSD, NetBSD, illumos and Solaris they wait in
/// `poll(2)` ([`NetBackend::Poll`]), and a connect still waits for the operating system inside
/// `reap`. Elsewhere they run [`NetBackend::Portable`]: `reap` retries
/// the operations that would block, backing off between rounds, and a connect waits for the
/// operating system inside `reap`. [`OsNetQueue::backend`] says which one a queue runs.
/// Connections have Nagle's algorithm turned off, as database protocols send small requests
/// and answers, and never raise `SIGPIPE`, so a process embedding EggDB is not stopped by a
/// peer that went away.
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
        let capacity = options.validated()?;
        let engine = match options.requested_backend().or(NATIVE) {
            Some(NetBackend::Portable) | None => Engine::Portable(Vec::new()),
            Some(backend) if supports(backend) => {
                Engine::Readiness(Box::new(ReadinessQueue::new(capacity, backend)?))
            }
            Some(_) => return Err(Error::new(ErrorKind::Unsupported)),
        };
        Ok(OsNetQueue {
            capacity,
            in_flight: 0,
            engine,
            canceled: Vec::new(),
        })
    }
}

/// A listener made by [`OsNetwork`].
#[derive(Debug)]
pub struct OsListener {
    pub(crate) listener: TcpListener,
}

impl NetListener for OsListener {
    fn local_address(&self) -> Result<Address> {
        Ok(Address::Tcp(self.listener.local_addr()?))
    }
}

/// A connection made by [`OsNetwork`].
#[derive(Debug)]
pub struct OsStream {
    pub(crate) stream: TcpStream,
}

impl OsStream {
    pub(crate) fn new(stream: TcpStream) -> Result<Self> {
        stream.set_nonblocking(true)?;
        suppress_sigpipe(&stream)?;
        let _ = stream.set_nodelay(true);
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
    engine: Engine,
    canceled: Vec<Completion>,
}

#[derive(Debug)]
enum Engine {
    Portable(Vec<Waiting>),
    Readiness(Box<ReadinessQueue>),
}

impl OsNetQueue {
    const FIRST_BACKOFF: Duration = Duration::from_micros(20);
    const LONGEST_BACKOFF: Duration = Duration::from_millis(2);

    /// Which backend runs this queue's operations.
    pub fn backend(&self) -> NetBackend {
        match &self.engine {
            Engine::Portable(_) => NetBackend::Portable,
            Engine::Readiness(queue) => queue.backend(),
        }
    }

    fn poll(waiting: &mut Vec<Waiting>, completions: &mut Vec<Completion>) -> usize {
        let before = completions.len();
        let mut index = 0;
        while let Some(next) = waiting.get_mut(index) {
            match attempt(&mut next.op, &mut connect_blocking) {
                Some(result) => {
                    let Waiting { op, user_data } = waiting.swap_remove(index);
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

    fn poll_until(
        waiting: &mut Vec<Waiting>,
        completions: &mut Vec<Completion>,
        min: usize,
        deadline: Option<Instant>,
    ) {
        let mut reaped = Self::poll(waiting, completions);
        let mut backoff = Self::FIRST_BACKOFF;
        while reaped < min {
            let nap = match deadline {
                None => backoff,
                Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                    Some(left) if !left.is_zero() => backoff.min(left),
                    _ => return,
                },
            };
            std::thread::sleep(nap);
            backoff = (backoff * 2).min(Self::LONGEST_BACKOFF);
            reaped += Self::poll(waiting, completions);
        }
    }

    fn reap_until(
        &mut self,
        completions: &mut Vec<Completion>,
        min: usize,
        deadline: Option<Instant>,
    ) -> Result<usize> {
        if min > self.in_flight {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        let before = completions.len();
        completions.append(&mut self.canceled);
        let left = min.saturating_sub(completions.len() - before);
        let outcome = match &mut self.engine {
            Engine::Portable(waiting) => {
                Self::poll_until(waiting, completions, left, deadline);
                Ok(())
            }
            Engine::Readiness(queue) => queue.reap(completions, left, deadline),
        };
        let reaped = completions.len() - before;
        self.in_flight -= reaped;
        outcome.map(|()| reaped)
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
        match &mut self.engine {
            Engine::Portable(waiting) => waiting.push(Waiting { op, user_data }),
            Engine::Readiness(queue) => queue.submit(op, user_data),
        }
        self.in_flight += 1;
        Ok(())
    }

    fn reap(&mut self, completions: &mut Vec<Completion>, min: usize) -> Result<usize> {
        self.reap_until(completions, min, None)
    }

    fn reap_timeout(
        &mut self,
        completions: &mut Vec<Completion>,
        min: usize,
        timeout: Duration,
    ) -> Result<usize> {
        let deadline = Instant::now().checked_add(timeout);
        match deadline {
            Some(deadline) => self.reap_until(completions, min, Some(deadline)),
            None => self.reap_until(completions, min, None),
        }
    }

    fn cancel(&mut self, user_data: u64) -> Result<()> {
        let before = self.canceled.len();
        match &mut self.engine {
            Engine::Portable(waiting) => {
                let mut index = 0;
                while let Some(next) = waiting.get(index) {
                    if next.user_data == user_data {
                        let Waiting { op, user_data } = waiting.swap_remove(index);
                        self.canceled.push(NetCompletion {
                            user_data,
                            result: Err(Error::new(ErrorKind::Canceled)),
                            op,
                        });
                    } else {
                        index += 1;
                    }
                }
            }
            Engine::Readiness(queue) => queue.cancel(user_data, &mut self.canceled),
        }
        if self.canceled.len() > before
            || self
                .canceled
                .iter()
                .any(|completion| completion.user_data == user_data)
        {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::NotFound))
        }
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn capacity(&self) -> usize {
        self.capacity
    }
}

type Connector<'a> = dyn FnMut(SocketAddr) -> Option<Result<NetOutput<OsStream>>> + 'a;

pub(crate) fn attempt(
    op: &mut Op,
    connect: &mut Connector<'_>,
) -> Option<Result<NetOutput<OsStream>>> {
    if let Some(answer) = answered_at_once(op) {
        return Some(answer);
    }
    match op {
        NetOp::Accept { listener } => match listener.listener.accept() {
            Err(error) if error.kind() == io::ErrorKind::ConnectionAborted => None,
            accepted => finished(accepted).map(|accepted| {
                accepted
                    .map_err(Error::from)
                    .and_then(|(stream, _)| OsStream::new(stream))
                    .map(NetOutput::Connected)
            }),
        },
        NetOp::Connect { address } => {
            let Address::Tcp(address) = *address;
            connect(address)
        }
        NetOp::Receive { stream, buffer } => finished((&stream.stream).read(buffer))
            .map(|received| received.map(NetOutput::Transferred).map_err(Error::from)),
        NetOp::Send { stream, buffer } => finished((&stream.stream).write(buffer))
            .map(|sent| sent.map(NetOutput::Transferred).map_err(Error::from)),
    }
}

fn connect_blocking(address: SocketAddr) -> Option<Result<NetOutput<OsStream>>> {
    finished(TcpStream::connect(address)).map(|connected| {
        connected
            .map_err(Error::from)
            .and_then(OsStream::new)
            .map(NetOutput::Connected)
    })
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

#[cfg(test)]
mod tests {
    use super::{NATIVE, OsNetwork, supports};
    use crate::{
        Address, ErrorKind, NetBackend, NetListener, NetOp, NetQueue, NetQueueOptions, Network,
    };
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn backends() -> Vec<NetBackend> {
        let mut backends = vec![NetBackend::Portable];
        backends.extend(NATIVE);
        if supports(NetBackend::Poll) && NATIVE != Some(NetBackend::Poll) {
            backends.push(NetBackend::Poll);
        }
        backends
    }

    fn options(backend: NetBackend) -> NetQueueOptions {
        NetQueueOptions::new().backend(backend)
    }

    fn any_port() -> Address {
        Address::Tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
    }

    #[test]
    fn the_automatic_choice_is_the_systems_readiness_backend() {
        let queue = OsNetwork::new().net_queue(NetQueueOptions::new()).unwrap();
        assert_eq!(queue.backend(), NATIVE.unwrap_or(NetBackend::Portable));
        for backend in backends() {
            let queue = OsNetwork::new().net_queue(options(backend)).unwrap();
            assert_eq!(queue.backend(), backend);
        }
    }

    #[test]
    fn a_backend_the_system_lacks_is_refused() {
        for backend in [NetBackend::Epoll, NetBackend::Kqueue, NetBackend::Poll] {
            if !supports(backend) {
                let error = OsNetwork::new().net_queue(options(backend)).unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Unsupported);
            }
        }
    }

    #[test]
    fn reaping_nothing_returns_at_once_while_operations_wait() {
        for backend in backends() {
            let network = OsNetwork::new();
            let listener = Arc::new(network.listen(any_port()).unwrap());
            let mut queue = network.net_queue(options(backend)).unwrap();
            for user_data in 0..4 {
                let accept = NetOp::Accept {
                    listener: Arc::clone(&listener),
                };
                queue.submit(accept, user_data).unwrap();
            }
            let started = Instant::now();
            let mut completions = Vec::new();
            assert_eq!(queue.reap(&mut completions, 0).unwrap(), 0);
            assert!(started.elapsed() < Duration::from_secs(1), "{backend:?}");
            assert_eq!(queue.in_flight(), 4);
        }
    }

    #[test]
    fn many_connections_exchange_bytes_at_once() {
        const CONNECTIONS: u64 = 48;
        for backend in backends() {
            let network = OsNetwork::new();
            let listener = Arc::new(network.listen(any_port()).unwrap());
            let address = listener.local_address().unwrap();
            let mut queue = network.net_queue(options(backend)).unwrap();
            for at in 0..CONNECTIONS {
                let accept = NetOp::Accept {
                    listener: Arc::clone(&listener),
                };
                queue.submit(accept, at).unwrap();
                queue
                    .submit(NetOp::Connect { address }, CONNECTIONS + at)
                    .unwrap();
            }
            let mut completions = Vec::new();
            while completions.len() < 2 * CONNECTIONS as usize {
                queue.reap(&mut completions, 1).unwrap();
            }
            let mut servers = Vec::new();
            let mut clients = Vec::new();
            for completion in completions.drain(..) {
                let stream = Arc::new(completion.result.unwrap().into_stream().unwrap());
                if completion.user_data < CONNECTIONS {
                    servers.push(stream);
                } else {
                    clients.push(stream);
                }
            }
            for (at, server) in servers.iter().enumerate() {
                let receive = NetOp::Receive {
                    stream: Arc::clone(server),
                    buffer: vec![0; 1].into(),
                };
                queue.submit(receive, at as u64).unwrap();
            }
            for (at, client) in clients.iter().enumerate() {
                let send = NetOp::Send {
                    stream: Arc::clone(client),
                    buffer: vec![7; 1].into(),
                };
                queue.submit(send, CONNECTIONS + at as u64).unwrap();
            }
            while completions.len() < 2 * CONNECTIONS as usize {
                queue.reap(&mut completions, 1).unwrap();
            }
            for completion in completions.drain(..) {
                assert_eq!(completion.result.unwrap().transferred(), Some(1));
                if completion.user_data < CONNECTIONS {
                    assert_eq!(completion.op.into_buffer().unwrap()[0], 7);
                }
            }
            assert_eq!(queue.in_flight(), 0);
        }
    }
}
