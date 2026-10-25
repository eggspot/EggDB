use crate::clock::{Clock, SimClock};
use crate::error::{Error, ErrorKind, Result};
use crate::net::{
    Address, NetCompletion, NetListener, NetOp, NetOutput, NetQueue, NetQueueOptions, NetStream,
    Network, RejectedNetOp, answered_at_once, net_queue_full,
};
use crate::rng::SplitMix64;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// A deterministic simulation of a network of hosts, for tests (D9).
///
/// [`SimNetwork::host`] gives each IP address a [`SimHost`], which implements [`Network`].
/// Connections carry bytes in order in each direction, through bounded buffers of
/// [`SimNetwork::BUFFER_BYTES`]. Bytes sent are first in transit and arrive later, a seeded
/// amount at a time, so connections advance in a different order on every seed while each
/// stays in order. A queue performs its operations only inside `reap`, in a seeded order, and
/// fails with [`ErrorKind::Stalled`] instead of waiting when its operations cannot finish
/// unless another queue acts first and nothing is on its way, so a test that drives several
/// queues reaps them in turn. A queue refuses, with [`ErrorKind::InvalidInput`], a connection or
/// listener of another `SimNetwork`.
///
/// A queue also refuses a second send, or a second receive, on one end of a connection while
/// the first is in flight, since the two could interleave.
///
/// Connecting where nothing listens is [`ErrorKind::ConnectionRefused`]. Dropping a connection
/// with bytes still owed to it resets it, so its peer's operations fail with
/// [`ErrorKind::ConnectionReset`]; dropping it otherwise ends its sending, as
/// [`NetStream::shutdown_write`] does. As over TCP, the peer's first send after that succeeds
/// and its bytes are lost, and it resets the connection, so the operations after it fail with
/// [`ErrorKind::ConnectionReset`]; bytes that had already arrived are lost too, as on Windows,
/// which is stricter than Linux. Every listen and every operation that completes is
/// numbered from zero, empty receives and sends too, and any of them can be made to fail with
/// [`ErrorKind::InjectedFault`], by number or at random from a seed, changing nothing. Clones
/// share the same network.
///
/// Time is a [`SimClock`] that moves only inside `reap`: when no operation can finish and
/// nothing is due, the queue moves the clock to the next thing that will happen, such as bytes
/// arriving after the latency set with [`SimNetwork::latency`], or to the end of
/// [`NetQueue::reap_timeout`]'s wait, whichever comes first. [`SimNetwork::partition`] cuts two
/// hosts apart: bytes between them stay in transit until [`SimNetwork::heal`], and a connect
/// across the cut fails with [`ErrorKind::TimedOut`] after [`SimNetwork::CONNECT_TIMEOUT`], as
/// an unanswered TCP handshake does. A receive across the cut waits until the cut heals or the
/// operation is canceled.
#[derive(Debug, Clone, Default)]
pub struct SimNetwork {
    shared: Arc<Mutex<World>>,
}

#[derive(Debug, Default)]
struct World {
    rng: SplitMix64,
    short_transfers: bool,
    operations: u64,
    failing_operations: BTreeSet<u64>,
    random_failures: Option<RandomFailures>,
    listeners: BTreeMap<SocketAddr, VecDeque<u64>>,
    bound: BTreeSet<SocketAddr>,
    connections: BTreeMap<u64, Connection>,
    next_connection: u64,
    clock: Arc<SimClock>,
    latency: [Duration; 2],
    partitions: BTreeSet<[IpAddr; 2]>,
}

#[derive(Debug)]
struct RandomFailures {
    rng: SplitMix64,
    per_million: u32,
}

#[derive(Debug)]
struct Connection {
    ends: [SocketAddr; 2],
    flows: [Flow; 2],
    open: [bool; 2],
    reset: bool,
}

#[derive(Debug, Default)]
struct Flow {
    in_transit: VecDeque<u8>,
    arrivals: VecDeque<Arrival>,
    arrived: VecDeque<u8>,
    ended: bool,
    end_at: Duration,
    end_arrived: bool,
}

#[derive(Debug, Clone, Copy)]
struct Arrival {
    at: Duration,
    bytes: usize,
}

impl Flow {
    fn buffered(&self) -> usize {
        self.in_transit.len() + self.arrived.len()
    }

    fn last_arrival(&self) -> Duration {
        self.arrivals
            .back()
            .map_or(Duration::ZERO, |arrival| arrival.at)
    }

    fn due(&self, now: Duration) -> usize {
        self.arrivals
            .iter()
            .take_while(|arrival| arrival.at <= now)
            .map(|arrival| arrival.bytes)
            .sum()
    }

    fn end_due(&self, now: Duration) -> bool {
        self.ended && !self.end_arrived && self.in_transit.is_empty() && self.end_at <= now
    }

    fn next_event(&self) -> Option<Duration> {
        match self.arrivals.front() {
            Some(arrival) => Some(arrival.at),
            None => (self.ended && !self.end_arrived).then_some(self.end_at),
        }
    }

    fn send(&mut self, bytes: &[u8], at: Duration) {
        let at = at.max(self.last_arrival());
        self.in_transit.extend(bytes);
        match self.arrivals.back_mut() {
            Some(last) if last.at == at => last.bytes += bytes.len(),
            _ => self.arrivals.push_back(Arrival {
                at,
                bytes: bytes.len(),
            }),
        }
    }

    fn end(&mut self, at: Duration) {
        if !self.ended {
            self.ended = true;
            self.end_at = at.max(self.last_arrival());
        }
    }

    fn deliver(&mut self, mut count: usize) {
        let bytes: Vec<u8> = self.in_transit.drain(..count).collect();
        self.arrived.extend(bytes);
        while count > 0 {
            let Some(front) = self.arrivals.front_mut() else {
                return;
            };
            let taken = front.bytes.min(count);
            front.bytes -= taken;
            count -= taken;
            if front.bytes == 0 {
                self.arrivals.pop_front();
            }
        }
    }

    fn clear(&mut self) {
        self.in_transit.clear();
        self.arrivals.clear();
        self.arrived.clear();
    }
}

const CLIENT: usize = 0;
const SERVER: usize = 1;

impl SimNetwork {
    /// The most bytes each direction of a connection holds in transit and unreceived; sends
    /// wait for room beyond it.
    pub const BUFFER_BYTES: usize = 64 * 1024;

