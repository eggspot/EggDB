use crate::log::{self, error};
use crate::producers::{self, INVALID_PRODUCER_EPOCH, INVALID_TXN_STATE};
use crate::wire::{Parsed, Reader, Writer};
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};

const TRANSACTION: u8 = 6;
const OPEN_PARTITION: u8 = 7;
const ABORTED: u8 = 8;
const INVALID_PRODUCER_ID_MAPPING: i16 = 49;
const MOST_PARTITIONS: usize = 10_000;
const UNKNOWN_FIRST_OFFSET: i64 = -1;

/// A pending offset commit of a transaction: group, topic, partition, offset and metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingOffset {
    group: String,
    topic: String,
    partition: i32,
    offset: i64,
    metadata: String,
}

/// What the coordinator keeps for one `transactional.id`: its producer id and epoch, and the
/// partitions and group offsets of the transaction now open.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Transaction {
    producer_id: i64,
    epoch: i16,
    partitions: Vec<(String, i32)>,
    offsets: Vec<PendingOffset>,
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}

struct Cursor<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, count: usize) -> Option<&[u8]> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len())?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Some(taken)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn i16(&mut self) -> Option<i16> {
        Some(i16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn text(&mut self) -> Option<String> {
        let length = self.u32()? as usize;
        String::from_utf8(self.take(length)?.to_vec()).ok()
    }
}

impl Transaction {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.producer_id.to_be_bytes());
        out.extend_from_slice(&self.epoch.to_be_bytes());
        out.extend_from_slice(&(self.partitions.len() as u32).to_be_bytes());
        for (topic, partition) in &self.partitions {
            put_text(&mut out, topic);
            out.extend_from_slice(&partition.to_be_bytes());
        }
        out.extend_from_slice(&(self.offsets.len() as u32).to_be_bytes());
        for pending in &self.offsets {
            put_text(&mut out, &pending.group);
            put_text(&mut out, &pending.topic);
            out.extend_from_slice(&pending.partition.to_be_bytes());
            out.extend_from_slice(&pending.offset.to_be_bytes());
            put_text(&mut out, &pending.metadata);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut cursor = Cursor { bytes, at: 0 };
        let producer_id = cursor.i64()?;
        let epoch = cursor.i16()?;
        let mut partitions = Vec::new();
        for _ in 0..cursor.u32()?.min(MOST_PARTITIONS as u32) {
            partitions.push((cursor.text()?, cursor.i32()?));
        }
        let mut offsets = Vec::new();
        for _ in 0..cursor.u32()?.min(MOST_PARTITIONS as u32) {
            offsets.push(PendingOffset {
                group: cursor.text()?,
                topic: cursor.text()?,
                partition: cursor.i32()?,
                offset: cursor.i64()?,
                metadata: cursor.text()?,
            });
        }
        Some(Self { producer_id, epoch, partitions, offsets })
    }
}

fn transaction_key(transactional_id: &str) -> Vec<u8> {
    let mut key = vec![TRANSACTION];
    key.extend_from_slice(transactional_id.as_bytes());
    key
}

fn partition_key(prefix: u8, topic: &str, partition: i32) -> Vec<u8> {
    let mut key = vec![prefix];
    key.extend_from_slice(topic.as_bytes());
    key.push(0);
    key.extend_from_slice(&partition.to_be_bytes());
    key
}

fn open_key(topic: &str, partition: i32, producer_id: i64) -> Vec<u8> {
    let mut key = partition_key(OPEN_PARTITION, topic, partition);
    key.extend_from_slice(&producer_id.to_be_bytes());
    key
}

fn aborted_key(topic: &str, partition: i32, first_offset: i64) -> Vec<u8> {
    let mut key = partition_key(ABORTED, topic, partition);
    key.extend_from_slice(&(first_offset as u64).to_be_bytes());
    key
}

fn stored(keys: &dyn KeysRead, transactional_id: &str) -> Result<Option<Transaction>, String> {
    Ok(keys.get(&transaction_key(transactional_id))?.and_then(|raw| Transaction::decode(&raw)))
}

/// Whether `producer_id` has added `topic`'s `partition` to its open transaction, so its
/// transactional batches may be written there.
pub(crate) fn partition_open(keys: &dyn KeysRead, topic: &str, partition: i32, producer_id: i64) -> Result<bool, String> {
    Ok(keys.get(&open_key(topic, partition, producer_id))?.is_some())
}

