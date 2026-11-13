//! Raft consensus for EggDB (D211), from Ongaro and Ousterhout's paper "In Search of an
//! Understandable Consensus Algorithm" and Ongaro's thesis: leader election with randomized
//! timeouts and a pre-vote round, log replication with consistency checks, commitment by
//! majority of entries from the leader's own term, membership changes through a joint
//! configuration, and leader leases for reads. A [`Node`] is a pure state machine: the caller
//! delivers messages ([`Node::step`]), advances time ([`Node::tick`]) and sends what they
//! return, so the same code runs over the network and in deterministic simulation. Persistent
//! state (term, vote, log and the last snapshot) is reported by [`Node::take_changes`], which the
//! caller makes durable before sending the messages a call returned; [`Node::recover`] rebuilds
//! a node from it after a crash. A log compacted into a snapshot ([`Node::compact`]) reaches a
//! follower that lags behind it as an `InstallSnapshot` message, as the paper's section 7
//! describes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

mod membership;
pub mod storage;

pub use membership::Membership;

/// A node's identity.
pub type NodeId = u64;

/// What a log entry carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EntryKind {
    /// A command for the state machine (empty for a leader's no-op).
    #[default]
    Command,
    /// A cluster configuration, as [`Membership::encode`] writes it.
    Membership,
}

/// An entry of the replicated log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The term it was created in.
    pub term: u64,
    /// Its position, from 1.
    pub index: u64,
    /// What it carries.
    pub kind: EntryKind,
    /// The command the state machine applies, or the encoded configuration.
    pub command: Vec<u8>,
}

/// What a node keeps durably before it answers: its term and its vote in that term.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HardState {
    /// The current term.
    pub term: u64,
    /// The candidate voted for in that term.
    pub voted_for: Option<NodeId>,
}

/// The state machine as of a log position, which replaces every entry up to it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// The last index it includes.
    pub index: u64,
    /// The term of that entry.
    pub term: u64,
    /// The configuration as of that entry.
    pub membership: Membership,
    /// The state machine's own encoding of its state; empty when the state machine is its
    /// own snapshot, as a database file is.
    pub data: Vec<u8>,
}

/// What changed in a node's durable state since the last [`Node::take_changes`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Changes {
    /// The term and vote, when either changed.
    pub hard_state: Option<HardState>,
    /// The first log index rewritten: stored entries from it on are replaced by `entries`.
    pub entries_from: Option<u64>,
    /// The log's entries from `entries_from` to its end.
    pub entries: Vec<Entry>,
    /// A snapshot the leader sent: the state machine restores it, and stored entries up to its
    /// index are dropped.
    pub installed: Option<Snapshot>,
    /// The log was compacted into this snapshot: stored entries up to its index are dropped.
    pub compacted: Option<Snapshot>,
}

impl Changes {
    /// Whether nothing needs to be stored.
    pub fn is_empty(&self) -> bool {
        self.hard_state.is_none()
            && self.entries_from.is_none()
            && self.installed.is_none()
            && self.compacted.is_none()
    }
}

/// A node's durable state as storage read it back after a crash.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Recovered {
    /// The term and vote.
    pub hard_state: HardState,
    /// The last snapshot, if the log was ever compacted or a snapshot installed.
    pub snapshot: Option<Snapshot>,
    /// The entries after the snapshot, in order.
    pub entries: Vec<Entry>,
}

/// Why stored state could not be recovered: the entries after the snapshot do not follow one
/// another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrokenLog {
    /// The index the next entry should have had.
    pub expected: u64,
    /// The index it had.
    pub found: u64,
}

impl std::fmt::Display for BrokenLog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the stored consensus log jumps from index {} to {}",
            self.expected.saturating_sub(1),
            self.found
        )
    }
}

impl std::error::Error for BrokenLog {}