    /// The most connections the whole network holds; connects beyond it fail with
    /// [`ErrorKind::AddrNotAvailable`].
    pub const MAX_CONNECTIONS: usize = 16_384;

    /// The most connections waiting to be accepted on one listener; connects beyond it wait.
    pub const MAX_BACKLOG: usize = 128;

    /// The ports given out for port 0 and for the local end of a connect.
    pub const EPHEMERAL_PORTS: std::ops::RangeInclusive<u16> = 49_152..=65_535;

    /// How long a connect across a partition waits before failing with
    /// [`ErrorKind::TimedOut`]: Linux's default of six handshake retries, doubling from one
    /// second.
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(127);

    /// Creates an empty network, seeded with 0, with its own clock at zero and no latency.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty network that keeps time on `clock`, so other simulators can share it.
    pub fn with_clock(clock: Arc<SimClock>) -> Self {
        let network = Self::new();
        lock(&network.shared).clock = clock;
        network
    }

    /// The clock this network keeps time on.
    pub fn clock(&self) -> Arc<SimClock> {
        Arc::clone(&lock(&self.shared).clock)
    }

    /// Makes bytes, ends of sending and connection handshakes take between `least` and `most`
    /// to cross the network one way, drawn from the seed for each send; bytes still arrive in
    /// order. Both zero, the default, makes them due at once. Swapped bounds are put in order.
    pub fn latency(&self, least: Duration, most: Duration) {
        lock(&self.shared).latency = [least.min(most), least.max(most)];
    }

    /// Cuts hosts `a` and `b` apart until [`SimNetwork::heal`]. Cutting a host from itself
    /// does nothing.
    pub fn partition(&self, a: IpAddr, b: IpAddr) {
        if a != b {
            lock(&self.shared).partitions.insert(ordered(a, b));
        }
    }

    /// Joins hosts `a` and `b` again: bytes held at the cut become due.
    pub fn heal(&self, a: IpAddr, b: IpAddr) {
        lock(&self.shared).partitions.remove(&ordered(a, b));
    }

    /// Joins every pair of hosts cut apart.
    pub fn heal_all(&self) {
        lock(&self.shared).partitions.clear();
    }

    /// The host with IP address `ip`.
    pub fn host(&self, ip: IpAddr) -> SimHost {
        SimHost {
            network: self.clone(),
            ip,
        }
    }

    /// Seeds the order in which operations complete and bytes arrive from now on.
    pub fn seed(&self, seed: u64) {
        lock(&self.shared).rng = SplitMix64::new(seed);
    }

    /// Makes receives and sends transfer fewer bytes than they could at a seeded rate, or
    /// stops it.
    pub fn short_transfers(&self, enabled: bool) {
        lock(&self.shared).short_transfers = enabled;
    }

    /// How many listens and completed operations have been numbered so far.
    pub fn operation_count(&self) -> u64 {
        lock(&self.shared).operations
    }

    /// Makes the listen or operation numbered `index` fail.
    pub fn fail_operation(&self, index: u64) {
        lock(&self.shared).failing_operations.insert(index);
    }

    /// Makes each later listen or operation fail with probability `failures_per_million` in a
    /// million, drawn from a generator seeded with `seed`. A rate of zero stops it.
    pub fn fail_randomly(&self, seed: u64, failures_per_million: u32) {
        lock(&self.shared).random_failures = (failures_per_million > 0).then(|| RandomFailures {
            rng: SplitMix64::new(seed),
            per_million: failures_per_million,
        });
    }

    /// How many connections exist, counting those not yet accepted and those one end still
    /// holds.
    pub fn connection_count(&self) -> usize {
        lock(&self.shared).connections.len()
    }
}

/// One host of a [`SimNetwork`].
#[derive(Debug, Clone)]
pub struct SimHost {
    network: SimNetwork,
    ip: IpAddr,
}

impl Network for SimHost {
    type Listener = SimListener;
    type Stream = SimStream;
    type Queue = SimNetQueue;

    fn listen(&self, address: Address) -> Result<SimListener> {
        let Address::Tcp(address) = address;
        let mut world = lock(&self.network.shared);
        let ip = if address.ip().is_unspecified() {
            self.ip
        } else if address.ip() == self.ip {
            address.ip()
        } else {
            return Err(Error::new(ErrorKind::AddrNotAvailable));
        };
        let address = if address.port() == 0 {
            world
                .free_port(ip)
                .ok_or_else(|| Error::new(ErrorKind::AddrNotAvailable))?
        } else {
            SocketAddr::new(ip, address.port())
        };
        if world.bound.contains(&address) {
            return Err(Error::new(ErrorKind::AddrInUse));
        }
        world.next_operation()?;
        world.bound.insert(address);
        world.listeners.insert(address, VecDeque::new());
        Ok(SimListener {
            network: self.network.clone(),
            address,
        })
    }

    fn net_queue(&self, options: NetQueueOptions) -> Result<SimNetQueue> {
        Ok(SimNetQueue {
            network: self.network.clone(),
            ip: self.ip,
            capacity: options.validated()?,
            in_flight: 0,
            waiting: Vec::new(),
            transferring: BTreeSet::new(),
        })
    }
}

/// A listener of a [`SimHost`]. Dropping it resets the connections it has not accepted.
pub struct SimListener {
    network: SimNetwork,
    address: SocketAddr,
}

impl fmt::Debug for SimListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimListener")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl NetListener for SimListener {
    fn local_address(&self) -> Result<Address> {
        Ok(Address::Tcp(self.address))
    }
}

impl Drop for SimListener {
    fn drop(&mut self) {
        let mut world = lock(&self.network.shared);
        world.bound.remove(&self.address);
        let backlog = world.listeners.remove(&self.address).unwrap_or_default();
        for connection in backlog {
            world.reset(connection);
            world.close(connection, SERVER);
        }
    }
}

/// A connection of a [`SimHost`].
pub struct SimStream {
    network: SimNetwork,
    connection: u64,
    side: usize,
}

impl fmt::Debug for SimStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimStream")
            .field("connection", &self.connection)
            .field("side", &self.side)
            .finish_non_exhaustive()
    }
}

