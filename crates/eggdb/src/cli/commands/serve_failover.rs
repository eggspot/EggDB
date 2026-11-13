use super::serve_replication::{PrimaryAddress, ReplicationCredentials, query_status, send_follow, send_promote};
use eggdb::{Database, ReplicaId, ReplicationMode, ReplicationPrimary};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TICK: Duration = Duration::from_secs(1);
const SWITCHOVER_CATCH_UP: Duration = Duration::from_secs(30);
const SWITCHOVER_REPLY: Duration = Duration::from_secs(60);

type SwitchoverRequest = (String, Sender<Result<String, String>>);

static SWITCHOVER_REQUESTS: Mutex<Option<Sender<SwitchoverRequest>>> = Mutex::new(None);

/// The hook `SWITCHOVER TO` calls in this process: it hands the request to the server's
/// replication monitor and waits for its answer.
pub(super) fn request_switchover(target: &str) -> Result<String, String> {
    let sender = SWITCHOVER_REQUESTS
        .lock()
        .map_err(|_| "the replication monitor failed".to_owned())?
        .clone()
        .ok_or("this server does not replicate")?;
    let (reply, answer) = channel();
    sender.send((target.to_owned(), reply)).map_err(|_| "the replication monitor stopped".to_owned())?;
    answer.recv_timeout(SWITCHOVER_REPLY).map_err(|_| "the switchover did not finish in time".to_owned())?
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// What one server of a replication group reports about itself (D1026).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NodeStatus {
    pub(super) id: ReplicaId,
    pub(super) term: u64,
    pub(super) applied: u64,
    pub(super) is_primary: bool,
    pub(super) contact_age_ms: u64,
}

/// One server's place in its replication group: the primary it is, or the primary it follows,
/// the peers it can ask, and what it saw of them last.
pub(super) struct ReplicationNode<'d> {
    pub(super) db: &'d Database,
    pub(super) credentials: ReplicationCredentials,
    pub(super) mode: ReplicationMode,
    pub(super) primary: Mutex<Option<Arc<ReplicationPrimary>>>,
    pub(super) following: Mutex<Option<PrimaryAddress>>,
    pub(super) peers: Vec<PrimaryAddress>,
    pub(super) advertise: Option<String>,
    pub(super) failover_after: Option<Duration>,
    pub(super) last_contact_ms: AtomicU64,
    pub(super) connected_replicas: AtomicUsize,
    pub(super) replica_id: ReplicaId,
    pub(super) replica_ca: Option<std::path::PathBuf>,
    lease_fenced: std::sync::atomic::AtomicBool,
}

