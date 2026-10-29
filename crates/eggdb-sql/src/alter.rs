use crate::ast::{
    AlterAction, AlterTable, ColumnDefinition, ConstraintKind, ConstraintSpec, CreateIndex,
    CreateSequence, IndexSpec, Name, SequenceOptions,
};
use crate::constraints::{self, PreparedChecks};
use crate::dialect::Dialect;
use crate::encoding::CommentKind;
use crate::encoding::{
    Column, MAX_COLUMNS, TableDefinition, decode_meta, decode_row, encode_meta, encode_row,
    meta_key, prefix_end, reference_key, rows_prefix, table_key,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::indexes;
use crate::memory::WorkMemory;
use crate::session::QueryResult;
use crate::store::{KeyValueWrite, ScanDirection};
use crate::types::{Conversion, SqlType, Value, convert};
use std::sync::Arc;

const REWRITE_BATCH: usize = 1024;

#[cold]
#[inline(never)]
fn missing_column(table: &TableDefinition, name: &Name, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!("Can't DROP '{}'; check that column/key exists", name.text),
        )
        .with_vendor_code(1091),
        _ => SqlError::new(
            SqlState::UNDEFINED_COLUMN,
            format!(
                "column \"{}\" of relation \"{}\" does not exist",
                name.text, table.name
            ),
        ),
    }
    .at(name.position)
}

fn column_index(table: &TableDefinition, name: &Name, dialect: Dialect) -> SqlResult<usize> {
    table
        .column_index(&name.text)
        .ok_or_else(|| missing_column(table, name, dialect))
}

fn current(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
) -> SqlResult<TableDefinition> {
    match tables.table(writer, name)? {
        Some(table) => Ok((*table).clone()),
        None => Err(SqlError::new(
            SqlState::UNDEFINED_TABLE,
            format!("relation \"{name}\" does not exist"),
        )),
    }
}

fn save(writer: &mut dyn KeyValueWrite, table: &TableDefinition) -> SqlResult<()> {
    let mut table = table.clone();
    (table.referenced, table.owns_sequence) = constraints::flags_of(writer, &table.name)?;
    let bytes = table.encode();
    if TableDefinition::decode(&table.name, &bytes).is_err() {
        return Err(SqlError::not_supported(format!(
            "this change would leave table \"{}\" with a definition EggDB cannot store",
            table.name
        )));
    }
    writer.put(&table_key(&table.name), &bytes)
}

pub(crate) fn alter_table(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    alter: &AlterTable,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let mut result = QueryResult::command("ALTER TABLE");
    if tables.table(writer, &alter.name.text)?.is_none() {
        if alter.if_exists {
            return Ok(result.with_notice(format!(
                "NOTICE:  relation \"{}\" does not exist, skipping",
                alter.name.text
            )));
        }
        return Err(SqlError::new(
            SqlState::UNDEFINED_TABLE,
            format!("relation \"{}\" does not exist", alter.name.text),
        )
        .at(alter.name.position));
    }
    let mut name = alter.name.text.clone();
    for action in &alter.actions {
        if let Some(notice) = apply(writer, tables, &mut name, action, dialect)? {
            result = result.with_notice(notice);
        }
        if dialect == Dialect::PostgreSql
            && let Some(change) = column_change(action, &name)
        {
            crate::views::check_dependents(writer, tables, &name, &change, None)?;
        }
        if let AlterAction::DropColumn { name: column, .. } = action {
            crate::comments::forget_part(writer, &name, CommentKind::Column, &column.text)?;
        }
    }
    Ok(result)
}

fn column_change(action: &AlterAction, table: &str) -> Option<String> {
    let shown = crate::schemas::display(table);
    Some(match action {
        AlterAction::DropColumn { name, .. } => {
            format!("drop column {} of table {shown}", name.text)
        }
        AlterAction::RenameColumn { from, .. } => {
            format!("rename column {} of table {shown}", from.text)
        }
        AlterAction::SetType { column, .. } => {
            format!("alter type of column {} of table {shown}", column.text)
        }
        AlterAction::Redefine { old_name, .. } => {
            format!("change column {} of table {shown}", old_name.text)
        }
        _ => return None,
    })
}

fn apply(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &mut String,
    action: &AlterAction,
    dialect: Dialect,
) -> SqlResult<Option<String>> {
    match action {
        AlterAction::Accepted => Ok(None),
        AlterAction::AddColumn {
            column,
            if_not_exists,
        } => add_column(writer, tables, name, column, *if_not_exists, dialect),
        AlterAction::DropColumn {
            name: column,
            if_exists,
            cascade,
        } => drop_column(writer, tables, name, column, *if_exists, *cascade, dialect),
        AlterAction::RenameColumn { from, to } => {
            rename_column(writer, tables, name, from, to, dialect)?;
            crate::comments::move_part(writer, name, CommentKind::Column, &from.text, &to.text)?;
            Ok(None)
        }
        AlterAction::RenameTable(to) => {
            let target = Name {
                text: crate::schemas::qualified(crate::schemas::schema_of(name), &to.text),
                position: to.position,
            };
            constraints::check_identifier(to)?;
            if dialect == Dialect::PostgreSql {
                refuse_with_dependent_views(writer, name)?;
            }
            rename_table(writer, tables, name, &target, dialect)?;
            *name = target.text;
            Ok(None)
        }
        AlterAction::SetSchema(schema) => {
            if !crate::schemas::schema_exists(writer, &schema.text)? {
                return Err(
                    crate::schemas::missing_schema(&schema.text, dialect).at(schema.position)
                );
            }
            let target = Name {
                text: crate::schemas::qualified(&schema.text, crate::schemas::base_name(name)),
                position: schema.position,
            };
            refuse_with_dependent_views(writer, name)?;
            let moving = current(writer, tables, name)?;
            check_index_names_free(writer, &moving, &schema.text)?;
            rename_table(writer, tables, name, &target, dialect)?;
            *name = target.text;
            Ok(None)
        }
        AlterAction::SetDefault { column, default } => {
            set_default(writer, tables, name, column, default.as_ref(), dialect).map(|()| None)
        }
        AlterAction::AddDefault {
            column,
            default,
            position,
        } => add_default(writer, tables, name, column, default, *position, dialect).map(|()| None),
        AlterAction::EnableConstraint(constraint) => {
            if current(writer, tables, name)?.constraint_name_taken(&constraint.text) {
                return Ok(None);
            }
            Err(SqlError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("Constraint '{}' does not exist.", constraint.text),
            )
            .with_sqlserver_code(4917)
            .at(constraint.position))
        }
        AlterAction::SetNotNull { column, not_null } => {
            set_not_null(writer, tables, name, column, *not_null, dialect).map(|()| None)
        }
        AlterAction::AddIdentity {
            column,
            sequence,
            options,
        } => add_identity(
            writer,
            tables,
            name,
            column,
            sequence.as_ref(),
            options,
            dialect,
        )
        .map(|()| None),
        AlterAction::SetType { column, sql_type } => {
            set_type(writer, tables, name, column, *sql_type, dialect).map(|()| None)
        }
        AlterAction::Redefine { old_name, column } => {
            redefine(writer, tables, name, old_name, column, dialect).map(|()| None)
        }
        AlterAction::AddConstraint(spec) => {
            add_constraint(writer, tables, name, spec, dialect).map(|()| None)
        }
        AlterAction::DropConstraint {
            name: constraint,
            if_exists,
            cascade,
        } => drop_constraint(
            writer, tables, name, constraint, *if_exists, *cascade, dialect,
        ),
        AlterAction::DropPrimaryKey(position) => {
            let table = current(writer, tables, name)?;
            if table.primary_key.is_empty() {
                return Err(match dialect {
                    Dialect::Oracle => SqlError::new(
                        SqlState::UNDEFINED_OBJECT,
                        "cannot drop constraint - nonexistent constraint",
                    )
                    .with_oracle_code(2441),
                    _ => SqlError::new(
                        SqlState::MYSQL_SYNTAX_OR_ACCESS,
                        "Can't DROP 'PRIMARY'; check that column/key exists",
                    )
                    .with_vendor_code(1091),
                }
                .at(*position));
            }
            drop_primary_key(writer, tables, table, false, dialect).map(|()| None)
        }
    }
}

