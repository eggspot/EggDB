use crate::aggregate::{self, shape_of};
use crate::ast::{
    BinaryOperator, CreateTable, Delete, Expr, ExprKind, Insert, InsertSource, InsertValue, Name,
    OnDuplicate, OrderItem, Select, SelectItem, TableReference, Update,
};
use crate::encoding::{
    Column, MAX_COLUMNS, TableDefinition, decode_meta, decode_row, decode_row_into,
    encode_key_value, encode_meta, encode_row, encode_row_into, meta_key, prefix_end, rows_prefix,
    table_key,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{
    Evaluator, Scope, Source, check_columns, children_mut, compare_truth, output_name,
    reject_aggregates,
};
use crate::join::{self, JoinedRows, RowCountHints};
use crate::lexer::tokenize;
use crate::memory::WorkMemory;
use crate::mysql_errors::Warning;
use crate::parser::{LiteralRows, LiteralToken, Parser};
use crate::session::{OutputColumn, QueryResult};
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::{Conversion, SqlType, Value, compare_values, convert, equality_key};
use crate::typing;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const DELETE_BATCH: usize = 1024;

fn undefined_table(name: &Name) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_TABLE,
        format!("relation \"{}\" does not exist", name.text),
    )
    .at(name.position)
}

const CACHED_TABLES: usize = 256;

struct CachedTable {
    bytes: Vec<u8>,
    table: Arc<TableDefinition>,
    checked_at: Option<u64>,
}

#[derive(Default)]
pub(crate) struct TableCache {
    entries: HashMap<String, CachedTable>,
    row_counts: RowCountHints,
}

impl TableCache {
    fn table(
        &mut self,
        reader: &dyn KeyValueRead,
        name: &str,
    ) -> SqlResult<Option<Arc<TableDefinition>>> {
        let version = reader.committed_version();
        if let Some(cached) = self.entries.get(name)
            && version.is_some()
            && cached.checked_at == version
        {
            return Ok(Some(Arc::clone(&cached.table)));
        }
        let Some(bytes) = reader.get(&table_key(name))? else {
            return Ok(None);
        };
        if let Some(cached) = self.entries.get_mut(name)
            && cached.bytes == bytes
        {
            cached.checked_at = version;
            return Ok(Some(Arc::clone(&cached.table)));
        }
        let table = Arc::new(TableDefinition::decode(name, &bytes)?);
        if self.entries.len() >= CACHED_TABLES {
            self.entries.clear();
        }
        self.entries.insert(
            name.to_owned(),
            CachedTable {
                bytes,
                table: Arc::clone(&table),
                checked_at: version,
            },
        );
        Ok(Some(table))
    }

    fn require(
        &mut self,
        reader: &dyn KeyValueRead,
        name: &Name,
    ) -> SqlResult<Arc<TableDefinition>> {
        self.table(reader, &name.text)?
            .ok_or_else(|| undefined_table(name))
    }
}

fn scope_of<'a>(table: &'a TableDefinition, reference: &'a TableReference) -> Scope<'a> {
    Scope::One(Source {
        table,
        label: reference.alias.as_deref().unwrap_or(&table.name),
        offset: 0,
    })
}

pub(crate) fn create_table(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreateTable,
) -> SqlResult<QueryResult> {
    if create.columns.len() > MAX_COLUMNS {
        return Err(SqlError::new(
            SqlState::TOO_MANY_COLUMNS,
            format!("tables can have at most {MAX_COLUMNS} columns"),
        ));
    }
    let mut columns: Vec<Column> = Vec::new();
    let mut primary_key = Vec::new();
    let mut primary_key_position = None;
    for (index, definition) in create.columns.iter().enumerate() {
        if columns
            .iter()
            .any(|column| column.name == definition.name.text)
        {
            return Err(SqlError::new(
                SqlState::DUPLICATE_COLUMN,
                format!(
                    "column \"{}\" specified more than once",
                    definition.name.text
                ),
            )
            .at(definition.name.position));
        }
        if let Some((expr, _)) = &definition.default {
            check_default(expr)?;
        }
        if definition.primary_key {
            if primary_key_position.is_some() {
                return Err(multiple_primary_keys(create, definition.name.position));
            }
            primary_key_position = Some(definition.name.position);
            primary_key.push(index);
        }
        columns.push(Column {
            name: definition.name.text.clone(),
            sql_type: definition.sql_type,
            not_null: definition.not_null,
            default: definition.default.as_ref().map(|(_, text)| text.clone()),
            auto_increment: definition.auto_increment,
        });
    }
    if let Some((names, position)) = &create.primary_key {
        if primary_key_position.is_some() {
            return Err(multiple_primary_keys(create, *position));
        }
        for name in names {
            let Some(index) = columns.iter().position(|column| column.name == name.text) else {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("column \"{}\" named in key does not exist", name.text),
                )
                .at(name.position));
            };
            if !primary_key.contains(&index) {
                primary_key.push(index);
            }
        }
    }
    for &index in &primary_key {
        columns[index].not_null = true;
    }
    check_auto_increment(create, &columns, &primary_key)?;
    if tables.table(writer, &create.name.text)?.is_some() {
        if create.if_not_exists {
            return Ok(QueryResult::command("CREATE TABLE").with_notice(format!(
                "NOTICE:  relation \"{}\" already exists, skipping",
                create.name.text
            )));
        }
        return Err(SqlError::new(
            SqlState::DUPLICATE_TABLE,
            format!("relation \"{}\" already exists", create.name.text),
        )
        .at(create.name.position));
    }
    let next_id = match writer.get(&meta_key())? {
        Some(bytes) => decode_meta(&bytes)?,
        None => 1,
    };
    let following = next_id.checked_add(1).ok_or_else(|| {
        SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            "the database has used every table identifier",
        )
    })?;
    let table = TableDefinition {
        id: next_id,
        name: create.name.text.clone(),
        columns,
        primary_key,
    };
    writer.put(&meta_key(), &encode_meta(following))?;
    writer.put(&table_key(&table.name), &table.encode())?;
    Ok(QueryResult::command("CREATE TABLE"))
}

fn check_auto_increment(
    create: &CreateTable,
    columns: &[Column],
    primary_key: &[usize],
) -> SqlResult<()> {
    let auto: Vec<usize> = (0..columns.len())
        .filter(|&index| columns[index].auto_increment)
        .collect();
    let Some(&first) = auto.first() else {
        return Ok(());
    };
    let integer = matches!(
        columns[first].sql_type,
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
    );
    if auto.len() > 1
        || primary_key.first() != Some(&first)
        || !integer
        || columns[first].default.is_some()
    {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "Incorrect table definition; there can be only one auto column and it must be defined as a key",
        )
        .with_hint("EggDB needs the AUTO_INCREMENT column to be an integer, without a DEFAULT, and the first column of the primary key.")
        .at(create.columns[first].name.position));
    }
    Ok(())
}

