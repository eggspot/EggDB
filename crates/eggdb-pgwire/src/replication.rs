//! PostgreSQL's logical replication stream as a subscriber reads it: the copy-both messages of
//! the streaming replication protocol (XLogData, keepalives, standby status updates), the
//! `pgoutput` plugin's messages (protocol version 1), and the SQL that applies each change to an
//! EggDB database in the PostgreSQL dialect.

use std::collections::HashMap;

/// A WAL position.
pub type Lsn = u64;

/// Microseconds from the Unix epoch to PostgreSQL's epoch, 2000-01-01.
const POSTGRES_EPOCH_MICROS: i64 = 946_684_800_000_000;

/// `X/Y` as PostgreSQL prints a WAL position.
pub fn format_lsn(lsn: Lsn) -> String {
    format!("{:X}/{:X}", lsn >> 32, lsn & 0xFFFF_FFFF)
}

/// Reads `X/Y`.
pub fn parse_lsn(text: &str) -> Option<Lsn> {
    let (high, low) = text.split_once('/')?;
    let high = u64::from_str_radix(high, 16).ok()?;
    let low = u64::from_str_radix(low, 16).ok()?;
    (high <= 0xFFFF_FFFF && low <= 0xFFFF_FFFF).then_some(high << 32 | low)
}

/// Why a replication message was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationError(pub String);

impl std::fmt::Display for ReplicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ReplicationError {}

struct Cursor<'a> {
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize, what: &str) -> Result<&'a [u8], ReplicationError> {
        let (taken, rest) = self
            .bytes
            .split_at_checked(count)
            .ok_or_else(|| ReplicationError(format!("the message ends inside its {what}")))?;
        self.bytes = rest;
        Ok(taken)
    }

    fn u8(&mut self, what: &str) -> Result<u8, ReplicationError> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &str) -> Result<u16, ReplicationError> {
        let bytes = self.take(2, what)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self, what: &str) -> Result<u32, ReplicationError> {
        let bytes = self.take(4, what)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self, what: &str) -> Result<u64, ReplicationError> {
        let bytes = self.take(8, what)?;
        let mut array = [0u8; 8];
        array.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(array))
    }

    fn string(&mut self, what: &str) -> Result<String, ReplicationError> {
        let end = self
            .bytes
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| ReplicationError(format!("the {what} has no end")))?;
        let text = String::from_utf8(self.bytes[..end].to_vec())
            .map_err(|_| ReplicationError(format!("the {what} is not UTF-8")))?;
        self.bytes = &self.bytes[end + 1..];
        Ok(text)
    }
}

/// A message of the copy-both stream from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamMessage {
    /// A piece of WAL: here, one `pgoutput` message.
    XLogData {
        /// Where the data starts.
        start: Lsn,
        /// The end of WAL on the server.
        wal_end: Lsn,
        /// The payload.
        data: Vec<u8>,
    },
    /// A keepalive.
    Keepalive {
        /// The end of WAL on the server.
        wal_end: Lsn,
        /// Whether the server wants a status update at once.
        reply_requested: bool,
    },
}

/// Reads one copy-data message of a logical replication stream.
pub fn parse_stream_message(bytes: &[u8]) -> Result<StreamMessage, ReplicationError> {
    let mut cursor = Cursor { bytes };
    match cursor.u8("kind")? {
        b'w' => {
            let start = cursor.u64("start position")?;
            let wal_end = cursor.u64("WAL end")?;
            cursor.u64("send time")?;
            Ok(StreamMessage::XLogData { start, wal_end, data: cursor.bytes.to_vec() })
        }
        b'k' => {
            let wal_end = cursor.u64("WAL end")?;
            cursor.u64("send time")?;
            let reply_requested = cursor.u8("reply flag")? == 1;
            Ok(StreamMessage::Keepalive { wal_end, reply_requested })
        }
        other => Err(ReplicationError(format!("unknown replication message '{}'", char::from(other)))),
    }
}

