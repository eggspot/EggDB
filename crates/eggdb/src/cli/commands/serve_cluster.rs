use super::serve_replication::{
    PrimaryAddress, ReplicationCredentials, Stream, checked_greeting, connect, read_frame,
    receive_snapshot, signed_greeting, stream_file, write_frame,
};
use eggdb::consensus::wire::{decode_message, encode_message};
use eggdb::consensus::{EntryKind, Membership, Message, Node, NodeId, Role, Snapshot, Timing};
use eggdb::{Database, ReplicaId, ReplicationMode, ReplicationPrimary};
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CHALLENGE: u8 = 1;
const REFUSED: u8 = 6;
const JOIN: u8 = 20;
const RAFT: u8 = 21;
const SEED: u8 = 22;
const NOT_LEADER: u8 = 23;
const SEED_POSITION: u8 = 24;
const TICK: Duration = Duration::from_millis(100);
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_WAIT: Duration = Duration::from_secs(1);
const FAILED_LOGIN_PAUSE: Duration = Duration::from_millis(250);
const SEED_ATTEMPTS: u32 = 20;
const SEED_WAIT: Duration = Duration::from_millis(200);
const BATCHES_PER_ROUND: usize = 64;
const MESSAGES_PER_ROUND: usize = 1024;
const COMPACT_ABOVE_ENTRIES: usize = 20_000;
const ENTRIES_KEPT_AFTER_COMPACTION: u64 = 10_000;
const SEQUENCES_REMEMBERED: usize = 100_000;
const NO_LEADER: u64 = u64::MAX;
const CLUSTER_REPLICA: ReplicaId = u32::MAX;

/// A server's place in a cluster (D1038): its id and the address every voter listens on for
/// the cluster's own traffic, this server's included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClusterSettings {
    pub(super) node: NodeId,
    pub(super) members: BTreeMap<NodeId, String>,
    pub(super) ca_file: Option<std::path::PathBuf>,
}

impl ClusterSettings {
    /// The address this server listens on.
    pub(super) fn own_address(&self) -> Option<&str> {
        self.members.get(&self.node).map(String::as_str)
    }

    fn peer(&self, id: NodeId) -> Result<PrimaryAddress, String> {
        let address = self
            .members
            .get(&id)
            .ok_or_else(|| format!("node {id} is not in --cluster"))?;
        Ok(PrimaryAddress {
            address: address.clone(),
            tls: super::serve_replication::client_tls(address, self.ca_file.as_deref())?,
        })
    }
}

/// Whether `address` (`host:port`) is on this machine, where peers may talk without TLS.
pub(super) fn is_loopback(address: &str) -> bool {
    let host = address
        .rsplit_once(':')
        .map_or(address, |(host, _)| host)
        .trim_start_matches('[')
        .trim_end_matches(']');
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Reads `--cluster 1=host:port,2=host:port,3=host:port`: every voter's id and address.
pub(super) fn parse_members(text: &str) -> Result<BTreeMap<NodeId, String>, String> {
    let mut members = BTreeMap::new();
    for part in text.split(',').map(str::trim).filter(|part| !part.is_empty()) {
        let (id, address) = part
            .split_once('=')
            .ok_or_else(|| format!("--cluster: '{part}' is not id=host:port"))?;
        let id: NodeId = id
            .trim()
            .parse()
            .ok()
            .filter(|id| *id != NO_LEADER)
            .ok_or_else(|| format!("--cluster: '{id}' is not a node id"))?;
        let address = address.trim();
        if !address.contains(':') {
            return Err(format!("--cluster: '{address}' is not host:port"));
        }
        if members.insert(id, address.to_owned()).is_some() {
            return Err(format!("--cluster names node {id} twice"));
        }
    }
    if members.is_empty() {
        return Err("--cluster takes id=host:port for every voter".to_owned());
    }
    Ok(members)
}

#[derive(Debug, Clone, Default)]
struct SeedPoint {
    batches: BTreeMap<u64, (u64, u64)>,
    membership: Membership,
}

/// What the cluster's threads share with the driver.
struct Shared {
    node: NodeId,
    leader: AtomicU64,
    finished: AtomicBool,
    seed_point: Mutex<SeedPoint>,
}

impl Shared {
    fn leader(&self) -> Option<NodeId> {
        Some(self.leader.load(Ordering::Acquire)).filter(|id| *id != NO_LEADER)
    }

    fn done(&self, stop: &(dyn Fn() -> bool + Sync)) -> bool {
        stop() || self.finished.load(Ordering::Acquire)
    }
}

fn seed_of(path: &std::path::Path, node: NodeId) -> u64 {
    let digest = eggdb_crypto::sha256(format!("{}:{node}", path.display()).as_bytes());
    u64::from_le_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
    ])
}

