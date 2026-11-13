//! Logical decoding of the engine's capture records (D1031): replication slots kept in the
//! file, the capture position as a WAL-style LSN, and each captured transaction decoded into
//! row changes with their table's columns, for a PostgreSQL walsender to send as `pgoutput`.

use crate::cdc::CAPTURE_PREFIX;
use crate::encoding::{TableDefinition, decode_row, prefix_end};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::store::{KeyValueRead, ScanDirection};
use crate::types::{SqlType, Value};

pub(crate) const SLOT_PREFIX: &[u8] = b"\xffCOL\x00\x7fslot\x00";
const CDC_POSITION_KEY: &[u8] = b"\xffCOL\x00\x7fcdc-position";
const MOST_SLOT_NAME_BYTES: usize = 63;

/// What a decoded change did to its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodedOperation {
    /// The row is new.
    Insert,
    /// The row changed.
    Update,
    /// The row is gone.
    Delete,
}

/// One row change of a captured transaction, with what a subscriber needs to apply it.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedChange {
    /// The table's id, stable while the table exists.
    pub table_id: u32,
    /// The table's schema.
    pub schema: String,
    /// The table's name in its schema.
    pub table: String,
    /// Its columns: name and type.
    pub columns: Vec<(String, SqlType)>,
    /// The positions of its primary key's columns.
    pub key_columns: Vec<usize>,
    /// What happened.
    pub operation: DecodedOperation,
    /// The row before, for an update or a delete.
    pub before: Option<Vec<Value>>,
    /// The row after, for an insert or an update.
    pub after: Option<Vec<Value>>,
}

/// One captured transaction: its position and its row changes in commit order.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedTransaction {
    /// Its capture sequence, used as its LSN.
    pub lsn: u64,
    /// Its row changes.
    pub changes: Vec<DecodedChange>,
}

pub(crate) fn slot_key(name: &str) -> Vec<u8> {
    let mut key = SLOT_PREFIX.to_vec();
    key.extend_from_slice(name.as_bytes());
    key
}

pub(crate) fn valid_slot_name(name: &str) -> SqlResult<()> {
    let fine = !name.is_empty()
        && name.len() <= MOST_SLOT_NAME_BYTES
        && name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    if fine {
        Ok(())
    } else {
        Err(SqlError::new(
            SqlState::INVALID_NAME,
            format!("replication slot name \"{name}\" contains invalid character"),
        )
        .with_hint("Replication slot names may only contain lower case letters, numbers, and the underscore character."))
    }
}

pub(crate) fn number(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?))
}

fn capture_sequence(key: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(key.get(CAPTURE_PREFIX.len()..)?.try_into().ok()?))
}

/// The LSN of the last captured transaction `reader` sees, 0 before the first.
pub fn capture_lsn(reader: &dyn KeyValueRead) -> SqlResult<u64> {
    let mut last = 0;
    reader.scan(CAPTURE_PREFIX, &prefix_end(CAPTURE_PREFIX), ScanDirection::Backward, &mut |key, _| {
        last = capture_sequence(key).unwrap_or(0);
        Ok(false)
    })?;
    Ok(last)
}

fn slots(reader: &dyn KeyValueRead) -> SqlResult<Vec<(String, u64)>> {
    let mut found = Vec::new();
    reader.scan(SLOT_PREFIX, &prefix_end(SLOT_PREFIX), ScanDirection::Forward, &mut |key, value| {
        found.push((String::from_utf8_lossy(&key[SLOT_PREFIX.len()..]).into_owned(), number(value).unwrap_or(0)));
        Ok(true)
    })?;
    Ok(found)
}

/// The capture records every consumer has passed: the change data capture deliverer, when it
/// ran, and every replication slot. `None` while there is no consumer.
pub(crate) fn passed_by_all(reader: &dyn KeyValueRead) -> SqlResult<Option<u64>> {
    let mut least: Option<u64> = reader.get(CDC_POSITION_KEY)?.as_deref().and_then(number);
    for (_, confirmed) in slots(reader)? {
        least = Some(least.map_or(confirmed, |known| known.min(confirmed)));
    }
    Ok(least)
}