fn next_table_id(writer: &mut dyn KeyValueWrite) -> SqlResult<u32> {
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
    writer.put(&meta_key(), &encode_meta(following))?;
    Ok(next_id)
}

type RowMap<'m> = dyn FnMut(Vec<Value>) -> SqlResult<Vec<Value>> + 'm;

fn rebuild(
    writer: &mut dyn KeyValueWrite,
    old: &TableDefinition,
    mut new: TableDefinition,
    map: &mut RowMap<'_>,
    duplicate_name: Option<String>,
) -> SqlResult<TableDefinition> {
    new.id = next_table_id(writer)?;
    if has_rows(writer, &new)? {
        return Err(crate::encoding::corrupt("table numbering"));
    }
    let checks = PreparedChecks::of(&new)?;
    let prefix = rows_prefix(old.id);
    let end = prefix_end(&prefix);
    let mut memory = WorkMemory::new();
    {
        let mut stored = execute::StoredRows::new(&new);
        loop {
            let mut batch: Vec<(Vec<u8>, Vec<Value>)> = Vec::new();
            let mut charged = 0;
            writer.scan(&prefix, &end, ScanDirection::Forward, &mut |key, bytes| {
                let row = decode_row(bytes, &old.columns)?;
                let size = WorkMemory::row_bytes(&row) + key.len();
                memory.charge(size)?;
                charged += size;
                batch.push((key.to_vec(), row));
                Ok(batch.len() < REWRITE_BATCH)
            })?;
            if batch.is_empty() {
                break;
            }
            for (key, row) in batch {
                writer.delete(&key)?;
                let mut mapped = map(row)?;
                if !checks.is_empty() {
                    checks.check(&new, &mapped)?;
                }
                stored.insert(writer, &mut mapped).map_err(|error| {
                    match (&duplicate_name, error.state()) {
                        (Some(index), SqlState::UNIQUE_VIOLATION) => SqlError::new(
                            SqlState::UNIQUE_VIOLATION,
                            format!("could not create unique index \"{index}\""),
                        )
                        .with_detail(
                            error
                                .detail()
                                .unwrap_or("")
                                .replace("already exists", "is duplicated"),
                        )
                        .with_table(&new.name),
                        _ => error,
                    }
                })?;
            }
            memory.release(charged);
        }
    }
    indexes::delete_every_index_entry(writer, old)?;
    save(writer, &new)?;
    Ok(new)
}

fn rewrite_in_place(
    writer: &mut dyn KeyValueWrite,
    table: &TableDefinition,
    read_columns: &[Column],
    map: &mut RowMap<'_>,
) -> SqlResult<()> {
    let prefix = rows_prefix(table.id);
    let end = prefix_end(&prefix);
    let mut start = prefix;
    let mut memory = WorkMemory::new();
    loop {
        let mut batch: Vec<(Vec<u8>, Vec<Value>)> = Vec::new();
        let mut charged = 0;
        writer.scan(&start, &end, ScanDirection::Forward, &mut |key, bytes| {
            let row = decode_row(bytes, read_columns)?;
            let size = WorkMemory::row_bytes(&row) + key.len();
            memory.charge(size)?;
            charged += size;
            batch.push((key.to_vec(), row));
            Ok(batch.len() < REWRITE_BATCH)
        })?;
        let Some((last, _)) = batch.last() else {
            return Ok(());
        };
        let mut next = last.clone();
        next.push(0);
        let full = batch.len() == REWRITE_BATCH;
        for (key, row) in batch {
            let mapped = map(row)?;
            writer.put(&key, &encode_row(&mapped))?;
        }
        memory.release(charged);
        if !full {
            return Ok(());
        }
        start = next;
    }
}

fn has_rows(writer: &mut dyn KeyValueWrite, table: &TableDefinition) -> SqlResult<bool> {
    let prefix = rows_prefix(table.id);
    let mut found = false;
    writer.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |_, _| {
            found = true;
            Ok(false)
        },
    )?;
    Ok(found)
}

fn column_of(definition: &ColumnDefinition) -> Column {
    Column {
        name: definition.name.text.clone(),
        sql_type: definition.sql_type,
        not_null: definition.not_null || definition.primary_key,
        default: definition.default.as_ref().map(|(_, text)| text.clone()),
        auto_increment: definition.auto_increment,
    }
}

