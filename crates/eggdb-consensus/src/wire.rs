//! Raft messages as bytes between nodes: a kind byte, then fixed-width big-endian numbers,
//! with entries, commands, snapshot data and configurations length-prefixed. Decoding checks
//! every length against the bytes left, so a hostile peer gets an error, never a panic or a
//! large allocation.

use crate::{Entry, EntryKind, Membership, Message, Snapshot};

const PRE_VOTE: u8 = 1;
const PRE_VOTE_REPLY: u8 = 2;
const REQUEST_VOTE: u8 = 3;
const VOTE: u8 = 4;
const APPEND: u8 = 5;
const APPEND_REPLY: u8 = 6;
const INSTALL_SNAPSHOT: u8 = 7;

/// Why bytes are not a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

impl std::fmt::Display for Malformed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a consensus message does not have its layout")
    }
}

impl std::error::Error for Malformed {}

fn put(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_bytes(bytes: &mut Vec<u8>, value: &[u8]) {
    put(bytes, value.len() as u64);
    bytes.extend_from_slice(value);
}

/// `message` as bytes.
pub fn encode_message(message: &Message) -> Vec<u8> {
    let mut bytes = Vec::new();
    match message {
        Message::PreVote {
            term,
            last_index,
            last_term,
        }
        | Message::RequestVote {
            term,
            last_index,
            last_term,
        } => {
            bytes.push(if matches!(message, Message::PreVote { .. }) {
                PRE_VOTE
            } else {
                REQUEST_VOTE
            });
            put(&mut bytes, *term);
            put(&mut bytes, *last_index);
            put(&mut bytes, *last_term);
        }
        Message::PreVoteReply { term, granted } | Message::Vote { term, granted } => {
            bytes.push(if matches!(message, Message::PreVoteReply { .. }) {
                PRE_VOTE_REPLY
            } else {
                VOTE
            });
            put(&mut bytes, *term);
            bytes.push(u8::from(*granted));
        }
        Message::Append {
            term,
            previous_index,
            previous_term,
            entries,
            commit,
            round,
        } => {
            bytes.push(APPEND);
            for value in [*term, *previous_index, *previous_term, *commit, *round] {
                put(&mut bytes, value);
            }
            put(&mut bytes, entries.len() as u64);
            for entry in entries {
                put(&mut bytes, entry.term);
                put(&mut bytes, entry.index);
                bytes.push(match entry.kind {
                    EntryKind::Command => 0,
                    EntryKind::Membership => 1,
                });
                put_bytes(&mut bytes, &entry.command);
            }
        }
        Message::AppendReply {
            term,
            success,
            index,
            round,
        } => {
            bytes.push(APPEND_REPLY);
            put(&mut bytes, *term);
            bytes.push(u8::from(*success));
            put(&mut bytes, *index);
            put(&mut bytes, *round);
        }
        Message::InstallSnapshot {
            term,
            snapshot,
            round,
        } => {
            bytes.push(INSTALL_SNAPSHOT);
            put(&mut bytes, *term);
            put(&mut bytes, *round);
            put(&mut bytes, snapshot.index);
            put(&mut bytes, snapshot.term);
            put_bytes(&mut bytes, &snapshot.membership.encode());
            put_bytes(&mut bytes, &snapshot.data);
        }
    }
    bytes
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], Malformed> {
        let (taken, rest) = self.bytes.split_at_checked(count).ok_or(Malformed)?;
        self.bytes = rest;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8, Malformed> {
        Ok(self.take(1)?[0])
    }

    fn flag(&mut self) -> Result<bool, Malformed> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Malformed),
        }
    }

    fn number(&mut self) -> Result<u64, Malformed> {
        let bytes: [u8; 8] = self.take(8)?.try_into().map_err(|_| Malformed)?;
        Ok(u64::from_be_bytes(bytes))
    }

    fn length(&mut self, smallest_item: usize) -> Result<usize, Malformed> {
        let length = usize::try_from(self.number()?).map_err(|_| Malformed)?;
        if length > self.bytes.len() / smallest_item.max(1) {
            return Err(Malformed);
        }
        Ok(length)
    }

    fn bytes(&mut self) -> Result<Vec<u8>, Malformed> {
        let length = self.length(1)?;
        Ok(self.take(length)?.to_vec())
    }
}

const SMALLEST_ENTRY: usize = 8 + 8 + 1 + 8;

/// Reads [`encode_message`]'s bytes.
pub fn decode_message(bytes: &[u8]) -> Result<Message, Malformed> {
    let mut reader = Reader { bytes };
    let message = match reader.byte()? {
        kind @ (PRE_VOTE | REQUEST_VOTE) => {
            let term = reader.number()?;
            let last_index = reader.number()?;
            let last_term = reader.number()?;
            if kind == PRE_VOTE {
                Message::PreVote {
                    term,
                    last_index,
                    last_term,
                }
            } else {
                Message::RequestVote {
                    term,
                    last_index,
                    last_term,
                }
            }
        }
        kind @ (PRE_VOTE_REPLY | VOTE) => {
            let term = reader.number()?;
            let granted = reader.flag()?;
            if kind == PRE_VOTE_REPLY {
                Message::PreVoteReply { term, granted }
            } else {
                Message::Vote { term, granted }
            }
        }
        APPEND => {
            let term = reader.number()?;
            let previous_index = reader.number()?;
            let previous_term = reader.number()?;
            let commit = reader.number()?;
            let round = reader.number()?;
            let count = reader.length(SMALLEST_ENTRY)?;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let term = reader.number()?;
                let index = reader.number()?;
                let kind = match reader.byte()? {
                    0 => EntryKind::Command,
                    1 => EntryKind::Membership,
                    _ => return Err(Malformed),
                };
                let command = reader.bytes()?;
                entries.push(Entry {
                    term,
                    index,
                    kind,
                    command,
                });
            }
            Message::Append {
                term,
                previous_index,
                previous_term,
                entries,
                commit,
                round,
            }
        }
        APPEND_REPLY => Message::AppendReply {
            term: reader.number()?,
            success: reader.flag()?,
            index: reader.number()?,
            round: reader.number()?,
        },
        INSTALL_SNAPSHOT => {
            let term = reader.number()?;
            let round = reader.number()?;
            let index = reader.number()?;
            let snapshot_term = reader.number()?;
            let membership = Membership::decode(&reader.bytes()?).ok_or(Malformed)?;
            let data = reader.bytes()?;
            Message::InstallSnapshot {
                term,
                snapshot: Snapshot {
                    index,
                    term: snapshot_term,
                    membership,
                    data,
                },
                round,
            }
        }
        _ => return Err(Malformed),
    };
    if reader.bytes.is_empty() {
        Ok(message)
    } else {
        Err(Malformed)
    }
}
