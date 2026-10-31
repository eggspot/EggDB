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
    pub(crate) const UNKNOWN_SERVER_ERROR: i16 = -1;
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

pub(crate) fn valid_topic(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_TOPIC_NAME && name != "." && name != ".." && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn read<T>(keyspace: &dyn Keyspace, body: impl FnOnce(&dyn KeysRead) -> Result<T, String>) -> Result<T, String> {
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

fn write<T>(keyspace: &dyn Keyspace, body: impl FnOnce(&mut dyn KeysWrite) -> Result<T, String>) -> Result<T, String> {
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
pub(crate) fn partitions(keyspace: &dyn Keyspace, topic: &str, create: Option<i32>) -> Result<Option<i32>, String> {
    let existing = read(keyspace, |keys| Ok(keys.get(&topic_key(topic))?.and_then(|value| Some(i32::from_be_bytes(value.get(..4)?.try_into().ok()?)))))?;
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
        base + i64::from(i32::from_be_bytes(batch.get(23..27).and_then(|raw| raw.try_into().ok()).unwrap_or_default())) + 1
    }))
}

/// Appends the record batches in `records` to a partition, assigning offsets; returns the base
/// offset of the first, or a Kafka error code.
pub(crate) fn append(keyspace: &dyn Keyspace, topic: &str, partition: i32, records: &[u8]) -> Result<Result<i64, i16>, String> {
    let mut batches = Vec::new();
    let mut at = 0;
    while at < records.len() {
        let Some(header) = records.get(at..at + BATCH_HEADER) else {
            return Ok(Err(error::CORRUPT_MESSAGE));
        };
        let length = i32::from_be_bytes(header[8..12].try_into().unwrap_or_default());
        let Some(end) = usize::try_from(length).ok().and_then(|length| (at + 12).checked_add(length)).filter(|end| *end <= records.len() && *end >= at + BATCH_HEADER) else {
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
        if keys.get(&topic_key(topic))?.is_none() {
            return Ok(Err(error::UNKNOWN_TOPIC_OR_PARTITION));
        }
        let mut next = next_offset(&*keys, topic, partition)?;
        let first = next;
        for mut batch in batches {
            batch[..8].copy_from_slice(&next.to_be_bytes());
            let last_delta = i32::from_be_bytes(batch[23..27].try_into().unwrap_or_default());
            keys.put(&batch_key(topic, partition, next), &batch)?;
            next += i64::from(last_delta) + 1;
        }
        Ok(Ok(first))
    })
}

/// The earliest and the next offset of a partition.
pub(crate) fn offsets(keyspace: &dyn Keyspace, topic: &str, partition: i32) -> Result<(i64, i64), String> {
    read(keyspace, |keys| {
        let batches = keys.scan(&partition_prefix(topic, partition))?;
        let earliest = batches.first().map_or(0, |(key, _)| u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap_or_default()) as i64);
        Ok((earliest, next_offset(keys, topic, partition)?))
    })
}

/// The stored batches holding offsets from `from`, up to about `max_bytes`; and the next
/// offset (the high watermark).
pub(crate) fn fetch(keyspace: &dyn Keyspace, topic: &str, partition: i32, from: i64, max_bytes: usize) -> Result<(Vec<u8>, i64), String> {
    read(keyspace, |keys| {
        let high = next_offset(keys, topic, partition)?;
        let mut out = Vec::new();
        for (key, batch) in keys.scan(&partition_prefix(topic, partition))? {
            let base = u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap_or_default()) as i64;
            let last_delta = i64::from(i32::from_be_bytes(batch.get(23..27).and_then(|raw| raw.try_into().ok()).unwrap_or_default()));
            if base + last_delta < from {
                continue;
            }
            if !out.is_empty() && out.len() + batch.len() > max_bytes {
                break;
            }
            out.extend_from_slice(&batch);
        }
        Ok((out, high))
    })
}
