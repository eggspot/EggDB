//! Raft consensus for EggDB (D211), from Ongaro and Ousterhout's paper "In Search of an
//! Understandable Consensus Algorithm": leader election with randomized timeouts, log
//! replication with consistency checks, and commitment by majority of entries from the leader's
//! own term. A [`Node`] is a pure state machine: the caller delivers messages ([`Node::step`]),
//! advances time ([`Node::tick`]) and sends what they return, so the same code runs over the
//! network and in deterministic simulation. Persistent state (term, vote, log and the last
//! snapshot's position) is reported by [`Node::take_changes`], which the caller makes durable
//! before sending the messages a call returned; [`Node::recover`] rebuilds a node from it after
//! a crash. A log compacted into a snapshot ([`Node::compact`]) reaches a follower that lags
//! behind it as an `InstallSnapshot` message, as the paper's section 7 describes.

use std::collections::{BTreeMap, BTreeSet};

pub mod storage;

/// A node's identity.
pub type NodeId = u64;

/// An entry of the replicated log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The term it was created in.
    pub term: u64,
    /// Its position, from 1.
    pub index: u64,
    /// The command the state machine applies.
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
    },
    /// The answer to an append or a snapshot.
    AppendReply {
        /// The follower's term.
        term: u64,
        /// Whether the entries matched and were stored.
        success: bool,
        /// The follower's last matching index on success, or a hint where to retry from.
        index: u64,
    },
    /// The leader sends its snapshot to a follower whose next entry it no longer has.
    InstallSnapshot {
        /// The leader's term.
        term: u64,
        /// The snapshot.
        snapshot: Snapshot,
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
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            election_ticks: 10,
            heartbeat_ticks: 2,
        }
    }
}

/// Why a proposal was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotLeader {
    /// The leader this node knows of, if any.
    pub leader: Option<NodeId>,
}

const MAX_ENTRIES_PER_APPEND: usize = 64;

/// One Raft participant.
#[derive(Debug, Clone)]
pub struct Node {
    id: NodeId,
    peers: Vec<NodeId>,
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
    hard_state_changed: bool,
    unstable_from: Option<u64>,
    installed: Option<Snapshot>,
    compacted: Option<Snapshot>,
}

impl Node {
    /// A node of the cluster `members` (which includes `id`), with a seed for its randomized
    /// timeouts.
    pub fn new(id: NodeId, members: &[NodeId], timing: Timing, seed: u64) -> Self {
        let mut node = Self {
            id,
            peers: members
                .iter()
                .copied()
                .filter(|member| *member != id)
                .collect(),
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
            hard_state_changed: false,
            unstable_from: None,
            installed: None,
            compacted: None,
        };
        node.reset_timeout();
        node
    }

    /// A node rebuilt from the durable state storage kept, starting as a follower with its
    /// snapshot applied and nothing after it committed yet.
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
        Ok(node)
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
        self.log.drain(..=position);
        self.snapshot_index = through;
        self.snapshot_term = term;
        if self.unstable_from.is_some_and(|from| from <= through) {
            self.unstable_from = (self.last_index() > through).then_some(through + 1);
        }
        let snapshot = Snapshot {
            index: through,
            term,
            data,
        };
        self.compacted = Some(snapshot.clone());
        self.snapshot = Some(snapshot);
        true
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

    /// Its log after the snapshot.
    pub fn log(&self) -> &[Entry] {
        &self.log
    }

    /// Restarts after a crash: the term, vote, snapshot and log stay; role, leader, commit
    /// index and timers start again.
    pub fn restart(&mut self) {
        self.role = Role::Follower;
        self.leader = None;
        self.votes.clear();
        self.next_index.clear();
        self.match_index.clear();
        self.commit = self.snapshot_index;
        self.applied = self.snapshot_index;
        self.elapsed = 0;
        self.reset_timeout();
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

    fn majority(&self) -> usize {
        self.peers.len().div_ceil(2) + 1
    }

    fn send(&self, to: NodeId, message: Message) -> Envelope {
        Envelope {
            from: self.id,
            to,
            message,
        }
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

    fn start_election(&mut self) -> Vec<Envelope> {
        self.term += 1;
        self.hard_state_changed = true;
        self.role = Role::Candidate;
        self.leader = None;
        self.voted_for = Some(self.id);
        self.votes = BTreeSet::from([self.id]);
        self.elapsed = 0;
        self.reset_timeout();
        if self.votes.len() >= self.majority() {
            return self.become_leader();
        }
        let (last_index, last_term) = (self.last_index(), self.last_term());
        self.peers
            .iter()
            .map(|peer| {
                self.send(
                    *peer,
                    Message::RequestVote {
                        term: self.term,
                        last_index,
                        last_term,
                    },
                )
            })
            .collect()
    }

    fn become_leader(&mut self) -> Vec<Envelope> {
        self.role = Role::Leader;
        self.leader = Some(self.id);
        let next = self.last_index() + 1;
        self.next_index = self.peers.iter().map(|peer| (*peer, next)).collect();
        self.match_index = self.peers.iter().map(|peer| (*peer, 0)).collect();
        self.elapsed = 0;
        self.log.push(Entry {
            term: self.term,
            index: next,
            command: Vec::new(),
        });
        self.mark_unstable(next);
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
            },
        )
    }

