//! The server side of PostgreSQL's streaming replication protocol for logical replication
//! (D1031): `IDENTIFY_SYSTEM`, `CREATE_REPLICATION_SLOT ... LOGICAL pgoutput`,
//! `DROP_REPLICATION_SLOT` and `START_REPLICATION SLOT ... LOGICAL`, streaming the changes the
//! engine captured (D1030) as `pgoutput` protocol version 1 messages in `XLogData`, so a
//! PostgreSQL subscription, Debezium's PostgreSQL connector or `eggdb follow` can follow EggDB.

use crate::replication::{format_lsn, parse_lsn};
use eggdb_sql::logical_decoding::{DecodedChange, DecodedOperation, DecodedTransaction};
use eggdb_sql::{Session, SqlType, TimeZone, Value};
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
    StartReplication { slot: String, lsn: u64, options: Vec<(String, String)> },
}

/// The highest `pgoutput` protocol version a subscriber may ask for. Versions 2 to 4 add
/// streamed, two-phase and parallel transactions, which a server may always send whole, so
/// every version gets version 1's messages (D1243).
pub(crate) const MOST_PROTOCOL_VERSION: u32 = 4;

/// The options in parentheses after `START_REPLICATION ... LOGICAL <lsn>`, as name and value
/// with their quotes taken off.
fn replication_options(text: &str) -> Vec<(String, String)> {
    let Some(open) = text.find('(') else {
        return Vec::new();
    };
    let inside = text[open + 1..].trim_end().trim_end_matches(';').trim_end().trim_end_matches(')');
    let mut options = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut pieces = Vec::new();
    for character in inside.chars() {
        match character {
            '\'' => {
                quoted = !quoted;
                current.push(character);
            }
            ',' if !quoted => pieces.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    pieces.push(current);
    for piece in pieces {
        let piece = piece.trim();
        let (name, value) = piece.split_once(char::is_whitespace).unwrap_or((piece, ""));
        if name.is_empty() {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('\'')
            .and_then(|rest| rest.strip_suffix('\''))
            .map_or_else(|| value.to_owned(), |inner| inner.replace("''", "'"));
        options.push((name.trim_matches('"').to_ascii_lowercase(), value));
    }
    options
}

/// The publications `options` name in `publication_names` (a comma-separated list of
/// identifiers, double-quoted or not).
///
/// # Errors
///
/// No `publication_names` option, as PostgreSQL refuses it.
pub(crate) fn publication_names(options: &[(String, String)]) -> Result<Vec<String>, String> {
    let written = options
        .iter()
        .find(|(name, _)| name == "publication_names")
        .map(|(_, value)| value.as_str())
        .ok_or_else(|| "publication_names parameter missing".to_owned())?;
    let names: Vec<String> = written
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| match name.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) {
            Some(quoted) => quoted.replace("\"\"", "\""),
            None => name.to_ascii_lowercase(),
        })
        .collect();
    if names.is_empty() {
        return Err("publication_names parameter missing".to_owned());
    }
    Ok(names)
}

/// `transaction` with only the changes `filter` publishes; `None` when none is left, since
/// an empty transaction is not sent.
pub(crate) fn published(transaction: &DecodedTransaction, filter: &eggdb_sql::PublicationFilter) -> Option<DecodedTransaction> {
    let changes: Vec<_> = transaction
        .changes
        .iter()
        .filter(|change| filter.publishes(change))
        .map(|change| filter.narrowed(change))
        .collect();
    (!changes.is_empty()).then(|| DecodedTransaction {
        lsn: transaction.lsn,
        changes,
    })
}

/// The `pgoutput` protocol version `options` ask for.
///
/// # Errors
///
/// A missing, unreadable or unsupported `proto_version`, with PostgreSQL's messages.
pub(crate) fn protocol_version(options: &[(String, String)]) -> Result<u32, String> {
    let written = options
        .iter()
        .find(|(name, _)| name == "proto_version")
        .map(|(_, value)| value.as_str())
        .ok_or_else(|| "proto_version option missing".to_owned())?;
    let version: u32 = written
        .trim()
        .parse()
        .map_err(|_| format!("invalid proto_version: \"{written}\""))?;
    if version == 0 || version > MOST_PROTOCOL_VERSION {
        return Err(format!(
            "client sent proto_version={version} but server only supports protocol {MOST_PROTOCOL_VERSION} or lower"
        ));
    }
    Ok(version)
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
            options: replication_options(text),
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

/// Whether `START_REPLICATION`'s options ask for `binary` tuples (D1263).
pub(crate) fn binary(options: &[(String, String)]) -> bool {
    options
        .iter()
        .any(|(key, value)| key == "binary" && matches!(value.to_ascii_lowercase().as_str(), "true" | "on" | "1" | "yes"))
}

fn binary_column(value: &Value, sql_type: SqlType) -> Option<Vec<u8>> {
    eggdb_sql::encode_pg_binary(value, type_oid(sql_type), TimeZone::UTC).ok().flatten()
}

fn tuple(out: &mut Vec<u8>, values: &[Value], only: Option<&[usize]>, binary_types: Option<&[(String, SqlType)]>) {
    out.extend_from_slice(&(values.len() as u16).to_be_bytes());
    for (index, value) in values.iter().enumerate() {
        let wanted = only.is_none_or(|columns| columns.contains(&index));
        let binary = binary_types
            .filter(|_| wanted && !matches!(value, Value::Null))
            .and_then(|columns| columns.get(index))
            .and_then(|(_, sql_type)| binary_column(value, *sql_type));
        if let Some(bytes) = binary {
            out.push(b'b');
            out.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
            out.extend_from_slice(&bytes);
            continue;
        }
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

fn change_message(change: &DecodedChange, binary: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let types = binary.then_some(change.columns.as_slice());
    match change.operation {
        DecodedOperation::Insert => {
            out.push(b'I');
            out.extend_from_slice(&change.table_id.to_be_bytes());
            out.push(b'N');
            tuple(&mut out, change.after.as_deref().unwrap_or_default(), None, types);
        }
        DecodedOperation::Update => {
            out.push(b'U');
            out.extend_from_slice(&change.table_id.to_be_bytes());
            if let Some(before) = &change.before {
                out.push(b'O');
                tuple(&mut out, before, None, types);
            }
            out.push(b'N');
            tuple(&mut out, change.after.as_deref().unwrap_or_default(), None, types);
        }
        DecodedOperation::Delete => {
            out.push(b'D');
            out.extend_from_slice(&change.table_id.to_be_bytes());
            out.push(b'O');
            tuple(&mut out, change.before.as_deref().unwrap_or_default(), None, types);
        }
        DecodedOperation::Truncate => {
            out.push(b'T');
            out.extend_from_slice(&1u32.to_be_bytes());
            out.push(0);
            out.extend_from_slice(&change.table_id.to_be_bytes());
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
/// has not described yet, its changes (with binary column values when `binary`), commit.
pub(crate) fn transaction_messages(transaction: &DecodedTransaction, described: &mut HashSet<u32>, binary: bool) -> Vec<(u64, Vec<u8>)> {
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
        messages.push((lsn, change_message(change, binary)));
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
            Some(ReplicationCommand::StartReplication {
                slot: "s".into(),
                lsn: 16,
                options: vec![("proto_version".into(), "1".into()), ("publication_names".into(), "\"p\"".into())],
            })
        );
        assert_eq!(replication_command("SELECT 1"), None);
        assert_eq!(flushed_lsn(&[b'r', 0, 0, 0, 0, 0, 0, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0x30]), Some(0x30));
    }

    #[test]
    fn publication_names_are_read_as_postgresql_writes_them() {
        let options = |value: &str| vec![("publication_names".to_owned(), value.to_owned())];
        assert_eq!(publication_names(&options("\"Orders\",audit")), Ok(vec!["Orders".to_owned(), "audit".to_owned()]));
        assert_eq!(publication_names(&options("FEED")), Ok(vec!["feed".to_owned()]));
        assert!(publication_names(&[]).is_err());
        assert!(publication_names(&options("")).is_err());
    }

    #[test]
    fn protocol_versions_one_to_four_are_accepted_and_others_refused() {
        let options = |text: &str| match replication_command(text) {
            Some(ReplicationCommand::StartReplication { options, .. }) => options,
            other => panic!("{other:?}"),
        };
        let four = options("START_REPLICATION SLOT \"s\" LOGICAL 0/10 (proto_version '4', publication_names '\"p\"', streaming 'parallel', binary 'true')");
        assert_eq!(protocol_version(&four), Ok(4));
        assert!(four.contains(&("streaming".to_owned(), "parallel".to_owned())));
        let five = options("START_REPLICATION SLOT \"s\" LOGICAL 0/10 (proto_version '5', publication_names '\"p\"')");
        assert!(protocol_version(&five).unwrap_err().contains("only supports protocol 4"));
        assert!(protocol_version(&options("START_REPLICATION SLOT \"s\" LOGICAL 0/10")).is_err());
        assert!(binary(&four));
        assert!(!binary(&five));
    }

    #[test]
    fn binary_tuples_carry_each_value_in_its_binary_form() {
        let change = DecodedChange {
            table_id: 7,
            schema: "public".to_owned(),
            table: "eggs".to_owned(),
            columns: vec![("id".to_owned(), SqlType::Integer), ("colour".to_owned(), SqlType::Text)],
            key_columns: vec![0],
            operation: DecodedOperation::Insert,
            before: None,
            after: Some(vec![Value::Integer(5), Value::Null]),
        };
        let message = change_message(&change, true);
        assert_eq!(&message[..6], &[b'I', 0, 0, 0, 7, b'N']);
        assert_eq!(&message[6..8], &2u16.to_be_bytes());
        assert_eq!(message[8], b'b');
        assert_eq!(&message[9..13], &4i32.to_be_bytes());
        assert_eq!(&message[13..17], &5i32.to_be_bytes());
        assert_eq!(message[17], b'n');
        let text = change_message(&change, false);
        assert_eq!(text[8], b't');
        assert_eq!(&text[13..14], b"5");
        let truncate = DecodedChange { operation: DecodedOperation::Truncate, after: None, ..change };
        assert_eq!(change_message(&truncate, false), [b'T', 0, 0, 0, 1, 0, 0, 0, 0, 7]);
    }
}