/// Notes the offset of a transaction's first batch in a partition, for the last stable offset.
pub(crate) fn note_first_offset(keys: &mut dyn KeysWrite, topic: &str, partition: i32, producer_id: i64, offset: i64) -> Result<(), String> {
    let key = open_key(topic, partition, producer_id);
    let known = keys
        .get(&key)?
        .and_then(|raw| raw.get(..8).and_then(|raw| raw.try_into().ok()))
        .map_or(UNKNOWN_FIRST_OFFSET, i64::from_be_bytes);
    if known == UNKNOWN_FIRST_OFFSET {
        keys.put(&key, &offset.to_be_bytes())?;
    }
    Ok(())
}

/// The last stable offset of a partition: the first offset of its oldest open transaction, or
/// `high` when none is open.
pub(crate) fn last_stable_offset(keys: &dyn KeysRead, topic: &str, partition: i32, high: i64) -> Result<i64, String> {
    Ok(keys
        .scan(&partition_key(OPEN_PARTITION, topic, partition))?
        .iter()
        .filter_map(|(_, value)| value.get(..8).and_then(|raw| raw.try_into().ok()).map(i64::from_be_bytes))
        .filter(|first| *first >= 0)
        .min()
        .unwrap_or(high)
        .min(high))
}

/// The aborted transactions of a partition that touch offsets from `from` up to `below`:
/// `(producer id, first offset)` pairs, as a `read_committed` fetch reports them.
pub(crate) fn aborted_between(keys: &dyn KeysRead, topic: &str, partition: i32, from: i64, below: i64) -> Result<Vec<(i64, i64)>, String> {
    let prefix = partition_key(ABORTED, topic, partition);
    let mut found = Vec::new();
    for (key, value) in keys.scan(&prefix)? {
        let Some(first) = key.get(prefix.len()..prefix.len() + 8).and_then(|raw| raw.try_into().ok()).map(u64::from_be_bytes) else {
            continue;
        };
        let first = first as i64;
        let (Some(producer), Some(last)) = (
            value.get(..8).and_then(|raw| raw.try_into().ok()).map(i64::from_be_bytes),
            value.get(8..16).and_then(|raw| raw.try_into().ok()).map(i64::from_be_bytes),
        ) else {
            continue;
        };
        if first < below && last >= from {
            found.push((producer, first));
        }
    }
    Ok(found)
}

fn finish(keys: &mut dyn KeysWrite, transaction: &mut Transaction, commit: bool) -> Result<(), String> {
    for (topic, partition) in std::mem::take(&mut transaction.partitions) {
        let key = open_key(&topic, partition, transaction.producer_id);
        let first = keys
            .get(&key)?
            .and_then(|raw| raw.get(..8).and_then(|raw| raw.try_into().ok()))
            .map_or(UNKNOWN_FIRST_OFFSET, i64::from_be_bytes);
        let marker = log::append_marker(keys, &topic, partition, transaction.producer_id, transaction.epoch, commit)?;
        if !commit && first >= 0 {
            let mut value = transaction.producer_id.to_be_bytes().to_vec();
            value.extend_from_slice(&marker.to_be_bytes());
            keys.put(&aborted_key(&topic, partition, first), &value)?;
        }
        keys.delete(&key)?;
    }
    let offsets = std::mem::take(&mut transaction.offsets);
    if commit {
        for pending in offsets {
            let mut value = pending.offset.to_be_bytes().to_vec();
            value.extend_from_slice(pending.metadata.as_bytes());
            keys.put(&crate::groups::offset_key(&pending.group, &pending.topic, pending.partition), &value)?;
        }
    }
    Ok(())
}

/// `InitProducerId` with a `transactional.id`: the same producer id with its epoch raised (a new
/// id once the epoch runs out), aborting the transaction an earlier instance left open.
pub(crate) fn init(keyspace: &dyn Keyspace, transactional_id: &str) -> Result<(i64, i16), String> {
    log::write(keyspace, |keys| {
        let mut transaction = match stored(&*keys, transactional_id)? {
            Some(mut found) => {
                finish(keys, &mut found, false)?;
                if found.epoch == i16::MAX {
                    found.producer_id = producers::next_producer_id(keys)?;
                    found.epoch = 0;
                } else {
                    found.epoch += 1;
                }
                found
            }
            None => Transaction {
                producer_id: producers::next_producer_id(keys)?,
                epoch: 0,
                ..Transaction::default()
            },
        };
        transaction.partitions.clear();
        keys.put(&transaction_key(transactional_id), &transaction.encode())?;
        Ok((transaction.producer_id, transaction.epoch))
    })
}