impl SimStream {
    fn end(&self, side: usize) -> Result<Address> {
        lock(&self.network.shared)
            .connections
            .get(&self.connection)
            .map(|connection| Address::Tcp(connection.ends[side]))
            .ok_or_else(|| Error::new(ErrorKind::NotConnected))
    }
}

impl NetStream for SimStream {
    fn local_address(&self) -> Result<Address> {
        self.end(self.side)
    }

    fn peer_address(&self) -> Result<Address> {
        self.end(1 - self.side)
    }

    fn shutdown_write(&self) -> Result<()> {
        let mut world = lock(&self.network.shared);
        if !world.connections.contains_key(&self.connection) {
            return Err(Error::new(ErrorKind::NotConnected));
        }
        world.end_flow(self.connection, self.side);
        Ok(())
    }
}

impl Drop for SimStream {
    fn drop(&mut self) {
        let mut world = lock(&self.network.shared);
        let owed = world
            .connections
            .get(&self.connection)
            .is_some_and(|connection| connection.flows[1 - self.side].buffered() > 0);
        if owed {
            world.reset(self.connection);
        } else {
            world.end_flow(self.connection, self.side);
        }
        world.close(self.connection, self.side);
    }
}

type Op = NetOp<SimListener, SimStream>;
type Completion = NetCompletion<SimListener, SimStream>;

#[derive(Debug)]
struct Waiting {
    op: Op,
    user_data: u64,
    since: Duration,
    handshake_done_at: Option<Duration>,
    canceled: bool,
}

/// The queue of a [`SimHost`].
#[derive(Debug)]
pub struct SimNetQueue {
    network: SimNetwork,
    ip: IpAddr,
    capacity: usize,
    in_flight: usize,
    waiting: Vec<Waiting>,
    transferring: BTreeSet<Transfer>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Transfer {
    connection: u64,
    side: usize,
    sending: bool,
}

impl Transfer {
    fn of(op: &Op) -> Option<Self> {
        match op {
            NetOp::Receive { stream, .. } => Some(Self {
                connection: stream.connection,
                side: stream.side,
                sending: false,
            }),
            NetOp::Send { stream, .. } => Some(Self {
                connection: stream.connection,
                side: stream.side,
                sending: true,
            }),
            NetOp::Accept { .. } | NetOp::Connect { .. } => None,
        }
    }
}

impl SimNetQueue {
    fn owns(&self, op: &Op) -> bool {
        let network = match op {
            NetOp::Accept { listener } => &listener.network,
            NetOp::Receive { stream, .. } | NetOp::Send { stream, .. } => &stream.network,
            NetOp::Connect { .. } => return true,
        };
        Arc::ptr_eq(&network.shared, &self.network.shared)
    }

    fn perform(&mut self, world: &mut World, completions: &mut Vec<Completion>) -> usize {
        let mut order: Vec<usize> = (0..self.waiting.len()).collect();
        for at in (1..order.len()).rev() {
            let other = world.random_below(at + 1);
            order.swap(at, other);
        }
        let mut finished = Vec::new();
        for index in order {
            let waiting = &mut self.waiting[index];
            let result = if waiting.canceled {
                Some(Err(Error::new(ErrorKind::Canceled)))
            } else {
                world.attempt(&self.network, self.ip, waiting)
            };
            if let Some(result) = result {
                finished.push((index, result));
            }
        }
        let count = finished.len();
        finished.sort_by_key(|(index, _)| std::cmp::Reverse(*index));
        let mut done = Vec::with_capacity(count);
        for (index, result) in finished {
            let Waiting { op, user_data, .. } = self.waiting.swap_remove(index);
            if let Some(transfer) = Transfer::of(&op) {
                self.transferring.remove(&transfer);
            }
            done.push(NetCompletion {
                user_data,
                result,
                op,
            });
        }
        done.reverse();
        completions.extend(done);
        count
    }

    fn next_event(&self, world: &World) -> Option<Duration> {
        let connects = self.waiting.iter().filter_map(|waiting| {
            let NetOp::Connect { address } = &waiting.op else {
                return None;
            };
            let Address::Tcp(address) = address;
            if world.cut(self.ip, address.ip()) {
                Some(waiting.since.saturating_add(SimNetwork::CONNECT_TIMEOUT))
            } else {
                waiting.handshake_done_at
            }
        });
        let now = world.now();
        connects
            .chain(world.next_event())
            .filter(|&at| at > now)
            .min()
    }

    fn reap_until(
        &mut self,
        completions: &mut Vec<Completion>,
        min: usize,
        deadline: Option<Duration>,
    ) -> Result<usize> {
        if min > self.in_flight {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        let network = self.network.clone();
        let mut world = lock(&network.shared);
        let mut reaped = 0;
        let outcome = loop {
            world.deliver_some();
            let done = self.perform(&mut world, completions);
            reaped += done;
            if reaped >= min {
                break Ok(());
            }
            if done > 0 || world.deliver_due() {
                continue;
            }
            match (self.next_event(&world), deadline) {
                (Some(at), Some(deadline)) if at > deadline => {
                    world.advance_to(deadline);
                    break Ok(());
                }
                (Some(at), _) => {
                    world.advance_to(at);
                    if world.now() < at {
                        break match deadline {
                            Some(_) => Ok(()),
                            None => Err(Error::new(ErrorKind::Stalled)),
                        };
                    }
                }
                (None, Some(deadline)) => {
                    world.advance_to(deadline);
                    break Ok(());
                }
                (None, None) => break Err(Error::new(ErrorKind::Stalled)),
            }
        };
        self.in_flight -= reaped;
        outcome.map(|()| reaped)
    }
}

impl NetQueue for SimNetQueue {
    type Listener = SimListener;
    type Stream = SimStream;

    fn submit(
        &mut self,
        op: Op,
        user_data: u64,
    ) -> core::result::Result<(), RejectedNetOp<SimListener, SimStream>> {
        if self.in_flight >= self.capacity {
            return Err(net_queue_full(op));
        }
        let transfer = Transfer::of(&op);
        let overlapping = transfer.is_some_and(|transfer| self.transferring.contains(&transfer));
        if overlapping || !self.owns(&op) {
            return Err(RejectedNetOp {
                error: Error::new(ErrorKind::InvalidInput),
                op,
            });
        }
        if let Some(transfer) = transfer {
            self.transferring.insert(transfer);
        }
        let since = lock(&self.network.shared).now();
        self.waiting.push(Waiting {
            op,
            user_data,
            since,
            handshake_done_at: None,
            canceled: false,
        });
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
        let deadline = lock(&self.network.shared).now().checked_add(timeout);
        self.reap_until(completions, min, deadline)
    }