/// A standby status update: everything up to `applied` is written, flushed and applied.
pub fn standby_status_update(applied: Lsn, unix_micros: i64, reply_requested: bool) -> Vec<u8> {
    let mut message = vec![b'r'];
    for _ in 0..3 {
        message.extend_from_slice(&applied.to_be_bytes());
    }
    message.extend_from_slice(&(unix_micros - POSTGRES_EPOCH_MICROS).to_be_bytes());
    message.push(u8::from(reply_requested));
    message
}

/// One column of a published relation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationColumn {
    /// Whether the column is part of the replica identity (the key).
    pub key: bool,
    /// The column's name.
    pub name: String,
    /// The type's OID.
    pub type_oid: u32,
}

/// A published table, as a `Relation` message describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// The relation's OID on the source.
    pub id: u32,
    /// The schema.
    pub namespace: String,
    /// The table name.
    pub name: String,
    /// The replica identity setting: `d`, `n`, `f` or `i`.
    pub replica_identity: u8,
    /// The columns.
    pub columns: Vec<RelationColumn>,
}

/// One column's value in a tuple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TupleValue {
    /// SQL null.
    Null,
    /// An unchanged TOASTed value, not sent.
    Unchanged,
    /// The value in its text form.
    Text(String),
}

/// A `pgoutput` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalMessage {
    /// A transaction starts.
    Begin {
        /// The transaction's commit position.
        final_lsn: Lsn,
        /// The transaction ID.
        xid: u32,
    },
    /// A transaction commits.
    Commit {
        /// The commit record's position.
        commit_lsn: Lsn,
        /// The end of the transaction's WAL.
        end_lsn: Lsn,
        /// The commit time, microseconds since the Unix epoch.
        commit_unix_micros: i64,
    },
    /// A table's shape.
    Relation(Relation),
    /// A row inserted.
    Insert {
        /// The relation's OID.
        relation: u32,
        /// The new row.
        new: Vec<TupleValue>,
    },
    /// A row updated.
    Update {
        /// The relation's OID.
        relation: u32,
        /// The old key (`K`) or old row (`O`), when sent.
        old: Option<Vec<TupleValue>>,
        /// The new row.
        new: Vec<TupleValue>,
    },
    /// A row deleted.
    Delete {
        /// The relation's OID.
        relation: u32,
        /// The old key or old row.
        old: Vec<TupleValue>,
    },
    /// Tables truncated.
    Truncate {
        /// Whether `CASCADE` was given.
        cascade: bool,
        /// The relations' OIDs.
        relations: Vec<u32>,
    },
    /// A message this subscriber does not need (type, origin, logical message).
    Ignored,
}

fn tuple(cursor: &mut Cursor<'_>) -> Result<Vec<TupleValue>, ReplicationError> {
    let count = cursor.u16("column count")?;
    let mut values = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        values.push(match cursor.u8("column kind")? {
            b'n' => TupleValue::Null,
            b'u' => TupleValue::Unchanged,
            b't' => {
                let length = cursor.u32("value length")? as usize;
                let bytes = cursor.take(length, "value")?;
                TupleValue::Text(
                    String::from_utf8(bytes.to_vec()).map_err(|_| ReplicationError("a value is not UTF-8".to_owned()))?,
                )
            }
            b'b' => return Err(ReplicationError("binary tuple values are not requested and not supported".to_owned())),
            other => return Err(ReplicationError(format!("unknown column kind '{}'", char::from(other)))),
        });
    }
    Ok(values)
}