fn multiple_primary_keys(create: &CreateTable, position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TABLE_DEFINITION,
        format!(
            "multiple primary keys for table \"{}\" are not allowed",
            create.name.text
        ),
    )
    .at(position)
}

fn check_default(expr: &Expr) -> SqlResult<()> {
    reject_aggregates(expr, "DEFAULT expressions")?;
    check_columns(expr, Scope::empty()).map_err(|error| {
        if error.state() == SqlState::UNDEFINED_COLUMN || error.state() == SqlState::UNDEFINED_TABLE
        {
            SqlError::not_supported("cannot use column reference in DEFAULT expression")
                .at(expr.position)
        } else {
            error
        }
    })
}

pub(crate) fn drop_tables(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    names: &[Name],
    if_exists: bool,
) -> SqlResult<QueryResult> {
    let mut result = QueryResult::command("DROP TABLE");
    for name in names {
        let Some(table) = tables.table(writer, &name.text)? else {
            if if_exists {
                result = result.with_notice(format!(
                    "NOTICE:  table \"{}\" does not exist, skipping",
                    name.text
                ));
                continue;
            }
            return Err(undefined_table(name));
        };
        let prefix = rows_prefix(table.id);
        let end = prefix_end(&prefix);
        loop {
            let mut keys = Vec::new();
            writer.scan(&prefix, &end, ScanDirection::Forward, &mut |key, _| {
                keys.push(key.to_vec());
                Ok(keys.len() < DELETE_BATCH)
            })?;
            if keys.is_empty() {
                break;
            }
            for key in &keys {
                writer.delete(key)?;
            }
        }
        writer.delete(&table_key(&table.name))?;
    }
    Ok(result)
}

fn row_key(table: &TableDefinition, row: &[Value]) -> Vec<u8> {
    let mut key = rows_prefix(table.id);
    for &index in &table.primary_key {
        encode_key_value(&mut key, &row[index]);
    }
    key
}

fn next_hidden_row_id(reader: &dyn KeyValueRead, table: &TableDefinition) -> SqlResult<u64> {
    let prefix = rows_prefix(table.id);
    let mut last = None;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Backward,
        &mut |key, _| {
            last = Some(key.to_vec());
            Ok(false)
        },
    )?;
    let Some(key) = last else {
        return Ok(1);
    };
    let suffix: [u8; 8] = key
        .get(prefix.len()..)
        .and_then(|suffix| suffix.try_into().ok())
        .ok_or_else(|| crate::encoding::corrupt("row key"))?;
    u64::from_be_bytes(suffix)
        .checked_add(1)
        .ok_or_else(|| row_identifiers_used(table))
}

fn row_identifiers_used(table: &TableDefinition) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("table \"{}\" has used every row identifier", table.name),
    )
}

fn default_value(column: &Column) -> SqlResult<Value> {
    let Some(text) = &column.default else {
        return Ok(Value::Null);
    };
    let tokens = tokenize(text)?;
    let mut parser = Parser::new(text, &tokens);
    let expr = parser.expression()?;
    if !parser.at_end() {
        return Err(crate::encoding::corrupt("column default"));
    }
    let evaluator = Evaluator {
        scope: Scope::empty(),
        row: &[],
    };
    let value = evaluator
        .eval(&expr, Some(column.sql_type))
        .map_err(|error| SqlError::new(error.state(), error.message().to_owned()))?;
    assign(value, column)
}

fn assign(value: Value, column: &Column) -> SqlResult<Value> {
    let source = value.sql_type();
    convert(value, column.sql_type, Conversion::Assignment).map_err(|error| {
        let source = source.map_or_else(|| "unknown".to_owned(), SqlType::name);
        if error.state() == SqlState::CANNOT_COERCE {
            SqlError::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "column \"{}\" is of type {} but expression is of type {source}",
                    column.name,
                    column.sql_type.name()
                ),
            )
            .with_hint("You will need to rewrite or cast the expression.")
        } else {
            error
        }
    })
}

fn assigned_value(
    evaluator: &Evaluator<'_>,
    value: &InsertValue,
    column: &Column,
) -> SqlResult<Value> {
    match value {
        InsertValue::Default => default_value(column),
        InsertValue::Expr(expr) => {
            let evaluated = evaluator.eval(expr, Some(column.sql_type))?;
            assign(evaluated, column).map_err(|error| error.at_if_unplaced(expr.position))
        }
    }
}

fn check_not_null(table: &TableDefinition, row: &[Value]) -> SqlResult<()> {
    for (column, value) in table.columns.iter().zip(row) {
        if column.not_null && value.is_null() {
            let shown: Vec<String> = row
                .iter()
                .map(|value| value.to_text().unwrap_or_else(|| "null".into()))
                .collect();
            return Err(SqlError::new(
                SqlState::NOT_NULL_VIOLATION,
                format!(
                    "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                    column.name, table.name
                ),
            )
            .with_detail(format!("Failing row contains ({}).", shown.join(", "))));
        }
    }
    Ok(())
}

fn duplicate_key(table: &TableDefinition, row: &[Value]) -> SqlError {
    let names: Vec<&str> = table
        .primary_key
        .iter()
        .map(|&index| table.columns[index].name.as_str())
        .collect();
    let values: Vec<String> = table
        .primary_key
        .iter()
        .map(|&index| row[index].to_text().unwrap_or_else(|| "null".into()))
        .collect();
    SqlError::new(
        SqlState::UNIQUE_VIOLATION,
        format!(
            "duplicate key value violates unique constraint \"{}_pkey\"",
            table.name
        ),
    )
    .with_detail(format!(
        "Key ({})=({}) already exists.",
        names.join(", "),
        values.join(", ")
    ))
}

fn target_columns(table: &TableDefinition, names: &[Name]) -> SqlResult<Vec<usize>> {
    let mut indexes = Vec::with_capacity(names.len());
    for name in names {
        let index = table.column_index(&name.text).ok_or_else(|| {
            SqlError::new(
                SqlState::UNDEFINED_COLUMN,
                format!(
                    "column \"{}\" of relation \"{}\" does not exist",
                    name.text, table.name
                ),
            )
            .at(name.position)
        })?;
        if indexes.contains(&index) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column \"{}\" specified more than once", name.text),
            )
            .at(name.position));
        }
        indexes.push(index);
    }
    Ok(indexes)
}

