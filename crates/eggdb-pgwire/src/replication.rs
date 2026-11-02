//! PostgreSQL's logical replication stream as a subscriber reads it: the copy-both messages of
//! the streaming replication protocol (XLogData, keepalives, standby status updates), the
//! `pgoutput` plugin's messages (protocol version 1), and the SQL that applies each change to an
//! EggDB database in the PostgreSQL dialect, including the `ALTER TABLE` that follows a change
//! to a table's columns on the source.

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
            Ok(StreamMessage::XLogData {
                start,
                wal_end,
                data: cursor.bytes.to_vec(),
            })
        }
        b'k' => {
            let wal_end = cursor.u64("WAL end")?;
            cursor.u64("send time")?;
            let reply_requested = cursor.u8("reply flag")? == 1;
            Ok(StreamMessage::Keepalive {
                wal_end,
                reply_requested,
            })
        }
        other => Err(ReplicationError(format!(
            "unknown replication message '{}'",
            char::from(other)
        ))),
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
    /// The type modifier, such as a `varchar`'s length plus 4; -1 for none.
    pub type_modifier: i32,
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
                    String::from_utf8(bytes.to_vec())
                        .map_err(|_| ReplicationError("a value is not UTF-8".to_owned()))?,
                )
            }
            b'b' => {
                return Err(ReplicationError(
                    "binary tuple values are not requested and not supported".to_owned(),
                ));
            }
            other => {
                return Err(ReplicationError(format!(
                    "unknown column kind '{}'",
                    char::from(other)
                )));
            }
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
            Ok(LogicalMessage::Commit {
                commit_lsn,
                end_lsn,
                commit_unix_micros: commit_time + POSTGRES_EPOCH_MICROS,
            })
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
                let type_modifier = cursor.u32("type modifier")? as i32;
                columns.push(RelationColumn {
                    key: flags & 1 != 0,
                    name,
                    type_oid,
                    type_modifier,
                });
            }
            let namespace = if namespace.is_empty() {
                "pg_catalog".to_owned()
            } else {
                namespace
            };
            Ok(LogicalMessage::Relation(Relation {
                id,
                namespace,
                name,
                replica_identity,
                columns,
            }))
        }
        b'I' => {
            let relation = cursor.u32("relation ID")?;
            if cursor.u8("tuple kind")? != b'N' {
                return Err(ReplicationError("an insert without its new row".to_owned()));
            }
            Ok(LogicalMessage::Insert {
                relation,
                new: tuple(&mut cursor)?,
            })
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
            Ok(LogicalMessage::Update {
                relation,
                old,
                new: tuple(&mut cursor)?,
            })
        }
        b'D' => {
            let relation = cursor.u32("relation ID")?;
            let kind = cursor.u8("tuple kind")?;
            if kind != b'K' && kind != b'O' {
                return Err(ReplicationError("a delete without its old key".to_owned()));
            }
            Ok(LogicalMessage::Delete {
                relation,
                old: tuple(&mut cursor)?,
            })
        }
        b'T' => {
            let count = cursor.u32("relation count")?;
            let options = cursor.u8("truncate options")?;
            let relations = (0..count)
                .map(|_| cursor.u32("relation ID"))
                .collect::<Result<_, _>>()?;
            Ok(LogicalMessage::Truncate {
                cascade: options & 1 != 0,
                relations,
            })
        }
        b'Y' | b'O' | b'M' => Ok(LogicalMessage::Ignored),
        other => Err(ReplicationError(format!(
            "unknown pgoutput message '{}'",
            char::from(other)
        ))),
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
    format!(
        "{}.{}",
        quote_identifier(&relation.namespace),
        quote_identifier(&relation.name)
    )
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
    target_tables: Option<HashMap<(String, String), Vec<String>>>,
    schemas: Vec<String>,
    skipped: std::collections::HashSet<u32>,
}