#[cold]
#[inline(never)]
fn unreadable_in_transact_sql(constraint: &str, to: &Name) -> SqlError {
    SqlError::not_supported(format!(
        "renaming a column to \"{}\", which the Transact-SQL check constraint \"{constraint}\" names",
        to.text
    ))
    .with_hint(
        "Transact-SQL reads every name in lower case, so the constraint could no longer find the column: choose a name in lower case.",
    )
    .at(to.position)
}

#[cold]
#[inline(never)]
fn duplicate_column(table: &TableDefinition, name: &Name) -> SqlError {
    SqlError::new(
        SqlState::DUPLICATE_COLUMN,
        format!(
            "column \"{}\" of relation \"{}\" already exists",
            name.text, table.name
        ),
    )
    .at(name.position)
}

fn add_column(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    definition: &ColumnDefinition,
    if_not_exists: bool,
    dialect: Dialect,
) -> SqlResult<Option<String>> {
    let mut table = current(writer, tables, name)?;
    constraints::check_identifier(&definition.name)?;
    if let Some((_, text)) = &definition.default {
        crate::sequences::check_default_sequences(writer, text)?;
    }
    if table.column_index(&definition.name.text).is_some() {
        if if_not_exists {
            return Ok(Some(format!(
                "NOTICE:  column \"{}\" of relation \"{}\" already exists, skipping",
                definition.name.text, table.name
            )));
        }
        return Err(duplicate_column(&table, &definition.name));
    }
    if table.columns.len() >= MAX_COLUMNS {
        return Err(SqlError::new(
            SqlState::TOO_MANY_COLUMNS,
            format!("tables can have at most {MAX_COLUMNS} columns"),
        ));
    }
    if definition.auto_increment || definition.primary_key {
        return Err(SqlError::not_supported(
            "adding a serial, identity, AUTO_INCREMENT or PRIMARY KEY column to an existing table",
        )
        .at(definition.name.position));
    }
    if let Some((expr, _)) = &definition.default {
        execute::check_default_expr(expr)?;
    }
    let column = column_of(definition);
    let old_columns = table.columns.clone();
    table.columns.push(column.clone());
    let fills = column.default.is_some() && has_rows(writer, &table)?;
    if column.not_null && !fills && has_rows(writer, &table)? {
        return Err(SqlError::new(
            SqlState::NOT_NULL_VIOLATION,
            format!(
                "column \"{}\" of relation \"{}\" contains null values",
                column.name, table.name
            ),
        )
        .with_table(&table.name));
    }
    if fills {
        if column
            .default
            .as_deref()
            .is_some_and(|text| text.to_ascii_lowercase().contains("nextval"))
        {
            crate::sequences::load_all(writer)?;
        }
        let mut map = |mut row: Vec<Value>| -> SqlResult<Vec<Value>> {
            let value = execute::default_of(&column)?;
            if value.is_null() && column.not_null {
                return Err(SqlError::new(
                    SqlState::NOT_NULL_VIOLATION,
                    format!(
                        "column \"{}\" of relation \"{}\" contains null values",
                        column.name, name
                    ),
                ));
            }
            row.push(value);
            Ok(row)
        };
        rewrite_in_place(writer, &table, &old_columns, &mut map)?;
    }
    save(writer, &table)?;
    if definition.unique.is_some() {
        add_constraint(
            writer,
            tables,
            name,
            &ConstraintSpec {
                name: definition.unique.clone().flatten(),
                kind: ConstraintKind::Unique(vec![definition.name.clone()]),
                position: definition.name.position,
                column: Some(definition.name.clone()),
            },
            dialect,
        )?;
    }
    for spec in &definition.constraints {
        add_constraint(writer, tables, name, spec, dialect)?;
    }
    Ok(None)
}

type ReferencingKey = (Arc<TableDefinition>, String, Vec<String>);

fn referencing_keys(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &TableDefinition,
) -> SqlResult<Vec<ReferencingKey>> {
    let mut found = Vec::new();
    for child in constraints::referencing_tables(writer, &table.name)? {
        let Some(definition) = tables.table(writer, &child)? else {
            continue;
        };
        for key in &definition.foreign_keys {
            if key.parent == table.name {
                found.push((
                    Arc::clone(&definition),
                    key.name.clone(),
                    key.parent_columns.clone(),
                ));
            }
        }
    }
    Ok(found)
}

enum Dropping<'d> {
    Column(&'d str),
    Index(&'d str),
    PrimaryKey(&'d str),
}

#[cold]
#[inline(never)]
fn depends_error(
    dropping: &Dropping<'_>,
    table: &str,
    constraint: &str,
    child: &str,
    dialect: Dialect,
) -> SqlError {
    match (dialect, dropping) {
        (Dialect::Oracle, Dropping::Column(_)) => SqlError::new(
            SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
            "cannot drop parent key column",
        )
        .with_oracle_code(12992),
        (Dialect::Oracle, _) => SqlError::new(
            SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
            "this unique/primary key is referenced by some foreign keys",
        )
        .with_oracle_code(2273),
        (Dialect::MySql, Dropping::Column(column)) => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!(
                "Cannot drop column '{column}': needed in a foreign key constraint '{constraint}' of table '{child}'"
            ),
        )
        .with_vendor_code(1846),
        (Dialect::MySql, Dropping::Index(index)) => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!("Cannot drop index '{index}': needed in a foreign key constraint"),
        )
        .with_vendor_code(1553),
        (Dialect::MySql, Dropping::PrimaryKey(_)) => SqlError::new(
            SqlState::MYSQL_GENERAL,
            "Cannot drop index 'PRIMARY': needed in a foreign key constraint",
        )
        .with_vendor_code(1553),
        _ => {
            let what = match dropping {
                Dropping::Column(column) => format!("column {column} of table {table}"),
                Dropping::Index(name) | Dropping::PrimaryKey(name) => {
                    format!("constraint {name} on table {table}")
                }
            };
            SqlError::new(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop {what} because other objects depend on it"),
            )
            .with_detail(format!(
                "constraint {constraint} on table {child} depends on {what}"
            ))
            .with_hint("Use DROP ... CASCADE to drop the dependent objects too.")
        }
    }
}