    fn broadcast(&self) -> Vec<Envelope> {
        self.peers
            .iter()
            .map(|peer| self.append_to(*peer))
            .collect()
    }

    fn advance_commit(&mut self) {
        let mut indexes: Vec<u64> = self.match_index.values().copied().collect();
        indexes.push(self.last_index());
        indexes.sort_unstable_by(|a, b| b.cmp(a));
        let candidate = indexes[self.majority() - 1];
        if candidate > self.commit && self.term_at(candidate) == Some(self.term) {
            self.commit = candidate;
        }
    }

    /// Advances time by one tick: a follower or candidate may start an election, a leader
    /// sends heartbeats.
    pub fn tick(&mut self) -> Vec<Envelope> {
        self.elapsed += 1;
        match self.role {
            Role::Leader => {
                if self.elapsed >= self.timing.heartbeat_ticks {
                    self.elapsed = 0;
                    return self.broadcast();
                }
                Vec::new()
            }
            Role::Follower | Role::Candidate => {
                if self.elapsed >= self.timeout {
                    return self.start_election();
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
        let index = self.last_index() + 1;
        self.log.push(Entry {
            term: self.term,
            index,
            command,
        });
        self.mark_unstable(index);
        self.advance_commit();
        Ok((index, self.broadcast()))
    }

    /// Handles a message from `from`; returns the messages to send.
    pub fn step(&mut self, from: NodeId, message: Message) -> Vec<Envelope> {
        let message_term = match &message {
            Message::RequestVote { term, .. }
            | Message::Vote { term, .. }
            | Message::Append { term, .. }
            | Message::AppendReply { term, .. }
            | Message::InstallSnapshot { term, .. } => *term,
        };
        if message_term > self.term {
            let leader = matches!(
                message,
                Message::Append { .. } | Message::InstallSnapshot { .. }
            )
            .then_some(from);
            self.become_follower(message_term, leader);
        }
        match message {
            Message::RequestVote {
                term,
                last_index,
                last_term,
            } => {
                let up_to_date = (last_term, last_index) >= (self.last_term(), self.last_index());
                let granted = term == self.term
                    && up_to_date
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
                    if self.votes.len() >= self.majority() {
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
            } => {
                if term < self.term {
                    return vec![self.send(
                        from,
                        Message::AppendReply {
                            term: self.term,
                            success: false,
                            index: self.last_index(),
                        },
                    )];
                }
                self.become_follower(term, Some(from));
                let consecutive = entries.iter().enumerate().all(|(at, entry)| {
                    previous_index
                        .checked_add(at as u64 + 1)
                        .is_some_and(|expected| entry.index == expected)
                });
                if !consecutive {
                    return vec![self.send(
                        from,
                        Message::AppendReply {
                            term: self.term,
                            success: false,
                            index: previous_index.min(self.last_index()),
                        },
                    )];
                }
                let (previous_index, previous_term, entries) =
                    if previous_index < self.snapshot_index {
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
                    return vec![self.send(
                        from,
                        Message::AppendReply {
                            term: self.term,
                            success: false,
                            index: hint,
                        },
                    )];
                }
                let received = entries.len() as u64;
                for entry in entries {
                    match self.term_at(entry.index) {
                        Some(existing) if existing == entry.term => {}
                        Some(_) => {
                            if let Some(position) = self.position(entry.index) {
                                self.log.truncate(position);
                            }
                            self.mark_unstable(entry.index);
                            self.log.push(entry);
                        }
                        None => {
                            self.mark_unstable(entry.index);
                            self.log.push(entry);
                        }
                    }
                }
                let matched = previous_index + received;
                if commit > self.commit {
                    self.commit = self.commit.max(commit.min(matched));
                }
                vec![self.send(
                    from,
                    Message::AppendReply {
                        term: self.term,
                        success: true,
                        index: matched,
                    },
                )]
            }
            Message::AppendReply {
                term,
                success,
                index,
            } => {
                if self.role != Role::Leader || term != self.term {
                    return Vec::new();
                }
                if success {
                    let matched = self.match_index.entry(from).or_insert(0);
                    *matched = (*matched).max(index);
                    self.next_index.insert(from, *matched + 1);
                    self.advance_commit();
                    if index < self.last_index() {
                        return vec![self.append_to(from)];
                    }
                    Vec::new()
                } else {
                    let next = self.next_index.entry(from).or_insert(1);
                    *next = (index + 1).min(next.saturating_sub(1)).max(1);
                    vec![self.append_to(from)]
                }
            }
            Message::InstallSnapshot { term, snapshot } => {
                if term < self.term {
                    return vec![self.send(
                        from,
                        Message::AppendReply {
                            term: self.term,
                            success: false,
                            index: self.last_index(),
                        },
                    )];
                }
                self.become_follower(term, Some(from));
                let index = snapshot.index;
                if index <= self.commit {
                    return vec![self.send(
                        from,
                        Message::AppendReply {
                            term: self.term,
                            success: true,
                            index: self.commit,
                        },
                    )];
                }
                self.install(snapshot);
                vec![self.send(
                    from,
                    Message::AppendReply {
                        term: self.term,
                        success: true,
                        index,
                    },
                )]
            }
        }
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
    }

    /// The entries committed since the last call, in order, for the state machine to apply
    /// (the leader's empty no-op entries included, with an empty command).
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
