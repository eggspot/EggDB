use crate::wire::crc32c;
use eggdb_keyspace::KeysWrite;
use std::collections::HashMap;

const RECORDS_COUNT_AT: usize = 57;
const BATCH_HEADER: usize = 61;
const COMPRESSION_MASK: u16 = 0x07;
const CONTROL_FLAG: u16 = 0x20;

/// How long a compacted topic keeps a key's tombstone (a record with a null value) before
/// compaction drops it: Kafka's default `delete.retention.ms`, one day.
pub(crate) const TOMBSTONE_RETENTION_MS: i64 = 24 * 60 * 60 * 1000;

/// Compaction runs on a partition each time its next offset passes a multiple of this.
pub(crate) const COMPACT_EVERY: i64 = 1000;

struct Record<'b> {
    offset_delta: i64,
    key: Option<&'b [u8]>,
    null_value: bool,
    framed: &'b [u8],
}

struct Cursor<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Cursor<'b> {
    fn varint(&mut self) -> Option<i64> {
        let mut value: u64 = 0;
        for shift in (0..64).step_by(7) {
            let byte = *self.bytes.get(self.at)?;
            self.at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(((value >> 1) as i64) ^ -((value & 1) as i64));
            }
        }
        None
    }

    fn take(&mut self, length: usize) -> Option<&'b [u8]> {
        let taken = self.bytes.get(self.at..self.at.checked_add(length)?)?;
        self.at += length;
        Some(taken)
    }

    fn sized(&mut self) -> Option<Option<&'b [u8]>> {
        let length = self.varint()?;
        if length < 0 {
            return Some(None);
        }
        self.take(usize::try_from(length).ok()?).map(Some)
    }
}

fn attributes(batch: &[u8]) -> u16 {
    batch
        .get(21..23)
        .and_then(|raw| raw.try_into().ok())
        .map_or(0, u16::from_be_bytes)
}

fn records(batch: &[u8]) -> Option<Vec<Record<'_>>> {
    let flags = attributes(batch);
    if flags & COMPRESSION_MASK != 0 || flags & CONTROL_FLAG != 0 {
        return None;
    }
    let count = i32::from_be_bytes(batch.get(RECORDS_COUNT_AT..BATCH_HEADER)?.try_into().ok()?);
    let mut cursor = Cursor {
        bytes: batch,
        at: BATCH_HEADER,
    };
    let mut found = Vec::with_capacity(usize::try_from(count).ok()?.min(100_000));
    for _ in 0..count {
        let start = cursor.at;
        let length = usize::try_from(cursor.varint()?).ok()?;
        let body_start = cursor.at;
        let body = cursor.take(length)?;
        let mut inner = Cursor { bytes: body, at: 0 };
        inner.take(1)?;
        inner.varint()?;
        let offset_delta = inner.varint()?;
        let key = inner.sized()?;
        let value = inner.sized()?;
        found.push(Record {
            offset_delta,
            key,
            null_value: value.is_none(),
            framed: &batch[start..body_start + length],
        });
    }
    Some(found)
}

