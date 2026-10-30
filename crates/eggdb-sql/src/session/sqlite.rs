use super::{OutputColumn, QueryResult, Session};
use crate::ast::{
    ColumnDefinition, CreateTable, Expr, ExprKind, Insert, InsertSource, InsertValue, Literal,
    Name, OnDuplicate, Pragma, Select, TableReference,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::schema::{ColumnSchema, TableSchema, read_tables};
use crate::store::{EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection};
use crate::types::{SqlType, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

const SCHEMA_COLUMNS: [(&str, SqlType); 5] = [
    ("type", SqlType::Text),
    ("name", SqlType::Text),
    ("tbl_name", SqlType::Text),
    ("rootpage", SqlType::BigInt),
    ("sql", SqlType::Text),
];

const JOURNAL_MODES: [&str; 6] = ["delete", "truncate", "persist", "memory", "wal", "off"];

#[derive(Default)]
pub(super) struct Scratch(BTreeMap<Vec<u8>, Vec<u8>>);

impl KeyValueRead for Scratch {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        Ok(self.0.get(key).cloned())
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        if start >= end {
            return Ok(());
        }
        let range = self.0.range(start.to_vec()..end.to_vec());
        let entries: Box<dyn Iterator<Item = (&Vec<u8>, &Vec<u8>)>> = match direction {
            ScanDirection::Forward => Box::new(range),
            ScanDirection::Backward => Box::new(range.rev()),
        };
        for (key, value) in entries {
            if !visit(key, value)? {
                break;
            }
        }
        Ok(())
    }
}

impl KeyValueWrite for Scratch {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.0.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        Ok(self.0.remove(key).is_some())
    }

    fn commit(self) -> SqlResult<()> {
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        Ok(())
    }
}

pub(crate) fn sqlite_type_name(sql_type: SqlType) -> &'static str {
    match sql_type {
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt => "INTEGER",
        SqlType::Real | SqlType::Double => "REAL",
        SqlType::Boolean => "BOOLEAN",
        SqlType::Text | SqlType::Varchar(_) => "TEXT",
        SqlType::Numeric(_) => "NUMERIC",
        SqlType::Date => "DATE",
        SqlType::Timestamp(_) => "DATETIME",
        SqlType::TimestampTz(_) => "TIMESTAMP",
        SqlType::Json | SqlType::Jsonb => "JSON",
        SqlType::Bpchar(_) => "CHARACTER",
        SqlType::Bytea => "BLOB",
        SqlType::Time(_) | SqlType::TimeTz(_) => "TIME",
        SqlType::Interval { .. } => "INTERVAL",
        SqlType::Uuid => "UUID",
        SqlType::Array(_) | SqlType::Catalog(_) => "TEXT",
    }
}