fn drop_foreign_keys_on(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    child: &str,
    names: &[String],
) -> SqlResult<()> {
    let mut definition = current(writer, tables, child)?;
    constraints::unregister_references(writer, tables, &definition)?;
    definition
        .foreign_keys
        .retain(|key| !names.contains(&key.name));
    save(writer, &definition)?;
    constraints::register_references(writer, tables, &definition)
}

fn release_dependents(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &TableDefinition,
    columns: &[String],
    dropping: &Dropping<'_>,
    cascade: bool,
    dialect: Dialect,
) -> SqlResult<Vec<String>> {
    let mut notices = Vec::new();
    let same_set = |left: &[String], right: &[String]| {
        left.len() == right.len() && left.iter().all(|name| right.contains(name))
    };
    let names_of = |indices: &[usize]| -> Vec<String> {
        indices
            .iter()
            .filter_map(|&index| table.columns.get(index))
            .map(|column| column.name.clone())
            .collect()
    };
    let keys_after: Vec<Vec<String>> = match dropping {
        Dropping::Column(_) => Vec::new(),
        Dropping::PrimaryKey(_) => table
            .indexes
            .iter()
            .filter(|index| index.unique)
            .map(|index| names_of(&index.columns))
            .collect(),
        Dropping::Index(name) => table
            .indexes
            .iter()
            .filter(|index| index.unique && index.name != *name)
            .map(|index| names_of(&index.columns))
            .chain((!table.primary_key.is_empty()).then(|| names_of(&table.primary_key)))
            .collect(),
    };
    for (child, constraint, parent_columns) in referencing_keys(writer, tables, table)? {
        let depends = match dropping {
            Dropping::Column(_) => parent_columns.iter().any(|column| columns.contains(column)),
            _ => {
                same_set(&parent_columns, columns)
                    && !keys_after.iter().any(|key| same_set(&parent_columns, key))
            }
        };
        if !depends {
            continue;
        }
        if !cascade {
            return Err(depends_error(
                dropping,
                &table.name,
                &constraint,
                &child.name,
                dialect,
            ));
        }
        drop_foreign_keys_on(
            writer,
            tables,
            &child.name,
            std::slice::from_ref(&constraint),
        )?;
        notices.push(format!(
            "NOTICE:  drop cascades to constraint {constraint} on table {}",
            child.name
        ));
    }
    Ok(notices)
}

fn mentions_column(check: &crate::encoding::CheckConstraint, column: &str) -> bool {
    let Ok(expr) = constraints::parse_check(check) else {
        return true;
    };
    mentions(&expr, column)
}

fn mentions(expr: &crate::ast::Expr, column: &str) -> bool {
    if let crate::ast::ExprKind::Column { name, .. } = &expr.kind
        && name == column
    {
        return true;
    }
    crate::eval::children(expr)
        .into_iter()
        .any(|child| mentions(child, column))
}

fn remap(columns: &[usize], removed: usize) -> Option<Vec<usize>> {
    if columns.contains(&removed) {
        return None;
    }
    Some(
        columns
            .iter()
            .map(|&column| if column > removed { column - 1 } else { column })
            .collect(),
    )
}

fn drop_column(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &Name,
    if_exists: bool,
    cascade: bool,
    dialect: Dialect,
) -> SqlResult<Option<String>> {
    let table = current(writer, tables, name)?;
    let Some(index) = table.column_index(&column.text) else {
        if if_exists {
            return Ok(Some(format!(
                "NOTICE:  column \"{}\" of relation \"{}\" does not exist, skipping",
                column.text, table.name
            )));
        }
        return Err(missing_column(&table, column, dialect));
    };
    if table.columns.len() == 1 {
        return Err(match dialect {
            Dialect::MySql => SqlError::new(
                SqlState::MYSQL_GENERAL,
                "You can't delete all columns with ALTER TABLE; use DROP TABLE instead",
            )
            .with_vendor_code(1090),
            Dialect::Oracle => SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "cannot drop all columns in a table",
            )
            .with_oracle_code(12983),
            _ => SqlError::not_supported("dropping the only column of a table"),
        });
    }
    let in_key = table.primary_key.contains(&index)
        || table.indexes.iter().any(|index_definition| {
            index_definition.unique && index_definition.columns.contains(&index)
        });
    let notices = if in_key {
        release_dependents(
            writer,
            tables,
            &table,
            std::slice::from_ref(&column.text),
            &Dropping::Column(&column.text),
            cascade,
            dialect,
        )?
    } else {
        Vec::new()
    };
    let table = current(writer, tables, name)?;
    let mut new = table.clone();
    new.columns.remove(index);
    new.primary_key = remap(&table.primary_key, index).unwrap_or_default();
    if new.primary_key.is_empty() {
        new.primary_key_name = None;
    }
    let first_key = new.primary_key.first().copied();
    for (position, column) in new.columns.iter_mut().enumerate() {
        if column.auto_increment && first_key != Some(position) {
            column.auto_increment = false;
        }
    }
    new.indexes = table
        .indexes
        .iter()
        .filter_map(|definition| {
            remap(&definition.columns, index).map(|columns| crate::encoding::IndexDefinition {
                columns,
                ..definition.clone()
            })
        })
        .collect();
    new.checks
        .retain(|check| !mentions_column(check, &column.text));
    new.foreign_keys = table
        .foreign_keys
        .iter()
        .filter(|key| key.parent != table.name || !key.parent_columns.contains(&column.text))
        .filter_map(|key| {
            remap(&key.columns, index).map(|columns| crate::encoding::ForeignKey {
                columns,
                ..key.clone()
            })
        })
        .collect();
    constraints::unregister_references(writer, tables, &table)?;
    let mut map = |mut row: Vec<Value>| -> SqlResult<Vec<Value>> {
        if index < row.len() {
            row.remove(index);
        }
        Ok(row)
    };
    let rebuilt = rebuild(writer, &table, new, &mut map, None)?;
    constraints::register_references(writer, tables, &rebuilt)?;
    crate::sequences::drop_owned_by(writer, tables, &table.name, Some(&column.text))?;
    Ok(notices.into_iter().next())
}

