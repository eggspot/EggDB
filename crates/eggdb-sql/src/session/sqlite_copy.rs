use super::sqlite::{kept_create_text, kept_index_text, listed_views};
use super::{QueryResult, Session};
use crate::ast::VacuumInto;
use crate::encoding::TableDefinition;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::schema::{IndexSchema, TableSchema, read_tables};
use crate::store::KeyValueRead;
use crate::types::Value;
use eggdb_sqlite_format::{KeyOrder, SqliteObject, SqliteRow, SqliteValue};

const CONSTRAINT_INDEX_PREFIX: &str = "sqlite_autoindex_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConstraintKind {
    PrimaryKey,
    Unique,
}

struct Word {
    upper: String,
    depth: usize,
}

fn not_written_yet(what: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("VACUUM INTO does not write {what} yet"),
    )
    .at(position)
}

fn closing_quote(chars: &[char], open: usize) -> usize {
    let close = if chars[open] == '[' { ']' } else { chars[open] };
    let mut at = open + 1;
    while at < chars.len() {
        if chars[at] == close {
            if close != ']' && chars.get(at + 1) == Some(&close) {
                at += 2;
                continue;
            }
            return at + 1;
        }
        at += 1;
    }
    chars.len()
}

fn words_of(text: &str) -> Vec<Word> {
    let chars: Vec<char> = text.chars().collect();
    let mut words = Vec::new();
    let mut depth = 0usize;
    let mut at = 0;
    while at < chars.len() {
        match chars[at] {
            '\'' | '"' | '`' | '[' => at = closing_quote(&chars, at),
            '(' => {
                depth += 1;
                at += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                at += 1;
            }
            '-' if chars.get(at + 1) == Some(&'-') => {
                while at < chars.len() && chars[at] != '\n' {
                    at += 1;
                }
            }
            letter if letter.is_ascii_alphabetic() || letter == '_' => {
                let start = at;
                while at < chars.len()
                    && (chars[at].is_ascii_alphanumeric() || chars[at] == '_' || chars[at] == '$')
                {
                    at += 1;
                }
                words.push(Word {
                    upper: chars[start..at]
                        .iter()
                        .collect::<String>()
                        .to_ascii_uppercase(),
                    depth,
                });
            }
            _ => at += 1,
        }
    }
    words
}

fn without_rowid(words: &[Word]) -> bool {
    words.windows(2).any(|pair| {
        pair[0].depth == 0
            && pair[1].depth == 0
            && pair[0].upper == "WITHOUT"
            && pair[1].upper == "ROWID"
    })
}

fn non_binary_collation(words: &[Word]) -> bool {
    words
        .windows(2)
        .any(|pair| pair[0].upper == "COLLATE" && pair[1].upper != "BINARY")
}

fn has_top_level_where(words: &[Word]) -> bool {
    words
        .iter()
        .any(|word| word.depth == 0 && word.upper == "WHERE")
}

fn constraint_order(words: &[Word]) -> Vec<ConstraintKind> {
    words
        .iter()
        .filter(|word| word.depth == 1)
        .filter_map(|word| match word.upper.as_str() {
            "PRIMARY" => Some(ConstraintKind::PrimaryKey),
            "UNIQUE" => Some(ConstraintKind::Unique),
            _ => None,
        })
        .collect()
}

fn rowid_alias(definition: &TableDefinition) -> Option<usize> {
    match definition.primary_key.as_slice() {
        [only]
            if definition
                .columns
                .get(*only)
                .is_some_and(|column| column.auto_increment) =>
        {
            Some(*only)
        }
        _ => None,
    }
}

fn integer_of(value: &Value) -> Option<i64> {
    match value {
        Value::SmallInt(number) => Some(i64::from(*number)),
        Value::Integer(number) => Some(i64::from(*number)),
        Value::BigInt(number) => Some(*number),
        _ => None,
    }
}

fn sqlite_value(value: &Value) -> SqliteValue {
    match value {
        Value::Null => SqliteValue::Null,
        Value::Boolean(flag) => SqliteValue::Integer(i64::from(*flag)),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => {
            integer_of(value).map_or(SqliteValue::Null, SqliteValue::Integer)
        }
        Value::Real(number) => SqliteValue::Real(f64::from(*number)),
        Value::Double(number) => SqliteValue::Real(*number),
        Value::Text(text) | Value::Bpchar(text) | Value::Json(text) | Value::Jsonb(text) => {
            SqliteValue::Text(text.clone())
        }
        Value::Bytea(bytes) => SqliteValue::Blob(bytes.clone()),
        Value::Numeric(_) => {
            let text = value.to_text().unwrap_or_default();
            if let Ok(number) = text.parse::<i64>() {
                SqliteValue::Integer(number)
            } else if let Ok(number) = text.parse::<f64>() {
                SqliteValue::Real(number)
            } else {
                SqliteValue::Text(text)
            }
        }
        other => other.to_text().map_or(SqliteValue::Null, SqliteValue::Text),
    }
}

