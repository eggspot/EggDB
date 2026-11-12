use crate::producers::{self, Accepted, BatchProducer, PartitionProducers};
use crate::wire::crc32c;
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};

const TOPIC: u8 = 1;
const BATCH: u8 = 2;
const MAX_TOPIC_NAME: usize = 249;
const BATCH_HEADER: usize = 61;

/// Kafka's error codes, as its protocol guide numbers them.
pub(crate) mod error {
    pub(crate) const NONE: i16 = 0;
    pub(crate) const OFFSET_OUT_OF_RANGE: i16 = 1;
    pub(crate) const CORRUPT_MESSAGE: i16 = 2;
    pub(crate) const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
    pub(crate) const INVALID_TOPIC: i16 = 17;
    pub(crate) const TOPIC_ALREADY_EXISTS: i16 = 36;
    pub(crate) const INVALID_PARTITIONS: i16 = 37;
    pub(crate) const INVALID_REPLICATION_FACTOR: i16 = 38;
}

const MAX_PARTITIONS: i32 = 1024;

/// How long and how much a topic's partitions keep: `retention.ms` and `retention.bytes`,
/// `-1` for no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Retention {
    pub(crate) ms: i64,
    pub(crate) bytes: i64,
    pub(crate) compact: bool,
}

impl Retention {
    pub(crate) const UNLIMITED: Self = Self {
        ms: -1,
        bytes: -1,
        compact: false,
    };
}

