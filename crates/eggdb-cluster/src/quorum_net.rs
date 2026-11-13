//! Leaderless quorums between processes (D1097): the same placement, versions and resolution as
//! [`crate::quorum::Quorum`], with each node a separate process that keeps its replica in a
//! [`ReplicaStore`] and talks to the others over TCP in the frames of [`crate::quorum_wire`].
//! Any node coordinates a client's [`Request::Put`] or [`Request::Get`]: it sends the version to
//! the key's home replicas at once, counts the acknowledgements against `w` (or the answers
//! against `r`), leaves a hint with the next node for a home it cannot reach, and repairs the
//! homes a read found behind. [`QuorumNode::maintain`] hands hints off when their owner answers
//! again and runs anti-entropy, comparing Merkle trees with each peer over the keys both are
//! homes of.

use crate::NodeId;
use crate::merkle::MerkleTree;
use crate::quorum::{
    MERKLE_DEPTH, QuorumError, ReadResult, Replica, Resolution, Stamp, Version, VersionVector,
    reconcile, rendezvous, versions_digest,
};
use crate::quorum_wire::{Request, Response, read_frame, write_frame};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HAND_OFF_EVERY: Duration = Duration::from_secs(1);
const ANTI_ENTROPY_EVERY: Duration = Duration::from_secs(30);
const LISTEN_POLL: Duration = Duration::from_millis(100);

/// A connection to a node or from one: plain TCP, or TLS over it.
pub trait Stream: Read + Write + Send {}

impl<T: Read + Write + Send> Stream for T {}

/// Opens a connection to a node: its id, its `host:port`, and how long to wait.
pub type Dialer = dyn Fn(NodeId, &str, Duration) -> std::io::Result<Box<dyn Stream>> + Send + Sync;

/// Turns an accepted TCP connection into the stream requests are read from, such as a TLS server
/// handshake.
pub type Acceptor = dyn Fn(TcpStream) -> std::io::Result<Box<dyn Stream>> + Sync;

/// Opens a plain TCP connection to `address` within `timeout`, with reads and writes held to it.
pub fn dial_tcp(address: &str, timeout: Duration) -> std::io::Result<TcpStream> {
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, format!("{address} has no address"));
    for resolved in address.to_socket_addrs()? {
        match TcpStream::connect_timeout(&resolved, timeout) {
            Ok(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))?;
                stream.set_nodelay(true)?;
                return Ok(stream);
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}

fn plain_dialer(_: NodeId, address: &str, timeout: Duration) -> std::io::Result<Box<dyn Stream>> {
    Ok(Box::new(dial_tcp(address, timeout)?))
}

/// Accepts a connection as it is, with no TLS.
pub fn plain_acceptor(stream: TcpStream) -> std::io::Result<Box<dyn Stream>> {
    Ok(Box::new(stream))
}

fn same_bytes(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0u8, |differ, (a, b)| differ | (a ^ b)) == 0
}

/// Where a node keeps its replica and the hints it holds for others.
pub trait ReplicaStore: Send + Sync {
    /// The versions kept for `key`.
    fn versions(&self, key: &[u8]) -> Result<Vec<Version>, String>;
    /// Takes `version` into `key`'s versions by `resolution`, atomically; returns whether they
    /// changed.
    fn apply(&self, key: &[u8], version: &Version, resolution: Resolution) -> Result<bool, String>;
    /// Every key kept.
    fn keys(&self) -> Result<Vec<Vec<u8>>, String>;
    /// Keeps `version` of `key` for `owner` until it can be handed off.
    fn add_hint(&self, owner: NodeId, key: &[u8], version: &Version) -> Result<(), String>;
    /// The hints kept: an identity, the owner, the key and the version.
    fn hints(&self) -> Result<Vec<(u64, NodeId, Vec<u8>, Version)>, String>;
    /// Forgets the hint with this identity.
    fn drop_hint(&self, id: u64) -> Result<(), String>;
}

