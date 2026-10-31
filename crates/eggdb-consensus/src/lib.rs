//! Raft consensus for EggDB (D211), from Ongaro and Ousterhout's paper "In Search of an
//! Understandable Consensus Algorithm": leader election with randomized timeouts, log
//! replication with consistency checks, and commitment by majority of entries from the leader's
//! own term. A [`Node`] is a pure state machine: the caller delivers messages ([`Node::step`]),
//! advances time ([`Node::tick`]) and sends what they return, so the same code runs over the
//! network and in deterministic simulation. Persistent state (term, vote, log) survives
//! [`Node::restart`]; everything else is rebuilt.

use std::collections::{BTreeMap, BTreeSet};

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
    /// The answer to an append.
    AppendReply {
        /// The follower's term.
        term: u64,
        /// Whether the entries matched and were stored.
        success: bool,
        /// The follower's last matching index on success, or a hint where to retry from.
        index: u64,
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
        Self { election_ticks: 10, heartbeat_ticks: 2 }
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
}

impl Node {
    /// A node of the cluster `members` (which includes `id`), with a seed for its randomized
    /// timeouts.
    pub fn new(id: NodeId, members: &[NodeId], timing: Timing, seed: u64) -> Self {
        let mut node = Self {
            id,
            peers: members.iter().copied().filter(|member| *member != id).collect(),
            timing,
            term: 0,
            voted_for: None,
            log: Vec::new(),
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
        };
        node.reset_timeout();
        node
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

    /// Its log.
    pub fn log(&self) -> &[Entry] {
        &self.log
    }

    /// Restarts after a crash: the term, vote and log stay; role, leader, commit index and
    /// timers start again.
    pub fn restart(&mut self) {
        self.role = Role::Follower;
        self.leader = None;
        self.votes.clear();
        self.next_index.clear();
        self.match_index.clear();
        self.commit = 0;
        self.applied = 0;
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
        self.log.last().map_or(0, |entry| entry.index)
    }

    fn last_term(&self) -> u64 {
        self.log.last().map_or(0, |entry| entry.term)
    }

    fn term_at(&self, index: u64) -> Option<u64> {
        if index == 0 {
            return Some(0);
        }
        self.log.get(usize::try_from(index - 1).ok()?).map(|entry| entry.term)
    }

    fn majority(&self) -> usize {
        (self.peers.len() + 1) / 2 + 1
    }

    fn send(&self, to: NodeId, message: Message) -> Envelope {
        Envelope { from: self.id, to, message }
    }

    fn become_follower(&mut self, term: u64, leader: Option<NodeId>) {
        if term > self.term {
            self.term = term;
            self.voted_for = None;
        }
        self.role = Role::Follower;
        self.leader = leader;
        self.elapsed = 0;
        self.reset_timeout();
    }

    fn start_election(&mut self) -> Vec<Envelope> {
        self.term += 1;
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
        self.peers.iter().map(|peer| self.send(*peer, Message::RequestVote { term: self.term, last_index, last_term })).collect()
    }

    fn become_leader(&mut self) -> Vec<Envelope> {
        self.role = Role::Leader;
        self.leader = Some(self.id);
        let next = self.last_index() + 1;
        self.next_index = self.peers.iter().map(|peer| (*peer, next)).collect();
        self.match_index = self.peers.iter().map(|peer| (*peer, 0)).collect();
        self.elapsed = 0;
        self.log.push(Entry { term: self.term, index: next, command: Vec::new() });
        self.advance_commit();
        self.broadcast()
    }

    fn append_to(&self, peer: NodeId) -> Envelope {
        let next = self.next_index.get(&peer).copied().unwrap_or(1).max(1);
        let previous_index = next - 1;
        let previous_term = self.term_at(previous_index).unwrap_or(0);
        let entries: Vec<Entry> = self.log.iter().skip(previous_index as usize).take(MAX_ENTRIES_PER_APPEND).cloned().collect();
        self.send(peer, Message::Append { term: self.term, previous_index, previous_term, entries, commit: self.commit })
    }

    fn broadcast(&self) -> Vec<Envelope> {
        self.peers.iter().map(|peer| self.append_to(*peer)).collect()
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
            return Err(NotLeader { leader: self.leader });
        }
        let index = self.last_index() + 1;
        self.log.push(Entry { term: self.term, index, command });
        self.advance_commit();
        Ok((index, self.broadcast()))
    }

    /// Handles a message from `from`; returns the messages to send.
    pub fn step(&mut self, from: NodeId, message: Message) -> Vec<Envelope> {
        let message_term = match &message {
            Message::RequestVote { term, .. } | Message::Vote { term, .. } | Message::Append { term, .. } | Message::AppendReply { term, .. } => *term,
        };
        if message_term > self.term {
            let leader = matches!(message, Message::Append { .. }).then_some(from);
            self.become_follower(message_term, leader);
        }
        match message {
            Message::RequestVote { term, last_index, last_term } => {
                let up_to_date = (last_term, last_index) >= (self.last_term(), self.last_index());
                let granted = term == self.term && up_to_date && self.voted_for.is_none_or(|voted| voted == from);
                if granted {
                    self.voted_for = Some(from);
                    self.elapsed = 0;
                }
                vec![self.send(from, Message::Vote { term: self.term, granted })]
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
            Message::Append { term, previous_index, previous_term, entries, commit } => {
                if term < self.term {
                    return vec![self.send(from, Message::AppendReply { term: self.term, success: false, index: self.last_index() })];
                }
                self.become_follower(term, Some(from));
                if self.term_at(previous_index) != Some(previous_term) {
                    let hint = previous_index.saturating_sub(1).min(self.last_index());
                    return vec![self.send(from, Message::AppendReply { term: self.term, success: false, index: hint })];
                }
                let received = entries.len() as u64;
                for entry in entries {
                    match self.term_at(entry.index) {
                        Some(existing) if existing == entry.term => {}
                        Some(_) => {
                            self.log.truncate((entry.index - 1) as usize);
                            self.log.push(entry);
                        }
                        None => self.log.push(entry),
                    }
                }
                let matched = previous_index + received;
                if commit > self.commit {
                    self.commit = commit.min(self.last_index());
                }
                vec![self.send(from, Message::AppendReply { term: self.term, success: true, index: matched })]
            }
            Message::AppendReply { term, success, index } => {
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
        }
    }

    /// The entries committed since the last call, in order, for the state machine to apply
    /// (the leader's empty no-op entries included, with an empty command).
    pub fn take_committed(&mut self) -> Vec<Entry> {
        let from = self.applied as usize;
        let to = (self.commit as usize).min(self.log.len());
        self.applied = to as u64;
        self.log.get(from..to).map(<[Entry]>::to_vec).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests;