fn retention_of(value: &[u8]) -> Retention {
    let field = |range: std::ops::Range<usize>| {
        value
            .get(range)
            .and_then(|raw| raw.try_into().ok())
            .map(i64::from_be_bytes)
    };
    match (field(4..12), field(12..20)) {
        (Some(ms), Some(bytes)) => Retention {
            ms,
            bytes,
            compact: value.get(20) == Some(&1),
        },
        _ => Retention::UNLIMITED,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

fn enforce_retention(
    keys: &mut dyn KeysWrite,
    topic: &str,
    partition: i32,
    retention: Retention,
) -> Result<(), String> {
    if retention.ms < 0 && retention.bytes < 0 {
        return Ok(());
    }
    let batches = keys.scan(&partition_prefix(topic, partition))?;
    let mut total: i64 = batches.iter().map(|(_, batch)| batch.len() as i64).sum();
    let oldest_kept = now_ms().saturating_sub(retention.ms);
    let last = batches.len().saturating_sub(1);
    for (at, (key, batch)) in batches.iter().enumerate() {
        if at == last {
            break;
        }
        let newest = batch
            .get(35..43)
            .and_then(|raw| raw.try_into().ok())
            .map_or(i64::MAX, i64::from_be_bytes);
        let too_old = retention.ms >= 0 && newest < oldest_kept;
        let too_big = retention.bytes >= 0 && total > retention.bytes;
        if !too_old && !too_big {
            break;
        }
        keys.delete(key)?;
        total -= batch.len() as i64;
    }
    Ok(())
}

/// Creates `topic` with `partitions` (`-1` for one) and `retention`; returns a Kafka error
/// code. With `validate_only` nothing is written.
pub(crate) fn create_topic(
    keyspace: &dyn Keyspace,
    topic: &str,
    partitions: i32,
    retention: Retention,
    validate_only: bool,
) -> Result<i16, String> {
    if !valid_topic(topic) {
        return Ok(error::INVALID_TOPIC);
    }
    let partitions = if partitions == -1 { 1 } else { partitions };
    if !(1..=MAX_PARTITIONS).contains(&partitions) {
        return Ok(error::INVALID_PARTITIONS);
    }
    write(keyspace, |keys| {
        if keys.get(&topic_key(topic))?.is_some() {
            return Ok(error::TOPIC_ALREADY_EXISTS);
        }
        if !validate_only {
            let mut value = partitions.to_be_bytes().to_vec();
            value.extend_from_slice(&retention.ms.to_be_bytes());
            value.extend_from_slice(&retention.bytes.to_be_bytes());
            value.push(u8::from(retention.compact));
            keys.put(&topic_key(topic), &value)?;
        }
        Ok(error::NONE)
    })
}

/// Deletes `topic` and every record of its partitions; returns a Kafka error code.
pub(crate) fn delete_topic(keyspace: &dyn Keyspace, topic: &str) -> Result<i16, String> {
    write(keyspace, |keys| {
        let Some(value) = keys.get(&topic_key(topic))? else {
            return Ok(error::UNKNOWN_TOPIC_OR_PARTITION);
        };
        let partitions = value
            .get(..4)
            .and_then(|raw| raw.try_into().ok())
            .map_or(1, i32::from_be_bytes);
        for partition in 0..partitions.max(1) {
            for (key, _) in keys.scan(&partition_prefix(topic, partition))? {
                keys.delete(&key)?;
            }
        }
        producers::delete_topic_states(keys, topic)?;
        keys.delete(&topic_key(topic))?;
        Ok(error::NONE)
    })
}

fn topic_key(topic: &str) -> Vec<u8> {
    let mut key = vec![TOPIC];
    key.extend_from_slice(topic.as_bytes());
    key
}

fn partition_prefix(topic: &str, partition: i32) -> Vec<u8> {
    let mut key = vec![BATCH];
    key.extend_from_slice(topic.as_bytes());
    key.push(0);
    key.extend_from_slice(&partition.to_be_bytes());
    key
}

fn batch_key(topic: &str, partition: i32, base_offset: i64) -> Vec<u8> {
    let mut key = partition_prefix(topic, partition);
    key.extend_from_slice(&(base_offset as u64).to_be_bytes());
    key
}

/// Hands out a new idempotent producer id with epoch 0.
pub(crate) fn init_producer(keyspace: &dyn Keyspace) -> Result<i64, String> {
    write(keyspace, producers::next_producer_id)
}

pub(crate) fn valid_topic(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TOPIC_NAME
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

pub(crate) fn read<T>(
    keyspace: &dyn Keyspace,
    body: impl FnOnce(&dyn KeysRead) -> Result<T, String>,
) -> Result<T, String> {
    let mut body = Some(body);
    let mut out = None;
    keyspace.read(0, &mut |keys| {
        if let Some(body) = body.take() {
            out = Some(body(keys)?);
        }
        Ok(())
    })?;
    out.ok_or_else(|| "the read did not run".to_owned())
}

pub(crate) fn write<T>(
    keyspace: &dyn Keyspace,
    body: impl FnOnce(&mut dyn KeysWrite) -> Result<T, String>,
) -> Result<T, String> {
    let mut body = Some(body);
    let mut out = None;
    keyspace.write(0, &mut |keys| {
        if let Some(body) = body.take() {
            out = Some(body(keys)?);
        }
        Ok(())
    })?;
    out.ok_or_else(|| "the write did not run".to_owned())
}

/// The partitions of every topic.
pub(crate) fn topics(keyspace: &dyn Keyspace) -> Result<Vec<(String, i32)>, String> {
    read(keyspace, |keys| {
        Ok(keys
            .scan(&[TOPIC])?
            .into_iter()
            .filter_map(|(key, value)| {
                let partitions = i32::from_be_bytes(value.get(..4)?.try_into().ok()?);
                Some((String::from_utf8(key[1..].to_vec()).ok()?, partitions))
            })
            .collect())
    })
}

/// The number of partitions of `topic`, creating it with `partitions` when it is missing and
/// `create` is set.
pub(crate) fn partitions(
    keyspace: &dyn Keyspace,
    topic: &str,
    create: Option<i32>,
) -> Result<Option<i32>, String> {
    let existing = read(keyspace, |keys| {
        Ok(keys
            .get(&topic_key(topic))?
            .and_then(|value| Some(i32::from_be_bytes(value.get(..4)?.try_into().ok()?))))
    })?;
    match (existing, create) {
        (Some(count), _) => Ok(Some(count)),
        (None, Some(count)) if valid_topic(topic) => write(keyspace, |keys| {
            if keys.get(&topic_key(topic))?.is_none() {
                keys.put(&topic_key(topic), &count.to_be_bytes())?;
            }
            Ok(Some(count))
        }),
        _ => Ok(None),
    }
}

fn next_offset(keys: &dyn KeysRead, topic: &str, partition: i32) -> Result<i64, String> {
    let batches = keys.scan(&partition_prefix(topic, partition))?;
    Ok(batches.last().map_or(0, |(key, batch)| {
        let base = u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap_or_default()) as i64;
        base + i64::from(i32::from_be_bytes(
            batch
                .get(23..27)
                .and_then(|raw| raw.try_into().ok())
                .unwrap_or_default(),
        )) + 1
    }))
}

fn put_varint(out: &mut Vec<u8>, value: i64) {
    let mut zigzag = ((value << 1) ^ (value >> 63)) as u64;
    loop {
        let byte = (zigzag & 0x7f) as u8;
        zigzag >>= 7;
        if zigzag == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A record batch (magic 2) holding one record with no key, `value` and no headers, stamped now.
pub(crate) fn single_record_batch(value: &[u8]) -> Vec<u8> {
    let mut record = vec![0u8];
    put_varint(&mut record, 0);
    put_varint(&mut record, 0);
    put_varint(&mut record, -1);
    put_varint(&mut record, value.len() as i64);
    record.extend_from_slice(value);
    put_varint(&mut record, 0);
    let mut records = Vec::with_capacity(record.len() + 5);
    put_varint(&mut records, record.len() as i64);
    records.extend(record);
    let now = now_ms();
    let mut tail = Vec::with_capacity(records.len() + 40);
    tail.extend_from_slice(&0i16.to_be_bytes());
    tail.extend_from_slice(&0i32.to_be_bytes());
    tail.extend_from_slice(&now.to_be_bytes());
    tail.extend_from_slice(&now.to_be_bytes());
    tail.extend_from_slice(&(-1i64).to_be_bytes());
    tail.extend_from_slice(&(-1i16).to_be_bytes());
    tail.extend_from_slice(&(-1i32).to_be_bytes());
    tail.extend_from_slice(&1i32.to_be_bytes());
    tail.extend(records);
    let mut out = Vec::with_capacity(tail.len() + 21);
    out.extend_from_slice(&0i64.to_be_bytes());
    out.extend_from_slice(&((4 + 1 + 4 + tail.len()) as i32).to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.push(2);
    out.extend_from_slice(&crc32c(&tail).to_be_bytes());
    out.extend(tail);
    out
}

const CONTROL_ATTRIBUTES: i16 = 0x20 | 0x10;
const ABORT_MARKER: i16 = 0;
const COMMIT_MARKER: i16 = 1;

/// A control batch holding one commit or abort marker of `producer_id`'s transaction.
fn control_batch(producer_id: i64, epoch: i16, commit: bool) -> Vec<u8> {
    let mut key = 0i16.to_be_bytes().to_vec();
    key.extend_from_slice(&(if commit { COMMIT_MARKER } else { ABORT_MARKER }).to_be_bytes());
    let mut value = 0i16.to_be_bytes().to_vec();
    value.extend_from_slice(&0i32.to_be_bytes());
    let mut record = vec![0u8];
    put_varint(&mut record, 0);
    put_varint(&mut record, 0);
    put_varint(&mut record, key.len() as i64);
    record.extend_from_slice(&key);
    put_varint(&mut record, value.len() as i64);
    record.extend_from_slice(&value);
    put_varint(&mut record, 0);
    let mut records = Vec::with_capacity(record.len() + 5);
    put_varint(&mut records, record.len() as i64);
    records.extend(record);
    let now = now_ms();
    let mut tail = Vec::with_capacity(records.len() + 40);
    tail.extend_from_slice(&CONTROL_ATTRIBUTES.to_be_bytes());
    tail.extend_from_slice(&0i32.to_be_bytes());
    tail.extend_from_slice(&now.to_be_bytes());
    tail.extend_from_slice(&now.to_be_bytes());
    tail.extend_from_slice(&producer_id.to_be_bytes());
    tail.extend_from_slice(&epoch.to_be_bytes());
    tail.extend_from_slice(&(-1i32).to_be_bytes());
    tail.extend_from_slice(&1i32.to_be_bytes());
    tail.extend(records);
    let mut out = Vec::with_capacity(tail.len() + 21);
    out.extend_from_slice(&0i64.to_be_bytes());
    out.extend_from_slice(&((4 + 1 + 4 + tail.len()) as i32).to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.push(2);
    out.extend_from_slice(&crc32c(&tail).to_be_bytes());
    out.extend(tail);
    out
}

/// Writes a transaction's commit or abort marker to a partition, inside the caller's write;
/// returns the marker's offset.
pub(crate) fn append_marker(
    keys: &mut dyn KeysWrite,
    topic: &str,
    partition: i32,
    producer_id: i64,
    epoch: i16,
    commit: bool,
) -> Result<i64, String> {
    let offset = next_offset(&*keys, topic, partition)?;
    let mut batch = control_batch(producer_id, epoch, commit);
    batch[..8].copy_from_slice(&offset.to_be_bytes());
    keys.put(&batch_key(topic, partition, offset), &batch)?;
    Ok(offset)
}

/// Appends the record batches in `records` to a partition, assigning offsets; returns the base
/// offset of the first, or a Kafka error code.
pub(crate) fn append(
    keyspace: &dyn Keyspace,
    topic: &str,
    partition: i32,
    records: &[u8],
) -> Result<Result<i64, i16>, String> {
    let mut batches = Vec::new();
    let mut at = 0;
    while at < records.len() {
        let Some(header) = records.get(at..at + BATCH_HEADER) else {
            return Ok(Err(error::CORRUPT_MESSAGE));
        };
        let length = i32::from_be_bytes(header[8..12].try_into().unwrap_or_default());
        let Some(end) = usize::try_from(length)
            .ok()
            .and_then(|length| (at + 12).checked_add(length))
            .filter(|end| *end <= records.len() && *end >= at + BATCH_HEADER)
        else {
            return Ok(Err(error::CORRUPT_MESSAGE));
        };
        let batch = &records[at..end];
        let magic = batch[16];
        let crc = u32::from_be_bytes(batch[17..21].try_into().unwrap_or_default());
        let last_delta = i32::from_be_bytes(batch[23..27].try_into().unwrap_or_default());
        if magic != 2 || crc32c(&batch[21..]) != crc || last_delta < 0 {
            return Ok(Err(error::CORRUPT_MESSAGE));
        }
        batches.push(batch.to_vec());
        at = end;
    }
    write(keyspace, |keys| {
        let Some(described) = keys.get(&topic_key(topic))? else {
            return Ok(Err(error::UNKNOWN_TOPIC_OR_PARTITION));
        };
        let retention = retention_of(&described);
        let mut next = next_offset(&*keys, topic, partition)?;
        let started = next;
        let mut first = None;
        let mut producers = PartitionProducers::read(&*keys, topic, partition)?;
        let mut accepted = Vec::with_capacity(batches.len());
        let mut transactional_starts = Vec::new();
        for batch in batches {
            let producer = BatchProducer::of(&batch);
            if let Some(producer) = producer.filter(|producer| producer.transactional)
                && !crate::transactions::partition_open(&*keys, topic, partition, producer.id)?
            {
                return Ok(Err(producers::INVALID_TXN_STATE));
            }
            if let Some(producer) = &producer {
                match producers.check(&*keys, producer)? {
                    Ok(Accepted::Append) => {}
                    Ok(Accepted::Duplicate(offset)) => {
                        first.get_or_insert(offset);
                        continue;
                    }
                    Err(code) => return Ok(Err(code)),
                }
            }
            let last_delta = i32::from_be_bytes(batch[23..27].try_into().unwrap_or_default());
            if let Some(producer) = &producer {
                producers.accept(producer, next);
                if producer.transactional {
                    transactional_starts.push((producer.id, next));
                }
            }
            first.get_or_insert(next);
            accepted.push((next, batch));
            next += i64::from(last_delta) + 1;
        }
        for (base, mut batch) in accepted {
            batch[..8].copy_from_slice(&base.to_be_bytes());
            keys.put(&batch_key(topic, partition, base), &batch)?;
        }
        producers.store(keys)?;
        for (producer_id, offset) in transactional_starts {
            crate::transactions::note_first_offset(keys, topic, partition, producer_id, offset)?;
        }
        enforce_retention(keys, topic, partition, retention)?;
        let every = crate::compaction::COMPACT_EVERY;
        if retention.compact && started / every != next / every {
            crate::compaction::compact(keys, &partition_prefix(topic, partition), now_ms())?;
        }
        Ok(Ok(first.unwrap_or(next)))
    })
}

/// The earliest and the next offset of a partition.
pub(crate) fn offsets(
    keyspace: &dyn Keyspace,
    topic: &str,
    partition: i32,
) -> Result<(i64, i64), String> {
    read(keyspace, |keys| {
        let batches = keys.scan(&partition_prefix(topic, partition))?;
        let earliest = batches.first().map_or(0, |(key, _)| {
            u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap_or_default()) as i64
        });
        Ok((earliest, next_offset(keys, topic, partition)?))
    })
}

/// What a fetch of one partition returns: its batches, the high watermark, the last stable
/// offset, and, for `read_committed`, the aborted transactions among the batches.
pub(crate) struct Fetched {
    pub(crate) records: Vec<u8>,
    pub(crate) high: i64,
    pub(crate) last_stable: i64,
    pub(crate) aborted: Option<Vec<(i64, i64)>>,
}

/// The stored batches holding offsets from `from`, up to about `max_bytes`; with
/// `read_committed`, only those below the last stable offset, with the aborted transactions
/// among them.
pub(crate) fn fetch(
    keyspace: &dyn Keyspace,
    topic: &str,
    partition: i32,
    from: i64,
    max_bytes: usize,
    read_committed: bool,
) -> Result<Fetched, String> {
    read(keyspace, |keys| {
        let high = next_offset(keys, topic, partition)?;
        let last_stable = crate::transactions::last_stable_offset(keys, topic, partition, high)?;
        let below = if read_committed { last_stable } else { high };
        let mut out = Vec::new();
        for (key, batch) in keys.scan(&partition_prefix(topic, partition))? {
            let base =
                u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap_or_default()) as i64;
            if base >= below {
                break;
            }
            let last_delta = i64::from(i32::from_be_bytes(
                batch
                    .get(23..27)
                    .and_then(|raw| raw.try_into().ok())
                    .unwrap_or_default(),
            ));
            if base + last_delta < from {
                continue;
            }
            if !out.is_empty() && out.len() + batch.len() > max_bytes {
                break;
            }
            out.extend_from_slice(&batch);
        }
        let aborted = if read_committed {
            Some(crate::transactions::aborted_between(keys, topic, partition, from, below)?)
        } else {
            None
        };
        Ok(Fetched {
            records: out,
            high,
            last_stable,
            aborted,
        })
    })
}