pub(crate) fn insert(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    insert: &Insert,
) -> SqlResult<QueryResult> {
    let table = tables.require(writer, &insert.table.name)?;
    let scope = scope_of(&table, &insert.table);
    let targets = match &insert.columns {
        Some(names) => target_columns(&table, names)?,
        None => (0..table.columns.len()).collect(),
    };
    let returning = returning(&insert.returning, scope)?;
    let empty_row: Vec<Vec<InsertValue>> = vec![Vec::new()];
    let rows = match &insert.source {
        InsertSource::Values(rows) => rows,
        InsertSource::DefaultValues => &empty_row,
    };
    if let Some(first) = rows.first() {
        if rows.iter().any(|row| row.len() != first.len()) {
            return Err(SqlError::syntax(
                "VALUES lists must all be the same length",
                insert.table.name.position,
            ));
        }
        if first.len() > targets.len() {
            return Err(SqlError::syntax(
                "INSERT has more expressions than target columns",
                insert.table.name.position,
            ));
        }
        if insert.columns.is_some() && first.len() < targets.len() {
            return Err(SqlError::syntax(
                "INSERT has more target columns than expressions",
                insert.table.name.position,
            ));
        }
    }
    if let InsertSource::Values(rows) = &insert.source {
        for value in rows.iter().flatten() {
            if let InsertValue::Expr(expr) = value {
                reject_aggregates(expr, "VALUES")?;
            }
        }
    }
    let constants = Evaluator {
        scope: Scope::empty(),
        row: &[],
    };
    let updates = match &insert.on_duplicate {
        OnDuplicate::Update(assignments) => Some(resolved_updates(&table, scope, assignments)?),
        _ => None,
    };
    let mut stored = StoredRows::new(&table);
    let mut result_rows = Vec::new();
    let mut affected = 0usize;
    let mut warnings = Vec::new();
    for values in rows {
        let mut row = Vec::with_capacity(table.columns.len());
        for (index, column) in table.columns.iter().enumerate() {
            let given = targets
                .iter()
                .position(|&target| target == index)
                .and_then(|slot| values.get(slot));
            row.push(match given {
                Some(value) => assigned_value(&constants, value, column)?,
                None => default_value(column)?,
            });
        }
        if stored.try_insert(writer, &mut row)? {
            affected += 1;
        } else {
            match (&insert.on_duplicate, &updates) {
                (OnDuplicate::Ignore, _) => warnings.push(Warning {
                    level: "Warning",
                    code: 1062,
                    message: crate::mysql_errors::translate(duplicate_key(&table, &row), "", "")
                        .message()
                        .to_owned(),
                }),
                (OnDuplicate::Replace, _) => {
                    writer.put(&stored.key, &stored.encoded)?;
                    affected += 2;
                }
                (OnDuplicate::Update(_), Some(updates)) => {
                    affected += stored.update_existing(writer, scope, updates, &row)?;
                }
                _ => return Err(duplicate_key(&table, &row)),
            }
        }
        if let Some(projection) = &returning {
            result_rows.push(projection.project(&row)?);
        }
    }
    let tag = format!("INSERT 0 {affected}");
    let mut result = match returning {
        Some(projection) => QueryResult::rows(tag, projection.columns, result_rows),
        None => QueryResult::command(tag),
    };
    result.warnings = warnings;
    result.last_insert_id = stored.first_generated;
    result.last_insert_rowid = stored.last_rowid;
    Ok(result)
}

fn resolved_updates(
    table: &TableDefinition,
    scope: Scope<'_>,
    assignments: &[(Name, Expr)],
) -> SqlResult<Vec<(usize, Expr)>> {
    let mut updates = Vec::with_capacity(assignments.len());
    for (name, expr) in assignments {
        let index = target_columns(table, std::slice::from_ref(name))?[0];
        let mut expr = expr.clone();
        point_inserted_values(&mut expr, table)?;
        reject_aggregates(&expr, "ON DUPLICATE KEY UPDATE")?;
        check_columns(&expr, scope)?;
        updates.push((index, expr));
    }
    Ok(updates)
}

fn point_inserted_values(expr: &mut Expr, table: &TableDefinition) -> SqlResult<()> {
    if let ExprKind::Function { name, arguments } = &expr.kind
        && name == "values"
        && let [argument] = arguments.as_slice()
        && let ExprKind::Column { name: column, .. } = &argument.kind
    {
        let index = table.column_index(column).ok_or_else(|| {
            SqlError::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{column}\" does not exist"),
            )
            .at(argument.position)
        })?;
        expr.kind = ExprKind::Slot {
            index: table.columns.len() + index,
            sql_type: Some(table.columns[index].sql_type),
        };
        return Ok(());
    }
    for child in children_mut(expr) {
        point_inserted_values(child, table)?;
    }
    Ok(())
}

struct StoredRows<'a> {
    table: &'a TableDefinition,
    key: Vec<u8>,
    prefix_length: usize,
    encoded: Vec<u8>,
    next_row_id: NextRowId,
    auto_increment: Option<usize>,
    next_auto_value: Option<i64>,
    first_generated: Option<u64>,
    last_rowid: Option<i64>,
}

enum NextRowId {
    Unknown,
    Next(u64),
    Exhausted,
}

impl<'a> StoredRows<'a> {
    fn new(table: &'a TableDefinition) -> Self {
        let key = rows_prefix(table.id);
        Self {
            table,
            prefix_length: key.len(),
            key,
            encoded: Vec::new(),
            next_row_id: NextRowId::Unknown,
            auto_increment: table.auto_increment_column(),
            next_auto_value: None,
            first_generated: None,
            last_rowid: None,
        }
    }

    fn highest_auto_value(&self, reader: &dyn KeyValueRead, index: usize) -> SqlResult<i64> {
        let prefix = rows_prefix(self.table.id);
        let mut last = None;
        reader.scan(
            &prefix,
            &prefix_end(&prefix),
            ScanDirection::Backward,
            &mut |_, value| {
                last = Some(value.to_vec());
                Ok(false)
            },
        )?;
        let Some(bytes) = last else {
            return Ok(0);
        };
        let row = decode_row(&bytes, &self.table.columns)?;
        Ok(row
            .get(index)
            .and_then(Value::as_integer)
            .unwrap_or(0)
            .max(0))
    }