/// A message between nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A node about to stand asks whether it could win, without changing anyone's term
    /// (the thesis's section 9.6).
    PreVote {
        /// The term it would stand in.
        term: u64,
        /// The index of its last entry.
        last_index: u64,
        /// The term of its last entry.
        last_term: u64,
    },
    /// The answer to a pre-vote.
    PreVoteReply {
        /// The asked term when granted, or the voter's own term when refused.
        term: u64,
        /// Whether it would vote.
        granted: bool,
    },
    /// A candidate asks for a vote.
    RequestVote {
        /// The candidate's term.
        term: u64,
        /// The index of its last entry.
        last_index: u64,
        /// The term of its last entry.
        last_term: u64,
    },
    /// The answer to a vote request.
    Vote {
        /// The voter's term.
        term: u64,
        /// Whether the vote was given.
        granted: bool,
    },
    /// The leader appends entries (or sends a heartbeat with none).
    Append {
        /// The leader's term.
        term: u64,
        /// The index before the entries.
        previous_index: u64,
        /// The term of that entry.
        previous_term: u64,
        /// The entries.
        entries: Vec<Entry>,
        /// The leader's commit index.
        commit: u64,
        /// The leader's heartbeat round, echoed in the reply.
        round: u64,
    },
    /// The answer to an append or a snapshot.
    AppendReply {
        /// The follower's term.
        term: u64,
        /// Whether the entries matched and were stored.
        success: bool,
        /// The follower's last matching index on success, or a hint where to retry from.
        index: u64,
        /// The round of the message answered.
        round: u64,
    },
    /// The leader sends its snapshot to a follower whose next entry it no longer has.
    InstallSnapshot {
        /// The leader's term.
        term: u64,
        /// The snapshot.
        snapshot: Snapshot,
        /// The leader's heartbeat round.
        round: u64,
    },
}

/// A message to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// The sender.
    pub from: NodeId,
    /// The receiver.
    pub to: NodeId,
    /// The message.
    pub message: Message,
}

/// A node's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Follows a leader.
    Follower,
    /// Asks whether it could win before it stands.
    PreCandidate,
    /// Asks for votes.
    Candidate,
    /// Leads its term.
    Leader,
}

/// How a node keeps time, in ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// A follower starts an election after between this and twice this many ticks without a
    /// leader.
    pub election_ticks: u32,
    /// A leader sends heartbeats this often.
    pub heartbeat_ticks: u32,
    /// How much shorter than `election_ticks` a leader's read lease is, for clock drift
    /// between nodes.
    pub lease_margin_ticks: u32,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            election_ticks: 10,
            heartbeat_ticks: 2,
            lease_margin_ticks: 2,
        }
    }
}

/// Why a proposal was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotLeader {
    /// The leader this node knows of, if any.
    pub leader: Option<NodeId>,
}

/// Why a membership change was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipRefused {
    /// Only the leader changes the membership.
    NotLeader(NotLeader),
    /// The previous change has not finished: its configurations are not both committed.
    ChangeInProgress,
    /// A cluster needs at least one voter.
    NoVoters,
}

const MAX_ENTRIES_PER_APPEND: usize = 64;
const ROUNDS_KEPT: usize = 64;

/// One Raft participant.
#[derive(Debug, Clone)]
pub struct Node {
    id: NodeId,
    initial: Membership,
    membership: Membership,
    membership_index: u64,
    timing: Timing,
    term: u64,
    voted_for: Option<NodeId>,
    log: Vec<Entry>,
    snapshot: Option<Snapshot>,
    snapshot_index: u64,
    snapshot_term: u64,
    commit: u64,
    applied: u64,
    role: Role,
    leader: Option<NodeId>,
    votes: BTreeSet<NodeId>,
    next_index: BTreeMap<NodeId, u64>,
    match_index: BTreeMap<NodeId, u64>,
    elapsed: u32,
    timeout: u32,
    seed: u64,
    now: u64,
    round: u64,
    rounds_sent: VecDeque<(u64, u64)>,
    acked_round: BTreeMap<NodeId, u64>,
    hard_state_changed: bool,
    unstable_from: Option<u64>,
    installed: Option<Snapshot>,
    compacted: Option<Snapshot>,
}