/// A replica in memory, for tests and for nodes that keep nothing across restarts.
#[derive(Debug, Default)]
pub struct MemoryReplicaStore {
    state: Mutex<(Replica, BTreeMap<u64, (NodeId, Vec<u8>, Version)>, u64)>,
}

impl ReplicaStore for MemoryReplicaStore {
    fn versions(&self, key: &[u8]) -> Result<Vec<Version>, String> {
        let state = self.state.lock().map_err(|_| "the replica's lock is poisoned".to_owned())?;
        Ok(state.0.versions(key).to_vec())
    }

    fn apply(&self, key: &[u8], version: &Version, resolution: Resolution) -> Result<bool, String> {
        let mut state = self.state.lock().map_err(|_| "the replica's lock is poisoned".to_owned())?;
        Ok(state.0.apply(key, version, resolution))
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
        let state = self.state.lock().map_err(|_| "the replica's lock is poisoned".to_owned())?;
        Ok(state.0.keys().cloned().collect())
    }

    fn add_hint(&self, owner: NodeId, key: &[u8], version: &Version) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|_| "the replica's lock is poisoned".to_owned())?;
        state.2 += 1;
        let id = state.2;
        state.1.insert(id, (owner, key.to_vec(), version.clone()));
        Ok(())
    }

    fn hints(&self) -> Result<Vec<(u64, NodeId, Vec<u8>, Version)>, String> {
        let state = self.state.lock().map_err(|_| "the replica's lock is poisoned".to_owned())?;
        Ok(state
            .1
            .iter()
            .map(|(id, (owner, key, version))| (*id, *owner, key.clone(), version.clone()))
            .collect())
    }

    fn drop_hint(&self, id: u64) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|_| "the replica's lock is poisoned".to_owned())?;
        state.1.remove(&id);
        Ok(())
    }
}

/// A node's place in a leaderless cluster.
#[derive(Debug, Clone)]
pub struct QuorumConfig {
    /// This node.
    pub node: NodeId,
    /// Every other node and where it listens, as `host:port`.
    pub peers: BTreeMap<NodeId, String>,
    /// How many home replicas each key has.
    pub n: usize,
    /// How many answers a read needs.
    pub r: usize,
    /// How many acknowledgements a write needs.
    pub w: usize,
    /// How concurrent versions are reconciled.
    pub resolution: Resolution,
    /// How long a call to another node may take.
    pub timeout: Duration,
}

impl QuorumConfig {
    fn members(&self) -> Vec<NodeId> {
        let mut members: Vec<NodeId> = self.peers.keys().copied().collect();
        members.push(self.node);
        members.sort_unstable();
        members.dedup();
        members
    }

    /// Checks `n`, `r` and `w` against the members.
    pub fn validate(&self) -> Result<(), QuorumError> {
        let members = self.members().len();
        if self.n == 0 || self.r == 0 || self.w == 0 || self.n > members || self.r > self.n || self.w > self.n {
            return Err(QuorumError::BadConfiguration);
        }
        Ok(())
    }
}

/// One node of a leaderless cluster over the network.
pub struct QuorumNode<S: ReplicaStore> {
    config: QuorumConfig,
    store: S,
    clock: Mutex<Stamp>,
    dialer: Box<Dialer>,
    password: Option<Vec<u8>>,
}

impl<S: ReplicaStore> std::fmt::Debug for QuorumNode<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuorumNode").field("config", &self.config).finish_non_exhaustive()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

fn failed(error: impl std::fmt::Display) -> Response {
    Response::Failed(error.to_string())
}

impl<S: ReplicaStore> QuorumNode<S> {
    /// A node with `config`, keeping its replica in `store`.
    pub fn new(config: QuorumConfig, store: S) -> Result<Self, QuorumError> {
        config.validate()?;
        Ok(Self {
            config,
            store,
            clock: Mutex::new(Stamp::default()),
            dialer: Box::new(plain_dialer),
            password: None,
        })
    }