    fn fill_auto_increment(
        &mut self,
        reader: &dyn KeyValueRead,
        row: &mut [Value],
    ) -> SqlResult<()> {
        let Some(index) = self.auto_increment else {
            return Ok(());
        };
        let column = &self.table.columns[index];
        let given = row[index].as_integer();
        let next = match self.next_auto_value {
            Some(next) => next,
            None => self
                .highest_auto_value(reader, index)?
                .checked_add(1)
                .ok_or_else(|| auto_increment_exhausted(column))?,
        };
        match given {
            None | Some(0) => {
                row[index] = convert(Value::BigInt(next), column.sql_type, Conversion::Assignment)
                    .map_err(|_| auto_increment_exhausted(column))?;
                self.first_generated.get_or_insert(next as u64);
                self.next_auto_value = next.checked_add(1);
            }
            Some(value) if value >= next => self.next_auto_value = value.checked_add(1),
            Some(_) => self.next_auto_value = Some(next),
        }
        Ok(())
    }

    fn update_existing(
        &mut self,
        writer: &mut dyn KeyValueWrite,
        scope: Scope<'_>,
        updates: &[(usize, Expr)],
        inserted: &[Value],
    ) -> SqlResult<usize> {
        let table = self.table;
        let existing_bytes = writer
            .get(&self.key)?
            .ok_or_else(|| crate::encoding::corrupt("row"))?;
        let existing = decode_row(&existing_bytes, &table.columns)?;
        let mut combined = existing.clone();
        combined.extend_from_slice(inserted);
        for (index, expr) in updates {
            let column = &table.columns[*index];
            let value = Evaluator {
                scope,
                row: &combined,
            }
            .eval(expr, Some(column.sql_type))?;
            combined[*index] =
                assign(value, column).map_err(|error| error.at_if_unplaced(expr.position))?;
        }
        combined.truncate(table.columns.len());
        if combined == existing {
            return Ok(0);
        }
        check_not_null(table, &combined)?;
        let new_key = row_key(table, &combined);
        let encoded = encode_row(&combined);
        if new_key == self.key {
            writer.put(&self.key, &encoded)?;
        } else {
            writer.delete(&self.key)?;
            if !writer.put_if_absent(&new_key, &encoded)? {
                return Err(duplicate_key(table, &combined));
            }
        }
        Ok(2)
    }

    fn insert(&mut self, writer: &mut dyn KeyValueWrite, row: &mut [Value]) -> SqlResult<()> {
        if self.try_insert(writer, row)? {
            Ok(())
        } else {
            Err(duplicate_key(self.table, row))
        }
    }

    fn try_insert(&mut self, writer: &mut dyn KeyValueWrite, row: &mut [Value]) -> SqlResult<bool> {
        let table = self.table;
        if self.auto_increment.is_some() {
            self.fill_auto_increment(writer, row)?;
        }
        check_not_null(table, row)?;
        self.encoded.clear();
        encode_row_into(&mut self.encoded, row);
        self.key.truncate(self.prefix_length);
        if table.primary_key.is_empty() {
            let id = match self.next_row_id {
                NextRowId::Unknown => next_hidden_row_id(writer, table)?,
                NextRowId::Next(id) => id,
                NextRowId::Exhausted => return Err(row_identifiers_used(table)),
            };
            self.next_row_id = id
                .checked_add(1)
                .map_or(NextRowId::Exhausted, NextRowId::Next);
            self.key.extend_from_slice(&id.to_be_bytes());
            writer.put(&self.key, &self.encoded)?;
            self.last_rowid = i64::try_from(id).ok();
            return Ok(true);
        }
        for &index in &table.primary_key {
            encode_key_value(&mut self.key, &row[index]);
        }
        let inserted = writer.put_if_absent(&self.key, &self.encoded)?;
        if inserted && let Some(index) = self.auto_increment {
            self.last_rowid = row[index].as_integer();
        }
        Ok(inserted)
    }
}

fn auto_increment_exhausted(column: &Column) -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!(
            "the AUTO_INCREMENT column \"{}\" has used every value of type {}",
            column.name,
            column.sql_type.name()
        ),
    )
}

fn literal_value(literal: &LiteralToken<'_>, column: &Column, position: usize) -> SqlResult<Value> {
    let hint = Some(column.sql_type);
    let value = match literal {
        LiteralToken::Null => Ok(Value::Null),
        LiteralToken::Boolean(flag) => Ok(Value::Boolean(*flag)),
        LiteralToken::Integer(number) => {
            crate::eval::literal_value(&crate::ast::Literal::Integer(*number), hint)
        }
        LiteralToken::Decimal(text) => crate::eval::decimal_constant(text, hint),
        LiteralToken::String(text) => crate::eval::string_constant(text, hint),
    }
    .map_err(|error| error.at_if_unplaced(position))?;
    assign(value, column).map_err(|error| error.at_if_unplaced(position))
}

pub(crate) fn insert_literals(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    insert: &LiteralRows<'_>,
) -> SqlResult<QueryResult> {
    let table = tables.require(writer, &insert.table.name)?;
    if insert.width > table.columns.len() {
        return Err(SqlError::syntax(
            "INSERT has more expressions than target columns",
            insert.table.name.position,
        ));
    }
    let mut stored = StoredRows::new(&table);
    let mut literals = Vec::with_capacity(insert.width);
    let mut row = Vec::with_capacity(table.columns.len());
    let mut at = insert.first_row();
    for _ in 0..insert.rows {
        insert.next_row(&mut at, &mut literals);
        row.clear();
        for (index, column) in table.columns.iter().enumerate() {
            row.push(match literals.get(index) {
                Some((position, literal)) => literal_value(literal, column, *position)?,
                None => default_value(column)?,
            });
        }
        stored.insert(writer, &mut row)?;
    }
    let mut result = QueryResult::command(format!("INSERT 0 {}", insert.rows));
    result.last_insert_id = stored.first_generated;
    result.last_insert_rowid = stored.last_rowid;
    Ok(result)
}

struct Projection<'a> {
    columns: Vec<OutputColumn>,
    items: Vec<ProjectedItem<'a>>,
    scope: Scope<'a>,
    each_column_once: bool,
}

enum ProjectedItem<'a> {
    Column(usize),
    Expr(&'a Expr),
}

fn returning<'a>(
    items: &'a Option<Vec<SelectItem>>,
    scope: Scope<'a>,
) -> SqlResult<Option<Projection<'a>>> {
    let Some(items) = items else {
        return Ok(None);
    };
    for item in items {
        if let SelectItem::Expr { expr, .. } = item {
            reject_aggregates(expr, "RETURNING")?;
        }
    }
    Projection::new(items, scope).map(Some)
}