fn identifier(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        name.to_owned()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

fn column_text(table: &TableSchema, column: &ColumnSchema) -> String {
    let mut text = format!(
        "{} {}",
        identifier(&column.name),
        sqlite_type_name(column.sql_type)
    );
    let single_key = table.primary_key.len() == 1 && table.primary_key[0] == column.name;
    if single_key {
        text.push_str(" PRIMARY KEY");
    } else if column.not_null {
        text.push_str(" NOT NULL");
    }
    if let Some(default) = &column.default {
        text.push_str(&format!(" DEFAULT {default}"));
    }
    text
}

pub(crate) fn create_text(table: &TableSchema) -> String {
    let mut parts: Vec<String> = table
        .columns
        .iter()
        .map(|column| column_text(table, column))
        .collect();
    if table.primary_key.len() > 1 {
        let key: Vec<String> = table
            .primary_key
            .iter()
            .map(|name| identifier(name))
            .collect();
        parts.push(format!("PRIMARY KEY ({})", key.join(", ")));
    }
    format!(
        "CREATE TABLE {} ({})",
        identifier(&table.name),
        parts.join(", ")
    )
}

fn text_literal(text: String) -> InsertValue {
    InsertValue::Expr(Expr {
        kind: ExprKind::Literal(Literal::String(text)),
        position: 0,
    })
}

fn null_literal() -> InsertValue {
    InsertValue::Expr(Expr {
        kind: ExprKind::Literal(Literal::Null),
        position: 0,
    })
}

fn integer_literal(number: i64) -> InsertValue {
    InsertValue::Expr(Expr {
        kind: ExprKind::Literal(Literal::Integer(number)),
        position: 0,
    })
}

fn column(name: &str, sql_type: Option<SqlType>) -> OutputColumn {
    OutputColumn {
        name: name.to_owned(),
        sql_type,
    }
}

fn rows_result(columns: Vec<OutputColumn>, rows: Vec<Vec<Value>>) -> QueryResult {
    QueryResult::rows(format!("SELECT {}", rows.len()), columns, rows)
}

fn table_info_columns() -> Vec<OutputColumn> {
    vec![
        column("cid", Some(SqlType::BigInt)),
        column("name", Some(SqlType::Text)),
        column("type", Some(SqlType::Text)),
        column("notnull", Some(SqlType::BigInt)),
        column("dflt_value", Some(SqlType::Text)),
        column("pk", Some(SqlType::BigInt)),
    ]
}

fn table_list_columns() -> Vec<OutputColumn> {
    vec![
        column("schema", Some(SqlType::Text)),
        column("name", Some(SqlType::Text)),
        column("type", Some(SqlType::Text)),
        column("ncol", Some(SqlType::BigInt)),
        column("wr", Some(SqlType::BigInt)),
        column("strict", Some(SqlType::BigInt)),
    ]
}

fn index_list_columns() -> Vec<OutputColumn> {
    vec![
        column("seq", Some(SqlType::BigInt)),
        column("name", Some(SqlType::Text)),
        column("unique", Some(SqlType::BigInt)),
        column("origin", Some(SqlType::Text)),
        column("partial", Some(SqlType::BigInt)),
    ]
}

fn index_info_columns() -> Vec<OutputColumn> {
    vec![
        column("seqno", Some(SqlType::BigInt)),
        column("cid", Some(SqlType::BigInt)),
        column("name", Some(SqlType::Text)),
    ]
}

fn table_list_row(schema: &str, name: &str, columns: usize) -> Vec<Value> {
    vec![
        Value::Text(schema.to_owned()),
        Value::Text(name.to_owned()),
        Value::Text("table".to_owned()),
        Value::BigInt(columns as i64),
        Value::BigInt(0),
        Value::BigInt(0),
    ]
}

fn flag(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "yes" => Some(true),
        "0" | "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

fn synchronous_level(value: &str) -> Option<i64> {
    match value.to_ascii_lowercase().as_str() {
        "0" | "off" => Some(0),
        "1" | "normal" => Some(1),
        "2" | "full" => Some(2),
        "3" | "extra" => Some(3),
        _ => None,
    }
}

fn single(name: &str, sql_type: SqlType, value: Value) -> QueryResult {
    rows_result(vec![column(name, Some(sql_type))], vec![vec![value]])
}

impl<'s, S: KeyValueStore + 's> Session<'s, S> {
    pub(super) fn pragma_columns(&self, pragma: &Pragma) -> Option<Vec<OutputColumn>> {
        match pragma.name.as_str() {
            "table_info" if pragma.argument.is_some() => Some(table_info_columns()),
            "table_list" => Some(table_list_columns()),
            "index_list" if pragma.argument.is_some() => Some(index_list_columns()),
            "index_info" if pragma.argument.is_some() => Some(index_info_columns()),
            "foreign_keys" | "user_version" | "synchronous" if !pragma.assigned => {
                Some(vec![column(&pragma.name, Some(SqlType::BigInt))])
            }
            "journal_mode" | "encoding" => Some(vec![column(&pragma.name, Some(SqlType::Text))]),
            _ => None,
        }
    }

    pub(super) fn run_pragma(&mut self, pragma: &Pragma) -> SqlResult<QueryResult> {
        let argument = pragma.argument.as_deref();
        let state = &self.settings.sqlite;
        let pragma_done = || QueryResult::command("PRAGMA");
        Ok(match (pragma.name.as_str(), argument) {
            ("table_info", Some(table)) => {
                let table = table.to_lowercase();
                let tables = self.read(|reader, _| read_tables(reader))?;
                let rows = match tables.iter().find(|schema| schema.name == table) {
                    Some(schema) => schema
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(index, column)| {
                            let key_position = schema
                                .primary_key
                                .iter()
                                .position(|name| *name == column.name)
                                .map_or(0, |position| position as i64 + 1);
                            let not_null = column.not_null && key_position == 0;
                            vec![
                                Value::BigInt(index as i64),
                                Value::Text(column.name.clone()),
                                Value::Text(sqlite_type_name(column.sql_type).to_owned()),
                                Value::BigInt(i64::from(not_null)),
                                column.default.clone().map_or(Value::Null, Value::Text),
                                Value::BigInt(key_position),
                            ]
                        })
                        .collect(),
                    None => Vec::new(),
                };
                rows_result(table_info_columns(), rows)
            }
            ("index_list", Some(table)) => {
                let table = table.to_lowercase();
                let tables = self.read(|reader, _| read_tables(reader))?;
                let rows = tables
                    .iter()
                    .filter(|schema| schema.name == table)
                    .flat_map(|schema| schema.indexes.iter().rev())
                    .enumerate()
                    .map(|(seq, index)| {
                        vec![
                            Value::BigInt(seq as i64),
                            Value::Text(index.name.clone()),
                            Value::BigInt(i64::from(index.unique)),
                            Value::Text(
                                if index.name.starts_with("sqlite_autoindex_") {
                                    "u"
                                } else {
                                    "c"
                                }
                                .to_owned(),
                            ),
                            Value::BigInt(0),
                        ]
                    })
                    .collect();
                rows_result(index_list_columns(), rows)
            }
            ("index_info", Some(name)) => {
                let tables = self.read(|reader, _| read_tables(reader))?;
                let mut rows = Vec::new();
                for schema in &tables {
                    for index in schema.indexes.iter().filter(|index| index.name == name) {
                        for (seqno, column) in index.columns.iter().enumerate() {
                            let cid = schema
                                .columns
                                .iter()
                                .position(|known| &known.name == column)
                                .map_or(-1, |position| position as i64);
                            rows.push(vec![
                                Value::BigInt(seqno as i64),
                                Value::BigInt(cid),
                                Value::Text(column.clone()),
                            ]);
                        }
                    }
                }
                rows_result(index_info_columns(), rows)
            }
            ("table_list", only) => {
                let tables = self.read(|reader, _| read_tables(reader))?;
                let mut rows: Vec<Vec<Value>> = tables
                    .iter()
                    .map(|schema| table_list_row("main", &schema.name, schema.columns.len()))
                    .collect();
                rows.push(table_list_row("main", "sqlite_schema", 5));
                rows.push(table_list_row("temp", "sqlite_temp_schema", 5));
                if let Some(only) = only {
                    rows.retain(|row| row[1] == Value::Text(only.to_lowercase()));
                }
                rows_result(table_list_columns(), rows)
            }
            ("foreign_keys", None) => single(
                "foreign_keys",
                SqlType::BigInt,
                Value::BigInt(i64::from(state.foreign_keys)),
            ),
            ("foreign_keys", Some(value)) => {
                if let Some(enabled) = flag(value) {
                    Arc::make_mut(&mut self.settings).sqlite.foreign_keys = enabled;
                }
                pragma_done()
            }
            ("user_version", None) => single(
                "user_version",
                SqlType::BigInt,
                Value::BigInt(state.user_version),
            ),
            ("user_version", Some(value)) => {
                let version = value
                    .parse::<i64>()
                    .ok()
                    .and_then(|version| i32::try_from(version).ok())
                    .unwrap_or(0);
                Arc::make_mut(&mut self.settings).sqlite.user_version = i64::from(version);
                pragma_done()
            }
            ("synchronous", None) => single(
                "synchronous",
                SqlType::BigInt,
                Value::BigInt(state.synchronous),
            ),
            ("synchronous", Some(value)) => {
                if let Some(level) = synchronous_level(value) {
                    Arc::make_mut(&mut self.settings).sqlite.synchronous = level;
                }
                pragma_done()
            }
            ("journal_mode", value) => {
                if let Some(mode) = value.and_then(|value| {
                    JOURNAL_MODES
                        .iter()
                        .find(|mode| mode.eq_ignore_ascii_case(value))
                }) {
                    Arc::make_mut(&mut self.settings).sqlite.journal_mode = mode;
                }
                single(
                    "journal_mode",
                    SqlType::Text,
                    Value::Text(self.settings.sqlite.journal_mode.to_owned()),
                )
            }
            ("encoding", None) => single("encoding", SqlType::Text, Value::Text("UTF-8".into())),
            _ => pragma_done(),
        })
    }

    pub(super) fn select_schema(
        &mut self,
        select: &Select,
        columns_only: bool,
    ) -> SqlResult<QueryResult> {
        let Some(from) = &select.from else {
            return Err(SqlError::new(
                SqlState::INTERNAL_ERROR,
                "a schema query without FROM",
            ));
        };
        let temporary = from.first.name.text.starts_with("sqlite_temp");
        let tables = if temporary {
            Vec::new()
        } else {
            self.read(|reader, _| read_tables(reader))?
        };
        let name = from.first.name.text.clone();
        let mut scratch = Scratch::default();
        let mut cache = TableCache::default();
        let create = CreateTable {
            comment: None,
            name: Name {
                text: name.clone(),
                position: 0,
            },
            if_not_exists: false,
            columns: SCHEMA_COLUMNS
                .iter()
                .map(|(column, sql_type)| ColumnDefinition {
                    name: Name {
                        text: (*column).to_owned(),
                        position: 0,
                    },
                    sql_type: *sql_type,
                    not_null: false,
                    primary_key: false,
                    default: None,
                    auto_increment: false,
                    generated_always: false,
                    unique: None,
                    constraints: Vec::new(),
                    comment: None,
                })
                .collect(),
            primary_key: None,
            primary_key_name: None,
            indexes: Vec::new(),
            constraints: Vec::new(),
        };
        execute::create_table(
            &mut scratch,
            &mut cache,
            &create,
            crate::Dialect::PostgreSql,
        )?;
        if !tables.is_empty() {
            let mut rootpage = 1;
            let mut rows = Vec::new();
            for table in &tables {
                rootpage += 1;
                rows.push(vec![
                    text_literal("table".to_owned()),
                    text_literal(table.name.clone()),
                    text_literal(table.name.clone()),
                    integer_literal(rootpage),
                    text_literal(create_text(table)),
                ]);
                for index in &table.indexes {
                    rootpage += 1;
                    let sql = if index.name.starts_with("sqlite_autoindex_") {
                        null_literal()
                    } else {
                        let statement =
                            index.create_statement_in(&table.name, crate::Dialect::Sqlite);
                        text_literal(statement.trim_end_matches(';').to_owned())
                    };
                    rows.push(vec![
                        text_literal("index".to_owned()),
                        text_literal(index.name.clone()),
                        text_literal(table.name.clone()),
                        integer_literal(rootpage),
                        sql,
                    ]);
                }
            }
            let insert = Insert {
                table: TableReference::named(
                    Name {
                        text: name,
                        position: 0,
                    },
                    None,
                ),
                columns: None,
                source: InsertSource::Values(rows),
                returning: None,
                on_duplicate: OnDuplicate::Fail,
                rollback_on_conflict: false,
                overriding: None,
            };
            execute::insert(&mut scratch, &mut cache, &insert)?;
        }
        execute::select(&scratch, &mut cache, select, columns_only)
    }

    pub(super) fn sqlite_after_statement(&mut self, result: &mut QueryResult) {
        result.notices.clear();
        result.warnings.clear();
        let changed = match result.tag.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["INSERT", _, count] | ["UPDATE", count] | ["DELETE", count] => count.parse().ok(),
            _ => None,
        };
        let state = &mut Arc::make_mut(&mut self.settings).sqlite;
        if let Some(changed) = changed {
            state.changes = changed;
            state.total_changes = state.total_changes.saturating_add(changed);
        }
        if let Some(rowid) = result.last_insert_rowid {
            state.last_insert_rowid = rowid;
        }
    }
}

pub(super) fn transaction_error(state: SqlState, message: &str) -> SqlError {
    SqlError::new(state, message).with_sqlite_code(1)
}
