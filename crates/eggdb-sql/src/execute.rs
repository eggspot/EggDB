use crate::ast::{
    BinaryOperator, CreateTable, Delete, Expr, ExprKind, Insert, InsertSource, InsertValue, Name,
    Select, SelectItem, TableReference, Update,
};
use crate::encoding::{
    Column, MAX_COLUMNS, TableDefinition, decode_meta, decode_row, encode_key_value, encode_meta,
    encode_row, encode_row_into, meta_key, prefix_end, rows_prefix, table_key,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, check_columns, compare_truth, output_name};
use crate::lexer::tokenize;
use crate::parser::Parser;
use crate::session::{OutputColumn, QueryResult};
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::{Conversion, SqlType, Value, compare_values, convert};
use std::cmp::Ordering;
use std::collections::HashMap;
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
    Scope {
        table: Some(table),
        label: reference.alias.as_deref().unwrap_or(&table.name),
    }
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
    u64::from_be_bytes(suffix).checked_add(1).ok_or_else(|| {
        SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("table \"{}\" has used every row identifier", table.name),
        )
    })
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
    let returning = match &insert.returning {
        Some(items) => Some(Projection::new(items, scope)?),
        None => None,
    };
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
    let constants = Evaluator {
        scope: Scope::empty(),
        row: &[],
    };
    let mut next_row_id = None;
    let mut result_rows = Vec::new();
    let mut key = rows_prefix(table.id);
    let prefix_length = key.len();
    let mut encoded = Vec::new();
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
        check_not_null(&table, &row)?;
        encoded.clear();
        encode_row_into(&mut encoded, &row);
        key.truncate(prefix_length);
        if table.primary_key.is_empty() {
            let id = match next_row_id {
                Some(id) => id,
                None => next_hidden_row_id(writer, &table)?,
            };
            next_row_id = Some(id.saturating_add(1));
            key.extend_from_slice(&id.to_be_bytes());
            writer.put(&key, &encoded)?;
        } else {
            for &index in &table.primary_key {
                encode_key_value(&mut key, &row[index]);
            }
            if !writer.put_if_absent(&key, &encoded)? {
                return Err(duplicate_key(&table, &row));
            }
        }
        if let Some(projection) = &returning {
            result_rows.push(projection.project(&row)?);
        }
    }
    let tag = format!("INSERT 0 {}", rows.len());
    Ok(match returning {
        Some(projection) => QueryResult::rows(tag, projection.columns, result_rows),
        None => QueryResult::command(tag),
    })
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
                    let Some(table) = scope.table else {
                        return Err(SqlError::syntax(
                            "SELECT * with no tables specified is not valid",
                            *position,
                        ));
                    };
                    if let Some(qualifier) = qualifier
                        && qualifier != scope.label
                    {
                        return Err(SqlError::new(
                            SqlState::UNDEFINED_TABLE,
                            format!("missing FROM-clause entry for table \"{qualifier}\""),
                        )
                        .at(*position));
                    }
                    for (index, column) in table.columns.iter().enumerate() {
                        columns.push(OutputColumn {
                            name: column.name.clone(),
                            sql_type: Some(column.sql_type),
                        });
                        projected.push(ProjectedItem::Column(index));
                    }
                }
                SelectItem::Expr { expr, alias } => {
                    check_columns(expr, scope)?;
                    let column = match &expr.kind {
                        ExprKind::Column { qualifier, name } => scope
                            .column(qualifier.as_deref(), name, expr.position)
                            .ok()
                            .and_then(|index| {
                                scope
                                    .table
                                    .map(|table| (index, table.columns[index].sql_type))
                            }),
                        _ => None,
                    };
                    columns.push(OutputColumn {
                        name: alias.clone().unwrap_or_else(|| output_name(expr)),
                        sql_type: column.map(|(_, sql_type)| sql_type),
                    });
                    projected.push(match column {
                        Some((index, _)) => ProjectedItem::Column(index),
                        None => ProjectedItem::Expr(expr),
                    });
                }
            }
        }
        let mut taken = vec![false; scope.table.map_or(0, |table| table.columns.len())];
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

    fn project_owned(&self, mut row: Vec<Value>) -> SqlResult<Vec<Value>> {
        if !self.each_column_once {
            return self.project(&row);
        }
        Ok(self
            .items
            .iter()
            .map(|item| match item {
                ProjectedItem::Column(index) => std::mem::replace(&mut row[*index], Value::Null),
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
                ProjectedItem::Column(index) => Ok(row[*index].clone()),
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

pub(crate) fn select(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    select: &Select,
) -> SqlResult<QueryResult> {
    let table = match &select.from {
        Some(reference) => Some(tables.require(reader, &reference.name)?),
        None => None,
    };
    let scope = match (&table, &select.from) {
        (Some(table), Some(reference)) => scope_of(table, reference),
        _ => Scope::empty(),
    };
    let projection = Projection::new(&select.items, scope)?;
    if let Some(filter) = &select.filter {
        check_columns(filter, scope)?;
    }
    let mut sort_keys = Vec::new();
    for item in &select.order_by {
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
                check_columns(&item.expr, scope)?;
                SortKey::Expr(&item.expr)
            }
        };
        sort_keys.push(key);
    }
    let limit = row_count(select.limit.as_ref(), "LIMIT")?;
    let offset = row_count(select.offset.as_ref(), "OFFSET")?.unwrap_or(0);
    let skip = usize::try_from(offset).unwrap_or(usize::MAX);
    let take = limit.map_or(usize::MAX, |limit| {
        usize::try_from(limit).unwrap_or(usize::MAX)
    });
    let source = SourceRows {
        reader,
        table: table.as_deref(),
        scope,
        filter: select.filter.as_ref(),
    };
    let output = if sort_keys.is_empty() {
        unsorted_rows(&source, &projection, skip, take)?
    } else {
        sorted_rows(
            &source,
            &select.order_by,
            &projection,
            &sort_keys,
            skip,
            take,
        )?
    };
    let tag = format!("SELECT {}", output.len());
    Ok(QueryResult::rows(tag, projection.columns, output))
}

struct SourceRows<'a> {
    reader: &'a dyn KeyValueRead,
    table: Option<&'a TableDefinition>,
    scope: Scope<'a>,
    filter: Option<&'a Expr>,
}

impl SourceRows<'_> {
    fn for_each(&self, visit: &mut dyn FnMut(Vec<Value>) -> SqlResult<bool>) -> SqlResult<()> {
        match self.table {
            Some(table) => for_each_matching_row(
                self.reader,
                table,
                self.scope,
                self.filter,
                &mut |_, row| visit(row),
            ),
            None => {
                if filter_matches(self.scope, self.filter, &[])? {
                    visit(Vec::new())?;
                }
                Ok(())
            }
        }
    }
}