fn failed(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// Copies the current leader's database to the new file `path` before this server first opens
/// it (`--cluster-seed`), and resets the copy's Raft state to a snapshot at the last batch the
/// copy holds, so the node joins from there.
pub(super) fn seed_from_cluster(
    settings: &ClusterSettings,
    path: &std::path::Path,
    credentials: &ReplicationCredentials,
) -> Result<(), String> {
    let mut asked: Vec<NodeId> = settings
        .members
        .keys()
        .copied()
        .filter(|id| *id != settings.node)
        .collect();
    let mut last_error = "no other voter is named in --cluster".to_owned();
    for _ in 0..SEED_ATTEMPTS {
        let Some(id) = asked.first().copied() else {
            break;
        };
        match seed_once(settings, id, path, credentials) {
            Ok(Some(snapshot)) => {
                let db = Database::open(path).map_err(failed)?;
                return db.reset_consensus(&snapshot).map_err(failed);
            }
            Ok(None) => {}
            Err(message) => last_error = message,
        }
        asked.rotate_left(1);
        std::thread::sleep(SEED_WAIT);
    }
    Err(format!("cannot copy the cluster's database: {last_error}"))
}

fn seed_once(
    settings: &ClusterSettings,
    id: NodeId,
    path: &std::path::Path,
    credentials: &ReplicationCredentials,
) -> Result<Option<Snapshot>, String> {
    let mut stream = connect(&settings.peer(id)?)?;
    let io = |error: std::io::Error| format!("node {id}: {error}");
    let (kind, nonce) = read_frame(stream.as_mut()).map_err(io)?;
    if kind != CHALLENGE {
        return Err(format!("node {id} did not send its challenge"));
    }
    write_frame(
        stream.as_mut(),
        SEED,
        &signed_greeting(credentials, &nonce, &settings.node.to_le_bytes())?,
    )
    .map_err(io)?;
    let (kind, position) = read_frame(stream.as_mut()).map_err(io)?;
    match kind {
        SEED_POSITION => {}
        NOT_LEADER => return Ok(None),
        REFUSED => {
            return Err(format!(
                "node {id} refused: {}",
                String::from_utf8_lossy(&position)
            ));
        }
        _ => return Err(format!("node {id} sent an unexpected frame")),
    }
    let (Some(index), Some(term), Some(membership)) = (
        position
            .get(..8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_le_bytes),
        position
            .get(8..16)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_le_bytes),
        position.get(16..).and_then(Membership::decode),
    ) else {
        return Err(format!("node {id} sent a malformed seed position"));
    };
    receive_snapshot(stream.as_mut(), path)?;
    Ok(Some(Snapshot {
        index,
        term,
        membership,
        data: Vec::new(),
    }))
}

fn pinned_point(shared: &Shared, sequence: u64) -> Option<(u64, u64, Membership)> {
    for _ in 0..SEED_ATTEMPTS {
        if let Ok(point) = shared.seed_point.lock() {
            if sequence == 0 {
                return Some((0, 0, point.membership.clone()));
            }
            if let Some((index, term)) = point.batches.get(&sequence) {
                return Some((*index, *term, point.membership.clone()));
            }
        }
        std::thread::sleep(SEED_WAIT);
    }
    None
}