fn rebuilt(batch: &[u8], kept: &[&Record<'_>]) -> Vec<u8> {
    let mut out = batch[..BATCH_HEADER].to_vec();
    out[RECORDS_COUNT_AT..BATCH_HEADER].copy_from_slice(&(kept.len() as i32).to_be_bytes());
    for record in kept {
        out.extend_from_slice(record.framed);
    }
    let length = (out.len() - 12) as i32;
    out[8..12].copy_from_slice(&length.to_be_bytes());
    let crc = crc32c(&out[21..]);
    out[17..21].copy_from_slice(&crc.to_be_bytes());
    out
}

fn max_timestamp(batch: &[u8]) -> i64 {
    batch
        .get(35..43)
        .and_then(|raw| raw.try_into().ok())
        .map_or(i64::MAX, i64::from_be_bytes)
}

/// Compacts the partition whose batches are under `prefix`, as Kafka's log cleaner does: every
/// batch but the newest keeps only the records that are the latest for their key, and drops
/// tombstones older than [`TOMBSTONE_RETENTION_MS`]. Records keep their offsets; a batch left
/// with no record is removed; compressed and control batches are left as they are.
pub(crate) fn compact(keys: &mut dyn KeysWrite, prefix: &[u8], now_ms: i64) -> Result<(), String> {
    let batches = keys.scan(prefix)?;
    if batches.len() < 2 {
        return Ok(());
    }
    let base_of = |key: &[u8]| {
        key.get(key.len().saturating_sub(8)..)
            .and_then(|raw| raw.try_into().ok())
            .map_or(0, |raw| u64::from_be_bytes(raw) as i64)
    };
    let mut latest: HashMap<Vec<u8>, i64> = HashMap::new();
    for (key, batch) in &batches {
        let base = base_of(key);
        for record in records(batch).unwrap_or_default() {
            if let Some(record_key) = record.key {
                latest.insert(record_key.to_vec(), base + record.offset_delta);
            }
        }
    }
    let last = batches.len() - 1;
    for (key, batch) in &batches[..last] {
        let Some(found) = records(batch) else {
            continue;
        };
        let base = base_of(key);
        let tombstones_expired = max_timestamp(batch) < now_ms.saturating_sub(TOMBSTONE_RETENTION_MS);
        let kept: Vec<&Record<'_>> = found
            .iter()
            .filter(|record| match record.key {
                None => true,
                Some(record_key) => {
                    latest.get(record_key) == Some(&(base + record.offset_delta))
                        && !(record.null_value && tombstones_expired)
                }
            })
            .collect();
        if kept.len() == found.len() {
            continue;
        }
        if kept.is_empty() {
            keys.delete(key)?;
        } else {
            keys.put(key, &rebuilt(batch, &kept))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::{Keyspace, MemoryKeyspace};

    fn varint(out: &mut Vec<u8>, value: i64) {
        let mut zigzag = ((value << 1) ^ (value >> 63)) as u64;
        while zigzag >= 0x80 {
            out.push((zigzag as u8) | 0x80);
            zigzag >>= 7;
        }
        out.push(zigzag as u8);
    }

    fn batch(base: i64, records: &[(&str, Option<&str>)], timestamp: i64) -> Vec<u8> {
        let mut body = Vec::new();
        for (delta, (key, value)) in records.iter().enumerate() {
            let mut record = vec![0u8];
            varint(&mut record, 0);
            varint(&mut record, delta as i64);
            varint(&mut record, key.len() as i64);
            record.extend_from_slice(key.as_bytes());
            match value {
                Some(value) => {
                    varint(&mut record, value.len() as i64);
                    record.extend_from_slice(value.as_bytes());
                }
                None => varint(&mut record, -1),
            }
            varint(&mut record, 0);
            varint(&mut body, record.len() as i64);
            body.extend(record);
        }
        let mut out = base.to_be_bytes().to_vec();
        out.extend_from_slice(&0i32.to_be_bytes());
        out.extend_from_slice(&0i32.to_be_bytes());
        out.push(2);
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0i16.to_be_bytes());
        out.extend_from_slice(&((records.len() - 1) as i32).to_be_bytes());
        out.extend_from_slice(&timestamp.to_be_bytes());
        out.extend_from_slice(&timestamp.to_be_bytes());
        out.extend_from_slice(&(-1i64).to_be_bytes());
        out.extend_from_slice(&(-1i16).to_be_bytes());
        out.extend_from_slice(&(-1i32).to_be_bytes());
        out.extend_from_slice(&(records.len() as i32).to_be_bytes());
        out.extend(body);
        let length = (out.len() - 12) as i32;
        out[8..12].copy_from_slice(&length.to_be_bytes());
        let crc = crc32c(&out[21..]);
        out[17..21].copy_from_slice(&crc.to_be_bytes());
        out
    }

    fn key(base: i64) -> Vec<u8> {
        let mut key = b"p".to_vec();
        key.extend_from_slice(&(base as u64).to_be_bytes());
        key
    }

    #[test]
    fn older_batches_keep_only_each_keys_latest_record_and_fresh_tombstones() {
        let keyspace = MemoryKeyspace::default();
        let now = 10 * TOMBSTONE_RETENTION_MS;
        keyspace
            .write(0, &mut |keys| {
                keys.put(&key(0), &batch(0, &[("a", Some("1")), ("b", Some("1"))], now))?;
                keys.put(&key(2), &batch(2, &[("a", Some("2")), ("c", None)], 0))?;
                keys.put(&key(4), &batch(4, &[("b", None)], now))?;
                compact(keys, b"p", now)
            })
            .unwrap();
        let mut left = Vec::new();
        keyspace
            .read(0, &mut |keys| {
                left = keys.scan(b"p")?;
                Ok(())
            })
            .unwrap();
        assert_eq!(left.len(), 2, "the first batch held only overwritten keys");
        let second = records(&left[0].1).unwrap();
        assert_eq!(second.len(), 1, "the old tombstone for c is dropped");
        assert_eq!(second[0].key, Some(&b"a"[..]));
        let rebuilt_crc = u32::from_be_bytes(left[0].1[17..21].try_into().unwrap());
        assert_eq!(rebuilt_crc, crc32c(&left[0].1[21..]));
        assert_eq!(records(&left[1].1).unwrap().len(), 1, "the newest batch is not compacted");
    }
}