impl<'a> Projection<'a> {
    fn new(items: &'a [SelectItem], scope: Scope<'a>) -> SqlResult<Self> {
        let mut columns = Vec::with_capacity(items.len());
        let mut projected = Vec::with_capacity(items.len());
        for item in items {
            match item {
                SelectItem::Wildcard {
                    qualifier,
                    position,
                } => {
                    if matches!(scope, Scope::Empty) {
                        return Err(SqlError::syntax(
                            "SELECT * with no tables specified is not valid",
                            *position,
                        ));
                    }
                    let offsets: Vec<usize> = match qualifier {
                        None => scope.star(),
                        Some(qualifier) => {
                            let source = scope
                                .sources()
                                .iter()
                                .find(|source| source.label == qualifier)
                                .ok_or_else(|| {
                                    SqlError::new(
                                        SqlState::UNDEFINED_TABLE,
                                        format!(
                                            "missing FROM-clause entry for table \"{qualifier}\""
                                        ),
                                    )
                                    .at(*position)
                                })?;
                            (source.offset..source.offset + source.table.columns.len()).collect()
                        }
                    };
                    for offset in offsets {
                        if let Some((source, index)) = scope.source_of(offset) {
                            let column = &source.table.columns[index];
                            columns.push(OutputColumn {
                                name: column.name.clone(),
                                sql_type: Some(column.sql_type),
                            });
                            projected.push(ProjectedItem::Column(offset));
                        }
                    }
                }
                SelectItem::Expr { expr, alias } => {
                    check_columns(expr, scope)?;
                    let column = match &expr.kind {
                        ExprKind::Column { qualifier, name } => scope
                            .column(qualifier.as_deref(), name, expr.position)
                            .ok()
                            .map(|offset| (offset, scope.column_type(offset))),
                        ExprKind::Slot { index, sql_type } => Some((*index, *sql_type)),
                        _ => None,
                    };
                    columns.push(OutputColumn {
                        name: alias.clone().unwrap_or_else(|| output_name(expr)),
                        sql_type: match column {
                            Some((_, sql_type)) => sql_type,
                            None => typing::static_type(expr, scope),
                        },
                    });
                    projected.push(match column {
                        Some((index, _)) => ProjectedItem::Column(index),
                        None => ProjectedItem::Expr(expr),
                    });
                }
            }
        }
        let width = projected
            .iter()
            .filter_map(|item| match item {
                ProjectedItem::Column(index) => Some(index + 1),
                ProjectedItem::Expr(_) => None,
            })
            .max()
            .unwrap_or(0);
        let mut taken = vec![false; width];
        let each_column_once = projected.iter().all(|item| match item {
            ProjectedItem::Column(index) => !std::mem::replace(&mut taken[*index], true),
            ProjectedItem::Expr(_) => false,
        });
        Ok(Self {
            columns,
            items: projected,
            scope,
            each_column_once,
        })
    }

    fn output_named(&self, name: &str, position: usize) -> SqlResult<usize> {
        let mut found: Option<(usize, Option<usize>)> = None;
        for (index, column) in self.columns.iter().enumerate() {
            if column.name != name {
                continue;
            }
            let source = self.source_column(index);
            match found {
                None => found = Some((index, source)),
                Some((_, earlier)) if earlier.is_some() && earlier == source => {}
                Some(_) => {
                    return Err(SqlError::new(
                        SqlState::AMBIGUOUS_COLUMN,
                        format!("ORDER BY \"{name}\" is ambiguous"),
                    )
                    .at(position));
                }
            }
        }
        found.map(|(index, _)| index).ok_or_else(|| {
            SqlError::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{name}\" does not exist"),
            )
            .at(position)
        })
    }

    fn source_column(&self, index: usize) -> Option<usize> {
        match &self.items[index] {
            ProjectedItem::Column(column) => Some(*column),
            ProjectedItem::Expr(_) => None,
        }
    }

    fn output_of(&self, expr: &Expr) -> Option<usize> {
        let column = match &expr.kind {
            ExprKind::Column { qualifier, name } => self
                .scope
                .column(qualifier.as_deref(), name, expr.position)
                .ok(),
            ExprKind::Slot { index, .. } => Some(*index),
            _ => None,
        };
        if let Some(column) = column {
            return (0..self.items.len()).find(|&index| self.source_column(index) == Some(column));
        }
        let shape = shape_of(expr);
        self.items.iter().position(|item| match item {
            ProjectedItem::Expr(projected) => shape_of(projected) == shape,
            ProjectedItem::Column(_) => false,
        })
    }

    fn project_owned(&self, mut row: Vec<Value>) -> SqlResult<Vec<Value>> {
        if !self.each_column_once {
            return self.project(&row);
        }
        Ok(self
            .items
            .iter()
            .map(|item| match item {
                ProjectedItem::Column(index) => row
                    .get_mut(*index)
                    .map_or(Value::Null, |value| std::mem::replace(value, Value::Null)),
                ProjectedItem::Expr(_) => Value::Null,
            })
            .collect())
    }

    fn project(&self, row: &[Value]) -> SqlResult<Vec<Value>> {
        let evaluator = Evaluator {
            scope: self.scope,
            row,
        };
        self.items
            .iter()
            .map(|item| match item {
                ProjectedItem::Column(index) => Ok(row.get(*index).cloned().unwrap_or(Value::Null)),
                ProjectedItem::Expr(expr) => evaluator.eval(expr, None),
            })
            .collect()
    }
}

fn filter_matches(scope: Scope<'_>, filter: Option<&Expr>, row: &[Value]) -> SqlResult<bool> {
    let Some(filter) = filter else {
        return Ok(true);
    };
    let evaluator = Evaluator { scope, row };
    Ok(evaluator.boolean(filter, "WHERE")? == Some(true))
}

fn is_constant(expr: &Expr) -> bool {
    check_columns(expr, Scope::empty()).is_ok()
}