impl Node {
    /// A node of the cluster `members` (which includes `id`), with a seed for its randomized
    /// timeouts.
    pub fn new(id: NodeId, members: &[NodeId], timing: Timing, seed: u64) -> Self {
        let initial = Membership::new(members);
        let mut node = Self {
            id,
            membership: initial.clone(),
            initial,
            membership_index: 0,
            timing,
            term: 0,
            voted_for: None,
            log: Vec::new(),
            snapshot: None,
            snapshot_index: 0,
            snapshot_term: 0,
            commit: 0,
            applied: 0,
            role: Role::Follower,
            leader: None,
            votes: BTreeSet::new(),
            next_index: BTreeMap::new(),
            match_index: BTreeMap::new(),
            elapsed: 0,
            timeout: 0,
            seed: seed.max(1),
            now: 0,
            round: 0,
            rounds_sent: VecDeque::new(),
            acked_round: BTreeMap::new(),
            hard_state_changed: false,
            unstable_from: None,
            installed: None,
            compacted: None,
        };
        node.reset_timeout();
        node
    }

    /// A node rebuilt from the durable state storage kept, starting as a follower with its
    /// snapshot applied and nothing after it committed yet. `members` is the configuration
    /// the cluster started with, used until the log or the snapshot names another.
    pub fn recover(
        id: NodeId,
        members: &[NodeId],
        timing: Timing,
        seed: u64,
        recovered: Recovered,
    ) -> Result<Self, BrokenLog> {
        let mut node = Self::new(id, members, timing, seed);
        node.term = recovered.hard_state.term;
        node.voted_for = recovered.hard_state.voted_for;
        if let Some(snapshot) = recovered.snapshot {
            node.snapshot_index = snapshot.index;
            node.snapshot_term = snapshot.term;
            node.commit = snapshot.index;
            node.applied = snapshot.index;
            node.snapshot = Some(snapshot);
        }
        let mut expected = node.snapshot_index + 1;
        for entry in recovered.entries {
            if entry.index < expected {
                continue;
            }
            if entry.index != expected {
                return Err(BrokenLog {
                    expected,
                    found: entry.index,
                });
            }
            expected += 1;
            node.log.push(entry);
        }
        node.refresh_membership();
        Ok(node)
    }

    /// Its id.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// Its role.
    pub fn role(&self) -> Role {
        self.role
    }

    /// Its current term.
    pub fn term(&self) -> u64 {
        self.term
    }

    /// The leader it knows of.
    pub fn leader(&self) -> Option<NodeId> {
        self.leader
    }

    /// Its commit index.
    pub fn commit_index(&self) -> u64 {
        self.commit
    }

    /// The last index handed to the state machine by [`Self::take_committed`] or a snapshot.
    pub fn applied_index(&self) -> u64 {
        self.applied
    }

    /// The configuration it uses: the newest one in its log, committed or not.
    pub fn membership(&self) -> &Membership {
        &self.membership
    }

    /// Its term and vote.
    pub fn hard_state(&self) -> HardState {
        HardState {
            term: self.term,
            voted_for: self.voted_for,
        }
    }

    /// The index and term its log was last compacted at, or a snapshot installed at.
    pub fn snapshot_position(&self) -> (u64, u64) {
        (self.snapshot_index, self.snapshot_term)
    }

    /// Its log after the snapshot.
    pub fn log(&self) -> &[Entry] {
        &self.log
    }

    /// What changed in its durable state since the last call. The caller stores it, in one
    /// durable write, before sending the messages the calls since then returned; an
    /// `installed` snapshot is restored before the entries of [`Self::take_committed`] are
    /// applied.
    pub fn take_changes(&mut self) -> Changes {
        let hard_state = std::mem::take(&mut self.hard_state_changed).then(|| self.hard_state());
        let (entries_from, entries) = match self.unstable_from.take() {
            Some(from) => {
                let from = from.max(self.snapshot_index + 1);
                let entries = self
                    .position(from)
                    .and_then(|position| self.log.get(position..))
                    .map(<[Entry]>::to_vec)
                    .unwrap_or_default();
                (Some(from), entries)
            }
            None => (None, Vec::new()),
        };
        Changes {
            hard_state,
            entries_from,
            entries,
            installed: self.installed.take(),
            compacted: self.compacted.take(),
        }
    }