impl<'d> ReplicationNode<'d> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        db: &'d Database,
        credentials: ReplicationCredentials,
        mode: ReplicationMode,
        primary: Option<Arc<ReplicationPrimary>>,
        following: Option<PrimaryAddress>,
        peers: Vec<PrimaryAddress>,
        advertise: Option<String>,
        failover_after: Option<Duration>,
        replica_id: ReplicaId,
        replica_ca: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            db,
            credentials,
            mode,
            primary: Mutex::new(primary),
            following: Mutex::new(following),
            peers,
            advertise,
            failover_after,
            last_contact_ms: AtomicU64::new(now_ms()),
            connected_replicas: AtomicUsize::new(0),
            replica_id,
            replica_ca,
            lease_fenced: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(super) fn current_primary(&self) -> Option<Arc<ReplicationPrimary>> {
        self.primary.lock().ok().and_then(|primary| primary.clone())
    }

    pub(super) fn following(&self) -> Option<PrimaryAddress> {
        self.following.lock().ok().and_then(|following| following.clone())
    }

    pub(super) fn touch(&self) {
        self.last_contact_ms.store(now_ms(), Ordering::Release);
    }

    pub(super) fn status(&self) -> NodeStatus {
        let (term, applied) = self.db.replication_position().ok().flatten().unwrap_or((0, 0));
        NodeStatus {
            id: self.replica_id,
            term,
            applied,
            is_primary: self.current_primary().is_some(),
            contact_age_ms: now_ms().saturating_sub(self.last_contact_ms.load(Ordering::Acquire)),
        }
    }

    /// Becomes the primary of `term`: stops following, starts capturing commits and takes the
    /// fence away.
    pub(super) fn promote(&self, term: u64) -> Result<(), String> {
        let primary = self.db.promote(term, self.mode).map_err(|error| error.to_string())?;
        if let Ok(mut following) = self.following.lock() {
            *following = None;
        }
        if let Ok(mut current) = self.primary.lock() {
            *current = Some(primary);
        }
        self.touch();
        Ok(())
    }

    /// Follows the primary at `address` from now on, as told by a promoted peer.
    pub(super) fn follow(&self, address: &str) {
        let tls = super::serve_replication::client_tls(address, self.replica_ca.as_deref()).ok().flatten();
        if let Ok(mut following) = self.following.lock() {
            *following = Some(PrimaryAddress { address: address.to_owned(), tls });
        }
        self.touch();
    }

    fn peer_statuses(&self) -> Vec<(usize, NodeStatus)> {
        self.peers
            .iter()
            .enumerate()
            .filter_map(|(index, peer)| query_status(peer, &self.credentials).ok().map(|status| (index, status)))
            .collect()
    }

    /// A replica that heard nothing from its primary for longer than the failover delay asks its
    /// peers; when a majority of the group answers, none of them still reaches the primary, and
    /// the election picks this replica, it starts a new term and tells the peers to follow it.
    fn try_failover(&self, failover_after: Duration) {
        let statuses = self.peer_statuses();
        let limit = u64::try_from(failover_after.as_millis()).unwrap_or(u64::MAX);
        if statuses.iter().any(|(_, status)| status.is_primary || status.contact_age_ms < limit) {
            return;
        }
        let group = self.peers.len() + 2;
        let reachable = statuses.len() + 1;
        if reachable * 2 <= group {
            return;
        }
        let mine = self.status();
        let mut candidates = vec![eggdb_replication::Candidate { id: mine.id, term: mine.term, applied: mine.applied }];
        candidates.extend(statuses.iter().map(|(_, status)| eggdb_replication::Candidate {
            id: status.id,
            term: status.term,
            applied: status.applied,
        }));
        if eggdb_replication::elect(&candidates) != Some(mine.id) {
            return;
        }
        let term = candidates.iter().map(|candidate| candidate.term).max().unwrap_or(0) + 1;
        if self.promote(term).is_err() {
            return;
        }
        if let Some(address) = &self.advertise {
            for (index, _) in &statuses {
                let _ = send_follow(&self.peers[*index], &self.credentials, address);
            }
        }
    }

    /// A primary that lost contact with a majority of its group for longer than the failover
    /// delay fences itself, so a replica the others promote meanwhile is the only primary taking
    /// writes; it takes the fence away when the majority is back and no newer term appeared.
    fn check_lease(&self, failover_after: Duration, primary: &ReplicationPrimary) {
        let group = self.peers.len() + 1;
        let connected = self.connected_replicas.load(Ordering::Acquire) + 1;
        if connected * 2 > group {
            self.touch();
            if self.lease_fenced.swap(false, Ordering::AcqRel) {
                let own = primary.term().unwrap_or(0);
                let newer = self.peer_statuses().iter().any(|(_, status)| status.term > own);
                if newer {
                    self.lease_fenced.store(true, Ordering::Release);
                } else {
                    self.db.unfence();
                }
            }
            return;
        }
        let age = now_ms().saturating_sub(self.last_contact_ms.load(Ordering::Acquire));
        if age > u64::try_from(failover_after.as_millis()).unwrap_or(u64::MAX) && !self.lease_fenced.swap(true, Ordering::AcqRel) {
            self.db.fence("this primary lost contact with most of its replication group, so it takes no writes until they are back");
        }
    }

    /// Hands the primary's role to the replica listening at `target`: fences this primary,
    /// waits for the target to apply its last batch, promotes it in the next term, and follows
    /// it from then on.
    fn switchover(&self, target: &str) -> Result<String, String> {
        let primary = self.current_primary().ok_or("this server is not the primary")?;
        let peer = self
            .peers
            .iter()
            .find(|peer| peer.address == target)
            .cloned()
            .ok_or_else(|| format!("{target} is not among this server's --replication-peers"))?;
        self.db.fence(format!("this primary is handing over to {target}"));
        let undo = |message: String| {
            self.db.unfence();
            message
        };
        self.db.checkpoint().map_err(|error| undo(format!("cannot settle the last commit: {error}")))?;
        let last = primary.last().map_err(|error| undo(error.to_string()))?;
        let term = primary.term().map_err(|error| undo(error.to_string()))?;
        let started = std::time::Instant::now();
        loop {
            let status = query_status(&peer, &self.credentials).map_err(|message| undo(message))?;
            if status.applied >= last {
                break;
            }
            if started.elapsed() > SWITCHOVER_CATCH_UP {
                return Err(undo(format!("{target} applied batch {} of {last}; try again when it caught up", status.applied)));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        send_promote(&peer, &self.credentials, term + 1).map_err(|message| undo(message))?;
        primary.retire();
        if let Ok(mut current) = self.primary.lock() {
            *current = None;
        }
        if let Ok(mut following) = self.following.lock() {
            *following = Some(peer.clone());
        }
        for other in self.peers.iter().filter(|other| other.address != target) {
            let _ = send_follow(other, &self.credentials, target);
        }
        Ok(format!("{target} is the primary of term {} from batch {last}; this server follows it", term + 1))
    }

    /// Watches the group until `stop` turns true: failover on a replica, the lease on a primary,
    /// and switchover requests from `SWITCHOVER TO`.
    pub(super) fn monitor(&self, stop: &(dyn Fn() -> bool + Sync)) {
        let (sender, requests): (Sender<SwitchoverRequest>, Receiver<SwitchoverRequest>) = channel();
        if let Ok(mut slot) = SWITCHOVER_REQUESTS.lock() {
            *slot = Some(sender);
        }
        while !stop() {
            while let Ok((target, reply)) = requests.try_recv() {
                let _ = reply.send(self.switchover(&target));
            }
            if let Some(failover_after) = self.failover_after {
                match self.current_primary() {
                    Some(primary) if !self.peers.is_empty() => self.check_lease(failover_after, &primary),
                    Some(_) => {}
                    None => {
                        let age = now_ms().saturating_sub(self.last_contact_ms.load(Ordering::Acquire));
                        if self.following().is_some() && age > u64::try_from(failover_after.as_millis()).unwrap_or(u64::MAX) {
                            self.try_failover(failover_after);
                        }
                    }
                }
            }
            std::thread::sleep(TICK);
        }
        if let Ok(mut slot) = SWITCHOVER_REQUESTS.lock() {
            *slot = None;
        }
    }
}

pub(super) fn is_loopback(address: &str) -> bool {
    let host = address.rsplit_once(':').map_or(address, |(host, _)| host).trim_start_matches('[').trim_end_matches(']');
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}