/// One change to a table's shape that a `Relation` message shows was made on the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReshapeStep {
    /// The table was renamed within its schema; it was called `from`.
    RenameTable {
        /// The old name.
        from: String,
    },
    /// A column was dropped.
    DropColumn {
        /// The column's name.
        name: String,
    },
    /// A column was renamed.
    RenameColumn {
        /// The old name.
        from: String,
        /// The new name.
        to: String,
    },
    /// A column was added.
    AddColumn {
        /// The column's name.
        name: String,
        /// Its type's OID.
        type_oid: u32,
        /// Its type modifier.
        type_modifier: i32,
    },
    /// A column's type changed.
    ChangeType {
        /// The column's name.
        name: String,
        /// The new type's OID.
        type_oid: u32,
        /// The new type modifier.
        type_modifier: i32,
    },
}

impl ReshapeStep {
    /// Whether the SQL for this step needs the column's type spelled out.
    pub fn needs_type(&self) -> bool {
        matches!(self, Self::AddColumn { .. } | Self::ChangeType { .. })
    }

    /// The `ALTER TABLE` statement for this step on `namespace.name`, the table's name after
    /// the change. `column_type` is the type for an added or retyped column, with any
    /// `DEFAULT` clause after it.
    pub fn sql(
        &self,
        namespace: &str,
        name: &str,
        column_type: Option<&str>,
    ) -> Result<String, ReplicationError> {
        let table = format!("{}.{}", quote_identifier(namespace), quote_identifier(name));
        let typed = |column: &str| {
            column_type.ok_or_else(|| {
                ReplicationError(format!(
                    "the type of the column {column} of {table} is not known, so it cannot be added or changed"
                ))
            })
        };
        Ok(match self {
            Self::RenameTable { from } => format!(
                "ALTER TABLE {}.{} RENAME TO {}",
                quote_identifier(namespace),
                quote_identifier(from),
                quote_identifier(name)
            ),
            Self::DropColumn { name } => {
                format!("ALTER TABLE {table} DROP COLUMN {}", quote_identifier(name))
            }
            Self::RenameColumn { from, to } => format!(
                "ALTER TABLE {table} RENAME COLUMN {} TO {}",
                quote_identifier(from),
                quote_identifier(to)
            ),
            Self::AddColumn { name, .. } => format!(
                "ALTER TABLE {table} ADD COLUMN {} {}",
                quote_identifier(name),
                typed(name.as_str())?
            ),
            Self::ChangeType { name, .. } => format!(
                "ALTER TABLE {table} ALTER COLUMN {} TYPE {}",
                quote_identifier(name),
                typed(name.as_str())?
            ),
        })
    }
}

/// The steps that turn a table with the `previous` columns into one with the `current` ones.
/// A previous column with type OID 0 has an unknown type, so its type is not compared. When
/// the column count is the same and every differing name sits where a column of the same type
/// was, under a name that is new on both sides, the columns were renamed; otherwise columns
/// that disappeared are dropped and new ones added.
pub fn reshape_steps(previous: &[RelationColumn], current: &[RelationColumn]) -> Vec<ReshapeStep> {
    let old_names: Vec<&str> = previous.iter().map(|column| column.name.as_str()).collect();
    let new_names: Vec<&str> = current.iter().map(|column| column.name.as_str()).collect();
    let same_type = |old: &RelationColumn, new: &RelationColumn| {
        old.type_oid == 0
            || (old.type_oid == new.type_oid && old.type_modifier == new.type_modifier)
    };
    let mut steps = Vec::new();
    let renamed = previous.len() == current.len()
        && previous.iter().zip(current).any(|(old, new)| old.name != new.name)
        && previous.iter().zip(current).all(|(old, new)| {
            old.name == new.name
                || (same_type(old, new)
                    && !new_names.contains(&old.name.as_str())
                    && !old_names.contains(&new.name.as_str()))
        });
    if renamed {
        for (old, new) in previous.iter().zip(current) {
            if old.name != new.name {
                steps.push(ReshapeStep::RenameColumn {
                    from: old.name.clone(),
                    to: new.name.clone(),
                });
            }
        }
        return steps;
    }
    for old in previous {
        if !new_names.contains(&old.name.as_str()) {
            steps.push(ReshapeStep::DropColumn {
                name: old.name.clone(),
            });
        }
    }
    for new in current {
        match previous.iter().find(|old| old.name == new.name) {
            None => steps.push(ReshapeStep::AddColumn {
                name: new.name.clone(),
                type_oid: new.type_oid,
                type_modifier: new.type_modifier,
            }),
            Some(old) if !same_type(old, new) => steps.push(ReshapeStep::ChangeType {
                name: new.name.clone(),
                type_oid: new.type_oid,
                type_modifier: new.type_modifier,
            }),
            Some(_) => {}
        }
    }
    steps
}