    /// Opens connections to other nodes with `dialer`, such as TLS over TCP, instead of plain TCP.
    pub fn with_dialer(mut self, dialer: Box<Dialer>) -> Self {
        self.dialer = dialer;
        self
    }

    /// Asks every connection, from a client or a node, for `password` in its first frame, and
    /// gives it on every connection this node opens.
    pub fn with_password(mut self, password: Vec<u8>) -> Self {
        self.password = Some(password);
        self
    }

    /// The node's replica store.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Every member in `key`'s order: its first `n` are the key's home replicas.
    pub fn order(&self, key: &[u8]) -> Vec<NodeId> {
        let mut nodes = self.config.members();
        nodes.sort_by_key(|node| std::cmp::Reverse(rendezvous(key, *node)));
        nodes
    }

    fn homes(&self, key: &[u8]) -> Vec<NodeId> {
        self.order(key).into_iter().take(self.config.n).collect()
    }

    fn tick(&self) -> Stamp {
        let mut clock = self.clock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = now_ms();
        *clock = if now > clock.millis {
            Stamp { millis: now, counter: 0, node: self.config.node }
        } else {
            Stamp { millis: clock.millis, counter: clock.counter.saturating_add(1), node: self.config.node }
        };
        *clock
    }

    fn observe(&self, stamp: Stamp) {
        let mut clock = self.clock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if (stamp.millis, stamp.counter) > (clock.millis, clock.counter) {
            *clock = Stamp { node: self.config.node, ..stamp };
        }
    }

    fn call(&self, node: NodeId, request: &Request) -> Result<Response, String> {
        if node == self.config.node {
            return Ok(self.handle(request.clone()));
        }
        let address = self
            .config
            .peers
            .get(&node)
            .ok_or_else(|| format!("node {node} is not a member"))?;
        let mut stream = (self.dialer)(node, address, self.config.timeout).map_err(|error| error.to_string())?;
        if let Some(password) = &self.password {
            exchange(stream.as_mut(), &Request::Hello { password: password.clone() })?;
        }
        exchange(stream.as_mut(), request)
    }

    fn call_all(&self, nodes: &[NodeId], request: &Request) -> Vec<(NodeId, Result<Response, String>)> {
        std::thread::scope(|scope| {
            let calls: Vec<_> = nodes
                .iter()
                .map(|node| (*node, scope.spawn(move || self.call(*node, request))))
                .collect();
            calls
                .into_iter()
                .map(|(node, call)| (node, call.join().unwrap_or_else(|_| Err("the call stopped".to_owned()))))
                .collect()
        })
    }

    /// Writes `value` (`None` deletes) to `key` as coordinator, superseding what `context` read;
    /// succeeds once `w` home replicas, or nodes holding hints for them, have it.
    pub fn put(&self, key: &[u8], value: Option<Vec<u8>>, context: &VersionVector) -> Result<Version, QuorumError> {
        let stamp = self.tick();
        let mut vector = context.clone();
        vector.bump(self.config.node);
        let version = Version { vector, stamp, value };
        let order = self.order(key);
        let homes: Vec<NodeId> = order.iter().take(self.config.n).copied().collect();
        let store = Request::Store { key: key.to_vec(), version: version.clone() };
        let answers = self.call_all(&homes, &store);
        let mut stored = answers.iter().filter(|(_, answer)| answer.is_ok()).count();
        let mut spares = order.iter().skip(self.config.n).copied();
        for (home, answer) in &answers {
            if answer.is_ok() {
                continue;
            }
            let hint = Request::Hint { owner: *home, key: key.to_vec(), version: version.clone() };
            for spare in spares.by_ref() {
                if self.call(spare, &hint).is_ok() {
                    stored += 1;
                    break;
                }
            }
        }
        if stored < self.config.w {
            return Err(QuorumError::NotEnoughReplicas { answered: stored, needed: self.config.w });
        }
        Ok(version)
    }