fn unsorted_rows(
    source: &SourceRows<'_>,
    projection: &Projection<'_>,
    skip: usize,
    take: usize,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut output = Vec::new();
    if take == 0 && source.table.is_some() {
        return Ok(output);
    }
    let mut skipped = 0;
    source.for_each(&mut |row| {
        let projected = projection.project_owned(row)?;
        if skipped < skip {
            skipped += 1;
            return Ok(true);
        }
        if output.len() < take {
            output.push(projected);
        }
        Ok(output.len() < take)
    })?;
    Ok(output)
}

fn sorted_rows(
    source: &SourceRows<'_>,
    order_by: &[crate::ast::OrderItem],
    projection: &Projection<'_>,
    sort_keys: &[SortKey<'_>],
    skip: usize,
    take: usize,
) -> SqlResult<Vec<Vec<Value>>> {
    let scope = source.scope;
    let mut rows = Vec::new();
    source.for_each(&mut |row| {
        let output = projection.project(&row)?;
        let evaluator = Evaluator { scope, row: &row };
        let mut keys = Vec::with_capacity(sort_keys.len());
        for key in sort_keys {
            keys.push(match key {
                SortKey::Output(index) => output[*index].clone(),
                SortKey::Expr(expr) => evaluator.eval(expr, None)?,
            });
        }
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
        .skip(skip)
        .take(take)
        .map(|(_, row)| row)
        .collect())
}

fn check_sortable(
    order_by: &[crate::ast::OrderItem],
    rows: &[(Vec<Value>, Vec<Value>)],
) -> SqlResult<()> {
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
            check_columns(expr, scope)?;
        }
    }
    if let Some(filter) = &update.filter {
        check_columns(filter, scope)?;
    }
    let returning = match &update.returning {
        Some(items) => Some(Projection::new(items, scope)?),
        None => None,
    };
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

pub(crate) fn delete(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    delete: &Delete,
) -> SqlResult<QueryResult> {
    let table = tables.require(writer, &delete.table.name)?;
    let scope = scope_of(&table, &delete.table);
    if let Some(filter) = &delete.filter {
        check_columns(filter, scope)?;
    }
    let returning = match &delete.returning {
        Some(items) => Some(Projection::new(items, scope)?),
        None => None,
    };
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