fn checked(found: Option<&Transaction>, producer_id: i64, epoch: i16) -> i16 {
    match found {
        None => INVALID_PRODUCER_ID_MAPPING,
        Some(transaction) if transaction.producer_id != producer_id => INVALID_PRODUCER_ID_MAPPING,
        Some(transaction) if transaction.epoch != epoch => INVALID_PRODUCER_EPOCH,
        Some(_) => error::NONE,
    }
}

/// `AddPartitionsToTxn` (versions 0 and 1).
pub(crate) fn add_partitions(keyspace: &dyn Keyspace, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
    let transactional_id = request.string()?.unwrap_or_default();
    let producer_id = request.i64()?;
    let epoch = request.i16()?;
    let topics = request.array(|reader| Ok((reader.string()?.unwrap_or_default(), reader.array(Reader::i32)?)))?;
    let mut existing = Vec::new();
    for (topic, partitions) in &topics {
        let count = log::partitions(keyspace, topic, None)?.unwrap_or(0);
        existing.push(partitions.iter().map(|partition| (0..count).contains(partition)).collect::<Vec<_>>());
    }
    let codes = log::write(keyspace, |keys| {
        let mut found = stored(&*keys, &transactional_id)?;
        let check = checked(found.as_ref(), producer_id, epoch);
        let mut codes = Vec::new();
        for ((topic, partitions), exists) in topics.iter().zip(&existing) {
            let mut per = Vec::new();
            for (partition, exists) in partitions.iter().zip(exists) {
                let code = if check != error::NONE {
                    check
                } else if !exists {
                    error::UNKNOWN_TOPIC_OR_PARTITION
                } else if let Some(transaction) = found.as_mut() {
                    if !transaction.partitions.iter().any(|(known, index)| known == topic && index == partition) {
                        if transaction.partitions.len() >= MOST_PARTITIONS {
                            return Err("a transaction may hold at most 10,000 partitions".to_owned());
                        }
                        transaction.partitions.push((topic.clone(), *partition));
                        keys.put(&open_key(topic, *partition, producer_id), &UNKNOWN_FIRST_OFFSET.to_be_bytes())?;
                    }
                    error::NONE
                } else {
                    INVALID_PRODUCER_ID_MAPPING
                };
                per.push((*partition, code));
            }
            codes.push((topic.clone(), per));
        }
        if let Some(transaction) = &found {
            keys.put(&transaction_key(&transactional_id), &transaction.encode())?;
        }
        Ok(codes)
    })?;
    body.i32(0).count(codes.len());
    for (topic, partitions) in codes {
        body.string(Some(&topic)).count(partitions.len());
        for (partition, code) in partitions {
            body.i32(partition).i16(code);
        }
    }
    Ok(())
}

/// `AddOffsetsToTxn` (versions 0 and 1): the group's offsets will be committed with the
/// transaction through `TxnOffsetCommit`.
pub(crate) fn add_offsets(keyspace: &dyn Keyspace, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
    let transactional_id = request.string()?.unwrap_or_default();
    let producer_id = request.i64()?;
    let epoch = request.i16()?;
    let _group = request.string()?;
    let code = log::read(keyspace, |keys| Ok(checked(stored(keys, &transactional_id)?.as_ref(), producer_id, epoch)))?;
    body.i32(0).i16(code);
    Ok(())
}