fn rename_column(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    from: &Name,
    to: &Name,
    dialect: Dialect,
) -> SqlResult<()> {
    let mut table = current(writer, tables, name)?;
    constraints::check_identifier(to)?;
    let index = match table.column_index(&from.text) {
        Some(index) => index,
        None if dialect == Dialect::MySql => {
            return Err(SqlError::new(
                SqlState::MYSQL_COLUMN_NOT_FOUND,
                format!("Unknown column '{}' in '{}'", from.text, table.name),
            )
            .with_vendor_code(1054)
            .at(from.position));
        }
        None => column_index(&table, from, dialect)?,
    };
    if table.column_index(&to.text).is_some() {
        return Err(duplicate_column(&table, to));
    }
    table.columns[index].name = to.text.clone();
    for check in &mut table.checks {
        let renamed =
            constraints::renamed_in_text(&check.expression, check.dialect, &from.text, &to.text);
        if check.dialect == Dialect::SqlServer
            && renamed != check.expression
            && to.text.to_lowercase() != to.text
        {
            return Err(unreadable_in_transact_sql(&check.name, to));
        }
        check.expression = renamed;
    }
    for key in &mut table.foreign_keys {
        if key.parent == table.name {
            for column in &mut key.parent_columns {
                if *column == from.text {
                    column.clone_from(&to.text);
                }
            }
        }
    }
    save(writer, &table)?;
    for child in constraints::referencing_tables(writer, &table.name)? {
        if child == table.name {
            continue;
        }
        let mut definition = current(writer, tables, &child)?;
        let mut changed = false;
        for key in &mut definition.foreign_keys {
            if key.parent == table.name {
                for column in &mut key.parent_columns {
                    if *column == from.text {
                        column.clone_from(&to.text);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            save(writer, &definition)?;
        }
    }
    crate::sequences::rename_owner(
        writer,
        &table.name,
        Some((&from.text, &to.text)),
        &table.name,
    )
}

fn index_names_of(table: &TableDefinition) -> Vec<String> {
    let mut names: Vec<String> = table
        .indexes
        .iter()
        .map(|index| index.name.clone())
        .collect();
    if !table.primary_key.is_empty() {
        names.push(table.primary_key_constraint_name());
    }
    names
}

fn check_index_names_free(
    writer: &mut dyn KeyValueWrite,
    table: &TableDefinition,
    schema: &str,
) -> SqlResult<()> {
    let names = index_names_of(table);
    if names.is_empty() {
        return Ok(());
    }
    let others: Vec<TableDefinition> = indexes::every_table(writer)?
        .into_iter()
        .filter(|other| {
            other.name != table.name && crate::schemas::schema_of(&other.name) == schema
        })
        .collect();
    for name in names {
        let taken = others
            .iter()
            .any(|other| index_names_of(other).contains(&name))
            || crate::sequences::relation_exists(
                writer,
                &crate::schemas::qualified(schema, &name),
            )?;
        if taken {
            return Err(SqlError::new(
                SqlState::DUPLICATE_TABLE,
                format!("relation \"{name}\" already exists in schema \"{schema}\""),
            ));
        }
    }
    Ok(())
}

fn refuse_with_dependent_views(writer: &mut dyn KeyValueWrite, name: &str) -> SqlResult<()> {
    match crate::views::first_dependent(writer, name)? {
        Some(view) => Err(crate::views::rename_refused("table", name, &view)),
        None => Ok(()),
    }
}

pub(crate) fn move_table(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    from: &str,
    to: &Name,
    dialect: Dialect,
) -> SqlResult<()> {
    rename_table(writer, tables, from, to, dialect)
}

fn rename_table(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    to: &Name,
    dialect: Dialect,
) -> SqlResult<()> {
    let mut table = current(writer, tables, name)?;
    constraints::check_identifier(to)?;
    if to.text == table.name {
        return Ok(());
    }
    if crate::sequences::relation_exists(writer, &to.text)? {
        return Err(match dialect {
            Dialect::MySql => SqlError::new(
                SqlState::MYSQL_TABLE_EXISTS,
                format!("Table '{}' already exists", to.text),
            )
            .with_vendor_code(1050),
            Dialect::Oracle => SqlError::new(
                SqlState::DUPLICATE_TABLE,
                "name is already used by an existing object",
            )
            .with_oracle_code(955),
            Dialect::Sqlite => SqlError::new(
                SqlState::DUPLICATE_TABLE,
                format!(
                    "there is already another table or index with this name: {}",
                    to.text
                ),
            )
            .with_sqlite_code(1),
            Dialect::PostgreSql | Dialect::SqlServer => SqlError::new(
                SqlState::DUPLICATE_TABLE,
                format!("relation \"{}\" already exists", to.text),
            ),
        }
        .at(to.position));
    }
    let children = constraints::referencing_tables(writer, &table.name)?;
    constraints::unregister_references(writer, tables, &table)?;
    let old_name = table.name.clone();
    if !table.primary_key.is_empty() && table.primary_key_name.is_none() {
        table.primary_key_name = Some(table.primary_key_constraint_name());
    }
    table.name = to.text.clone();
    for key in &mut table.foreign_keys {
        if key.parent == old_name {
            key.parent = to.text.clone();
        }
    }
    writer.delete(&table_key(&old_name))?;
    save(writer, &table)?;
    constraints::register_references(writer, tables, &table)?;
    for child in children {
        if child == old_name {
            continue;
        }
        let mut definition = current(writer, tables, &child)?;
        for key in &mut definition.foreign_keys {
            if key.parent == old_name {
                key.parent = to.text.clone();
            }
        }
        writer.delete(&reference_key(&old_name, &child))?;
        save(writer, &definition)?;
        constraints::register_references(writer, tables, &definition)?;
    }
    crate::sequences::rename_owner(writer, &old_name, None, &to.text)?;
    crate::comments::move_owner(writer, &old_name, &to.text)?;
    constraints::refresh_flags(writer, tables, &to.text)
}

fn set_default(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &Name,
    default: Option<&(crate::ast::Expr, String)>,
    dialect: Dialect,
) -> SqlResult<()> {
    let mut table = current(writer, tables, name)?;
    let index = column_index(&table, column, dialect)?;
    if let Some((expr, text)) = default {
        execute::check_default_expr(expr)?;
        crate::sequences::check_default_sequences(writer, text)?;
    }
    let target = &mut table.columns[index];
    target.default = default.map(|(_, text)| text.clone());
    target.auto_increment = false;
    save(writer, &table)
}

fn add_default(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &Name,
    default: &crate::ast::Expr,
    position: usize,
    dialect: Dialect,
) -> SqlResult<()> {
    let table = current(writer, tables, name)?;
    let target = &table.columns[column_index(&table, column, dialect)?];
    if target.auto_increment {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "Defaults cannot be created on columns with an IDENTITY attribute.",
        )
        .with_sqlserver_code(1754)
        .at(column.position));
    }
    let source =
        crate::parser::sqlserver_default_source(default, target.sql_type).ok_or_else(|| {
            crate::parser::sqlserver_not_supported("A DEFAULT other than a constant or GETDATE()")
                .at(position)
        })?;
    set_default(
        writer,
        tables,
        name,
        column,
        Some(&(default.clone(), source)),
        dialect,
    )
}

fn add_identity(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &Name,
    sequence: Option<&Name>,
    options: &SequenceOptions,
    dialect: Dialect,
) -> SqlResult<()> {
    let table = current(writer, tables, name)?;
    let index = column_index(&table, column, dialect)?;
    let target = &table.columns[index];
    if !matches!(
        target.sql_type,
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
    ) {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!(
                "identity column type must be smallint, integer, or bigint, not {}",
                target.sql_type.name()
            ),
        )
        .at(column.position));
    }
    if target.auto_increment || target.default.is_some() {
        let shown = crate::schemas::display(name);
        return Err(SqlError::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            if target.auto_increment {
                format!(
                    "column \"{}\" of relation \"{shown}\" is already an identity column",
                    column.text
                )
            } else {
                format!(
                    "column \"{}\" of relation \"{shown}\" already has a default value",
                    column.text
                )
            },
        )
        .at(column.position));
    }
    let schema = crate::schemas::schema_of(name);
    let sequence_name = match sequence {
        Some(given) if crate::schemas::is_qualified(&given.text) => {
            crate::schemas::normalized(&given.text).to_owned()
        }
        Some(given) => crate::schemas::qualified(schema, &given.text),
        None => crate::schemas::qualified(
            schema,
            &format!("{}_{}_seq", crate::schemas::base_name(name), column.text),
        ),
    };
    let position = sequence.map_or(column.position, |given| given.position);
    let sequence_schema = crate::schemas::schema_of(&sequence_name);
    if !crate::schemas::schema_exists(writer, sequence_schema)? {
        return Err(crate::schemas::missing_schema(sequence_schema, dialect).at(position));
    }
    let owner = if crate::schemas::is_qualified(name) {
        name.to_owned()
    } else {
        crate::schemas::written_qualified(crate::schemas::DEFAULT_SCHEMA, name)
    };
    let mut options = options.clone();
    if options.sql_type.is_none() {
        options.sql_type = Some((target.sql_type, column.position));
    }
    options.owned_by = Some(Some((
        Name {
            text: owner,
            position: column.position,
        },
        column.clone(),
    )));
    let create = CreateSequence {
        name: Name {
            text: sequence_name.clone(),
            position,
        },
        if_not_exists: false,
        options,
    };
    crate::sequences::create_sequence(writer, tables, &create, dialect)?;
    set_not_null(writer, tables, name, column, true, dialect)?;
    let regclass = format!(
        "{}.{}",
        crate::schema::quoted(crate::schemas::schema_of(&sequence_name)),
        crate::schema::quoted(crate::schemas::base_name(&sequence_name))
    );
    let default = format!("nextval('{}'::regclass)", regclass.replace('\'', "''"));
    let mut table = current(writer, tables, name)?;
    let target = &mut table.columns[index];
    target.default = Some(default);
    target.auto_increment = false;
    save(writer, &table)
}