/// Reads one `pgoutput` message (protocol version 1).
pub fn parse_logical_message(bytes: &[u8]) -> Result<LogicalMessage, ReplicationError> {
    let mut cursor = Cursor { bytes };
    match cursor.u8("message kind")? {
        b'B' => {
            let final_lsn = cursor.u64("final position")?;
            cursor.u64("commit time")?;
            let xid = cursor.u32("transaction ID")?;
            Ok(LogicalMessage::Begin { final_lsn, xid })
        }
        b'C' => {
            cursor.u8("flags")?;
            let commit_lsn = cursor.u64("commit position")?;
            let end_lsn = cursor.u64("end position")?;
            let commit_time = cursor.u64("commit time")? as i64;
            Ok(LogicalMessage::Commit { commit_lsn, end_lsn, commit_unix_micros: commit_time + POSTGRES_EPOCH_MICROS })
        }
        b'R' => {
            let id = cursor.u32("relation ID")?;
            let namespace = cursor.string("schema name")?;
            let name = cursor.string("table name")?;
            let replica_identity = cursor.u8("replica identity")?;
            let count = cursor.u16("column count")?;
            let mut columns = Vec::with_capacity(usize::from(count));
            for _ in 0..count {
                let flags = cursor.u8("column flags")?;
                let name = cursor.string("column name")?;
                let type_oid = cursor.u32("type OID")?;
                cursor.u32("type modifier")?;
                columns.push(RelationColumn { key: flags & 1 != 0, name, type_oid });
            }
            let namespace = if namespace.is_empty() { "pg_catalog".to_owned() } else { namespace };
            Ok(LogicalMessage::Relation(Relation { id, namespace, name, replica_identity, columns }))
        }
        b'I' => {
            let relation = cursor.u32("relation ID")?;
            if cursor.u8("tuple kind")? != b'N' {
                return Err(ReplicationError("an insert without its new row".to_owned()));
            }
            Ok(LogicalMessage::Insert { relation, new: tuple(&mut cursor)? })
        }
        b'U' => {
            let relation = cursor.u32("relation ID")?;
            let mut kind = cursor.u8("tuple kind")?;
            let mut old = None;
            if kind == b'K' || kind == b'O' {
                old = Some(tuple(&mut cursor)?);
                kind = cursor.u8("tuple kind")?;
            }
            if kind != b'N' {
                return Err(ReplicationError("an update without its new row".to_owned()));
            }
            Ok(LogicalMessage::Update { relation, old, new: tuple(&mut cursor)? })
        }
        b'D' => {
            let relation = cursor.u32("relation ID")?;
            let kind = cursor.u8("tuple kind")?;
            if kind != b'K' && kind != b'O' {
                return Err(ReplicationError("a delete without its old key".to_owned()));
            }
            Ok(LogicalMessage::Delete { relation, old: tuple(&mut cursor)? })
        }
        b'T' => {
            let count = cursor.u32("relation count")?;
            let options = cursor.u8("truncate options")?;
            let relations = (0..count).map(|_| cursor.u32("relation ID")).collect::<Result<_, _>>()?;
            Ok(LogicalMessage::Truncate { cascade: options & 1 != 0, relations })
        }
        b'Y' | b'O' | b'M' => Ok(LogicalMessage::Ignored),
        other => Err(ReplicationError(format!("unknown pgoutput message '{}'", char::from(other)))),
    }
}

/// `name` quoted as a PostgreSQL identifier.
pub fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `text` as a PostgreSQL string literal.
pub fn quote_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn qualified(relation: &Relation) -> String {
    format!("{}.{}", quote_identifier(&relation.namespace), quote_identifier(&relation.name))
}

fn value_sql(value: &TupleValue) -> String {
    match value {
        TupleValue::Null | TupleValue::Unchanged => "NULL".to_owned(),
        TupleValue::Text(text) => quote_literal(text),
    }
}

/// The SQL that applies the changes of a logical stream, keeping each table's shape as
/// `Relation` messages describe it.
#[derive(Debug, Default)]
pub struct ChangeApplier {
    relations: HashMap<u32, Relation>,
}

/// What one message asks of the subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Start a transaction.
    Begin,
    /// Run this statement inside the transaction.
    Statement(String),
    /// Commit, then report `end_lsn` as applied.
    Commit {
        /// The end of the transaction's WAL.
        end_lsn: Lsn,
        /// The source's commit time.
        commit_unix_micros: i64,
    },
    /// Nothing to do.
    Nothing,
}

impl ChangeApplier {
    /// An applier that knows no relations yet.
    pub fn new() -> Self {
        Self::default()
    }

    fn relation(&self, id: u32) -> Result<&Relation, ReplicationError> {
        self.relations
            .get(&id)
            .ok_or_else(|| ReplicationError(format!("a change to relation {id} came before its description")))
    }