    fn cancel(&mut self, user_data: u64) -> Result<()> {
        let mut found = false;
        for waiting in &mut self.waiting {
            if waiting.user_data == user_data {
                waiting.canceled = true;
                found = true;
            }
        }
        if found {
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

impl World {
    fn now(&self) -> Duration {
        self.clock.monotonic()
    }

    fn advance_to(&self, at: Duration) {
        self.clock.advance(at.saturating_sub(self.now()));
    }

    fn delay(&mut self) -> Duration {
        let [least, most] = self.latency;
        let spread = u64::try_from((most - least).as_nanos()).unwrap_or(u64::MAX);
        if spread == 0 {
            return least;
        }
        let extra = self.rng.next_u64() % spread.saturating_add(1);
        least.saturating_add(Duration::from_nanos(extra))
    }

    fn cut(&self, a: IpAddr, b: IpAddr) -> bool {
        a != b && self.partitions.contains(&ordered(a, b))
    }

    fn connection_cut(&self, connection: &Connection) -> bool {
        self.cut(connection.ends[CLIENT].ip(), connection.ends[SERVER].ip())
    }

    fn end_flow(&mut self, id: u64, side: usize) {
        if !self.connections.contains_key(&id) {
            return;
        }
        let at = self.now().saturating_add(self.delay());
        if let Some(connection) = self.connections.get_mut(&id) {
            connection.flows[side].end(at);
        }
    }

    fn next_event(&self) -> Option<Duration> {
        self.connections
            .values()
            .filter(|connection| !self.connection_cut(connection))
            .flat_map(|connection| connection.flows.iter().filter_map(Flow::next_event))
            .min()
    }

    fn random_below(&mut self, bound: usize) -> usize {
        usize::try_from(self.rng.next_u64() % bound as u64).unwrap_or(0)
    }

    fn next_operation(&mut self) -> Result<()> {
        let index = self.operations;
        self.operations += 1;
        let random = self
            .random_failures
            .as_mut()
            .is_some_and(|failures| failures.rng.one_in_million(failures.per_million));
        if self.failing_operations.remove(&index) || random {
            return Err(Error::new(ErrorKind::InjectedFault));
        }
        Ok(())
    }

    fn free_port(&self, ip: IpAddr) -> Option<SocketAddr> {
        SimNetwork::EPHEMERAL_PORTS
            .map(|port| SocketAddr::new(ip, port))
            .find(|address| !self.bound.contains(address))
    }

    fn reset(&mut self, connection: u64) {
        if let Some(connection) = self.connections.get_mut(&connection) {
            connection.reset = true;
            for flow in &mut connection.flows {
                flow.clear();
            }
        }
    }

    fn close(&mut self, id: u64, side: usize) {
        let Some(connection) = self.connections.get_mut(&id) else {
            return;
        };
        connection.open[side] = false;
        if connection.open == [false, false] {
            let client = connection.ends[CLIENT];
            self.connections.remove(&id);
            self.bound.remove(&client);
        }
    }

    fn deliver_some(&mut self) {
        let now = self.now();
        let partitions = std::mem::take(&mut self.partitions);
        let mut rng = std::mem::take(&mut self.rng);
        for connection in self.connections.values_mut() {
            let [client, server] = connection.ends.map(|end| end.ip());
            if client != server && partitions.contains(&ordered(client, server)) {
                continue;
            }
            for flow in &mut connection.flows {
                if !rng.coin() {
                    continue;
                }
                let due = flow.due(now);
                if due > 0 {
                    let arriving = 1 + usize::try_from(rng.next_u64() % due as u64).unwrap_or(0);
                    flow.deliver(arriving);
                } else if flow.end_due(now) {
                    flow.end_arrived = true;
                }
            }
        }
        self.rng = rng;
        self.partitions = partitions;
    }

    fn deliver_due(&mut self) -> bool {
        let now = self.now();
        let mut delivered = false;
        let ids: Vec<u64> = self
            .connections
            .iter()
            .filter(|(_, connection)| !self.connection_cut(connection))
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            let Some(connection) = self.connections.get_mut(&id) else {
                continue;
            };
            for flow in &mut connection.flows {
                let due = flow.due(now);
                if due > 0 {
                    flow.deliver(due);
                    delivered = true;
                }
                if flow.end_due(now) {
                    flow.end_arrived = true;
                    delivered = true;
                }
            }
        }
        delivered
    }

    fn transfer_size(&mut self, most: usize) -> usize {
        if self.short_transfers && most > 1 && self.rng.coin() {
            1 + self.random_below(most)
        } else {
            most
        }
    }

    fn attempt(
        &mut self,
        network: &SimNetwork,
        ip: IpAddr,
        waiting: &mut Waiting,
    ) -> Option<Result<NetOutput<SimStream>>> {
        if let Some(answer) = answered_at_once(&waiting.op) {
            return Some(self.next_operation().and(answer));
        }
        match &mut waiting.op {
            NetOp::Accept { listener } => self.accept(network, listener.address),
            NetOp::Connect { address } => {
                let Address::Tcp(address) = *address;
                let since = waiting.since;
                self.connect(network, ip, address, since, &mut waiting.handshake_done_at)
            }
            NetOp::Receive { stream, buffer } => self.receive(stream, buffer),
            NetOp::Send { stream, buffer } => self.send(stream, buffer),
        }
    }

    fn accept(
        &mut self,
        network: &SimNetwork,
        address: SocketAddr,
    ) -> Option<Result<NetOutput<SimStream>>> {
        if self.listeners.get(&address)?.is_empty() {
            return None;
        }
        if let Err(error) = self.next_operation() {
            return Some(Err(error));
        }
        let connection = self.listeners.get_mut(&address)?.pop_front()?;
        Some(Ok(NetOutput::Connected(SimStream {
            network: network.clone(),
            connection,
            side: SERVER,
        })))
    }

    fn connect(
        &mut self,
        network: &SimNetwork,
        ip: IpAddr,
        address: SocketAddr,
        since: Duration,
        handshake_done_at: &mut Option<Duration>,
    ) -> Option<Result<NetOutput<SimStream>>> {
        let now = self.now();
        if self.cut(ip, address.ip()) {
            if now < since.saturating_add(SimNetwork::CONNECT_TIMEOUT) {
                return None;
            }
            return Some(
                self.next_operation()
                    .and(Err(Error::new(ErrorKind::TimedOut))),
            );
        }
        let done_at = match *handshake_done_at {
            Some(at) => at,
            None => {
                let round_trip = self.delay().saturating_add(self.delay());
                let at = now.saturating_add(round_trip);
                *handshake_done_at = Some(at);
                at
            }
        };
        if done_at > now {
            return None;
        }
        let Some(backlog) = self.listeners.get(&address) else {
            return Some(
                self.next_operation()
                    .and(Err(Error::new(ErrorKind::ConnectionRefused))),
            );
        };
        if backlog.len() >= SimNetwork::MAX_BACKLOG {
            return None;
        }
        if let Err(error) = self.next_operation() {
            return Some(Err(error));
        }
        let local = self
            .free_port(ip)
            .filter(|_| self.connections.len() < SimNetwork::MAX_CONNECTIONS);
        let Some(local) = local else {
            return Some(Err(Error::new(ErrorKind::AddrNotAvailable)));
        };
        let id = self.next_connection;
        self.next_connection += 1;
        self.bound.insert(local);
        self.connections.insert(
            id,
            Connection {
                ends: [local, address],
                flows: [Flow::default(), Flow::default()],
                open: [true, true],
                reset: false,
            },
        );
        self.listeners.get_mut(&address)?.push_back(id);
        Some(Ok(NetOutput::Connected(SimStream {
            network: network.clone(),
            connection: id,
            side: CLIENT,
        })))
    }

    fn receive(
        &mut self,
        stream: &SimStream,
        buffer: &mut [u8],
    ) -> Option<Result<NetOutput<SimStream>>> {
        let connection = self.connections.get(&stream.connection)?;
        let flow = &connection.flows[1 - stream.side];
        let readable = flow.arrived.len().min(buffer.len());
        if !connection.reset && readable == 0 && !flow.end_arrived {
            return None;
        }
        let reset = connection.reset;
        if let Err(error) = self.next_operation() {
            return Some(Err(error));
        }
        if reset {
            return Some(Err(Error::new(ErrorKind::ConnectionReset)));
        }
        let count = self.transfer_size(readable);
        let flow = &mut self.connections.get_mut(&stream.connection)?.flows[1 - stream.side];
        for (slot, byte) in buffer.iter_mut().zip(flow.arrived.drain(..count)) {
            *slot = byte;
        }
        Some(Ok(NetOutput::Transferred(count)))
    }

    fn send(&mut self, stream: &SimStream, buffer: &[u8]) -> Option<Result<NetOutput<SimStream>>> {
        let connection = self.connections.get(&stream.connection)?;
        let refused = connection.reset || connection.flows[stream.side].ended;
        let peer_gone = !connection.open[1 - stream.side];
        let room = SimNetwork::BUFFER_BYTES
            .saturating_sub(connection.flows[stream.side].buffered())
            .min(buffer.len());
        if !refused && !peer_gone && room == 0 {
            return None;
        }
        if let Err(error) = self.next_operation() {
            return Some(Err(error));
        }
        if refused {
            return Some(Err(Error::new(ErrorKind::ConnectionReset)));
        }
        if peer_gone {
            let count = self.transfer_size(buffer.len().min(SimNetwork::BUFFER_BYTES));
            self.reset(stream.connection);
            return Some(Ok(NetOutput::Transferred(count)));
        }
        let count = self.transfer_size(room);
        let at = self.now().saturating_add(self.delay());
        let flow = &mut self.connections.get_mut(&stream.connection)?.flows[stream.side];
        flow.send(&buffer[..count], at);
        Some(Ok(NetOutput::Transferred(count)))
    }
}

fn ordered(a: IpAddr, b: IpAddr) -> [IpAddr; 2] {
    [a.min(b), a.max(b)]
}

fn lock(shared: &Mutex<World>) -> MutexGuard<'_, World> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::SimNetwork;
    use crate::{
        Address, Clock, ErrorKind, NetCompletion, NetListener, NetOp, NetQueue, NetQueueOptions,
        NetStream, Network, SimClock, SimListener, SimNetQueue, SimStream,
    };
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::time::Duration;

    const SERVER: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

    type Completion = NetCompletion<SimListener, SimStream>;

    struct Pair {
        network: SimNetwork,
        listener: Arc<SimListener>,
        queue: SimNetQueue,
    }

    impl Pair {
        fn new(seed: u64) -> Self {
            let network = SimNetwork::new();
            network.seed(seed);
            let address = Address::Tcp(SocketAddr::new(SERVER, 5432));
            let listener = Arc::new(network.host(SERVER).listen(address).unwrap());
            let queue = network
                .host(CLIENT)
                .net_queue(NetQueueOptions::new())
                .unwrap();
            Self {
                network,
                listener,
                queue,
            }
        }

        fn submit(&mut self, op: NetOp<SimListener, SimStream>, user_data: u64) {
            assert!(self.queue.submit(op, user_data).is_ok());
        }

        fn reap(&mut self, min: usize) -> Vec<Completion> {
            let mut completions = Vec::new();
            self.queue.reap(&mut completions, min).unwrap();
            completions
        }

        fn reap_for(&mut self, min: usize, timeout: Duration) -> Vec<Completion> {
            let mut completions = Vec::new();
            self.queue
                .reap_timeout(&mut completions, min, timeout)
                .unwrap();
            completions
        }

        fn now(&self) -> Duration {
            self.network.clock().monotonic()
        }

        fn clock_at_its_limit(&self) {
            self.network.clock().advance(Duration::MAX);
        }

        fn connect_op(&self) -> NetOp<SimListener, SimStream> {
            NetOp::Connect {
                address: self.listener.local_address().unwrap(),
            }
        }

        fn connect(&mut self) -> (Arc<SimStream>, Arc<SimStream>) {
            self.submit(
                NetOp::Accept {
                    listener: Arc::clone(&self.listener),
                },
                1,
            );
            self.submit(
                NetOp::Connect {
                    address: self.listener.local_address().unwrap(),
                },
                2,
            );
            let mut completions = self.reap(2);
            completions.sort_by_key(|completion| completion.user_data);
            let mut streams = completions
                .into_iter()
                .map(|completion| Arc::new(completion.result.unwrap().into_stream().unwrap()));
            let server = streams.next().unwrap();
            (streams.next().unwrap(), server)
        }

        fn send(&mut self, stream: &Arc<SimStream>, bytes: &[u8], user_data: u64) {
            let op = NetOp::Send {
                stream: Arc::clone(stream),
                buffer: bytes.to_vec().into(),
            };
            self.submit(op, user_data);
        }

        fn receive(&mut self, stream: &Arc<SimStream>, len: usize, user_data: u64) {
            let op = NetOp::Receive {
                stream: Arc::clone(stream),
                buffer: vec![0; len].into(),
            };
            self.submit(op, user_data);
        }
    }

    fn trace(seed: u64) -> Vec<(u64, usize)> {
        let mut pair = Pair::new(seed);
        pair.network.short_transfers(true);
        if seed % 2 == 1 {
            pair.network
                .latency(Duration::from_micros(10), Duration::from_millis(5));
        }
        let (first, first_server) = pair.connect();
        let (second, second_server) = pair.connect();
        let mut trace = Vec::new();
        for round in 0..8 {
            pair.send(&first, &[1; 300], 10 * round);
            pair.send(&second, &[2; 300], 10 * round + 1);
            pair.receive(&first_server, 300, 10 * round + 2);
            pair.receive(&second_server, 300, 10 * round + 3);
            for completion in pair.reap(4) {
                let count = completion.result.unwrap().transferred().unwrap();
                trace.push((completion.user_data, count));
            }
        }
        trace
    }

    #[test]
    fn a_seed_repeats_exactly_and_different_seeds_differ() {
        assert_eq!(trace(3), trace(3));
        assert!((0..8).any(|seed| trace(seed) != trace(3)));
    }

    #[test]
    fn latency_delays_bytes_and_moves_the_clock_only_as_far_as_needed() {
        let mut pair = Pair::new(8);
        let one_way = Duration::from_millis(10);
        pair.network.latency(one_way, one_way);
        let (client, server) = pair.connect();
        assert_eq!(pair.now(), 2 * one_way);
        pair.send(&client, b"hello", 0);
        pair.reap(1);
        pair.receive(&server, 5, 1);
        let mut completions = Vec::new();
        let early = pair.queue.reap_timeout(&mut completions, 1, one_way / 2);
        assert_eq!(early.unwrap(), 0);
        assert_eq!(pair.now(), 2 * one_way + one_way / 2);
        let completion = pair.reap(1).pop().unwrap();
        let count = completion.result.unwrap().transferred().unwrap();
        assert!(count > 0);
        assert_eq!(
            completion.op.into_buffer().unwrap()[..count],
            b"hello"[..count]
        );
        assert_eq!(pair.now(), 3 * one_way);
    }

    #[test]
    fn bytes_sent_later_never_overtake_bytes_sent_earlier() {
        let mut pair = Pair::new(9);
        pair.network
            .latency(Duration::from_millis(1), Duration::from_millis(50));
        let (client, server) = pair.connect();
        let mut expected = Vec::new();
        for round in 0..20u8 {
            pair.send(&client, &[round; 3], u64::from(round));
            pair.reap(1);
            expected.extend([round; 3]);
        }
        let mut received = Vec::new();
        while received.len() < expected.len() {
            pair.receive(&server, 64, 100);
            let completion = pair.reap(1).pop().unwrap();
            let count = completion.result.unwrap().transferred().unwrap();
            received.extend_from_slice(&completion.op.into_buffer().unwrap()[..count]);
        }
        assert_eq!(received, expected);
    }

    #[test]
    fn a_connect_across_a_partition_times_out_like_an_unanswered_handshake() {
        let mut pair = Pair::new(10);
        pair.network.partition(CLIENT, SERVER);
        let op = pair.connect_op();
        pair.submit(op, 0);
        assert!(pair.reap_for(1, Duration::from_secs(5)).is_empty());
        assert_eq!(pair.now(), Duration::from_secs(5));
        let completion = pair.reap(1).pop().unwrap();
        assert_eq!(completion.result.unwrap_err().kind(), ErrorKind::TimedOut);
        assert_eq!(pair.now(), SimNetwork::CONNECT_TIMEOUT);
        assert_eq!(pair.network.connection_count(), 0);
    }

    #[test]
    fn a_connect_finishes_once_the_partition_heals_in_time() {
        let mut pair = Pair::new(11);
        pair.network.partition(SERVER, CLIENT);
        let op = pair.connect_op();
        pair.submit(op, 0);
        assert!(pair.reap_for(1, Duration::from_secs(30)).is_empty());
        pair.network.heal(CLIENT, SERVER);
        let completion = pair.reap(1).pop().unwrap();
        assert!(completion.result.unwrap().into_stream().is_some());
        assert_eq!(pair.now(), Duration::from_secs(30));
    }

    #[test]
    fn bytes_wait_at_a_partition_until_it_heals() {
        let mut pair = Pair::new(12);
        let (client, server) = pair.connect();
        pair.network.partition(CLIENT, SERVER);
        pair.send(&client, b"held", 0);
        pair.reap(1);
        pair.receive(&server, 8, 1);
        let mut completions = Vec::new();
        let error = pair.queue.reap(&mut completions, 1).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Stalled);
        assert!(pair.reap_for(1, Duration::from_secs(600)).is_empty());
        assert_eq!(pair.now(), Duration::from_secs(600));
        pair.network.heal_all();
        let completion = pair.reap(1).pop().unwrap();
        assert!(completion.result.unwrap().transferred() > Some(0));
    }