fn set_not_null(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &Name,
    not_null: bool,
    dialect: Dialect,
) -> SqlResult<()> {
    let mut table = current(writer, tables, name)?;
    let index = column_index(&table, column, dialect)?;
    if !not_null {
        if table.primary_key.contains(&index) {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("column \"{}\" is in a primary key", column.text),
            ));
        }
        table.columns[index].not_null = false;
        return save(writer, &table);
    }
    if !table.columns[index].not_null {
        let prefix = rows_prefix(table.id);
        let mut found_null = false;
        let columns = table.columns.clone();
        writer.scan(
            &prefix,
            &prefix_end(&prefix),
            ScanDirection::Forward,
            &mut |_, bytes| {
                let row = decode_row(bytes, &columns)?;
                if row.get(index).is_none_or(Value::is_null) {
                    found_null = true;
                    return Ok(false);
                }
                Ok(true)
            },
        )?;
        if found_null {
            return Err(match dialect {
                Dialect::Oracle => SqlError::new(
                    SqlState::NOT_NULL_VIOLATION,
                    format!(
                        "cannot enable ({}) - null values found",
                        crate::lexer::swap_uniform_case(&column.text)
                    ),
                )
                .with_oracle_code(2296),
                _ => SqlError::new(
                    SqlState::NOT_NULL_VIOLATION,
                    format!(
                        "column \"{}\" of relation \"{}\" contains null values",
                        column.text, table.name
                    ),
                )
                .with_table(&table.name),
            });
        }
    }
    table.columns[index].not_null = true;
    save(writer, &table)
}

fn set_type(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &Name,
    sql_type: SqlType,
    dialect: Dialect,
) -> SqlResult<()> {
    let table = current(writer, tables, name)?;
    let index = column_index(&table, column, dialect)?;
    if table.columns[index].sql_type == sql_type {
        return Ok(());
    }
    if table.columns[index].auto_increment
        && !matches!(
            sql_type,
            SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
        )
    {
        return Err(SqlError::not_supported(
            "changing an AUTO_INCREMENT, serial or identity column to a non-integer type",
        ));
    }
    if table
        .indexes
        .iter()
        .any(|definition| definition.columns.contains(&index))
        || table.primary_key.contains(&index)
    {
        indexes::check_orderable(sql_type, column.position)?;
    }
    let mut new = table.clone();
    new.columns[index].sql_type = sql_type;
    let target = new.columns[index].clone();
    let mut map = |mut row: Vec<Value>| -> SqlResult<Vec<Value>> {
        if let Some(value) = row.get_mut(index) {
            let taken = std::mem::replace(value, Value::Null);
            *value = convert(taken, sql_type, Conversion::Assignment).map_err(|error| {
                if error.state() == SqlState::CANNOT_COERCE {
                    SqlError::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!(
                            "column \"{}\" cannot be cast automatically to type {}",
                            target.name,
                            sql_type.name()
                        ),
                    )
                    .with_hint(format!(
                        "You might need to specify \"USING {}::{}\".",
                        target.name,
                        sql_type.name()
                    ))
                } else {
                    error
                }
            })?;
        }
        Ok(row)
    };
    check_key_types(writer, tables, &new, index, dialect)?;
    rebuild(writer, &table, new, &mut map, None)?;
    revalidate_keys(writer, tables, name, &column.text)
}