fn serve_seed(stream: &mut dyn Stream, db: &Database, shared: &Shared) -> std::io::Result<()> {
    let mut nonce = [0u8; 8];
    eggdb_platform::secure_random::fill(&mut nonce)?;
    let path = std::env::temp_dir().join(format!(
        "eggdb-cluster-seed-{:016x}.eggdb",
        u64::from_le_bytes(nonce)
    ));
    let outcome = (|| -> std::io::Result<()> {
        db.backup_to(&path).map_err(std::io::Error::other)?;
        let sequence = Database::open(&path)
            .and_then(|copy| copy.replication_position())
            .map_err(std::io::Error::other)?
            .map_or(0, |(_, sequence)| sequence);
        let Some((index, term, membership)) = pinned_point(shared, sequence) else {
            return write_frame(
                stream,
                REFUSED,
                b"the leader's copy holds a batch its cluster has not committed yet; retry",
            );
        };
        let mut position = Vec::with_capacity(32);
        position.extend_from_slice(&index.to_le_bytes());
        position.extend_from_slice(&term.to_le_bytes());
        position.extend_from_slice(&membership.encode());
        write_frame(stream, SEED_POSITION, &position)?;
        stream_file(stream, &path)
    })();
    let _ = std::fs::remove_file(&path);
    outcome
}