fn key_equality<'e>(expr: &'e Expr, scope: Scope<'_>, index: usize) -> Option<&'e Expr> {
    match &expr.kind {
        ExprKind::And(operands) => operands
            .iter()
            .find_map(|operand| key_equality(operand, scope, index)),
        ExprKind::Binary(BinaryOperator::Equal, left, right) => {
            let names_column = |expr: &Expr| {
                matches!(&expr.kind, ExprKind::Column { qualifier, name }
                    if scope.column(qualifier.as_deref(), name, 0).ok() == Some(index))
            };
            if names_column(left) && is_constant(right) {
                Some(right)
            } else if names_column(right) && is_constant(left) {
                Some(left)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn point_key(table: &TableDefinition, scope: Scope<'_>, filter: &Expr) -> Option<Vec<u8>> {
    if table.primary_key.is_empty() {
        return None;
    }
    let constants = Evaluator {
        scope: Scope::empty(),
        row: &[],
    };
    let mut key = rows_prefix(table.id);
    for &index in &table.primary_key {
        let column = &table.columns[index];
        let found = key_equality(filter, scope, index)?;
        let original = constants.eval(found, Some(column.sql_type)).ok()?;
        let converted = convert(original.clone(), column.sql_type, Conversion::Assignment).ok()?;
        if converted.is_null() || compare_values(&converted, &original) != Ordering::Equal {
            return None;
        }
        encode_key_value(&mut key, &converted);
    }
    Some(key)
}

type RowVisitor<'v> = dyn FnMut(&[u8], Vec<Value>) -> SqlResult<bool> + 'v;

fn for_each_matching_row(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    scope: Scope<'_>,
    filter: Option<&Expr>,
    visit: &mut RowVisitor<'_>,
) -> SqlResult<()> {
    if let Some(filter) = filter
        && let Some(key) = point_key(table, scope, filter)
    {
        let mut found = None;
        reader.get_with(&key, &mut |bytes| {
            found = Some(decode_row(bytes, &table.columns)?);
            Ok(())
        })?;
        if let Some(row) = found {
            let filter_is_the_key = table.primary_key.len() == 1
                && matches!(filter.kind, ExprKind::Binary(BinaryOperator::Equal, ..));
            if filter_is_the_key || filter_matches(scope, Some(filter), &row)? {
                visit(&key, row)?;
            }
        }
        return Ok(());
    }
    let prefix = rows_prefix(table.id);
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, bytes| {
            let row = decode_row(bytes, &table.columns)?;
            if filter_matches(scope, filter, &row)? {
                return visit(key, row);
            }
            Ok(true)
        },
    )
}

fn matching_rows(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    scope: Scope<'_>,
    filter: Option<&Expr>,
) -> SqlResult<Vec<(Vec<u8>, Vec<Value>)>> {
    let mut rows = Vec::new();
    for_each_matching_row(reader, table, scope, filter, &mut |key, row| {
        rows.push((key.to_vec(), row));
        Ok(true)
    })?;
    Ok(rows)
}

fn row_count(expr: Option<&Expr>, clause: &str) -> SqlResult<Option<u64>> {
    let Some(expr) = expr else {
        return Ok(None);
    };
    let constants = Evaluator {
        scope: Scope::empty(),
        row: &[],
    };
    reject_aggregates(expr, clause)?;
    check_columns(expr, Scope::empty()).map_err(|_| {
        SqlError::syntax(
            format!("argument of {clause} must not contain variables"),
            expr.position,
        )
    })?;
    let value = convert(
        constants.eval(expr, Some(SqlType::BigInt))?,
        SqlType::BigInt,
        Conversion::Assignment,
    )
    .map_err(|error| error.at_if_unplaced(expr.position))?;
    match value {
        Value::Null => Ok(None),
        Value::BigInt(count) if count < 0 => Err(if clause == "LIMIT" {
            SqlError::new(
                SqlState::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE,
                "LIMIT must not be negative",
            )
        } else {
            SqlError::new(
                SqlState::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE,
                "OFFSET must not be negative",
            )
        }),
        Value::BigInt(count) => Ok(Some(count as u64)),
        _ => Ok(None),
    }
}

enum SortKey<'a> {
    Output(usize),
    Expr(&'a Expr),
}

fn sort_keys<'e>(
    order_by: &[OrderItem],
    expressions: &[&'e Expr],
    projection: &Projection<'_>,
    scope: Scope<'_>,
    distinct: bool,
) -> SqlResult<Vec<SortKey<'e>>> {
    let mut keys = Vec::with_capacity(order_by.len());
    for (item, &expr) in order_by.iter().zip(expressions) {
        let key = match &item.expr.kind {
            ExprKind::Literal(crate::ast::Literal::Integer(position)) => {
                let index = usize::try_from(*position)
                    .ok()
                    .filter(|&index| index >= 1 && index <= projection.columns.len())
                    .ok_or_else(|| {
                        SqlError::new(
                            SqlState::INVALID_COLUMN_REFERENCE,
                            format!("ORDER BY position {position} is not in select list"),
                        )
                        .at(item.expr.position)
                    })?;
                SortKey::Output(index - 1)
            }
            ExprKind::Column {
                qualifier: None,
                name,
            } if projection.columns.iter().any(|column| &column.name == name) => {
                SortKey::Output(projection.output_named(name, item.expr.position)?)
            }
            _ => {
                check_columns(expr, scope)?;
                if distinct {
                    let index = projection.output_of(expr).ok_or_else(|| {
                        SqlError::new(
                            SqlState::INVALID_COLUMN_REFERENCE,
                            "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
                        )
                        .at(item.expr.position)
                    })?;
                    SortKey::Output(index)
                } else {
                    SortKey::Expr(expr)
                }
            }
        };
        keys.push(key);
    }
    Ok(keys)
}

pub(crate) fn returning_columns(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    table: &TableReference,
    items: &Option<Vec<SelectItem>>,
) -> SqlResult<Option<Vec<OutputColumn>>> {
    let definition = tables.require(reader, &table.name)?;
    let scope = scope_of(&definition, table);
    Ok(returning(items, scope)?.map(|projection| projection.columns))
}

pub(crate) fn select(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    select: &Select,
    columns_only: bool,
) -> SqlResult<QueryResult> {
    let first = match &select.from {
        Some(from) => Some(tables.require(reader, &from.first.name)?),
        None => None,
    };
    let mut definitions: Vec<Arc<TableDefinition>> = Vec::new();
    if let (Some(from), Some(first)) = (&select.from, &first)
        && !from.joins.is_empty()
    {
        definitions.push(Arc::clone(first));
        for join in &from.joins {
            definitions.push(tables.require(reader, &join.table.name)?);
        }
    }
    let joined = match &select.from {
        Some(from) if !from.joins.is_empty() => Some(join::resolve(from, &definitions)?),
        _ => None,
    };
    let scope = match (&select.from, &joined, &first) {
        (_, Some(joined), _) => Scope::Many(&joined.sources),
        (Some(from), None, Some(table)) => scope_of(table, &from.first),
        _ => Scope::empty(),
    };
    if let Some(filter) = &select.filter {
        reject_aggregates(filter, "WHERE")?;
        check_columns(filter, scope)?;
    }
    let grouping = if aggregate::is_aggregate_query(select) {
        Some(aggregate::analyze(select, scope)?)
    } else {
        None
    };
    let (items, post_scope) = match &grouping {
        Some(grouping) => (grouping.items.as_slice(), Scope::empty()),
        None => (select.items.as_slice(), scope),
    };
    let projection = Projection::new(items, post_scope)?;
    let order_expressions: Vec<&Expr> = select
        .order_by
        .iter()
        .enumerate()
        .map(|(index, item)| {
            grouping
                .as_ref()
                .and_then(|grouping| grouping.order[index].as_ref())
                .unwrap_or(&item.expr)
        })
        .collect();
    let sort_keys = sort_keys(
        &select.order_by,
        &order_expressions,
        &projection,
        post_scope,
        select.distinct,
    )?;
    if columns_only {
        return Ok(QueryResult::rows(
            "SELECT 0".to_owned(),
            projection.columns,
            Vec::new(),
        ));
    }
    let limit = row_count(select.limit.as_ref(), "LIMIT")?;
    let offset = row_count(select.offset.as_ref(), "OFFSET")?.unwrap_or(0);
    let window = Window {
        skip: usize::try_from(offset).unwrap_or(usize::MAX),
        take: limit.map_or(usize::MAX, |limit| {
            usize::try_from(limit).unwrap_or(usize::MAX)
        }),
        distinct: select.distinct,
    };
    let mut memory = WorkMemory::new();
    let bound_filter = match (&joined, &select.filter) {
        (Some(_), Some(filter)) => Some(aggregate::bind(filter, scope)?),
        _ => None,
    };
    let rows = match (&joined, first.as_deref()) {
        (Some(joined), _) => {
            let first = joined.sources.list[0];
            let first_key = select
                .filter
                .as_ref()
                .and_then(|filter| point_key(first.table, Scope::One(first), filter));
            RowSource::Joined(JoinedRows::new(
                reader,
                joined,
                first_key,
                joined.needed_columns(select),
                joined.filters_by_level(bound_filter.as_ref()),
                &mut tables.row_counts,
                &mut memory,
            )?)
        }
        (None, Some(table)) => RowSource::Table(table),
        (None, None) => RowSource::Nothing,
    };
    let mut source = SourceRows {
        reader,
        rows,
        scope,
        filter: if joined.is_some() {
            None
        } else {
            select.filter.as_ref()
        },
    };
    if let Some(count) = select.source_row_limit {
        let wanted = usize::try_from(count).unwrap_or(usize::MAX);
        let kept = first_rows(|visit| source.for_each(visit), wanted, &mut memory)?;
        source = SourceRows {
            reader,
            rows: RowSource::Rows(kept),
            scope,
            filter: None,
        };
    }
    if let Some(grouping) = &grouping {
        let groups = grouping.run(
            scope,
            &mut |visit| source.for_each_borrowed(visit),
            &mut memory,
        )?;
        source = SourceRows {
            reader,
            rows: RowSource::Rows(groups),
            scope: post_scope,
            filter: None,
        };
    }
    let output = if sort_keys.is_empty() {
        unsorted_rows(&mut source, &projection, window, &mut memory)?
    } else {
        sorted_rows(
            &mut source,
            &select.order_by,
            &projection,
            &sort_keys,
            window,
            &mut memory,
        )?
    };
    let tag = format!("SELECT {}", output.len());
    Ok(QueryResult::rows(tag, projection.columns, output))
}

pub(crate) fn first_rows(
    for_each: impl FnOnce(&mut dyn FnMut(Vec<Value>) -> SqlResult<bool>) -> SqlResult<()>,
    wanted: usize,
    memory: &mut WorkMemory,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut kept = Vec::new();
    if wanted > 0 {
        for_each(&mut |row| {
            memory.charge_row(&row)?;
            kept.push(row);
            Ok(kept.len() < wanted)
        })?;
    }
    Ok(kept)
}

#[derive(Clone, Copy)]
struct Window {
    skip: usize,
    take: usize,
    distinct: bool,
}

enum RowSource<'a> {
    Nothing,
    Table(&'a TableDefinition),
    Joined(JoinedRows<'a>),
    Rows(Vec<Vec<Value>>),
}

struct SourceRows<'a> {
    reader: &'a dyn KeyValueRead,
    rows: RowSource<'a>,
    scope: Scope<'a>,
    filter: Option<&'a Expr>,
}

impl SourceRows<'_> {
    fn has_rows_to_read(&self) -> bool {
        !matches!(self.rows, RowSource::Nothing)
    }

    fn for_each(&mut self, visit: &mut dyn FnMut(Vec<Value>) -> SqlResult<bool>) -> SqlResult<()> {
        let (reader, scope, filter) = (self.reader, self.scope, self.filter);
        match &mut self.rows {
            RowSource::Table(table) => {
                for_each_matching_row(reader, table, scope, filter, &mut |_, row| visit(row))
            }
            RowSource::Joined(joined) => joined.for_each(&mut |row| {
                if filter_matches(scope, filter, row)? {
                    visit(row.to_vec())
                } else {
                    Ok(true)
                }
            }),
            RowSource::Rows(rows) => {
                for row in std::mem::take(rows) {
                    if !visit(row)? {
                        break;
                    }
                }
                Ok(())
            }
            RowSource::Nothing => {
                if filter_matches(scope, filter, &[])? {
                    visit(Vec::new())?;
                }
                Ok(())
            }
        }
    }
}

impl SourceRows<'_> {
    fn for_each_borrowed(
        &mut self,
        visit: &mut dyn FnMut(&[Value]) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        let (reader, scope, filter) = (self.reader, self.scope, self.filter);
        match &mut self.rows {
            RowSource::Joined(joined) => joined.for_each(&mut |row| {
                if filter_matches(scope, filter, row)? {
                    visit(row)
                } else {
                    Ok(true)
                }
            }),
            RowSource::Table(table)
                if filter.is_none_or(|filter| point_key(table, scope, filter).is_none()) =>
            {
                let prefix = rows_prefix(table.id);
                let mut row = Vec::with_capacity(table.columns.len());
                reader.scan(
                    &prefix,
                    &prefix_end(&prefix),
                    ScanDirection::Forward,
                    &mut |_, bytes| {
                        row.clear();
                        decode_row_into(bytes, &table.columns, None, &mut row)?;
                        if filter_matches(scope, filter, &row)? {
                            visit(&row)
                        } else {
                            Ok(true)
                        }
                    },
                )
            }
            _ => self.for_each(&mut |row| visit(&row)),
        }
    }
}

