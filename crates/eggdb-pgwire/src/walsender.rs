//! The server side of PostgreSQL's streaming replication protocol for logical replication
//! (D1031): `IDENTIFY_SYSTEM`, `CREATE_REPLICATION_SLOT ... LOGICAL pgoutput`,
//! `DROP_REPLICATION_SLOT` and `START_REPLICATION SLOT ... LOGICAL`, streaming the changes the
//! engine captured (D1030) as `pgoutput` protocol version 1 messages in `XLogData`, so a
//! PostgreSQL subscription, Debezium's PostgreSQL connector or `eggdb follow` can follow EggDB.

use crate::replication::{format_lsn, parse_lsn};
use eggdb_sql::logical_decoding::{DecodedChange, DecodedOperation, DecodedTransaction};
use eggdb_sql::{Session, SqlType, Value};
use std::collections::HashSet;

/// Microseconds from the Unix epoch to PostgreSQL's epoch, 2000-01-01.
const POSTGRES_EPOCH_MICROS: i64 = 946_684_800_000_000;
const SYSTEM_IDENTIFIER: &str = "7340012345678901234";
const PLUGIN: &str = "pgoutput";

/// A replication command of a `replication=database` connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplicationCommand {
    IdentifySystem,
    CreateSlot { name: String, export_snapshot: bool },
    DropSlot { name: String },
    StartReplication { slot: String, lsn: u64 },
}

fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in text.trim().trim_end_matches(';').chars() {
        match character {
            '"' => quoted = !quoted,
            ' ' | '\t' | '\n' | '\r' if !quoted => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            '(' if !quoted => break,
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// The replication command `text` is, if it is one.
pub(crate) fn replication_command(text: &str) -> Option<ReplicationCommand> {
    let words = words(text);
    let upper: Vec<String> = words.iter().map(|word| word.to_ascii_uppercase()).collect();
    match upper.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["IDENTIFY_SYSTEM"] => Some(ReplicationCommand::IdentifySystem),
        ["CREATE_REPLICATION_SLOT", _, rest @ ..] if rest.contains(&"LOGICAL") => Some(ReplicationCommand::CreateSlot {
            name: words[1].clone(),
            export_snapshot: !rest.contains(&"NOEXPORT_SNAPSHOT") && !rest.contains(&"USE_SNAPSHOT"),
        }),
        ["DROP_REPLICATION_SLOT", _, ..] => Some(ReplicationCommand::DropSlot { name: words[1].clone() }),
        ["START_REPLICATION", "SLOT", _, "LOGICAL", lsn, ..] => Some(ReplicationCommand::StartReplication {
            slot: words[2].clone(),
            lsn: parse_lsn(lsn).unwrap_or(0),
        }),
        _ => None,
    }
}

/// The type OID PostgreSQL gives a column of `sql_type`, for the relation message.
fn type_oid(sql_type: SqlType) -> u32 {
    crate::types::oid_of(sql_type)
}

fn put_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
    out.push(0);
}

fn relation_message(change: &DecodedChange) -> Vec<u8> {
    let mut out = vec![b'R'];
    out.extend_from_slice(&change.table_id.to_be_bytes());
    put_string(&mut out, &change.schema);
    put_string(&mut out, &change.table);
    out.push(b'd');
    out.extend_from_slice(&(change.columns.len() as u16).to_be_bytes());
    for (index, (name, sql_type)) in change.columns.iter().enumerate() {
        out.push(u8::from(change.key_columns.contains(&index)));
        put_string(&mut out, name);
        out.extend_from_slice(&type_oid(*sql_type).to_be_bytes());
        out.extend_from_slice(&(-1i32).to_be_bytes());
    }
    out
}

fn tuple(out: &mut Vec<u8>, values: &[Value], only: Option<&[usize]>) {
    out.extend_from_slice(&(values.len() as u16).to_be_bytes());
    for (index, value) in values.iter().enumerate() {
        let wanted = only.is_none_or(|columns| columns.contains(&index));
        match value.to_text() {
            Some(text) if wanted => {
                out.push(b't');
                out.extend_from_slice(&(text.len() as i32).to_be_bytes());
                out.extend_from_slice(text.as_bytes());
            }
            _ => out.push(b'n'),
        }
    }
}

fn change_message(change: &DecodedChange) -> Vec<u8> {
    let mut out = Vec::new();
    match change.operation {
        DecodedOperation::Insert => {
            out.push(b'I');
            out.extend_from_slice(&change.table_id.to_be_bytes());
            out.push(b'N');
            tuple(&mut out, change.after.as_deref().unwrap_or_default(), None);
        }
        DecodedOperation::Update => {
            out.push(b'U');
            out.extend_from_slice(&change.table_id.to_be_bytes());
            if let Some(before) = &change.before {
                out.push(b'O');
                tuple(&mut out, before, None);
            }
            out.push(b'N');
            tuple(&mut out, change.after.as_deref().unwrap_or_default(), None);
        }
        DecodedOperation::Delete => {
            out.push(b'D');
            out.extend_from_slice(&change.table_id.to_be_bytes());
            out.push(b'O');
            tuple(&mut out, change.before.as_deref().unwrap_or_default(), None);
        }
    }
    out
}