fn check_key_types(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    new: &TableDefinition,
    index: usize,
    dialect: Dialect,
) -> SqlResult<()> {
    let column = &new.columns[index];
    for key in &new.foreign_keys {
        for (position, &child_column) in key.columns.iter().enumerate() {
            if child_column != index {
                continue;
            }
            let parent = if key.parent == new.name {
                Some(Arc::new(new.clone()))
            } else {
                tables.table(writer, &key.parent)?
            };
            let Some(parent) = parent else {
                continue;
            };
            let Some(parent_column) = key
                .parent_columns
                .get(position)
                .and_then(|name| parent.column_index(name))
            else {
                continue;
            };
            let parent_type = parent.columns[parent_column].sql_type;
            if !constraints::compatible(column.sql_type, parent_type) {
                return Err(constraints::incompatible_key(
                    &key.name,
                    (&column.name, column.sql_type),
                    (&parent.columns[parent_column].name, parent_type),
                    dialect,
                ));
            }
        }
    }
    for child_name in constraints::referencing_tables(writer, &new.name)? {
        let child = if child_name == new.name {
            Some(Arc::new(new.clone()))
        } else {
            tables.table(writer, &child_name)?
        };
        let Some(child) = child else {
            continue;
        };
        for key in child
            .foreign_keys
            .iter()
            .filter(|key| key.parent == new.name)
        {
            for (position, parent_column) in key.parent_columns.iter().enumerate() {
                if *parent_column != column.name {
                    continue;
                }
                let Some(&child_column) = key.columns.get(position) else {
                    continue;
                };
                let child_type = child.columns[child_column].sql_type;
                if !constraints::compatible(child_type, column.sql_type) {
                    return Err(constraints::incompatible_key(
                        &key.name,
                        (&child.columns[child_column].name, child_type),
                        (&column.name, column.sql_type),
                        dialect,
                    ));
                }
            }
        }
    }
    Ok(())
}

fn revalidate_keys(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &str,
) -> SqlResult<()> {
    let table = current(writer, tables, name)?;
    if let Some(index) = table.column_index(column) {
        for position in 0..table.foreign_keys.len() {
            if table.foreign_keys[position].columns.contains(&index) {
                constraints::validate_existing(writer, tables, &table, None, Some(position))?;
            }
        }
    }
    for child_name in constraints::referencing_tables(writer, name)? {
        let child = current(writer, tables, &child_name)?;
        for position in 0..child.foreign_keys.len() {
            let key = &child.foreign_keys[position];
            if key.parent == name && key.parent_columns.iter().any(|parent| parent == column) {
                constraints::validate_existing(writer, tables, &child, None, Some(position))?;
            }
        }
    }
    Ok(())
}

fn redefine(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    old_name: &Name,
    column: &ColumnDefinition,
    dialect: Dialect,
) -> SqlResult<()> {
    let table = current(writer, tables, name)?;
    let index = table.column_index(&old_name.text).ok_or_else(|| {
        SqlError::new(
            SqlState::MYSQL_COLUMN_NOT_FOUND,
            format!("Unknown column '{}' in '{}'", old_name.text, table.name),
        )
        .with_vendor_code(1054)
        .at(old_name.position)
    })?;
    if column.auto_increment && !table.columns[index].auto_increment {
        return Err(SqlError::not_supported(
            "making an existing column AUTO_INCREMENT",
        ));
    }
    if old_name.text != column.name.text {
        rename_column(writer, tables, name, old_name, &column.name, dialect)?;
    }
    set_type(writer, tables, name, &column.name, column.sql_type, dialect)?;
    let mut table = current(writer, tables, name)?;
    let index = table.column_index(&column.name.text).unwrap_or(index);
    let in_key = table.primary_key.contains(&index);
    if column.not_null && !table.columns[index].not_null {
        set_not_null(writer, tables, name, &column.name, true, dialect)?;
        table = current(writer, tables, name)?;
    }
    if let Some((expr, text)) = &column.default {
        execute::check_default_expr(expr)?;
        crate::sequences::check_default_sequences(writer, text)?;
    }
    let target = &mut table.columns[index];
    target.not_null = column.not_null || in_key;
    if !target.auto_increment {
        target.default = column.default.as_ref().map(|(_, text)| text.clone());
    }
    save(writer, &table)?;
    if column.unique.is_some() {
        add_constraint(
            writer,
            tables,
            name,
            &ConstraintSpec {
                name: None,
                kind: ConstraintKind::Unique(vec![column.name.clone()]),
                position: column.name.position,
                column: Some(column.name.clone()),
            },
            dialect,
        )?;
    }
    if column.primary_key && !in_key {
        add_constraint(
            writer,
            tables,
            name,
            &ConstraintSpec {
                name: None,
                kind: ConstraintKind::PrimaryKey(vec![column.name.clone()]),
                position: column.name.position,
                column: Some(column.name.clone()),
            },
            dialect,
        )?;
    }
    for spec in &column.constraints {
        add_constraint(writer, tables, name, spec, dialect)?;
    }
    Ok(())
}