/// `TxnOffsetCommit` (versions 0 to 2): offsets held until the transaction commits.
pub(crate) fn offset_commit(keyspace: &dyn Keyspace, version: i16, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
    let transactional_id = request.string()?.unwrap_or_default();
    let group = request.string()?.unwrap_or_default();
    let producer_id = request.i64()?;
    let epoch = request.i16()?;
    let topics = request.array(|reader| {
        let topic = reader.string()?.unwrap_or_default();
        let partitions = reader.array(|reader| {
            let partition = reader.i32()?;
            let offset = reader.i64()?;
            if version >= 2 {
                let _leader_epoch = reader.i32()?;
            }
            let metadata = reader.string()?.unwrap_or_default();
            Ok((partition, offset, metadata))
        })?;
        Ok((topic, partitions))
    })?;
    let code = log::write(keyspace, |keys| {
        let mut found = stored(&*keys, &transactional_id)?;
        let check = checked(found.as_ref(), producer_id, epoch);
        if let (error::NONE, Some(transaction)) = (check, found.as_mut()) {
            for (topic, partitions) in &topics {
                for (partition, offset, metadata) in partitions {
                    transaction.offsets.retain(|pending| {
                        !(pending.group == group && pending.topic == *topic && pending.partition == *partition)
                    });
                    transaction.offsets.push(PendingOffset {
                        group: group.clone(),
                        topic: topic.clone(),
                        partition: *partition,
                        offset: *offset,
                        metadata: metadata.clone(),
                    });
                }
            }
            keys.put(&transaction_key(&transactional_id), &transaction.encode())?;
        }
        Ok(check)
    })?;
    body.i32(0).count(topics.len());
    for (topic, partitions) in &topics {
        body.string(Some(topic)).count(partitions.len());
        for (partition, _, _) in partitions {
            body.i32(*partition).i16(code);
        }
    }
    Ok(())
}

/// `EndTxn` (versions 0 and 1): writes the commit or abort marker to every partition of the
/// transaction, records an aborted one for `read_committed` readers, and applies its offsets
/// on commit.
pub(crate) fn end(keyspace: &dyn Keyspace, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
    let transactional_id = request.string()?.unwrap_or_default();
    let producer_id = request.i64()?;
    let epoch = request.i16()?;
    let commit = request.i8()? != 0;
    let code = log::write(keyspace, |keys| {
        let mut found = stored(&*keys, &transactional_id)?;
        let check = checked(found.as_ref(), producer_id, epoch);
        let Some(transaction) = found.as_mut().filter(|_| check == error::NONE) else {
            return Ok(check);
        };
        if transaction.partitions.is_empty() && transaction.offsets.is_empty() {
            return Ok(INVALID_TXN_STATE);
        }
        finish(keys, transaction, commit)?;
        keys.put(&transaction_key(&transactional_id), &transaction.encode())?;
        Ok(error::NONE)
    })?;
    body.i32(0).i16(code);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::MemoryKeyspace;

    #[test]
    fn transaction_state_round_trips_and_init_bumps_the_epoch() {
        let state = Transaction {
            producer_id: 7,
            epoch: 2,
            partitions: vec![("orders".to_owned(), 1)],
            offsets: vec![PendingOffset {
                group: "billing".to_owned(),
                topic: "orders".to_owned(),
                partition: 1,
                offset: 40,
                metadata: String::new(),
            }],
        };
        assert_eq!(Transaction::decode(&state.encode()), Some(state));
        let keyspace = MemoryKeyspace::default();
        let (id, epoch) = init(&keyspace, "payments").unwrap();
        assert_eq!(init(&keyspace, "payments").unwrap(), (id, epoch + 1));
        assert_ne!(init(&keyspace, "other").unwrap().0, id);
    }

    #[test]
    fn open_transactions_hold_the_last_stable_offset_and_aborts_are_listed() {
        let keyspace = MemoryKeyspace::default();
        log::write(&keyspace, |keys| {
            keys.put(&open_key("t", 0, 3), &UNKNOWN_FIRST_OFFSET.to_be_bytes())?;
            note_first_offset(keys, "t", 0, 3, 10)?;
            note_first_offset(keys, "t", 0, 3, 12)?;
            let mut value = 9i64.to_be_bytes().to_vec();
            value.extend_from_slice(&5i64.to_be_bytes());
            keys.put(&aborted_key("t", 0, 2), &value)
        })
        .unwrap();
        log::read(&keyspace, |keys| {
            assert_eq!(last_stable_offset(keys, "t", 0, 20)?, 10);
            assert_eq!(last_stable_offset(keys, "t", 1, 20)?, 20);
            assert_eq!(aborted_between(keys, "t", 0, 0, 10)?, vec![(9, 2)]);
            assert!(aborted_between(keys, "t", 0, 6, 10)?.is_empty());
            Ok(())
        })
        .unwrap();
    }
}