    /// Compacts the log through `through`, an index the state machine has applied, into a
    /// snapshot whose state is `data`. Returns whether it did: an index not applied yet, or
    /// one at or before the current snapshot, changes nothing.
    pub fn compact(&mut self, through: u64, data: Vec<u8>) -> bool {
        if through <= self.snapshot_index || through > self.applied {
            return false;
        }
        let (Some(term), Some(position)) = (self.term_at(through), self.position(through)) else {
            return false;
        };
        let membership = self.membership_at(through);
        self.log.drain(..=position);
        self.snapshot_index = through;
        self.snapshot_term = term;
        if self.unstable_from.is_some_and(|from| from <= through) {
            self.unstable_from = (self.last_index() > through).then_some(through + 1);
        }
        let snapshot = Snapshot {
            index: through,
            term,
            membership,
            data,
        };
        self.compacted = Some(snapshot.clone());
        self.snapshot = Some(snapshot);
        true
    }

    /// Restarts after a crash: the term, vote, snapshot and log stay; role, leader, commit
    /// index and timers start again.
    pub fn restart(&mut self) {
        self.role = Role::Follower;
        self.leader = None;
        self.votes.clear();
        self.next_index.clear();
        self.match_index.clear();
        self.acked_round.clear();
        self.rounds_sent.clear();
        self.commit = self.snapshot_index;
        self.applied = self.snapshot_index;
        self.elapsed = 0;
        self.reset_timeout();
    }

    /// The commit index a read may be served at without asking the other voters, while this
    /// leader holds its lease (the thesis's section 6.4.1): a quorum acknowledged a heartbeat
    /// round sent less than `election_ticks - lease_margin_ticks` ticks ago, so no other node
    /// can have been elected since, and an entry of its own term is committed. `None` when
    /// a read has to go through the log.
    pub fn lease_read_index(&self) -> Option<u64> {
        if self.role != Role::Leader || self.term_at(self.commit) != Some(self.term) {
            return None;
        }
        let acked = self.membership.quorum_value(|member| {
            if member == self.id {
                self.round
            } else {
                self.acked_round.get(&member).copied().unwrap_or(0)
            }
        });
        let sent_at = self
            .rounds_sent
            .iter()
            .find(|(round, _)| *round == acked)
            .map(|(_, at)| *at)?;
        let lease = u64::from(
            self.timing
                .election_ticks
                .saturating_sub(self.timing.lease_margin_ticks),
        );
        (self.now < sent_at + lease).then_some(self.commit)
    }

    /// Starts moving the cluster to the voters `voters`: the leader appends the joint
    /// configuration, and once that commits, the new one, after which a leader that is not a
    /// voter steps down. Returns the joint entry's index.
    pub fn propose_membership(
        &mut self,
        voters: &[NodeId],
    ) -> Result<(u64, Vec<Envelope>), MembershipRefused> {
        if self.role != Role::Leader {
            return Err(MembershipRefused::NotLeader(NotLeader {
                leader: self.leader,
            }));
        }
        if self.membership.is_joint() || self.membership_index > self.commit {
            return Err(MembershipRefused::ChangeInProgress);
        }
        if voters.is_empty() {
            return Err(MembershipRefused::NoVoters);
        }
        let joint = Membership::joint(&self.membership, voters);
        let index = self.append_own(EntryKind::Membership, joint.encode());
        self.advance_commit();
        Ok((index, self.broadcast()))
    }

    fn random(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }

    fn reset_timeout(&mut self) {
        let span = u64::from(self.timing.election_ticks.max(1));
        self.timeout = (span + self.random() % span) as u32;
    }