/// The SQL spelling of a built-in PostgreSQL type, by OID and type modifier; `None` for a type
/// this list does not know.
pub fn builtin_type_name(type_oid: u32, type_modifier: i32) -> Option<String> {
    let length = (type_modifier >= 4).then(|| type_modifier - 4);
    let sized = |name: &str| match length {
        Some(length) => format!("{name}({length})"),
        None => name.to_owned(),
    };
    Some(match type_oid {
        16 => "boolean".to_owned(),
        17 => "bytea".to_owned(),
        20 => "bigint".to_owned(),
        21 => "smallint".to_owned(),
        23 => "integer".to_owned(),
        25 => "text".to_owned(),
        114 => "json".to_owned(),
        700 => "real".to_owned(),
        701 => "double precision".to_owned(),
        1042 => sized("character"),
        1043 => sized("character varying"),
        1082 => "date".to_owned(),
        1083 => "time without time zone".to_owned(),
        1114 => "timestamp without time zone".to_owned(),
        1184 => "timestamp with time zone".to_owned(),
        1186 => "interval".to_owned(),
        1266 => "time with time zone".to_owned(),
        1700 => match length {
            Some(length) => format!("numeric({}, {})", length >> 16, length & 0xFFFF),
            None => "numeric".to_owned(),
        },
        2950 => "uuid".to_owned(),
        3802 => "jsonb".to_owned(),
        _ => return None,
    })
}