    /// Reads `key` as coordinator from its home replicas until `r` answer, reconciles their
    /// versions, and repairs the ones that were behind.
    pub fn get(&self, key: &[u8]) -> Result<ReadResult, QuorumError> {
        let homes = self.homes(key);
        let fetch = Request::Fetch { key: key.to_vec() };
        let answers: Vec<(NodeId, Vec<Version>)> = self
            .call_all(&homes, &fetch)
            .into_iter()
            .filter_map(|(node, answer)| match answer {
                Ok(Response::Versions(versions)) => Some((node, versions)),
                _ => None,
            })
            .collect();
        if answers.len() < self.config.r {
            return Err(QuorumError::NotEnoughReplicas { answered: answers.len(), needed: self.config.r });
        }
        let mut reconciled: Vec<Version> = Vec::new();
        for (_, versions) in &answers {
            for version in versions {
                reconcile(&mut reconciled, version, self.config.resolution);
            }
        }
        let behind: Vec<NodeId> = answers
            .iter()
            .filter(|(_, versions)| *versions != reconciled)
            .map(|(node, _)| *node)
            .collect();
        for version in &reconciled {
            self.observe(version.stamp);
            let repair = Request::Store { key: key.to_vec(), version: version.clone() };
            let _ = self.call_all(&behind, &repair);
        }
        let mut context = VersionVector::default();
        for version in &reconciled {
            context.merge(&version.vector);
        }
        Ok(ReadResult { values: reconciled.into_iter().map(|version| version.value).collect(), context })
    }

    fn shared_keys(&self, peer: NodeId) -> Result<Vec<Vec<u8>>, String> {
        Ok(self
            .store
            .keys()?
            .into_iter()
            .filter(|key| {
                let homes = self.homes(key);
                homes.contains(&self.config.node) && homes.contains(&peer)
            })
            .collect())
    }

    fn tree_with(&self, peer: NodeId) -> Result<(Vec<Vec<u8>>, MerkleTree), String> {
        let keys = self.shared_keys(peer)?;
        let mut digests = Vec::with_capacity(keys.len());
        for key in &keys {
            digests.push(versions_digest(&self.store.versions(key)?));
        }
        let tree = MerkleTree::build(
            MERKLE_DEPTH,
            keys.iter().map(Vec::as_slice).zip(digests.iter().copied()),
        );
        Ok((keys, tree))
    }

    /// Answers one request from a client or another node.
    pub fn handle(&self, request: Request) -> Response {
        match request {
            Request::Hello { .. } => Response::Ack,
            Request::Put { key, value, context } => match self.put(&key, value, &context) {
                Ok(version) => Response::Written(version),
                Err(error) => failed(format!("{error:?}")),
            },
            Request::Get { key } => match self.get(&key) {
                Ok(read) => Response::Values { values: read.values, context: read.context },
                Err(error) => failed(format!("{error:?}")),
            },
            Request::Store { key, version } => {
                self.observe(version.stamp);
                match self.store.apply(&key, &version, self.config.resolution) {
                    Ok(_) => Response::Ack,
                    Err(error) => failed(error),
                }
            }
            Request::Fetch { key } => match self.store.versions(&key) {
                Ok(versions) => Response::Versions(versions),
                Err(error) => failed(error),
            },
            Request::Hint { owner, key, version } => match self.store.add_hint(owner, &key, &version) {
                Ok(()) => Response::Ack,
                Err(error) => failed(error),
            },
            Request::Tree { peer } => match self.tree_with(peer) {
                Ok((_, tree)) => Response::Leaves(tree.leaves().to_vec()),
                Err(error) => failed(error),
            },
            Request::KeysAt { peer, leaves } => match self.shared_keys(peer) {
                Ok(keys) => {
                    let wanted: BTreeSet<usize> = leaves.iter().map(|leaf| *leaf as usize).collect();
                    Response::Keys(
                        keys.into_iter()
                            .filter(|key| wanted.contains(&MerkleTree::leaf_of(key, MERKLE_DEPTH)))
                            .collect(),
                    )
                }
                Err(error) => failed(error),
            },
        }
    }