fn add_constraint(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    spec: &ConstraintSpec,
    dialect: Dialect,
) -> SqlResult<()> {
    let mut table = current(writer, tables, name)?;
    if let Some(given) = &spec.name
        && table.constraint_name_taken(&given.text)
    {
        return Err(match dialect {
            Dialect::MySql => SqlError::new(
                SqlState::MYSQL_SYNTAX_OR_ACCESS,
                format!("Duplicate key name '{}'", given.text),
            )
            .with_vendor_code(1061),
            Dialect::Oracle => SqlError::new(
                SqlState::DUPLICATE_OBJECT,
                "name already used by an existing constraint",
            )
            .with_oracle_code(2264),
            _ => SqlError::new(
                SqlState::DUPLICATE_OBJECT,
                format!(
                    "constraint \"{}\" for relation \"{}\" already exists",
                    given.text, table.name
                ),
            ),
        }
        .at(given.position));
    }
    match &spec.kind {
        ConstraintKind::Check { expr, text } => {
            let position = constraints::define_check(&mut table, spec, expr, text, dialect)?;
            constraints::validate_existing(writer, tables, &table, Some(position), None)?;
            save(writer, &table)
        }
        ConstraintKind::ForeignKey(key) => {
            let position =
                constraints::define_foreign_key(writer, tables, &mut table, spec, key, dialect)?;
            constraints::validate_existing(writer, tables, &table, None, Some(position))?;
            save(writer, &table)?;
            constraints::register_references(writer, tables, &table)
        }
        ConstraintKind::Unique(columns) => {
            let create = CreateIndex {
                index: IndexSpec {
                    name: spec.name.clone(),
                    unique: true,
                    columns: columns.clone(),
                    position: spec.position,
                },
                table: Name {
                    text: table.name.clone(),
                    position: spec.position,
                },
                if_not_exists: false,
            };
            indexes::create_index_as(writer, tables, &create, dialect, true).map(|_| ())
        }
        ConstraintKind::PrimaryKey(columns) => {
            if !table.primary_key.is_empty() {
                return Err(match dialect {
                    Dialect::MySql => SqlError::new(
                        SqlState::MYSQL_SYNTAX_OR_ACCESS,
                        "Multiple primary key defined",
                    )
                    .with_vendor_code(1068),
                    Dialect::Oracle => SqlError::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        "table can have only one primary key",
                    )
                    .with_oracle_code(2260),
                    _ => SqlError::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        format!(
                            "multiple primary keys for table \"{}\" are not allowed",
                            table.name
                        ),
                    ),
                }
                .at(spec.position));
            }
            let mut key = Vec::with_capacity(columns.len());
            for column in columns {
                let index = column_index(&table, column, dialect)?;
                indexes::check_orderable(table.columns[index].sql_type, column.position)?;
                if !key.contains(&index) {
                    key.push(index);
                }
            }
            for &index in &key {
                if !table.columns[index].not_null {
                    let column = Name {
                        text: table.columns[index].name.clone(),
                        position: spec.position,
                    };
                    set_not_null(writer, tables, name, &column, true, dialect)?;
                }
            }
            let table = current(writer, tables, name)?;
            let mut new = table.clone();
            new.primary_key = key;
            if let Some(given) = &spec.name
                && given.text != format!("{}_pkey", crate::schemas::base_name(&table.name))
            {
                new.primary_key_name = Some(given.text.clone());
            }
            let index_name = new.primary_key_constraint_name();
            let mut map = |row: Vec<Value>| -> SqlResult<Vec<Value>> { Ok(row) };
            rebuild(writer, &table, new, &mut map, Some(index_name))?;
            Ok(())
        }
    }
}

fn drop_primary_key(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: TableDefinition,
    cascade: bool,
    dialect: Dialect,
) -> SqlResult<()> {
    let key_columns: Vec<String> = table
        .primary_key
        .iter()
        .map(|&index| table.columns[index].name.clone())
        .collect();
    let constraint = table.primary_key_constraint_name();
    release_dependents(
        writer,
        tables,
        &table,
        &key_columns,
        &Dropping::PrimaryKey(&constraint),
        cascade,
        dialect,
    )?;
    let table = current(writer, tables, &table.name)?;
    let mut new = table.clone();
    new.primary_key = Vec::new();
    new.primary_key_name = None;
    for column in &mut new.columns {
        column.auto_increment = false;
    }
    let mut map = |row: Vec<Value>| -> SqlResult<Vec<Value>> { Ok(row) };
    rebuild(writer, &table, new, &mut map, None)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn drop_constraint(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    constraint: &Name,
    if_exists: bool,
    cascade: bool,
    dialect: Dialect,
) -> SqlResult<Option<String>> {
    let mut table = current(writer, tables, name)?;
    if let Some(position) = table
        .checks
        .iter()
        .position(|check| check.name == constraint.text)
    {
        table.checks.remove(position);
        save(writer, &table)?;
        return Ok(None);
    }
    if let Some(position) = table
        .foreign_keys
        .iter()
        .position(|key| key.name == constraint.text)
    {
        constraints::unregister_references(writer, tables, &table)?;
        table.foreign_keys.remove(position);
        save(writer, &table)?;
        constraints::register_references(writer, tables, &table)?;
        return Ok(None);
    }
    if !table.primary_key.is_empty()
        && (table.primary_key_constraint_name() == constraint.text
            || (dialect == Dialect::MySql && constraint.text.eq_ignore_ascii_case("primary")))
    {
        drop_primary_key(writer, tables, table, cascade, dialect)?;
        return Ok(None);
    }
    if let Some(index) = table
        .indexes
        .iter()
        .find(|index| index.name == constraint.text)
        .cloned()
    {
        let columns: Vec<String> = index
            .columns
            .iter()
            .map(|&column| table.columns[column].name.clone())
            .collect();
        if index.unique {
            release_dependents(
                writer,
                tables,
                &table,
                &columns,
                &Dropping::Index(&index.name),
                cascade,
                dialect,
            )?;
        }
        let drop = crate::ast::DropIndex {
            names: vec![constraint.clone()],
            table: Some(Name {
                text: table.name.clone(),
                position: constraint.position,
            }),
            if_exists: false,
        };
        indexes::drop_index(writer, tables, &drop, dialect)?;
        return Ok(None);
    }
    if if_exists {
        return Ok(Some(format!(
            "NOTICE:  constraint \"{}\" of relation \"{}\" does not exist, skipping",
            constraint.text, table.name
        )));
    }
    Err(match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!(
                "Can't DROP '{}'; check that column/key exists",
                constraint.text
            ),
        )
        .with_vendor_code(1091),
        Dialect::Oracle => SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            "cannot drop constraint - nonexistent constraint",
        )
        .with_oracle_code(2443),
        _ => SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!(
                "constraint \"{}\" of relation \"{}\" does not exist",
                constraint.text, table.name
            ),
        ),
    }
    .at(constraint.position))
}