struct TableCopy<'t> {
    definition: &'t TableDefinition,
    rows: Vec<Vec<Value>>,
    rowids: Vec<i64>,
    alias: Option<usize>,
}

impl TableCopy<'_> {
    fn column(&self, name: &str, position: usize) -> SqlResult<usize> {
        self.definition
            .columns
            .iter()
            .position(|column| column.name == name)
            .ok_or_else(|| not_written_yet("indexes on expressions", position))
    }

    fn columns_of(&self, index: &IndexSchema, position: usize) -> SqlResult<Vec<usize>> {
        index
            .columns
            .iter()
            .map(|name| self.column(name, position))
            .collect()
    }

    fn key_value(&self, values: &[Value], column: usize, rowid: i64) -> SqliteValue {
        if Some(column) == self.alias {
            return SqliteValue::Integer(rowid);
        }
        values.get(column).map_or(SqliteValue::Null, sqlite_value)
    }

    fn entries(&self, columns: &[usize]) -> Vec<Vec<SqliteValue>> {
        self.rows
            .iter()
            .zip(&self.rowids)
            .map(|(values, rowid)| {
                let mut entry: Vec<SqliteValue> = columns
                    .iter()
                    .map(|column| self.key_value(values, *column, *rowid))
                    .collect();
                entry.push(SqliteValue::Integer(*rowid));
                entry
            })
            .collect()
    }

    fn records(&self) -> SqlResult<Vec<SqliteRow>> {
        let virtual_columns: Vec<usize> = self
            .definition
            .generated
            .iter()
            .filter(|generated| !generated.stored)
            .map(|generated| generated.column)
            .collect();
        Ok(self
            .rows
            .iter()
            .zip(&self.rowids)
            .map(|(values, rowid)| SqliteRow {
                rowid: *rowid,
                values: (0..self.definition.columns.len())
                    .filter(|column| !virtual_columns.contains(column))
                    .map(|column| {
                        if Some(column) == self.alias {
                            SqliteValue::Null
                        } else {
                            values.get(column).map_or(SqliteValue::Null, sqlite_value)
                        }
                    })
                    .collect(),
            })
            .collect())
    }
}

fn constraint_keys(
    copy: &TableCopy<'_>,
    table: &TableSchema,
    words: &[Word],
    position: usize,
) -> SqlResult<Vec<Vec<usize>>> {
    let primary_key = (copy.alias.is_none() && !copy.definition.primary_key.is_empty())
        .then(|| copy.definition.primary_key.clone());
    let mut uniques = table
        .indexes
        .iter()
        .filter(|index| index.name.starts_with(CONSTRAINT_INDEX_PREFIX));
    let mut keys = Vec::new();
    let mut primary_key_placed = primary_key.is_none();
    for kind in constraint_order(words) {
        match kind {
            ConstraintKind::PrimaryKey => {
                if let (false, Some(key)) = (primary_key_placed, &primary_key) {
                    keys.push(key.clone());
                    primary_key_placed = true;
                }
            }
            ConstraintKind::Unique => {
                if let Some(index) = uniques.next() {
                    keys.push(copy.columns_of(index, position)?);
                }
            }
        }
    }
    if let (false, Some(key)) = (primary_key_placed, primary_key) {
        keys.insert(0, key);
    }
    for index in uniques {
        keys.push(copy.columns_of(index, position)?);
    }
    Ok(keys)
}

