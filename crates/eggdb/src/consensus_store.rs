use crate::scan::Scan;
use crate::transaction::WriteTransaction;
use crate::{Database, Error, ErrorKind, Result};
use eggdb_consensus::storage::{
    decode_entry, decode_hard_state, decode_snapshot_position, encode_entry, encode_hard_state,
    encode_snapshot_position,
};
use eggdb_consensus::{Changes, HardState, Recovered, Snapshot};

const HARD_STATE_KEY: &[u8] = b"\xffCOL\x00\x7fraft\x00state";
const SNAPSHOT_KEY: &[u8] = b"\xffCOL\x00\x7fraft\x00snapshot";
const LOG_PREFIX: &[u8] = b"\xffCOL\x00\x7fraft\x00log\x00";

fn log_key(index: u64) -> Vec<u8> {
    let mut key = LOG_PREFIX.to_vec();
    key.extend_from_slice(&index.to_be_bytes());
    key
}

fn log_end() -> Vec<u8> {
    let mut end = LOG_PREFIX.to_vec();
    end.push(0xFF);
    end
}

fn broken(what: &str) -> Error {
    Error::new(
        ErrorKind::Corrupt,
        format!("the consensus {what} stored in the file does not have its layout"),
    )
}

fn index_of(key: &[u8]) -> Option<u64> {
    key.get(LOG_PREFIX.len()..)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_be_bytes)
}

fn delete_log_range(
    transaction: &mut WriteTransaction<'_>,
    range: std::ops::Range<Vec<u8>>,
) -> Result<()> {
    let keys: Vec<Vec<u8>> =
        Scan::within_between(transaction.reader(), transaction.tree(), b"", &range)
            .map(|item| item.map(|(key, _)| key))
            .collect::<Result<_>>()?;
    for key in keys {
        transaction.delete_raw(&key)?;
    }
    Ok(())
}

impl Database {
    /// The Raft state this file keeps for its node of a cluster (D211, D1036): the term and
    /// vote, the position of the last snapshot (the database itself is the snapshot's state,
    /// so its data is empty) and the log entries after it, for
    /// [`eggdb_consensus::Node::recover`].
    pub fn consensus_state(&self) -> Result<Recovered> {
        let read = self.begin_read()?;
        let hard_state = match read.get_raw(HARD_STATE_KEY)? {
            Some(bytes) => decode_hard_state(&bytes).ok_or_else(|| broken("term and vote"))?,
            None => HardState::default(),
        };
        let snapshot = match read.get_raw(SNAPSHOT_KEY)? {
            Some(bytes) => {
                let (index, term) =
                    decode_snapshot_position(&bytes).ok_or_else(|| broken("snapshot position"))?;
                Some(Snapshot {
                    index,
                    term,
                    data: Vec::new(),
                })
            }
            None => None,
        };
        let mut entries = Vec::new();
        for item in Scan::within_between(
            read.reader(),
            read.tree(),
            b"",
            &(LOG_PREFIX.to_vec()..log_end()),
        ) {
            let (key, value) = item?;
            let index = index_of(&key).ok_or_else(|| broken("log key"))?;
            entries.push(decode_entry(index, &value).ok_or_else(|| broken("log entry"))?);
        }
        Ok(Recovered {
            hard_state,
            snapshot,
            entries,
        })
    }

    /// Stores what a Raft node reported ([`eggdb_consensus::Node::take_changes`]) in one
    /// durable write transaction: the newest snapshot's position with the entries it covers
    /// dropped, the rewritten entries, then the term and vote. The node sends the messages it
    /// returned only after this succeeds.
    pub fn save_consensus(&self, changes: &Changes) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let mut transaction = self.begin_write_unfenced(self.write_timeout())?;
        let newest = [&changes.installed, &changes.compacted]
            .into_iter()
            .flatten()
            .max_by_key(|snapshot| snapshot.index);
        if let Some(snapshot) = newest {
            transaction.put_raw(
                SNAPSHOT_KEY,
                &encode_snapshot_position(snapshot.index, snapshot.term),
            )?;
            delete_log_range(
                &mut transaction,
                LOG_PREFIX.to_vec()..log_key(snapshot.index.saturating_add(1)),
            )?;
        }
        if let Some(from) = changes.entries_from {
            delete_log_range(&mut transaction, log_key(from)..log_end())?;
            for entry in &changes.entries {
                transaction.put_raw(&log_key(entry.index), &encode_entry(entry))?;
            }
        }
        if let Some(hard_state) = changes.hard_state {
            transaction.put_raw(HARD_STATE_KEY, &encode_hard_state(&hard_state))?;
        }
        transaction.commit()?;
        Ok(())
    }
}