/// What one message asks of the subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Start a transaction.
    Begin,
    /// Run this statement inside the transaction.
    Statement(String),
    /// Change the table's shape before applying its next change.
    Reshape {
        /// The relation's OID on the source.
        relation: u32,
        /// The table's schema.
        namespace: String,
        /// The table's name after the change.
        name: String,
        /// The changes, in the order to apply them.
        steps: Vec<ReshapeStep>,
    },
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

    /// Records a table the target already has, with its column names in order, so the first
    /// `Relation` message for it is compared with them. Once any table is recorded, a
    /// `Relation` for a table the target does not have is an error.
    pub fn know_target_table(&mut self, namespace: &str, name: &str, columns: Vec<String>) {
        self.target_tables
            .get_or_insert_with(HashMap::new)
            .insert((namespace.to_owned(), name.to_owned()), columns);
    }

    /// Applies only the changes of tables in these schemas; an empty list applies every table.
    pub fn only_schemas(&mut self, schemas: Vec<String>) {
        self.schemas = schemas;
    }

    fn reshape(&mut self, relation: &Relation) -> Result<Vec<ReshapeStep>, ReplicationError> {
        let shown = format!("{}.{}", relation.namespace, relation.name);
        if let Some(previous) = self.relations.get(&relation.id) {
            let mut steps = Vec::new();
            if previous.name != relation.name || previous.namespace != relation.namespace {
                if previous.namespace != relation.namespace {
                    return Err(ReplicationError(format!(
                        "the source moved the table {}.{} to the schema {}; follow cannot move tables between schemas",
                        previous.namespace, previous.name, relation.namespace
                    )));
                }
                steps.push(ReshapeStep::RenameTable {
                    from: previous.name.clone(),
                });
            }
            steps.extend(reshape_steps(&previous.columns, &relation.columns));
            return Ok(steps);
        }
        let Some(tables) = &mut self.target_tables else {
            return Ok(Vec::new());
        };
        let key = (relation.namespace.clone(), relation.name.clone());
        let Some(names) = tables.remove(&key) else {
            return Err(ReplicationError(format!(
                "the source has a new table {shown}, created after the copy; follow cannot create tables while it follows"
            )));
        };
        let previous: Vec<RelationColumn> = names
            .into_iter()
            .map(|name| RelationColumn {
                key: false,
                name,
                type_oid: 0,
                type_modifier: -1,
            })
            .collect();
        Ok(reshape_steps(&previous, &relation.columns))
    }

    fn relation(&self, id: u32) -> Result<&Relation, ReplicationError> {
        self.relations.get(&id).ok_or_else(|| {
            ReplicationError(format!(
                "a change to relation {id} came before its description"
            ))
        })
    }

    fn key_condition(
        relation: &Relation,
        row: &[TupleValue],
        whole_row: bool,
    ) -> Result<String, ReplicationError> {
        let mut conditions = Vec::new();
        for (column, value) in relation.columns.iter().zip(row) {
            if !whole_row && !column.key {
                continue;
            }
            match value {
                TupleValue::Null if whole_row => {
                    conditions.push(format!("{} IS NULL", quote_identifier(&column.name)))
                }
                TupleValue::Null | TupleValue::Unchanged if !whole_row => {
                    return Err(ReplicationError(format!(
                        "the key column {} of {} has no value",
                        column.name,
                        qualified(relation)
                    )));
                }
                TupleValue::Text(text) => {
                    conditions.push(format!(
                        "{} = {}",
                        quote_identifier(&column.name),
                        quote_literal(text)
                    ));
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
            LogicalMessage::Commit {
                end_lsn,
                commit_unix_micros,
                ..
            } => Change::Commit {
                end_lsn,
                commit_unix_micros,
            },
            LogicalMessage::Relation(relation) => {
                if !self.schemas.is_empty() && !self.schemas.contains(&relation.namespace) {
                    self.skipped.insert(relation.id);
                    return Ok(Change::Nothing);
                }
                let steps = self.reshape(&relation)?;
                let change = if steps.is_empty() {
                    Change::Nothing
                } else {
                    Change::Reshape {
                        relation: relation.id,
                        namespace: relation.namespace.clone(),
                        name: relation.name.clone(),
                        steps,
                    }
                };
                self.relations.insert(relation.id, relation);
                change
            }
            LogicalMessage::Insert { relation, .. }
            | LogicalMessage::Update { relation, .. }
            | LogicalMessage::Delete { relation, .. }
                if self.skipped.contains(&relation) =>
            {
                Change::Nothing
            }
            LogicalMessage::Insert { relation, new } => {
                let relation = self.relation(relation)?;
                let columns: Vec<String> = relation
                    .columns
                    .iter()
                    .map(|column| quote_identifier(&column.name))
                    .collect();
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
                    .map(|(column, value)| {
                        format!("{} = {}", quote_identifier(&column.name), value_sql(value))
                    })
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
                let condition =
                    Self::key_condition(relation, &old, relation.replica_identity == b'f')?;
                Change::Statement(format!(
                    "DELETE FROM {} WHERE {}",
                    qualified(relation),
                    condition
                ))
            }
            LogicalMessage::Truncate { cascade, relations } => {
                let relations: Vec<u32> = relations
                    .into_iter()
                    .filter(|id| !self.skipped.contains(id))
                    .collect();
                if relations.is_empty() {
                    return Ok(Change::Nothing);
                }
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
        assert_eq!(
            applier
                .apply(parse_logical_message(&relation_message()).unwrap())
                .unwrap(),
            Change::Nothing
        );

        let mut insert = vec![b'I'];
        insert.extend_from_slice(&16_384u32.to_be_bytes());
        insert.push(b'N');
        insert.extend(tuple_bytes(&[Some("7"), Some("it's")]));
        assert_eq!(
            applier
                .apply(parse_logical_message(&insert).unwrap())
                .unwrap(),
            Change::Statement(
                "INSERT INTO \"public\".\"orders\" (\"id\", \"note\") VALUES ('7', 'it''s')"
                    .to_owned()
            )
        );

        let mut update = vec![b'U'];
        update.extend_from_slice(&16_384u32.to_be_bytes());
        update.push(b'N');
        update.extend(tuple_bytes(&[Some("7"), None]));
        assert_eq!(
            applier
                .apply(parse_logical_message(&update).unwrap())
                .unwrap(),
            Change::Statement(
                "UPDATE \"public\".\"orders\" SET \"id\" = '7', \"note\" = NULL WHERE \"id\" = '7'"
                    .to_owned()
            )
        );

        let mut delete = vec![b'D'];
        delete.extend_from_slice(&16_384u32.to_be_bytes());
        delete.push(b'K');
        delete.extend(tuple_bytes(&[Some("7"), None]));
        assert_eq!(
            applier
                .apply(parse_logical_message(&delete).unwrap())
                .unwrap(),
            Change::Statement("DELETE FROM \"public\".\"orders\" WHERE \"id\" = '7'".to_owned())
        );
    }

    #[test]
    fn a_change_to_an_undescribed_relation_is_an_error() {
        let mut insert = vec![b'I'];
        insert.extend_from_slice(&99u32.to_be_bytes());
        insert.push(b'N');
        insert.extend(tuple_bytes(&[Some("1")]));
        assert!(
            ChangeApplier::new()
                .apply(parse_logical_message(&insert).unwrap())
                .is_err()
        );
    }

    #[test]
    fn stream_messages_and_status_updates_have_their_layout() {
        let mut keepalive = vec![b'k'];
        keepalive.extend_from_slice(&0x10u64.to_be_bytes());
        keepalive.extend_from_slice(&0u64.to_be_bytes());
        keepalive.push(1);
        assert_eq!(
            parse_stream_message(&keepalive).unwrap(),
            StreamMessage::Keepalive {
                wal_end: 0x10,
                reply_requested: true
            }
        );
        let update = standby_status_update(0x20, POSTGRES_EPOCH_MICROS + 5, false);
        assert_eq!(update.len(), 34);
        assert_eq!(&update[1..9], &0x20u64.to_be_bytes());
        assert_eq!(&update[25..33], &5i64.to_be_bytes());
    }

    fn column(name: &str, type_oid: u32) -> RelationColumn {
        RelationColumn {
            key: false,
            name: name.to_owned(),
            type_oid,
            type_modifier: -1,
        }
    }

    fn orders_with(columns: Vec<RelationColumn>) -> LogicalMessage {
        LogicalMessage::Relation(Relation {
            id: 16_384,
            namespace: "public".to_owned(),
            name: "orders".to_owned(),
            replica_identity: b'd',
            columns,
        })
    }

    #[test]
    fn added_and_dropped_columns_become_steps() {
        let previous = [column("id", 23), column("note", 25)];
        let current = [column("id", 23), column("amount", 1700)];
        assert_eq!(
            reshape_steps(&previous, &current),
            vec![
                ReshapeStep::DropColumn {
                    name: "note".to_owned()
                },
                ReshapeStep::AddColumn {
                    name: "amount".to_owned(),
                    type_oid: 1700,
                    type_modifier: -1
                },
            ]
        );
        assert!(reshape_steps(&previous, &previous).is_empty());
    }

    #[test]
    fn a_column_in_the_same_place_with_the_same_type_and_a_new_name_was_renamed() {
        let previous = [column("id", 23), column("note", 25)];
        let current = [column("id", 23), column("remark", 25)];
        assert_eq!(
            reshape_steps(&previous, &current),
            vec![ReshapeStep::RenameColumn {
                from: "note".to_owned(),
                to: "remark".to_owned()
            }]
        );
        let retyped = [column("id", 23), column("remark", 23)];
        assert_eq!(reshape_steps(&previous, &retyped).len(), 2);
    }

    #[test]
    fn a_changed_type_is_a_step_unless_the_old_type_is_unknown() {
        let previous = [column("id", 23)];
        let current = [column("id", 20)];
        assert_eq!(
            reshape_steps(&previous, &current),
            vec![ReshapeStep::ChangeType {
                name: "id".to_owned(),
                type_oid: 20,
                type_modifier: -1
            }]
        );
        assert!(reshape_steps(&[column("id", 0)], &current).is_empty());
    }

    #[test]
    fn steps_become_alter_table_statements() {
        let add = ReshapeStep::AddColumn {
            name: "amount".to_owned(),
            type_oid: 1700,
            type_modifier: -1,
        };
        assert_eq!(
            add.sql("public", "orders", Some("numeric DEFAULT 0")).unwrap(),
            "ALTER TABLE \"public\".\"orders\" ADD COLUMN \"amount\" numeric DEFAULT 0"
        );
        assert!(add.sql("public", "orders", None).is_err());
        assert_eq!(
            ReshapeStep::RenameTable {
                from: "old".to_owned()
            }
            .sql("public", "orders", None)
            .unwrap(),
            "ALTER TABLE \"public\".\"old\" RENAME TO \"orders\""
        );
        assert_eq!(
            ReshapeStep::DropColumn {
                name: "note".to_owned()
            }
            .sql("public", "orders", None)
            .unwrap(),
            "ALTER TABLE \"public\".\"orders\" DROP COLUMN \"note\""
        );
    }

    #[test]
    fn builtin_types_are_spelled_with_their_modifiers() {
        assert_eq!(builtin_type_name(23, -1).as_deref(), Some("integer"));
        assert_eq!(
            builtin_type_name(1043, 24).as_deref(),
            Some("character varying(20)")
        );
        assert_eq!(
            builtin_type_name(1700, ((10 << 16) | 2) + 4).as_deref(),
            Some("numeric(10, 2)")
        );
        assert_eq!(builtin_type_name(99_999, -1), None);
    }

    #[test]
    fn a_second_relation_message_with_other_columns_reshapes_the_table() {
        let mut applier = ChangeApplier::new();
        let first = vec![column("id", 23), column("note", 25)];
        assert_eq!(applier.apply(orders_with(first.clone())).unwrap(), Change::Nothing);
        assert_eq!(applier.apply(orders_with(first)).unwrap(), Change::Nothing);
        let change = applier
            .apply(orders_with(vec![
                column("id", 23),
                column("note", 25),
                column("priority", 23),
            ]))
            .unwrap();
        assert_eq!(
            change,
            Change::Reshape {
                relation: 16_384,
                namespace: "public".to_owned(),
                name: "orders".to_owned(),
                steps: vec![ReshapeStep::AddColumn {
                    name: "priority".to_owned(),
                    type_oid: 23,
                    type_modifier: -1
                }],
            }
        );
    }

    #[test]
    fn the_first_relation_message_is_compared_with_the_target_table() {
        let mut applier = ChangeApplier::new();
        applier.know_target_table("public", "orders", vec!["id".to_owned(), "gone".to_owned()]);
        let change = applier
            .apply(orders_with(vec![column("id", 23), column("note", 25), column("x", 25)]))
            .unwrap();
        assert!(matches!(change, Change::Reshape { ref steps, .. } if steps.len() == 3));
        let mut unknown = vec![b'R'];
        unknown.extend_from_slice(&7u32.to_be_bytes());
        unknown.extend_from_slice(b"public\0brand_new\0d");
        unknown.extend_from_slice(&0u16.to_be_bytes());
        let refused = applier.apply(parse_logical_message(&unknown).unwrap());
        assert!(refused.unwrap_err().0.contains("new table public.brand_new"));
    }

    #[test]
    fn changes_outside_the_followed_schemas_are_skipped() {
        let mut applier = ChangeApplier::new();
        applier.only_schemas(vec!["sales".to_owned()]);
        assert_eq!(
            applier
                .apply(parse_logical_message(&relation_message()).unwrap())
                .unwrap(),
            Change::Nothing
        );
        let mut insert = vec![b'I'];
        insert.extend_from_slice(&16_384u32.to_be_bytes());
        insert.push(b'N');
        insert.extend(tuple_bytes(&[Some("1"), None]));
        assert_eq!(
            applier
                .apply(parse_logical_message(&insert).unwrap())
                .unwrap(),
            Change::Nothing
        );
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
