//! How a node's durable state is laid out when stored: the hard state, the snapshot's position
//! and each log entry as bytes, and [`MemoryStorage`], which applies [`Changes`] the way durable
//! storage does, for simulation.

use crate::{Changes, Entry, HardState, NodeId, Recovered, Snapshot};
use std::collections::BTreeMap;

/// The bytes of an encoded [`HardState`]: the term, a vote flag and the vote.
pub const HARD_STATE_BYTES: usize = 17;

/// The bytes of an encoded snapshot position: its index and term.
pub const SNAPSHOT_POSITION_BYTES: usize = 16;

/// The hard state as stored: the term (8 bytes, big-endian), 1 when there is a vote, and the
/// vote (8 bytes).
pub fn encode_hard_state(state: &HardState) -> [u8; HARD_STATE_BYTES] {
    let mut bytes = [0u8; HARD_STATE_BYTES];
    bytes[..8].copy_from_slice(&state.term.to_be_bytes());
    if let Some(vote) = state.voted_for {
        bytes[8] = 1;
        bytes[9..].copy_from_slice(&vote.to_be_bytes());
    }
    bytes
}

/// Reads [`encode_hard_state`]'s bytes; `None` when they do not have its layout.
pub fn decode_hard_state(bytes: &[u8]) -> Option<HardState> {
    let bytes: &[u8; HARD_STATE_BYTES] = bytes.try_into().ok()?;
    let term = u64::from_be_bytes(bytes[..8].try_into().ok()?);
    let voted_for = match bytes[8] {
        0 => None,
        1 => Some(NodeId::from_be_bytes(bytes[9..].try_into().ok()?)),
        _ => return None,
    };
    Some(HardState { term, voted_for })
}

/// A snapshot's index and term as stored, 8 bytes each, big-endian.
pub fn encode_snapshot_position(index: u64, term: u64) -> [u8; SNAPSHOT_POSITION_BYTES] {
    let mut bytes = [0u8; SNAPSHOT_POSITION_BYTES];
    bytes[..8].copy_from_slice(&index.to_be_bytes());
    bytes[8..].copy_from_slice(&term.to_be_bytes());
    bytes
}

/// Reads [`encode_snapshot_position`]'s bytes.
pub fn decode_snapshot_position(bytes: &[u8]) -> Option<(u64, u64)> {
    let bytes: &[u8; SNAPSHOT_POSITION_BYTES] = bytes.try_into().ok()?;
    Some((
        u64::from_be_bytes(bytes[..8].try_into().ok()?),
        u64::from_be_bytes(bytes[8..].try_into().ok()?),
    ))
}

/// An entry's value as stored under its index: its term (8 bytes, big-endian), then its
/// command.
pub fn encode_entry(entry: &Entry) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + entry.command.len());
    bytes.extend_from_slice(&entry.term.to_be_bytes());
    bytes.extend_from_slice(&entry.command);
    bytes
}

/// Reads [`encode_entry`]'s bytes back into the entry at `index`.
pub fn decode_entry(index: u64, bytes: &[u8]) -> Option<Entry> {
    let (term, command) = bytes.split_first_chunk::<8>()?;
    Some(Entry {
        term: u64::from_be_bytes(*term),
        index,
        command: command.to_vec(),
    })
}

/// Durable state kept in memory, changed only by [`MemoryStorage::store`], so a simulated
/// crash loses exactly what a real one would.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryStorage {
    hard_state: HardState,
    snapshot: Option<Snapshot>,
    entries: BTreeMap<u64, Entry>,
}

impl MemoryStorage {
    /// Empty storage.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores what a node reported, in the order durable storage applies it: a snapshot
    /// first, then the rewritten entries, then the hard state.
    pub fn store(&mut self, changes: &Changes) {
        for snapshot in [&changes.installed, &changes.compacted].into_iter().flatten() {
            if self
                .snapshot
                .as_ref()
                .is_none_or(|known| known.index < snapshot.index)
            {
                self.snapshot = Some(snapshot.clone());
            }
            let index = snapshot.index;
            self.entries.retain(|stored, _| *stored > index);
        }
        if let Some(from) = changes.entries_from {
            self.entries.retain(|stored, _| *stored < from);
            for entry in &changes.entries {
                self.entries.insert(entry.index, entry.clone());
            }
        }
        if let Some(hard_state) = changes.hard_state {
            self.hard_state = hard_state;
        }
    }

    /// What a node recovers from this storage.
    pub fn recovered(&self) -> Recovered {
        Recovered {
            hard_state: self.hard_state,
            snapshot: self.snapshot.clone(),
            entries: self.entries.values().cloned().collect(),
        }
    }
}