    #[test]
    fn partitions_only_cut_the_hosts_named() {
        let mut pair = Pair::new(13);
        pair.network
            .partition(SERVER, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)));
        pair.network.partition(CLIENT, CLIENT);
        let (client, server) = pair.connect();
        pair.send(&client, b"free", 0);
        pair.receive(&server, 4, 1);
        assert_eq!(pair.reap(2).len(), 2);
        assert_eq!(pair.now(), Duration::ZERO);
    }

    #[test]
    fn canceled_operations_come_back_unnumbered_with_their_buffers() {
        let mut pair = Pair::new(14);
        let (_client, server) = pair.connect();
        pair.receive(&server, 32, 5);
        let before = pair.network.operation_count();
        pair.queue.cancel(5).unwrap();
        let completion = pair.reap(1).pop().unwrap();
        assert_eq!(completion.result.unwrap_err().kind(), ErrorKind::Canceled);
        assert_eq!(completion.op.into_buffer().unwrap().len(), 32);
        assert_eq!(pair.network.operation_count(), before);
        pair.receive(&server, 32, 6);
        assert_eq!(pair.queue.in_flight(), 1);
    }

    #[test]
    fn a_shared_clock_carries_the_networks_time() {
        let clock = Arc::new(SimClock::new(0));
        let network = SimNetwork::with_clock(Arc::clone(&clock));
        assert!(Arc::ptr_eq(&network.clock(), &clock));
        let mut queue = network
            .host(CLIENT)
            .net_queue(NetQueueOptions::new())
            .unwrap();
        let mut completions = Vec::new();
        let timeout = Duration::from_millis(7);
        assert_eq!(queue.reap_timeout(&mut completions, 0, timeout).unwrap(), 0);
        assert_eq!(clock.monotonic(), Duration::ZERO);
        let connect_across_cut = NetOp::Connect {
            address: Address::Tcp(SocketAddr::new(SERVER, 1)),
        };
        network.partition(CLIENT, SERVER);
        assert!(queue.submit(connect_across_cut, 0).is_ok());
        assert_eq!(queue.reap_timeout(&mut completions, 1, timeout).unwrap(), 0);
        assert_eq!(clock.monotonic(), timeout);
    }