    fn last_index(&self) -> u64 {
        self.log.last().map_or(self.snapshot_index, |entry| entry.index)
    }

    fn last_term(&self) -> u64 {
        self.log.last().map_or(self.snapshot_term, |entry| entry.term)
    }

    fn position(&self, index: u64) -> Option<usize> {
        usize::try_from(index.checked_sub(self.snapshot_index + 1)?).ok()
    }

    fn term_at(&self, index: u64) -> Option<u64> {
        if index == self.snapshot_index {
            return Some(self.snapshot_term);
        }
        self.log.get(self.position(index)?).map(|entry| entry.term)
    }

    fn mark_unstable(&mut self, index: u64) {
        self.unstable_from = Some(self.unstable_from.map_or(index, |from| from.min(index)));
    }

    fn set_vote(&mut self, voted_for: Option<NodeId>) {
        if self.voted_for != voted_for {
            self.voted_for = voted_for;
            self.hard_state_changed = true;
        }
    }

    fn peers(&self) -> Vec<NodeId> {
        self.membership
            .members()
            .into_iter()
            .filter(|member| *member != self.id)
            .collect()
    }

    fn base_membership(&self) -> (u64, Membership) {
        match &self.snapshot {
            Some(snapshot) if !snapshot.membership.voters.is_empty() => {
                (snapshot.index, snapshot.membership.clone())
            }
            _ => (0, self.initial.clone()),
        }
    }

    fn membership_at(&self, index: u64) -> Membership {
        self.log
            .iter()
            .rev()
            .filter(|entry| entry.index <= index && entry.kind == EntryKind::Membership)
            .find_map(|entry| Membership::decode(&entry.command))
            .unwrap_or_else(|| self.base_membership().1)
    }

    fn refresh_membership(&mut self) {
        let newest = self
            .log
            .iter()
            .rev()
            .filter(|entry| entry.kind == EntryKind::Membership)
            .find_map(|entry| Some((entry.index, Membership::decode(&entry.command)?)));
        let (index, membership) = newest.unwrap_or_else(|| self.base_membership());
        self.membership_index = index;
        self.membership = membership;
        if self.role == Role::Leader {
            let next = self.last_index() + 1;
            for peer in self.peers() {
                self.next_index.entry(peer).or_insert(next);
                self.match_index.entry(peer).or_insert(0);
            }
        }
    }

    fn append_own(&mut self, kind: EntryKind, command: Vec<u8>) -> u64 {
        let index = self.last_index() + 1;
        self.log.push(Entry {
            term: self.term,
            index,
            kind,
            command,
        });
        self.mark_unstable(index);
        if kind == EntryKind::Membership {
            self.refresh_membership();
        }
        index
    }

    fn send(&self, to: NodeId, message: Message) -> Envelope {
        Envelope {
            from: self.id,
            to,
            message,
        }
    }

    fn leader_is_live(&self) -> bool {
        self.role == Role::Leader
            || (self.leader.is_some() && self.elapsed < self.timing.election_ticks)
    }

    fn log_up_to_date(&self, last_index: u64, last_term: u64) -> bool {
        (last_term, last_index) >= (self.last_term(), self.last_index())
    }

    fn become_follower(&mut self, term: u64, leader: Option<NodeId>) {
        if term > self.term {
            self.term = term;
            self.voted_for = None;
            self.hard_state_changed = true;
        }
        self.role = Role::Follower;
        self.leader = leader;
        self.elapsed = 0;
        self.reset_timeout();
    }

    fn start_pre_vote(&mut self) -> Vec<Envelope> {
        self.role = Role::PreCandidate;
        self.leader = None;
        self.votes = BTreeSet::from([self.id]);
        self.elapsed = 0;
        self.reset_timeout();
        if self.membership.has_quorum(&self.votes) {
            return self.start_election();
        }
        let message = Message::PreVote {
            term: self.term + 1,
            last_index: self.last_index(),
            last_term: self.last_term(),
        };
        self.peers()
            .into_iter()
            .map(|peer| self.send(peer, message.clone()))
            .collect()
    }

