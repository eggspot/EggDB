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

/// A deterministic simulation of a network of hosts, for tests (D9).
///
/// [`SimNetwork::host`] gives each IP address a [`SimHost`], which implements [`Network`].
/// Connections carry bytes in order in each direction, through bounded buffers of
/// [`SimNetwork::BUFFER_BYTES`]. Bytes sent are first in transit and arrive later, a seeded
/// amount at a time, so connections advance in a different order on every seed while each
/// stays in order. A queue performs its operations only inside `reap`, in a seeded order, and
/// fails with [`ErrorKind::Stalled`] instead of waiting when its operations cannot finish
/// unless another queue acts first, so a test that drives several queues reaps them in turn.
/// A queue refuses, with [`ErrorKind::InvalidInput`], a connection or listener of another
/// `SimNetwork`.
///
/// A queue also refuses a second send, or a second receive, on one end of a connection while
/// the first is in flight, since the two could interleave.
///
/// Connecting where nothing listens is [`ErrorKind::ConnectionRefused`]. Dropping a connection
/// with bytes still owed to it resets it, so its peer's operations fail with
/// [`ErrorKind::ConnectionReset`]; dropping it otherwise ends its sending, as
/// [`NetStream::shutdown_write`] does. As over TCP, the peer's first send after that succeeds
/// and its bytes are lost, and it resets the connection, so the operations after it fail with
/// [`ErrorKind::ConnectionReset`]. Every listen and every operation that completes is
/// numbered from zero, empty receives and sends too, and any of them can be made to fail with
/// [`ErrorKind::InjectedFault`], by number or at random from a seed, changing nothing. Clones
/// share the same network.
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
    arrived: VecDeque<u8>,
    ended: bool,
    end_arrived: bool,
}

impl Flow {
    fn buffered(&self) -> usize {
        self.in_transit.len() + self.arrived.len()
    }

    fn in_transit(&self) -> bool {
        !self.in_transit.is_empty() || (self.ended && !self.end_arrived)
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

    /// Creates an empty network, seeded with 0.
    pub fn new() -> Self {
        Self::default()
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
        let connection = world
            .connections
            .get_mut(&self.connection)
            .ok_or_else(|| Error::new(ErrorKind::NotConnected))?;
        connection.flows[self.side].ended = true;
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
        } else if let Some(connection) = world.connections.get_mut(&self.connection) {
            connection.flows[self.side].ended = true;
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
            if let Some(result) = world.attempt(&self.network, self.ip, &mut waiting.op) {
                finished.push((index, result));
            }
        }
        let count = finished.len();
        finished.sort_by_key(|(index, _)| std::cmp::Reverse(*index));
        let mut done = Vec::with_capacity(count);
        for (index, result) in finished {
            let Waiting { op, user_data } = self.waiting.swap_remove(index);
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
        self.waiting.push(Waiting { op, user_data });
        self.in_flight += 1;
        Ok(())
    }

    fn reap(&mut self, completions: &mut Vec<Completion>, min: usize) -> Result<usize> {
        if min > self.in_flight {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        let network = self.network.clone();
        let mut world = lock(&network.shared);
        let mut reaped = 0;
        loop {
            world.deliver_some();
            let done = self.perform(&mut world, completions);
            reaped += done;
            if reaped >= min {
                break;
            }
            if done == 0 {
                if !world.anything_in_transit() {
                    self.in_flight -= reaped;
                    return Err(Error::new(ErrorKind::Stalled));
                }
                world.deliver_all();
            }
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

impl World {
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
                flow.in_transit.clear();
                flow.arrived.clear();
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

    fn anything_in_transit(&self) -> bool {
        self.connections
            .values()
            .any(|connection| connection.flows.iter().any(Flow::in_transit))
    }

    fn deliver_some(&mut self) {
        let mut rng = std::mem::take(&mut self.rng);
        for connection in self.connections.values_mut() {
            for flow in &mut connection.flows {
                if !rng.coin() {
                    continue;
                }
                let pending = flow.in_transit.len();
                if pending > 0 {
                    let arriving =
                        1 + usize::try_from(rng.next_u64() % pending as u64).unwrap_or(0);
                    let bytes: Vec<u8> = flow.in_transit.drain(..arriving).collect();
                    flow.arrived.extend(bytes);
                } else if flow.ended {
                    flow.end_arrived = true;
                }
            }
        }
        self.rng = rng;
    }

    fn deliver_all(&mut self) {
        for connection in self.connections.values_mut() {
            for flow in &mut connection.flows {
                let bytes: Vec<u8> = flow.in_transit.drain(..).collect();
                flow.arrived.extend(bytes);
                if flow.ended {
                    flow.end_arrived = true;
                }
            }
        }
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
        op: &mut Op,
    ) -> Option<Result<NetOutput<SimStream>>> {
        if let Some(answer) = answered_at_once(op) {
            return Some(self.next_operation().and(answer));
        }
        match op {
            NetOp::Accept { listener } => self.accept(network, listener.address),
            NetOp::Connect { address } => {
                let Address::Tcp(address) = *address;
                self.connect(network, ip, address)
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
    ) -> Option<Result<NetOutput<SimStream>>> {
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
        let flow = &mut self.connections.get_mut(&stream.connection)?.flows[stream.side];
        flow.in_transit.extend(&buffer[..count]);
        Some(Ok(NetOutput::Transferred(count)))
    }
}

fn lock(shared: &Mutex<World>) -> MutexGuard<'_, World> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::SimNetwork;
    use crate::{
        Address, ErrorKind, NetCompletion, NetListener, NetOp, NetQueue, NetQueueOptions,
        NetStream, Network, SimListener, SimNetQueue, SimStream,
    };
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;

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
            let mut streams: Vec<Arc<SimStream>> = Vec::new();
            for step in 0..300 {
                let choice = rng.next_u64() % 6;
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
                let _ = pair.queue.reap(&mut completions, 0);
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