pub(crate) fn cdc_position(reader: &dyn KeyValueRead) -> SqlResult<u64> {
    Ok(reader.get(CDC_POSITION_KEY)?.as_deref().and_then(number).unwrap_or(0))
}

pub(crate) fn set_cdc_position(write: &mut dyn crate::store::BoxedWrite, sequence: u64) -> SqlResult<()> {
    write.put(CDC_POSITION_KEY, &sequence.to_le_bytes())
}

/// Removes the capture records every consumer has passed, at most `limit` of them.
pub(crate) fn prune(write: &mut dyn crate::store::BoxedWrite, limit: usize) -> SqlResult<usize> {
    let read: &dyn KeyValueRead = &*write;
    let Some(passed) = passed_by_all(read)? else {
        return Ok(0);
    };
    let mut stale = Vec::new();
    read.scan(CAPTURE_PREFIX, &prefix_end(CAPTURE_PREFIX), ScanDirection::Forward, &mut |key, _| {
        if capture_sequence(key).is_none_or(|sequence| sequence > passed) || stale.len() >= limit {
            return Ok(false);
        }
        stale.push(key.to_vec());
        Ok(true)
    })?;
    for key in &stale {
        write.delete(key)?;
    }
    Ok(stale.len())
}

fn decoded(definition: &TableDefinition, stored: Option<&[u8]>) -> SqlResult<Option<Vec<Value>>> {
    stored.map(|bytes| decode_row(bytes, &definition.columns)).transpose()
}

/// The captured transactions after `after`, at most `limit`, decoded with the tables' current
/// definitions; changes to tables that no longer exist are left out.
pub fn decode_after(reader: &dyn KeyValueRead, after: u64, limit: usize) -> SqlResult<Vec<DecodedTransaction>> {
    let mut start = CAPTURE_PREFIX.to_vec();
    start.extend_from_slice(&after.saturating_add(1).to_be_bytes());
    let mut records = Vec::new();
    reader.scan(&start, &prefix_end(CAPTURE_PREFIX), ScanDirection::Forward, &mut |key, value| {
        records.push((capture_sequence(key).unwrap_or(0), value.to_vec()));
        Ok(records.len() < limit)
    })?;
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let tables = crate::cdc::table_definitions(reader)?;
    let id_at = crate::encoding::NAMESPACE.len() + 1;
    let mut transactions = Vec::with_capacity(records.len());
    for (lsn, value) in records {
        let keys = crate::cdc::decode_capture_record(&value).ok_or_else(|| crate::encoding::corrupt("captured changes"))?;
        let mut changes = Vec::new();
        for (key, before, after) in keys {
            let Some(id) = key.get(id_at..id_at + 4).and_then(|bytes| bytes.try_into().ok()).map(u32::from_be_bytes) else {
                continue;
            };
            let Some(definition) = tables.get(&id) else {
                continue;
            };
            let schema = crate::schemas::schema_of(&definition.name);
            let schema = if schema.is_empty() { crate::schemas::DEFAULT_SCHEMA } else { schema };
            let operation = match (&before, &after) {
                (None, Some(_)) => DecodedOperation::Insert,
                (Some(_), Some(_)) => DecodedOperation::Update,
                (Some(_), None) => DecodedOperation::Delete,
                (None, None) => continue,
            };
            changes.push(DecodedChange {
                table_id: id,
                schema: schema.to_owned(),
                table: crate::schemas::base_name(&definition.name).to_owned(),
                columns: definition.columns.iter().map(|column| (column.name.clone(), column.sql_type)).collect(),
                key_columns: definition.primary_key.clone(),
                operation,
                before: decoded(definition, before.as_deref())?,
                after: decoded(definition, after.as_deref())?,
            });
        }
        transactions.push(DecodedTransaction { lsn, changes });
    }
    Ok(transactions)
}