    fn start_election(&mut self) -> Vec<Envelope> {
        self.term += 1;
        self.hard_state_changed = true;
        self.role = Role::Candidate;
        self.leader = None;
        self.voted_for = Some(self.id);
        self.votes = BTreeSet::from([self.id]);
        self.elapsed = 0;
        self.reset_timeout();
        if self.membership.has_quorum(&self.votes) {
            return self.become_leader();
        }
        let message = Message::RequestVote {
            term: self.term,
            last_index: self.last_index(),
            last_term: self.last_term(),
        };
        self.peers()
            .into_iter()
            .map(|peer| self.send(peer, message.clone()))
            .collect()
    }

    fn become_leader(&mut self) -> Vec<Envelope> {
        self.role = Role::Leader;
        self.leader = Some(self.id);
        let next = self.last_index() + 1;
        self.next_index = self.peers().into_iter().map(|peer| (peer, next)).collect();
        self.match_index = self.peers().into_iter().map(|peer| (peer, 0)).collect();
        self.acked_round.clear();
        self.rounds_sent.clear();
        self.elapsed = 0;
        self.append_own(EntryKind::Command, Vec::new());
        self.advance_commit();
        self.broadcast()
    }

    fn append_to(&self, peer: NodeId) -> Envelope {
        let next = self.next_index.get(&peer).copied().unwrap_or(1).max(1);
        if next <= self.snapshot_index
            && let Some(snapshot) = &self.snapshot
        {
            return self.send(
                peer,
                Message::InstallSnapshot {
                    term: self.term,
                    snapshot: snapshot.clone(),
                    round: self.round,
                },
            );
        }
        let next = next.max(self.snapshot_index + 1);
        let previous_index = next - 1;
        let previous_term = self.term_at(previous_index).unwrap_or(0);
        let entries: Vec<Entry> = self
            .position(next)
            .and_then(|position| self.log.get(position..))
            .unwrap_or_default()
            .iter()
            .take(MAX_ENTRIES_PER_APPEND)
            .cloned()
            .collect();
        self.send(
            peer,
            Message::Append {
                term: self.term,
                previous_index,
                previous_term,
                entries,
                commit: self.commit,
                round: self.round,
            },
        )
    }

    fn broadcast(&mut self) -> Vec<Envelope> {
        self.round += 1;
        self.rounds_sent.push_back((self.round, self.now));
        if self.rounds_sent.len() > ROUNDS_KEPT {
            self.rounds_sent.pop_front();
        }
        self.peers()
            .into_iter()
            .map(|peer| self.append_to(peer))
            .collect()
    }

    fn advance_commit(&mut self) {
        loop {
            let last = self.last_index();
            let candidate = self.membership.quorum_value(|member| {
                if member == self.id {
                    last
                } else {
                    self.match_index.get(&member).copied().unwrap_or(0)
                }
            });
            if candidate > self.commit
                && candidate <= last
                && self.term_at(candidate) == Some(self.term)
            {
                self.commit = candidate;
            }
            if !self.finish_membership_change() {
                return;
            }
        }
    }

    fn finish_membership_change(&mut self) -> bool {
        if self.role != Role::Leader || self.membership_index > self.commit {
            return false;
        }
        if self.membership.is_joint() {
            let final_membership = Membership::new(&self.membership.voters);
            self.append_own(EntryKind::Membership, final_membership.encode());
            return true;
        }
        if !self.membership.voters.contains(&self.id) {
            self.role = Role::Follower;
            self.leader = None;
            self.elapsed = 0;
        }
        false
    }

    /// Advances time by one tick: a follower or candidate that heard from no leader for its
    /// timeout starts a pre-vote, a leader sends heartbeats. A node that is not a voter never
    /// stands.
    pub fn tick(&mut self) -> Vec<Envelope> {
        self.now += 1;
        self.elapsed += 1;
        match self.role {
            Role::Leader => {
                if self.elapsed >= self.timing.heartbeat_ticks {
                    self.elapsed = 0;
                    return self.broadcast();
                }
                Vec::new()
            }
            Role::Follower | Role::PreCandidate | Role::Candidate => {
                if self.elapsed >= self.timeout && self.membership.contains(self.id) {
                    return self.start_pre_vote();
                }
                Vec::new()
            }
        }
    }