fn first_time_seen(
    seen: &mut HashSet<Vec<u8>>,
    row: &[Value],
    memory: &mut WorkMemory,
) -> SqlResult<bool> {
    let mut key = Vec::new();
    for value in row {
        equality_key(&mut key, value);
    }
    if seen.contains(&key) {
        return Ok(false);
    }
    memory.charge(key.len() + 32)?;
    seen.insert(key);
    Ok(true)
}

fn unsorted_rows(
    source: &mut SourceRows<'_>,
    projection: &Projection<'_>,
    window: Window,
    memory: &mut WorkMemory,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut output = Vec::new();
    if window.take == 0 && source.has_rows_to_read() {
        return Ok(output);
    }
    let mut skipped = 0;
    let mut seen = window.distinct.then(HashSet::new);
    source.for_each(&mut |row| {
        let projected = projection.project_owned(row)?;
        if let Some(seen) = &mut seen
            && !first_time_seen(seen, &projected, memory)?
        {
            return Ok(true);
        }
        if skipped < window.skip {
            skipped += 1;
            return Ok(true);
        }
        if output.len() < window.take {
            memory.charge_row(&projected)?;
            output.push(projected);
        }
        Ok(output.len() < window.take)
    })?;
    Ok(output)
}

fn sorted_rows(
    source: &mut SourceRows<'_>,
    order_by: &[OrderItem],
    projection: &Projection<'_>,
    sort_keys: &[SortKey<'_>],
    window: Window,
    memory: &mut WorkMemory,
) -> SqlResult<Vec<Vec<Value>>> {
    let scope = source.scope;
    let mut rows = Vec::new();
    let mut seen = window.distinct.then(HashSet::new);
    source.for_each(&mut |row| {
        let output = projection.project(&row)?;
        if let Some(seen) = &mut seen
            && !first_time_seen(seen, &output, memory)?
        {
            return Ok(true);
        }
        let evaluator = Evaluator { scope, row: &row };
        let mut keys = Vec::with_capacity(sort_keys.len());
        for key in sort_keys {
            keys.push(match key {
                SortKey::Output(index) => output[*index].clone(),
                SortKey::Expr(expr) => evaluator.eval(expr, None)?,
            });
        }
        memory.charge_row(&output)?;
        memory.charge_row(&keys)?;
        rows.push((keys, output));
        Ok(true)
    })?;
    check_sortable(order_by, &rows)?;
    rows.sort_by(|(left, _), (right, _)| {
        for (index, item) in order_by.iter().enumerate() {
            let nulls_first = item.nulls_first.unwrap_or(item.descending);
            let ordering = match (&left[index], &right[index]) {
                (Value::Null, Value::Null) => Ordering::Equal,
                (Value::Null, _) => {
                    if nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (_, Value::Null) => {
                    if nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (a, b) => {
                    let ordering = compare_values(a, b);
                    if item.descending {
                        ordering.reverse()
                    } else {
                        ordering
                    }
                }
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    });
    Ok(rows
        .into_iter()
        .skip(window.skip)
        .take(window.take)
        .map(|(_, row)| row)
        .collect())
}

fn check_sortable(order_by: &[OrderItem], rows: &[(Vec<Value>, Vec<Value>)]) -> SqlResult<()> {
    for (index, item) in order_by.iter().enumerate() {
        let mut seen: Option<&Value> = None;
        for (keys, _) in rows {
            let value = &keys[index];
            if value.is_null() {
                continue;
            }
            if let Some(previous) = seen {
                compare_truth(previous, value, "<")
                    .map_err(|error| error.at(item.expr.position))?;
            } else {
                seen = Some(value);
            }
        }
    }
    Ok(())
}

pub(crate) fn update(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    update: &Update,
) -> SqlResult<QueryResult> {
    let table = tables.require(writer, &update.table.name)?;
    let scope = scope_of(&table, &update.table);
    let names: Vec<Name> = update
        .assignments
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    let targets = target_columns(&table, &names).map_err(|error| {
        if error.state() == SqlState::DUPLICATE_COLUMN {
            SqlError::syntax(
                format!(
                    "multiple assignments to same column \"{}\"",
                    error
                        .message()
                        .trim_start_matches("column \"")
                        .split('"')
                        .next()
                        .unwrap_or("")
                ),
                error.position().unwrap_or(0),
            )
        } else {
            error
        }
    })?;
    for (_, value) in &update.assignments {
        if let InsertValue::Expr(expr) = value {
            reject_aggregates(expr, "UPDATE")?;
            check_columns(expr, scope)?;
        }
    }
    if let Some(filter) = &update.filter {
        reject_aggregates(filter, "WHERE")?;
        check_columns(filter, scope)?;
    }
    let returning = returning(&update.returning, scope)?;
    let matched = matching_rows(writer, &table, scope, update.filter.as_ref())?;
    let mut changes = Vec::with_capacity(matched.len());
    for (old_key, old_row) in matched {
        let evaluator = Evaluator {
            scope,
            row: &old_row,
        };
        let mut new_row = old_row.clone();
        for ((_, value), &index) in update.assignments.iter().zip(&targets) {
            new_row[index] = assigned_value(&evaluator, value, &table.columns[index])?;
        }
        check_not_null(&table, &new_row)?;
        let new_key = if table.primary_key.is_empty() {
            old_key.clone()
        } else {
            row_key(&table, &new_row)
        };
        changes.push((old_key, new_key, new_row));
    }
    for (old_key, new_key, _) in &changes {
        if old_key != new_key {
            writer.delete(old_key)?;
        }
    }
    let mut result_rows = Vec::new();
    for (old_key, new_key, new_row) in &changes {
        if old_key != new_key && writer.get(new_key)?.is_some() {
            return Err(duplicate_key(&table, new_row));
        }
        writer.put(new_key, &encode_row(new_row))?;
        if let Some(projection) = &returning {
            result_rows.push(projection.project(new_row)?);
        }
    }
    let tag = format!("UPDATE {}", changes.len());
    Ok(match returning {
        Some(projection) => QueryResult::rows(tag, projection.columns, result_rows),
        None => QueryResult::command(tag),
    })
}

pub(crate) fn truncate(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &TableReference,
) -> SqlResult<QueryResult> {
    let every_row = Delete {
        table: table.clone(),
        filter: None,
        returning: None,
    };
    delete(writer, tables, &every_row)?;
    Ok(QueryResult::command("TRUNCATE TABLE".to_owned()))
}

pub(crate) fn delete(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    delete: &Delete,
) -> SqlResult<QueryResult> {
    let table = tables.require(writer, &delete.table.name)?;
    let scope = scope_of(&table, &delete.table);
    if let Some(filter) = &delete.filter {
        reject_aggregates(filter, "WHERE")?;
        check_columns(filter, scope)?;
    }
    let returning = returning(&delete.returning, scope)?;
    let matched = matching_rows(writer, &table, scope, delete.filter.as_ref())?;
    let mut result_rows = Vec::new();
    for (key, row) in &matched {
        writer.delete(key)?;
        if let Some(projection) = &returning {
            result_rows.push(projection.project(row)?);
        }
    }
    let tag = format!("DELETE {}", matched.len());
    Ok(match returning {
        Some(projection) => QueryResult::rows(tag, projection.columns, result_rows),
        None => QueryResult::command(tag),
    })
}