fn now_postgres_micros() -> i64 {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_micros()).unwrap_or(i64::MAX));
    unix - POSTGRES_EPOCH_MICROS
}

/// The LSN a transaction's commit is known by: its capture sequence, spaced so the begin and
/// commit positions of one transaction stay below the next.
fn commit_lsn(sequence: u64) -> u64 {
    sequence.saturating_mul(16)
}

/// The capture sequence a subscriber's flushed LSN covers.
pub(crate) fn sequence_of(lsn: u64) -> u64 {
    lsn / 16
}

/// One transaction as `pgoutput` messages: begin, a relation message for each table the stream
/// has not described yet, its changes, commit.
pub(crate) fn transaction_messages(transaction: &DecodedTransaction, described: &mut HashSet<u32>) -> Vec<(u64, Vec<u8>)> {
    let lsn = commit_lsn(transaction.lsn);
    let time = now_postgres_micros();
    let mut messages = Vec::with_capacity(transaction.changes.len() + 2);
    let mut begin = vec![b'B'];
    begin.extend_from_slice(&lsn.to_be_bytes());
    begin.extend_from_slice(&time.to_be_bytes());
    begin.extend_from_slice(&((transaction.lsn & 0xFFFF_FFFF) as u32).to_be_bytes());
    messages.push((lsn, begin));
    for change in &transaction.changes {
        if described.insert(change.table_id) {
            messages.push((lsn, relation_message(change)));
        }
        messages.push((lsn, change_message(change)));
    }
    let mut commit = vec![b'C', 0];
    commit.extend_from_slice(&lsn.to_be_bytes());
    commit.extend_from_slice(&(lsn + 1).to_be_bytes());
    commit.extend_from_slice(&time.to_be_bytes());
    messages.push((lsn + 1, commit));
    messages
}

/// `XLogData` carrying one `pgoutput` message.
pub(crate) fn xlog_data(lsn: u64, message: &[u8]) -> Vec<u8> {
    let mut out = vec![b'w'];
    out.extend_from_slice(&lsn.to_be_bytes());
    out.extend_from_slice(&lsn.to_be_bytes());
    out.extend_from_slice(&now_postgres_micros().to_be_bytes());
    out.extend_from_slice(message);
    out
}

/// A primary keepalive message at `lsn`, asking for a reply when `reply`.
pub(crate) fn keepalive(lsn: u64, reply: bool) -> Vec<u8> {
    let mut out = vec![b'k'];
    out.extend_from_slice(&lsn.to_be_bytes());
    out.extend_from_slice(&now_postgres_micros().to_be_bytes());
    out.push(u8::from(reply));
    out
}

/// The flushed LSN of a standby status update (`r`), if the copy data is one.
pub(crate) fn flushed_lsn(data: &[u8]) -> Option<u64> {
    if data.first() != Some(&b'r') {
        return None;
    }
    Some(u64::from_be_bytes(data.get(9..17)?.try_into().ok()?))
}

/// The row `IDENTIFY_SYSTEM` answers with: system id, timeline, current position, database.
pub(crate) fn identify_row(session: &mut Session<'_>, database: &str) -> Vec<Option<String>> {
    let lsn = session.current_capture_lsn().map(commit_lsn).unwrap_or(0);
    vec![Some(SYSTEM_IDENTIFIER.to_owned()), Some("1".to_owned()), Some(format_lsn(lsn)), Some(database.to_owned())]
}

/// The row `CREATE_REPLICATION_SLOT` answers with: slot name, consistent point, snapshot name
/// and output plugin.
pub(crate) fn slot_row(name: &str, lsn: u64, snapshot: Option<String>) -> Vec<Option<String>> {
    vec![Some(name.to_owned()), Some(format_lsn(commit_lsn(lsn))), snapshot, Some(PLUGIN.to_owned())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replication_commands_read_as_subscribers_send_them() {
        assert_eq!(replication_command("IDENTIFY_SYSTEM"), Some(ReplicationCommand::IdentifySystem));
        assert_eq!(
            replication_command("CREATE_REPLICATION_SLOT \"follow_1\" LOGICAL pgoutput EXPORT_SNAPSHOT"),
            Some(ReplicationCommand::CreateSlot { name: "follow_1".into(), export_snapshot: true })
        );
        assert_eq!(
            replication_command("START_REPLICATION SLOT \"s\" LOGICAL 0/10 (proto_version '1', publication_names '\"p\"')"),
            Some(ReplicationCommand::StartReplication { slot: "s".into(), lsn: 16 })
        );
        assert_eq!(replication_command("SELECT 1"), None);
        assert_eq!(flushed_lsn(&[b'r', 0, 0, 0, 0, 0, 0, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0x30]), Some(0x30));
    }
}