fn serve_peer(
    stream: &mut dyn Stream,
    db: &Database,
    credentials: &ReplicationCredentials,
    shared: &Shared,
    inbox: &Sender<(NodeId, Message)>,
    stop: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<()> {
    let mut nonce = [0u8; 32];
    eggdb_platform::secure_random::fill(&mut nonce)?;
    write_frame(stream, CHALLENGE, &nonce)?;
    let (kind, greeting) = read_frame(stream)?;
    let Some(rest) = checked_greeting(&greeting, credentials, &nonce) else {
        std::thread::sleep(FAILED_LOGIN_PAUSE);
        return write_frame(stream, REFUSED, b"the user or password is not this cluster's");
    };
    let Some(from) = rest
        .get(..8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(NodeId::from_le_bytes)
    else {
        return write_frame(stream, REFUSED, b"the greeting does not name a node");
    };
    match kind {
        SEED if shared.leader() == Some(shared.node) => {
            return serve_seed(stream, db, shared);
        }
        SEED => {
            let leader = shared.leader().unwrap_or(NO_LEADER);
            return write_frame(stream, NOT_LEADER, &leader.to_le_bytes());
        }
        JOIN => {}
        _ => return write_frame(stream, REFUSED, b"an unknown greeting"),
    }
    while !shared.done(stop) {
        let (kind, payload) = match read_frame(stream) {
            Ok(frame) => frame,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if kind != RAFT {
            return Ok(());
        }
        let Ok(message) = decode_message(&payload) else {
            return Ok(());
        };
        if inbox.send((from, message)).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

fn send_to_peer(
    settings: &ClusterSettings,
    peer: NodeId,
    credentials: &ReplicationCredentials,
    outbox: &Receiver<Message>,
    shared: &Shared,
    stop: &(dyn Fn() -> bool + Sync),
) {
    let mut stream: Option<Box<dyn Stream>> = None;
    let mut last_attempt: Option<Instant> = None;
    while !shared.done(stop) {
        let message = match outbox.recv_timeout(TICK) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if stream.is_none() && last_attempt.is_none_or(|at| at.elapsed() >= RECONNECT_WAIT) {
            last_attempt = Some(Instant::now());
            stream = join(settings, peer, credentials).ok();
        }
        let Some(open) = stream.as_mut() else {
            continue;
        };
        if write_frame(open.as_mut(), RAFT, &encode_message(&message)).is_err() {
            stream = None;
        }
    }
}

fn join(
    settings: &ClusterSettings,
    peer: NodeId,
    credentials: &ReplicationCredentials,
) -> Result<Box<dyn Stream>, String> {
    let mut stream = connect(&settings.peer(peer)?)?;
    let (kind, nonce) = read_frame(stream.as_mut()).map_err(failed)?;
    if kind != CHALLENGE {
        return Err(format!("node {peer} did not send its challenge"));
    }
    write_frame(
        stream.as_mut(),
        JOIN,
        &signed_greeting(credentials, &nonce, &settings.node.to_le_bytes())?,
    )
    .map_err(failed)?;
    Ok(stream)
}

fn accept_peers(
    listener: &TcpListener,
    tls: Option<Arc<eggdb_tls::ServerConfig>>,
    db: &Database,
    credentials: &ReplicationCredentials,
    shared: &Shared,
    inbox: &Sender<(NodeId, Message)>,
    stop: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    std::thread::scope(|scope| {
        while !shared.done(stop) {
            let stream = match eggdb_platform::accept_tcp(listener) {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err()
                || stream.set_read_timeout(Some(READ_TIMEOUT)).is_err()
            {
                continue;
            }
            let tls = tls.clone();
            let inbox = inbox.clone();
            let _ = std::thread::Builder::new()
                .name("eggdb-cluster-peer".to_owned())
                .spawn_scoped(scope, move || {
                    let _ = match tls {
                        Some(config) => match eggdb_tls::TlsStream::accept(stream, &config) {
                            Ok(mut secured) => {
                                serve_peer(&mut secured, db, credentials, shared, &inbox, stop)
                            }
                            Err(_) => Ok(()),
                        },
                        None => {
                            let mut plain = stream;
                            serve_peer(&mut plain, db, credentials, shared, &inbox, stop)
                        }
                    };
                });
        }
    });
    Ok(())
}

struct Driver<'d> {
    db: &'d Database,
    node: Node,
    shared: &'d Shared,
    primary: Option<Arc<ReplicationPrimary>>,
    proposed: u64,
    committed_batch: Option<(u64, u64)>,
    leader_hint: String,
}

impl Driver<'_> {
    fn local_position(&self) -> Result<(u64, u64), String> {
        Ok(self.db.replication_position().map_err(failed)?.unwrap_or((0, 0)))
    }

    fn hint(&self) -> String {
        match self.node.leader().filter(|leader| *leader != self.node.id()) {
            Some(leader) => {
                format!("this server is a follower in its cluster; send writes to node {leader}")
            }
            None => "this server's cluster has no leader at the moment; retry the write shortly"
                .to_owned(),
        }
    }

    fn fence(&mut self) {
        self.leader_hint = self.hint();
        self.db.fence(self.leader_hint.clone());
    }

    fn follow_role(&mut self) -> Result<(), String> {
        let leading =
            self.node.committed_in_own_term() && self.node.applied_index() >= self.node.commit_index();
        match (&self.primary, leading) {
            (None, true) => {
                let local = self.local_position()?;
                if let Some((term, sequence)) = self.committed_batch
                    && (local.1 > sequence || (local.1 == sequence && local.0 != term))
                {
                    return Err(diverged(self.db, local.1));
                }
                let term = self.node.term().max(local.0 + 1);
                let primary = self
                    .db
                    .promote(term, ReplicationMode::Synchronous { acknowledgements: 1 })
                    .map_err(failed)?;
                primary
                    .add_replica(CLUSTER_REPLICA, local.1)
                    .map_err(failed)?;
                self.proposed = local.1;
                self.primary = Some(primary);
            }
            (Some(primary), false) if self.node.role() != Role::Leader => {
                primary.retire();
                self.primary = None;
                self.fence();
            }
            (None, false) => {
                if self.hint() != self.leader_hint {
                    self.fence();
                }
            }
            _ => {}
        }
        let leader = self.node.leader().unwrap_or(NO_LEADER);
        self.shared.leader.store(leader, Ordering::Release);
        Ok(())
    }

    fn propose_local_batches(&mut self) -> Result<Vec<eggdb::consensus::Envelope>, String> {
        let Some(primary) = self.primary.clone() else {
            return Ok(Vec::new());
        };
        if self.node.role() != Role::Leader {
            return Ok(Vec::new());
        }
        let batches = match primary.batches_after(self.proposed, BATCHES_PER_ROUND).map_err(failed)? {
            Ok(batches) => batches,
            Err(first_kept) => {
                return Err(format!(
                    "the leader dropped captured batches before {first_kept} before proposing them"
                ));
            }
        };
        let mut outgoing = Vec::new();
        for batch in batches {
            match self.node.propose(eggdb::encode_batch(&batch)) {
                Ok((_, messages)) => {
                    self.proposed = batch.sequence;
                    outgoing.extend(messages);
                }
                Err(_) => break,
            }
        }
        Ok(outgoing)
    }

    fn apply_committed(&mut self) -> Result<(), String> {
        for entry in self.node.take_committed() {
            if entry.kind != EntryKind::Command || entry.command.is_empty() {
                continue;
            }
            let batch = eggdb::decode_batch(&entry.command)
                .map_err(|error| format!("a committed entry at index {} is not a batch: {error}", entry.index))?;
            let local = self.local_position()?;
            if batch.sequence > local.1 {
                if batch.sequence != local.1 + 1 || (local.1 > 0 && batch.previous_term != local.0) {
                    return Err(diverged(self.db, local.1));
                }
                self.db.apply_batch(&batch).map_err(failed)?;
            } else if batch.sequence == local.1 && batch.term != local.0 {
                return Err(diverged(self.db, local.1));
            }
            if let Some(primary) = &self.primary {
                primary
                    .acknowledge(CLUSTER_REPLICA, batch.sequence)
                    .map_err(failed)?;
            }
            self.committed_batch = Some((batch.term, batch.sequence));
            if let Ok(mut point) = self.shared.seed_point.lock() {
                point.batches.insert(batch.sequence, (entry.index, entry.term));
                while point.batches.len() > SEQUENCES_REMEMBERED {
                    point.batches.pop_first();
                }
                point.membership = self.node.membership().clone();
            }
        }
        Ok(())
    }

    fn compact(&mut self) {
        if self.node.log().len() > COMPACT_ABOVE_ENTRIES {
            let through = self
                .node
                .applied_index()
                .saturating_sub(ENTRIES_KEPT_AFTER_COMPACTION);
            self.node.compact(through, Vec::new());
        }
    }
}

fn diverged(db: &Database, sequence: u64) -> String {
    format!(
        "{} and its cluster disagree after batch {sequence} (a leader's last writes were lost before the cluster committed them); start this server again with --cluster-seed to copy the leader's database",
        db.path().display()
    )
}

/// Runs this server's Raft node until `stop` turns true (D1038): it listens for its peers on
/// `listener`, sends to each over its own connection, makes every change to its Raft state
/// durable in the database file before sending, applies committed batches, and while it leads
/// it lets clients write, proposing every commit's batch and acknowledging it to the waiting
/// commit once the cluster commits it. A follower's database is fenced with the leader's id.
pub(super) fn run_cluster(
    db: &Database,
    settings: &ClusterSettings,
    listener: &TcpListener,
    tls: Option<Arc<eggdb_tls::ServerConfig>>,
    credentials: &ReplicationCredentials,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<(), String> {
    let members: Vec<NodeId> = settings.members.keys().copied().collect();
    let recovered = db.consensus_state().map_err(failed)?;
    let node = Node::recover(
        settings.node,
        &members,
        Timing::default(),
        seed_of(db.path(), settings.node),
        recovered,
    )
    .map_err(failed)?;
    let shared = Shared {
        node: settings.node,
        leader: AtomicU64::new(NO_LEADER),
        finished: AtomicBool::new(false),
        seed_point: Mutex::new(SeedPoint::default()),
    };
    let (inbox_sender, inbox) = mpsc::channel::<(NodeId, Message)>();
    let mut outboxes: BTreeMap<NodeId, Sender<Message>> = BTreeMap::new();
    let mut receivers: Vec<(NodeId, Receiver<Message>)> = Vec::new();
    for peer in members.iter().copied().filter(|peer| *peer != settings.node) {
        let (sender, receiver) = mpsc::channel();
        outboxes.insert(peer, sender);
        receivers.push((peer, receiver));
    }
    let mut driver = Driver {
        db,
        node,
        shared: &shared,
        primary: None,
        proposed: 0,
        committed_batch: None,
        leader_hint: String::new(),
    };
    driver.fence();
    std::thread::scope(|scope| {
        let shared = &shared;
        scope.spawn(|| {
            let _ = accept_peers(listener, tls, db, credentials, shared, &inbox_sender, stop);
        });
        for (peer, receiver) in receivers {
            scope.spawn(move || send_to_peer(settings, peer, credentials, &receiver, shared, stop));
        }
        let outcome = drive(&mut driver, &inbox, &outboxes, stop);
        shared.finished.store(true, Ordering::Release);
        if let Some(primary) = &driver.primary {
            primary.retire();
        }
        outcome
    })
}

fn drive(
    driver: &mut Driver<'_>,
    inbox: &Receiver<(NodeId, Message)>,
    outboxes: &BTreeMap<NodeId, Sender<Message>>,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<(), String> {
    let mut last_tick = Instant::now();
    while !stop() {
        let mut outgoing = Vec::new();
        match inbox.recv_timeout(TICK) {
            Ok((from, message)) => outgoing.extend(driver.node.step(from, message)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        for (from, message) in inbox.try_iter().take(MESSAGES_PER_ROUND) {
            outgoing.extend(driver.node.step(from, message));
        }
        if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();
            outgoing.extend(driver.node.tick());
        }
        outgoing.extend(driver.propose_local_batches()?);
        let changes = driver.node.take_changes();
        if changes.installed.is_some() {
            return Err(format!(
                "{} fell behind the log its cluster keeps; start it again with --cluster-seed to copy the leader's database",
                driver.db.path().display()
            ));
        }
        driver.db.save_consensus(&changes).map_err(failed)?;
        for envelope in outgoing {
            if let Some(outbox) = outboxes.get(&envelope.to) {
                let _ = outbox.send(envelope.message);
            }
        }
        driver.apply_committed()?;
        driver.follow_role()?;
        driver.compact();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_members_read_as_ids_and_addresses() {
        let members = parse_members("1=10.0.0.1:7400, 2=10.0.0.2:7400,3=[::1]:7400").unwrap();
        assert_eq!(members.len(), 3);
        assert_eq!(members[&3], "[::1]:7400");
        assert!(parse_members("1=a:1,1=b:2").unwrap_err().contains("twice"));
        assert!(parse_members("x=a:1").is_err());
        assert!(parse_members("1=nohost").is_err());
        assert!(parse_members("").is_err());
        assert!(parse_members(&format!("{}=a:1", u64::MAX)).is_err());
        let settings = ClusterSettings {
            node: 2,
            members,
            ca_file: None,
        };
        assert_eq!(settings.own_address(), Some("10.0.0.2:7400"));
        assert!(settings.peer(9).is_err());
    }

    #[test]
    fn peers_on_this_machine_are_loopback() {
        assert!(is_loopback("127.0.0.1:7400"));
        assert!(is_loopback("[::1]:7400"));
        assert!(is_loopback("localhost:7400"));
        assert!(!is_loopback("10.0.0.2:7400"));
        assert!(!is_loopback("db1.example.com:7400"));
    }

    #[test]
    fn a_node_s_random_seed_is_the_same_at_every_start() {
        let path = std::path::Path::new("/data/app.eggdb");
        assert_eq!(seed_of(path, 1), seed_of(path, 1));
        assert_ne!(seed_of(path, 1), seed_of(path, 2));
    }
}