    /// Hands each hint to its owner when the owner answers; returns how many were handed off.
    pub fn hand_off(&self) -> usize {
        let Ok(hints) = self.store.hints() else {
            return 0;
        };
        let mut handed = 0;
        for (id, owner, key, version) in hints {
            if self.call(owner, &Request::Store { key, version }).is_ok() && self.store.drop_hint(id).is_ok() {
                handed += 1;
            }
        }
        handed
    }

    /// Brings this node and each peer to the same versions of the keys both are homes of,
    /// reconciling only the keys under Merkle leaves that differ; returns how many keys were
    /// reconciled.
    pub fn anti_entropy(&self) -> usize {
        let mut reconciled = 0;
        for peer in self.config.peers.keys().copied() {
            let Ok((keys, mine)) = self.tree_with(peer) else {
                continue;
            };
            let Ok(Response::Leaves(theirs)) = self.call(peer, &Request::Tree { peer: self.config.node }) else {
                continue;
            };
            let differing: Vec<u32> = mine
                .leaves()
                .iter()
                .zip(theirs.iter())
                .enumerate()
                .filter(|(_, (mine, theirs))| mine != theirs)
                .filter_map(|(leaf, _)| u32::try_from(leaf).ok())
                .collect();
            if differing.is_empty() && mine.leaves().len() == theirs.len() {
                continue;
            }
            let wanted: BTreeSet<usize> = differing.iter().map(|leaf| *leaf as usize).collect();
            let mut suspects: BTreeSet<Vec<u8>> = keys
                .into_iter()
                .filter(|key| wanted.contains(&MerkleTree::leaf_of(key, MERKLE_DEPTH)))
                .collect();
            if let Ok(Response::Keys(remote)) = self.call(peer, &Request::KeysAt { peer: self.config.node, leaves: differing }) {
                suspects.extend(remote);
            }
            for key in suspects {
                if let Ok(Response::Versions(remote)) = self.call(peer, &Request::Fetch { key: key.clone() }) {
                    for version in &remote {
                        let _ = self.store.apply(&key, version, self.config.resolution);
                    }
                }
                if let Ok(local) = self.store.versions(&key) {
                    for version in local {
                        let _ = self.call(peer, &Request::Store { key: key.clone(), version });
                    }
                }
                reconciled += 1;
            }
        }
        reconciled
    }

    /// Accepts connections on `listener` until `stop` says so, passing each through `accept`
    /// ([`plain_acceptor`], or a TLS handshake) and answering its requests in turn on a thread of
    /// its own.
    pub fn serve(&self, listener: &TcpListener, accept: &Acceptor, stop: &(dyn Fn() -> bool + Sync)) -> std::io::Result<()> {
        listener.set_nonblocking(true)?;
        std::thread::scope(|scope| {
            while !stop() {
                match listener.accept() {
                    Ok((stream, _)) => {
                        scope.spawn(move || {
                            if stream.set_nonblocking(false).is_err()
                                || stream.set_read_timeout(Some(self.config.timeout.max(Duration::from_secs(60)))).is_err()
                            {
                                return;
                            }
                            let _ = stream.set_nodelay(true);
                            if let Ok(mut opened) = accept(stream) {
                                self.answer(opened.as_mut(), stop);
                            }
                        });
                    }
                    Err(_) => std::thread::sleep(LISTEN_POLL),
                }
            }
        });
        Ok(())
    }

    fn admitted(&self, stream: &mut dyn Stream) -> bool {
        let Some(password) = &self.password else {
            return true;
        };
        let given = match read_frame(stream).ok().flatten().and_then(|body| Request::decode(&body)) {
            Some(Request::Hello { password }) => password,
            _ => Vec::new(),
        };
        let admitted = same_bytes(&given, password);
        let answer = if admitted { Response::Ack } else { Response::Failed("the password is wrong".to_owned()) };
        write_frame(stream, &answer.encode()).is_ok() && admitted
    }