    /// Appends a command when this node leads; returns its index.
    pub fn propose(&mut self, command: Vec<u8>) -> Result<(u64, Vec<Envelope>), NotLeader> {
        if self.role != Role::Leader {
            return Err(NotLeader {
                leader: self.leader,
            });
        }
        let index = self.append_own(EntryKind::Command, command);
        self.advance_commit();
        Ok((index, self.broadcast()))
    }

    /// Handles a message from `from`; returns the messages to send.
    pub fn step(&mut self, from: NodeId, message: Message) -> Vec<Envelope> {
        let message_term = match &message {
            Message::PreVote { term, .. }
            | Message::PreVoteReply { term, .. }
            | Message::RequestVote { term, .. }
            | Message::Vote { term, .. }
            | Message::Append { term, .. }
            | Message::AppendReply { term, .. }
            | Message::InstallSnapshot { term, .. } => *term,
        };
        let keeps_term = match &message {
            Message::PreVote { .. } | Message::PreVoteReply { granted: true, .. } => true,
            Message::RequestVote { .. } => self.leader_is_live(),
            _ => false,
        };
        if message_term > self.term && !keeps_term {
            let leader = matches!(
                message,
                Message::Append { .. } | Message::InstallSnapshot { .. }
            )
            .then_some(from);
            self.become_follower(message_term, leader);
        }
        match message {
            Message::PreVote {
                term,
                last_index,
                last_term,
            } => {
                let granted = term > self.term
                    && !self.leader_is_live()
                    && self.log_up_to_date(last_index, last_term);
                vec![self.send(
                    from,
                    Message::PreVoteReply {
                        term: if granted { term } else { self.term },
                        granted,
                    },
                )]
            }
            Message::PreVoteReply { term, granted } => {
                if self.role == Role::PreCandidate && granted && term == self.term + 1 {
                    self.votes.insert(from);
                    if self.membership.has_quorum(&self.votes) {
                        return self.start_election();
                    }
                }
                Vec::new()
            }
            Message::RequestVote {
                term,
                last_index,
                last_term,
            } => {
                let granted = term == self.term
                    && self.log_up_to_date(last_index, last_term)
                    && self.voted_for.is_none_or(|voted| voted == from);
                if granted {
                    self.set_vote(Some(from));
                    self.elapsed = 0;
                }
                vec![self.send(
                    from,
                    Message::Vote {
                        term: self.term,
                        granted,
                    },
                )]
            }
            Message::Vote { term, granted } => {
                if self.role == Role::Candidate && term == self.term && granted {
                    self.votes.insert(from);
                    if self.membership.has_quorum(&self.votes) {
                        return self.become_leader();
                    }
                }
                Vec::new()
            }
            Message::Append {
                term,
                previous_index,
                previous_term,
                entries,
                commit,
                round,
            } => self.receive_append(from, term, previous_index, previous_term, entries, commit, round),
            Message::AppendReply {
                term,
                success,
                index,
                round,
            } => {
                if self.role != Role::Leader || term != self.term {
                    return Vec::new();
                }
                let acked = self.acked_round.entry(from).or_insert(0);
                *acked = (*acked).max(round);
                if success {
                    let matched = self.match_index.entry(from).or_insert(0);
                    *matched = (*matched).max(index);
                    let matched = *matched;
                    self.next_index.insert(from, matched + 1);
                    self.advance_commit();
                    if self.role == Role::Leader && matched < self.last_index() {
                        return vec![self.append_to(from)];
                    }
                    Vec::new()
                } else {
                    let next = self.next_index.entry(from).or_insert(1);
                    *next = (index + 1).min(next.saturating_sub(1)).max(1);
                    vec![self.append_to(from)]
                }
            }
            Message::InstallSnapshot {
                term,
                snapshot,
                round,
            } => {
                if term < self.term {
                    return vec![self.reply(from, false, self.last_index(), round)];
                }
                self.become_follower(term, Some(from));
                let index = snapshot.index;
                if index <= self.commit {
                    return vec![self.reply(from, true, self.commit, round)];
                }
                self.install(snapshot);
                vec![self.reply(from, true, index, round)]
            }
        }
    }