fn table_objects(
    reader: &dyn KeyValueRead,
    cache: &mut TableCache,
    table: &TableSchema,
    position: usize,
    objects: &mut Vec<SqliteObject>,
) -> SqlResult<()> {
    let text = kept_create_text(reader, table)?;
    let words = words_of(&text);
    if without_rowid(&words) {
        return Err(not_written_yet("WITHOUT ROWID tables", position));
    }
    let definition = cache.table(reader, &table.name)?.ok_or_else(|| {
        SqlError::new(
            SqlState::INTERNAL_ERROR,
            format!("table \"{}\" vanished while it was copied", table.name),
        )
    })?;
    let alias = rowid_alias(&definition);
    let rows: Vec<Vec<Value>> = crate::execute::every_row(reader, &definition)?
        .into_iter()
        .map(|(_, values)| values)
        .collect();
    let mut rowids = Vec::with_capacity(rows.len());
    let mut counted = 0i64;
    for values in &rows {
        let rowid = match alias {
            Some(column) => values.get(column).and_then(integer_of).ok_or_else(|| {
                SqlError::new(
                    SqlState::INTERNAL_ERROR,
                    format!("a row of \"{}\" has no integer row id", table.name),
                )
            })?,
            None => {
                counted += 1;
                counted
            }
        };
        rowids.push(rowid);
    }
    let copy = TableCopy {
        definition: &definition,
        rows,
        rowids,
        alias,
    };
    objects.push(SqliteObject::Table {
        name: table.name.clone(),
        sql: text.clone(),
        rows: copy.records()?,
    });
    let keys = constraint_keys(&copy, table, &words, position)?;
    let explicit: Vec<&IndexSchema> = table
        .indexes
        .iter()
        .filter(|index| !index.name.starts_with(CONSTRAINT_INDEX_PREFIX))
        .collect();
    if (!keys.is_empty() || !explicit.is_empty()) && non_binary_collation(&words) {
        return Err(not_written_yet(
            "indexes of a table with NOCASE or RTRIM columns",
            position,
        ));
    }
    for (number, key) in keys.iter().enumerate() {
        objects.push(SqliteObject::Index {
            name: format!("{CONSTRAINT_INDEX_PREFIX}{}_{}", table.name, number + 1),
            table: table.name.clone(),
            sql: None,
            entries: copy.entries(key),
            orders: Vec::new(),
        });
    }
    for index in explicit {
        let Some(sql) = kept_index_text(reader, table, index)? else {
            continue;
        };
        let index_words = words_of(&sql);
        if has_top_level_where(&index_words) {
            return Err(not_written_yet("partial indexes", position));
        }
        if non_binary_collation(&index_words) {
            return Err(not_written_yet("indexes with a NOCASE or RTRIM key", position));
        }
        let columns = copy.columns_of(index, position)?;
        objects.push(SqliteObject::Index {
            name: index.name.clone(),
            table: table.name.clone(),
            sql: Some(sql),
            entries: copy.entries(&columns),
            orders: index
                .orders
                .iter()
                .map(|order| KeyOrder {
                    descending: order.descending,
                })
                .collect(),
        });
    }
    Ok(())
}

fn sqlite_objects(
    reader: &dyn KeyValueRead,
    cache: &mut TableCache,
    position: usize,
) -> SqlResult<Vec<SqliteObject>> {
    let mut objects = Vec::new();
    for table in read_tables(reader)? {
        if table.schema != crate::schemas::DEFAULT_SCHEMA
            || crate::temporary::is_temporary_name(&table.name)
        {
            continue;
        }
        table_objects(reader, cache, &table, position, &mut objects)?;
    }
    for view in listed_views(reader)? {
        if let Some(sql) = view.text {
            objects.push(SqliteObject::View {
                name: view.name,
                sql,
            });
        }
    }
    for listed in crate::triggers::every_trigger(reader)? {
        for trigger in &listed.triggers {
            let sql = if trigger.dialect == crate::Dialect::Sqlite {
                crate::sqlite_schema_text::stored_trigger_text(&trigger.text)
            } else {
                trigger.text.clone()
            };
            objects.push(SqliteObject::Trigger {
                name: trigger.name.clone(),
                table: listed.table.clone(),
                sql,
            });
        }
    }
    Ok(objects)
}

impl Session<'_> {
    pub(super) fn vacuum_into(&mut self, vacuum: &VacuumInto) -> SqlResult<QueryResult> {
        let position = vacuum.position;
        if self.in_block() {
            return Err(SqlError::new(
                SqlState::ACTIVE_SQL_TRANSACTION,
                "cannot VACUUM from within a transaction",
            )
            .at(position));
        }
        let Some(files) = self.new_file_writer else {
            return Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "VACUUM INTO writes a file, which only an embedded database does; a server writes no files for its clients",
            )
            .at(position));
        };
        let objects = self.read(|reader, cache| sqlite_objects(reader, cache, position))?;
        let bytes = eggdb_sqlite_format::write_database(&objects).map_err(|error| {
            SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, error.to_string()).at(position)
        })?;
        files
            .write_new_file(&vacuum.path, &bytes)
            .map_err(|message| SqlError::new(SqlState::IO_ERROR, message).at(position))?;
        Ok(QueryResult::command("VACUUM"))
    }
}

#[cfg(test)]
#[path = "sqlite_copy_tests.rs"]
mod tests;