    fn answer(&self, stream: &mut dyn Stream, stop: &(dyn Fn() -> bool + Sync)) {
        if !self.admitted(stream) {
            return;
        }
        while !stop() {
            let body = match read_frame(stream) {
                Ok(Some(body)) => body,
                _ => return,
            };
            let response = match Request::decode(&body) {
                Some(Request::Hello { .. }) => Response::Ack,
                Some(request) => self.handle(request),
                None => Response::Failed("the request is damaged".to_owned()),
            };
            if write_frame(stream, &response.encode()).is_err() {
                return;
            }
        }
    }

    /// Runs hand-off every second and anti-entropy every thirty seconds until `stop` says so.
    pub fn maintain(&self, stop: &(dyn Fn() -> bool + Sync)) {
        let mut last_entropy = Instant::now();
        while !stop() {
            std::thread::sleep(HAND_OFF_EVERY);
            self.hand_off();
            if last_entropy.elapsed() >= ANTI_ENTROPY_EVERY {
                self.anti_entropy();
                last_entropy = Instant::now();
            }
        }
    }
}

fn exchange(stream: &mut dyn Stream, request: &Request) -> Result<Response, String> {
    write_frame(stream, &request.encode()).map_err(|error| error.to_string())?;
    let body = read_frame(stream)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the node closed the connection".to_owned())?;
    match Response::decode(&body) {
        Some(Response::Failed(message)) => Err(message),
        Some(response) => Ok(response),
        None => Err("the node sent a damaged answer".to_owned()),
    }
}

/// A client of a leaderless cluster: any node coordinates its requests.
pub struct QuorumClient {
    stream: Box<dyn Stream>,
}

impl std::fmt::Debug for QuorumClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuorumClient").finish_non_exhaustive()
    }
}

impl QuorumClient {
    /// Connects to the node at `address` (`host:port`) over plain TCP.
    pub fn connect(address: &str, timeout: Duration) -> std::io::Result<Self> {
        Ok(Self::over(Box::new(dial_tcp(address, timeout)?)))
    }

    /// A client over a connection already open, such as TLS.
    pub fn over(stream: Box<dyn Stream>) -> Self {
        Self { stream }
    }

    /// Gives the node's password, the first thing on a connection to a node that has one.
    pub fn login(&mut self, password: &[u8]) -> Result<(), String> {
        self.ask(&Request::Hello { password: password.to_vec() }).map(|_| ())
    }

    fn ask(&mut self, request: &Request) -> Result<Response, String> {
        exchange(self.stream.as_mut(), request)
    }

    /// Reads `key`: its values (one, or siblings) and the context to write back with.
    pub fn get(&mut self, key: &[u8]) -> Result<ReadResult, String> {
        match self.ask(&Request::Get { key: key.to_vec() })? {
            Response::Values { values, context } => Ok(ReadResult { values, context }),
            other => Err(format!("unexpected answer {other:?}")),
        }
    }

    /// Writes `value` to `key`, superseding what `context` read.
    pub fn put(&mut self, key: &[u8], value: &[u8], context: &VersionVector) -> Result<Version, String> {
        self.write(key, Some(value.to_vec()), context)
    }

    /// Deletes `key`, superseding what `context` read.
    pub fn delete(&mut self, key: &[u8], context: &VersionVector) -> Result<Version, String> {
        self.write(key, None, context)
    }

    fn write(&mut self, key: &[u8], value: Option<Vec<u8>>, context: &VersionVector) -> Result<Version, String> {
        match self.ask(&Request::Put { key: key.to_vec(), value, context: context.clone() })? {
            Response::Written(version) => Ok(version),
            other => Err(format!("unexpected answer {other:?}")),
        }
    }
}