    fn reply(&self, to: NodeId, success: bool, index: u64, round: u64) -> Envelope {
        self.send(
            to,
            Message::AppendReply {
                term: self.term,
                success,
                index,
                round,
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn receive_append(
        &mut self,
        from: NodeId,
        term: u64,
        previous_index: u64,
        previous_term: u64,
        entries: Vec<Entry>,
        commit: u64,
        round: u64,
    ) -> Vec<Envelope> {
        if term < self.term {
            return vec![self.reply(from, false, self.last_index(), round)];
        }
        self.become_follower(term, Some(from));
        let consecutive = entries.iter().enumerate().all(|(at, entry)| {
            previous_index
                .checked_add(at as u64 + 1)
                .is_some_and(|expected| entry.index == expected)
        });
        if !consecutive {
            return vec![self.reply(from, false, previous_index.min(self.last_index()), round)];
        }
        let (previous_index, previous_term, entries) = if previous_index < self.snapshot_index {
            let covered = (self.snapshot_index - previous_index) as usize;
            (
                self.snapshot_index,
                self.snapshot_term,
                entries.into_iter().skip(covered).collect(),
            )
        } else {
            (previous_index, previous_term, entries)
        };
        if self.term_at(previous_index) != Some(previous_term) {
            let hint = previous_index.saturating_sub(1).min(self.last_index());
            return vec![self.reply(from, false, hint, round)];
        }
        let received = entries.len() as u64;
        let mut shape_changed = false;
        for entry in entries {
            match self.term_at(entry.index) {
                Some(existing) if existing == entry.term => {}
                found => {
                    if found.is_some()
                        && let Some(position) = self.position(entry.index)
                    {
                        shape_changed |= self.log[position..]
                            .iter()
                            .any(|dropped| dropped.kind == EntryKind::Membership);
                        self.log.truncate(position);
                    }
                    shape_changed |= entry.kind == EntryKind::Membership;
                    self.mark_unstable(entry.index);
                    self.log.push(entry);
                }
            }
        }
        if shape_changed {
            self.refresh_membership();
        }
        let matched = previous_index + received;
        if commit > self.commit {
            self.commit = self.commit.max(commit.min(matched));
        }
        vec![self.reply(from, true, matched, round)]
    }

    fn install(&mut self, snapshot: Snapshot) {
        let index = snapshot.index;
        match (self.term_at(index), self.position(index)) {
            (Some(term), Some(position)) if term == snapshot.term => {
                self.log.drain(..=position);
                if self.unstable_from.is_some_and(|from| from <= index) {
                    self.unstable_from = (self.last_index() > index).then_some(index + 1);
                }
            }
            _ => {
                self.log.clear();
                self.unstable_from = Some(index + 1);
            }
        }
        self.snapshot_index = index;
        self.snapshot_term = snapshot.term;
        self.commit = self.commit.max(index);
        self.applied = self.applied.max(index);
        self.installed = Some(snapshot.clone());
        self.snapshot = Some(snapshot);
        self.refresh_membership();
    }

    /// The entries committed since the last call, in order, for the state machine to apply
    /// (the leader's empty no-op entries and configuration entries included).
    pub fn take_committed(&mut self) -> Vec<Entry> {
        let to = self.commit.min(self.last_index());
        if to <= self.applied {
            return Vec::new();
        }
        let from = self.applied + 1;
        self.applied = to;
        match (self.position(from), self.position(to)) {
            (Some(first), Some(last)) => self
                .log
                .get(first..=last)
                .map(<[Entry]>::to_vec)
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests;