    #[test]
    fn waits_at_the_end_of_simulated_time_return_instead_of_spinning() {
        let mut pair = Pair::new(15);
        pair.network
            .latency(Duration::from_secs(1), Duration::from_secs(2));
        let (client, server) = pair.connect();
        pair.clock_at_its_limit();
        assert!(pair.reap_for(0, Duration::MAX).is_empty());
        pair.send(&client, b"late", 0);
        pair.reap(1);
        pair.receive(&server, 4, 1);
        let mut completions = Vec::new();
        let error = pair.queue.reap(&mut completions, 1).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Stalled);
        assert!(pair.reap_for(1, Duration::from_secs(5)).is_empty());
        pair.network.partition(CLIENT, SERVER);
        let op = pair.connect_op();
        pair.submit(op, 2);
        let error = pair.queue.reap(&mut completions, 1).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Stalled);
        let error = pair
            .queue
            .reap_timeout(&mut completions, 1, Duration::MAX)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Stalled);
    }

    #[test]
    fn a_receive_nobody_can_satisfy_stalls_instead_of_waiting() {
        let mut pair = Pair::new(0);
        let (client, _server) = pair.connect();
        pair.receive(&client, 8, 0);
        let mut completions = Vec::new();
        let error = pair.queue.reap(&mut completions, 1).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Stalled);
        assert_eq!(pair.queue.in_flight(), 1);
    }

    #[test]
    fn dropping_an_end_with_bytes_owed_to_it_resets_the_connection() {
        let mut pair = Pair::new(1);
        let (client, server) = pair.connect();
        pair.send(&client, b"unread", 0);
        pair.reap(1);
        drop(server);
        pair.receive(&client, 8, 1);
        let completion = pair.reap(1).pop().unwrap();
        assert_eq!(
            completion.result.unwrap_err().kind(),
            ErrorKind::ConnectionReset
        );
        pair.send(&client, b"more", 2);
        let completion = pair.reap(1).pop().unwrap();
        assert_eq!(
            completion.result.unwrap_err().kind(),
            ErrorKind::ConnectionReset
        );
    }

    #[test]
    fn dropping_an_end_with_nothing_owed_ends_its_sending() {
        let mut pair = Pair::new(2);
        let (client, server) = pair.connect();
        drop(client);
        pair.receive(&server, 8, 0);
        let completion = pair.reap(1).pop().unwrap();
        assert_eq!(completion.result.unwrap().transferred(), Some(0));
    }

    #[test]
    fn the_first_send_after_the_peer_left_succeeds_and_resets_the_connection() {
        let mut pair = Pair::new(4);
        let (client, server) = pair.connect();
        drop(server);
        pair.send(&client, b"lost", 0);
        let completion = pair.reap(1).pop().unwrap();
        assert!(completion.result.unwrap().transferred().unwrap() > 0);
        pair.send(&client, b"refused", 1);
        pair.receive(&client, 8, 2);
        for completion in pair.reap(2) {
            assert_eq!(
                completion.result.unwrap_err().kind(),
                ErrorKind::ConnectionReset
            );
        }
    }

    #[test]
    fn a_second_send_or_receive_on_one_end_is_refused_while_the_first_is_in_flight() {
        let mut pair = Pair::new(5);
        let (client, server) = pair.connect();
        pair.send(&client, b"first", 0);
        pair.receive(&server, 8, 1);
        for op in [
            NetOp::Send {
                stream: Arc::clone(&client),
                buffer: b"second".to_vec().into(),
            },
            NetOp::Receive {
                stream: Arc::clone(&server),
                buffer: vec![0; 8].into(),
            },
        ] {
            let rejected = pair.queue.submit(op, 2).unwrap_err();
            assert_eq!(rejected.error.kind(), ErrorKind::InvalidInput);
        }
        pair.send(&server, b"other way", 3);
        pair.receive(&client, 16, 4);
        pair.reap(4);
        pair.send(&client, b"again", 5);
        pair.reap(1);
    }

    #[test]
    fn connections_disappear_when_both_ends_are_dropped() {
        let mut pair = Pair::new(3);
        let (client, server) = pair.connect();
        assert_eq!(pair.network.connection_count(), 1);
        drop(client);
        drop(server);
        assert_eq!(pair.network.connection_count(), 0);
    }

    #[test]
    fn dropping_a_listener_resets_what_it_did_not_accept() {
        let mut pair = Pair::new(4);
        pair.submit(
            NetOp::Connect {
                address: pair.listener.local_address().unwrap(),
            },
            0,
        );
        let client = Arc::new(
            pair.reap(1)
                .pop()
                .unwrap()
                .result
                .unwrap()
                .into_stream()
                .unwrap(),
        );
        let Pair {
            network,
            listener,
            mut queue,
        } = pair;
        drop(listener);
        let op = NetOp::Receive {
            stream: Arc::clone(&client),
            buffer: vec![0; 4].into(),
        };
        assert!(queue.submit(op, 1).is_ok());
        let mut completions = Vec::new();
        queue.reap(&mut completions, 1).unwrap();
        let error = completions.pop().unwrap().result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConnectionReset);
        drop(client);
        assert_eq!(network.connection_count(), 0);
    }

    #[test]
    fn a_send_never_buffers_more_than_the_limit() {
        let mut pair = Pair::new(5);
        let (client, _server) = pair.connect();
        pair.send(&client, &vec![7; 3 * SimNetwork::BUFFER_BYTES], 0);
        let completion = pair.reap(1).pop().unwrap();
        let sent = completion.result.unwrap().transferred().unwrap();
        assert_eq!(sent, SimNetwork::BUFFER_BYTES);
        pair.send(&client, &[7], 1);
        let mut completions = Vec::new();
        let error = pair.queue.reap(&mut completions, 1).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Stalled);
    }

    #[test]
    fn injected_faults_fail_one_operation_and_change_nothing() {
        let mut pair = Pair::new(6);
        let next = pair.network.operation_count();
        pair.network.fail_operation(next);
        pair.submit(
            NetOp::Connect {
                address: pair.listener.local_address().unwrap(),
            },
            0,
        );
        let completion = pair.reap(1).pop().unwrap();
        assert_eq!(
            completion.result.unwrap_err().kind(),
            ErrorKind::InjectedFault
        );
        assert_eq!(pair.network.connection_count(), 0);
        let (client, server) = pair.connect();
        pair.send(&client, b"after", 3);
        pair.receive(&server, 5, 4);
        assert_eq!(pair.reap(2).len(), 2);
    }

    #[test]
    fn random_scripts_with_faults_never_panic_and_leave_nothing_behind() {
        for seed in 0..32 {
            let mut rng = crate::rng::SplitMix64::new(seed);
            let mut pair = Pair::new(seed);
            pair.network.short_transfers(seed % 2 == 0);
            pair.network.fail_randomly(seed, 50_000);
            if seed % 4 >= 2 {
                pair.network
                    .latency(Duration::ZERO, Duration::from_millis(20));
            }
            let mut streams: Vec<Arc<SimStream>> = Vec::new();
            for step in 0..300 {
                let choice = rng.next_u64() % 9;
                let pick = |rng: &mut crate::rng::SplitMix64, streams: &[Arc<SimStream>]| {
                    (!streams.is_empty())
                        .then(|| Arc::clone(&streams[(rng.next_u64() as usize) % streams.len()]))
                };
                let op = match choice {
                    0 => Some(NetOp::Accept {
                        listener: Arc::clone(&pair.listener),
                    }),
                    1 => Some(NetOp::Connect {
                        address: pair.listener.local_address().unwrap(),
                    }),
                    2 => pick(&mut rng, &streams).map(|stream| NetOp::Send {
                        stream,
                        buffer: vec![step as u8; (rng.next_u64() % 5000) as usize].into(),
                    }),
                    3 => pick(&mut rng, &streams).map(|stream| NetOp::Receive {
                        stream,
                        buffer: vec![0; 1 + (rng.next_u64() % 5000) as usize].into(),
                    }),
                    4 if !streams.is_empty() => {
                        let at = (rng.next_u64() as usize) % streams.len();
                        let _ = streams[at].shutdown_write();
                        None
                    }
                    6 => {
                        if rng.coin() {
                            pair.network.partition(CLIENT, SERVER);
                        } else {
                            pair.network.heal(CLIENT, SERVER);
                        }
                        None
                    }
                    7 => {
                        let _ = pair.queue.cancel(rng.next_u64() % (step + 1));
                        None
                    }
                    _ => {
                        if !streams.is_empty() {
                            let at = (rng.next_u64() as usize) % streams.len();
                            drop(streams.swap_remove(at));
                        }
                        None
                    }
                };
                if let Some(op) = op {
                    let _ = pair.queue.submit(op, step);
                }
                let mut completions = Vec::new();
                if choice == 8 {
                    let timeout = Duration::from_millis(rng.next_u64() % 200);
                    let wanted = pair.queue.in_flight().min(1);
                    let _ = pair.queue.reap_timeout(&mut completions, wanted, timeout);
                } else {
                    let _ = pair.queue.reap(&mut completions, 0);
                }
                for completion in completions {
                    if let Ok(output) = completion.result {
                        streams.extend(output.into_stream().map(Arc::new));
                    }
                }
            }
            let Pair {
                network,
                listener,
                queue,
            } = pair;
            drop(queue);
            drop(streams);
            drop(listener);
            assert_eq!(network.connection_count(), 0, "seed {seed}");
        }
    }

    #[test]
    fn a_queue_refuses_connections_of_another_network() {
        let mut first = Pair::new(7);
        let (client, _server) = first.connect();
        let mut other = Pair::new(7);
        let op = NetOp::Receive {
            stream: Arc::clone(&client),
            buffer: vec![0; 4].into(),
        };
        let rejected = other.queue.submit(op, 0).unwrap_err();
        assert_eq!(rejected.error.kind(), ErrorKind::InvalidInput);
        let accept = NetOp::Accept {
            listener: Arc::clone(&first.listener),
        };
        let rejected = other.queue.submit(accept, 1).unwrap_err();
        assert_eq!(rejected.error.kind(), ErrorKind::InvalidInput);
        assert_eq!(other.queue.in_flight(), 0);
    }

    #[test]
    fn listening_off_the_host_or_at_a_taken_port_is_refused() {
        let network = SimNetwork::new();
        let host = network.host(SERVER);
        let elsewhere = Address::Tcp(SocketAddr::new(CLIENT, 1));
        let error = host.listen(elsewhere).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AddrNotAvailable);
        let any = Address::Tcp(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
        let listener = host.listen(any).unwrap();
        let Address::Tcp(bound) = listener.local_address().unwrap();
        assert_eq!(bound.ip(), SERVER);
        assert!(SimNetwork::EPHEMERAL_PORTS.contains(&bound.port()));
        let error = host.listen(Address::Tcp(bound)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AddrInUse);
    }
}