    fn key_condition(relation: &Relation, row: &[TupleValue], whole_row: bool) -> Result<String, ReplicationError> {
        let mut conditions = Vec::new();
        for (column, value) in relation.columns.iter().zip(row) {
            if !whole_row && !column.key {
                continue;
            }
            match value {
                TupleValue::Null if whole_row => conditions.push(format!("{} IS NULL", quote_identifier(&column.name))),
                TupleValue::Null | TupleValue::Unchanged if !whole_row => {
                    return Err(ReplicationError(format!(
                        "the key column {} of {} has no value",
                        column.name,
                        qualified(relation)
                    )));
                }
                TupleValue::Text(text) => {
                    conditions.push(format!("{} = {}", quote_identifier(&column.name), quote_literal(text)));
                }
                _ => {}
            }
        }
        if conditions.is_empty() {
            return Err(ReplicationError(format!(
                "{} has no primary key or replica identity, so its updates and deletes cannot be applied",
                qualified(relation)
            )));
        }
        Ok(conditions.join(" AND "))
    }

    /// The change one `pgoutput` message makes.
    pub fn apply(&mut self, message: LogicalMessage) -> Result<Change, ReplicationError> {
        Ok(match message {
            LogicalMessage::Begin { .. } => Change::Begin,
            LogicalMessage::Commit { end_lsn, commit_unix_micros, .. } => Change::Commit { end_lsn, commit_unix_micros },
            LogicalMessage::Relation(relation) => {
                self.relations.insert(relation.id, relation);
                Change::Nothing
            }
            LogicalMessage::Insert { relation, new } => {
                let relation = self.relation(relation)?;
                let columns: Vec<String> = relation.columns.iter().map(|column| quote_identifier(&column.name)).collect();
                let values: Vec<String> = new.iter().map(value_sql).collect();
                Change::Statement(format!(
                    "INSERT INTO {} ({}) VALUES ({})",
                    qualified(relation),
                    columns.join(", "),
                    values.join(", ")
                ))
            }
            LogicalMessage::Update { relation, old, new } => {
                let relation = self.relation(relation)?;
                let assignments: Vec<String> = relation
                    .columns
                    .iter()
                    .zip(&new)
                    .filter(|(_, value)| **value != TupleValue::Unchanged)
                    .map(|(column, value)| format!("{} = {}", quote_identifier(&column.name), value_sql(value)))
                    .collect();
                let whole_row = relation.replica_identity == b'f';
                let condition = match &old {
                    Some(old) => Self::key_condition(relation, old, whole_row)?,
                    None => Self::key_condition(relation, &new, false)?,
                };
                if assignments.is_empty() {
                    Change::Nothing
                } else {
                    Change::Statement(format!(
                        "UPDATE {} SET {} WHERE {}",
                        qualified(relation),
                        assignments.join(", "),
                        condition
                    ))
                }
            }
            LogicalMessage::Delete { relation, old } => {
                let relation = self.relation(relation)?;
                let condition = Self::key_condition(relation, &old, relation.replica_identity == b'f')?;
                Change::Statement(format!("DELETE FROM {} WHERE {}", qualified(relation), condition))
            }
            LogicalMessage::Truncate { cascade, relations } => {
                let names = relations
                    .iter()
                    .map(|id| self.relation(*id).map(qualified))
                    .collect::<Result<Vec<_>, _>>()?;
                Change::Statement(format!(
                    "TRUNCATE {}{}",
                    names.join(", "),
                    if cascade { " CASCADE" } else { "" }
                ))
            }
            LogicalMessage::Ignored => Change::Nothing,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relation_message() -> Vec<u8> {
        let mut message = vec![b'R'];
        message.extend_from_slice(&16_384u32.to_be_bytes());
        message.extend_from_slice(b"public\0orders\0");
        message.push(b'd');
        message.extend_from_slice(&2u16.to_be_bytes());
        message.push(1);
        message.extend_from_slice(b"id\0");
        message.extend_from_slice(&23u32.to_be_bytes());
        message.extend_from_slice(&(-1i32).to_be_bytes());
        message.push(0);
        message.extend_from_slice(b"note\0");
        message.extend_from_slice(&25u32.to_be_bytes());
        message.extend_from_slice(&(-1i32).to_be_bytes());
        message
    }

    fn tuple_bytes(values: &[Option<&str>]) -> Vec<u8> {
        let mut bytes = (values.len() as u16).to_be_bytes().to_vec();
        for value in values {
            match value {
                None => bytes.push(b'n'),
                Some(text) => {
                    bytes.push(b't');
                    bytes.extend_from_slice(&(text.len() as u32).to_be_bytes());
                    bytes.extend_from_slice(text.as_bytes());
                }
            }
        }
        bytes
    }

    #[test]
    fn positions_print_and_read_like_postgresql() {
        assert_eq!(format_lsn(0x0000_0000_03A1_F2C8), "0/3A1F2C8");
        assert_eq!(parse_lsn("16/B374D848"), Some(0x16_B374_D848));
        assert_eq!(parse_lsn("nope"), None);
    }

    #[test]
    fn inserts_updates_and_deletes_become_sql() {
        let mut applier = ChangeApplier::new();
        assert_eq!(applier.apply(parse_logical_message(&relation_message()).unwrap()).unwrap(), Change::Nothing);

        let mut insert = vec![b'I'];
        insert.extend_from_slice(&16_384u32.to_be_bytes());
        insert.push(b'N');
        insert.extend(tuple_bytes(&[Some("7"), Some("it's")]));
        assert_eq!(
            applier.apply(parse_logical_message(&insert).unwrap()).unwrap(),
            Change::Statement("INSERT INTO \"public\".\"orders\" (\"id\", \"note\") VALUES ('7', 'it''s')".to_owned())
        );

        let mut update = vec![b'U'];
        update.extend_from_slice(&16_384u32.to_be_bytes());
        update.push(b'N');
        update.extend(tuple_bytes(&[Some("7"), None]));
        assert_eq!(
            applier.apply(parse_logical_message(&update).unwrap()).unwrap(),
            Change::Statement("UPDATE \"public\".\"orders\" SET \"id\" = '7', \"note\" = NULL WHERE \"id\" = '7'".to_owned())
        );

        let mut delete = vec![b'D'];
        delete.extend_from_slice(&16_384u32.to_be_bytes());
        delete.push(b'K');
        delete.extend(tuple_bytes(&[Some("7"), None]));
        assert_eq!(
            applier.apply(parse_logical_message(&delete).unwrap()).unwrap(),
            Change::Statement("DELETE FROM \"public\".\"orders\" WHERE \"id\" = '7'".to_owned())
        );
    }

    #[test]
    fn a_change_to_an_undescribed_relation_is_an_error() {
        let mut insert = vec![b'I'];
        insert.extend_from_slice(&99u32.to_be_bytes());
        insert.push(b'N');
        insert.extend(tuple_bytes(&[Some("1")]));
        assert!(ChangeApplier::new().apply(parse_logical_message(&insert).unwrap()).is_err());
    }

    #[test]
    fn stream_messages_and_status_updates_have_their_layout() {
        let mut keepalive = vec![b'k'];
        keepalive.extend_from_slice(&0x10u64.to_be_bytes());
        keepalive.extend_from_slice(&0u64.to_be_bytes());
        keepalive.push(1);
        assert_eq!(
            parse_stream_message(&keepalive).unwrap(),
            StreamMessage::Keepalive { wal_end: 0x10, reply_requested: true }
        );
        let update = standby_status_update(0x20, POSTGRES_EPOCH_MICROS + 5, false);
        assert_eq!(update.len(), 34);
        assert_eq!(&update[1..9], &0x20u64.to_be_bytes());
        assert_eq!(&update[25..33], &5i64.to_be_bytes());
    }

    #[test]
    fn every_truncation_of_a_message_is_an_error_not_a_panic() {
        let message = relation_message();
        for cut in 0..message.len() {
            assert!(parse_logical_message(&message[..cut]).is_err());
        }
        let mut state = 0x1234_5678u64;
        for _ in 0..2_000 {
            let mut mutated = message.clone();
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let position = (state as usize) % mutated.len();
            mutated[position] = (state >> 24) as u8;
            let _ = parse_logical_message(&mutated);
            let _ = parse_stream_message(&mutated);
        }
    }
}
