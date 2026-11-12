use crate::ast::{
    AlterAction, AlterTable, ColumnDefinition, ColumnPlacement, ConstraintKind, ConstraintSpec,
    CreateIndex, CreateSequence, IndexSpec, Name, SequenceOptions,
};
use crate::constraints::{self, PreparedChecks};
use crate::dialect::Dialect;
use crate::encoding::CommentKind;
use crate::encoding::{
    Column, IdentityGeneration, IndexMethod, MAX_COLUMNS, SortOptions, TableDefinition,
    decode_meta, decode_row, encode_meta, encode_row, meta_key, prefix_end, reference_key,
    rows_prefix, table_key,
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
    if let [AlterAction::AttachIndex(partition_index)] = alter.actions.as_slice() {
        return crate::partition_indexes::attach_index(
            writer,
            tables,
            &alter.name,
            partition_index,
            alter.if_exists,
        );
    }
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
    let sqlite_texts =
        crate::sqlite_schema_text::AlteredTexts::take(writer, &alter.name.text, dialect)?;
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
            crate::roles::forget_column(writer, &name, &column.text)?;
        }
    }
    sqlite_texts.save(writer, &name, &alter.actions)?;
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
        AlterAction::Accepted | AlterAction::SystemVersioning(_) => Ok(None),
        AlterAction::AddColumn {
            column,
            if_not_exists,
            ..
        } => add_column(writer, tables, name, column, *if_not_exists, dialect),
        AlterAction::DropColumn {
            name: column,
            if_exists,
            cascade,
        } => drop_column(writer, tables, name, column, *if_exists, *cascade, dialect),
        AlterAction::RenameColumn { from, to } => {
            rename_partitioned_column(writer, tables, name, from, to, dialect)?;
            crate::comments::move_part(writer, name, CommentKind::Column, &from.text, &to.text)?;
            crate::roles::move_column(writer, name, &from.text, &to.text)?;
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
        AlterAction::AttachPartition { partition, bound } => {
            let parent = current(writer, tables, name)?;
            crate::partition_ddl::attach(writer, tables, &parent, partition, bound)?;
            Ok(None)
        }
        AlterAction::DetachPartition(partition) => {
            let parent = current(writer, tables, name)?;
            crate::partition_ddl::detach(writer, tables, &parent, partition)?;
            Ok(None)
        }
        AlterAction::AddPartitions {
            partitions,
            hash_count,
            position,
        } => {
            let parent = current(writer, tables, name)?;
            crate::partition_alter::add(
                writer,
                tables,
                &parent,
                partitions,
                *hash_count,
                *position,
                dialect,
            )?;
            Ok(None)
        }
        AlterAction::DropPartitions(partitions) => {
            let parent = current(writer, tables, name)?;
            crate::partition_alter::drop(writer, tables, &parent, partitions, dialect)?;
            Ok(None)
        }
        AlterAction::TruncatePartitions(partitions) => {
            let parent = current(writer, tables, name)?;
            crate::partition_alter::truncate(writer, tables, &parent, partitions)?;
            Ok(None)
        }
        AlterAction::MaintainPartitions(partitions) => {
            let parent = current(writer, tables, name)?;
            crate::partition_alter::check_named(writer, tables, &parent, partitions)?;
            Ok(None)
        }
        AlterAction::ReorganizePartitions {
            from,
            into,
            position,
        } => {
            let parent = current(writer, tables, name)?;
            crate::partition_alter::reorganize(
                writer, tables, &parent, from, into, *position, dialect,
            )?;
            Ok(None)
        }
        AlterAction::SplitPartition {
            partition,
            point,
            into,
        } => {
            let parent = current(writer, tables, name)?;
            crate::partition_alter::split(writer, tables, &parent, partition, point, into, dialect)?;
            Ok(None)
        }
        AlterAction::ExchangePartition {
            partition,
            table,
            validate,
        } => {
            let parent = current(writer, tables, name)?;
            crate::partition_alter::exchange(
                writer, tables, &parent, partition, table, *validate, dialect,
            )?;
            Ok(None)
        }
        AlterAction::CoalescePartitions { count, position } => {
            let parent = current(writer, tables, name)?;
            crate::partition_alter::coalesce(writer, tables, &parent, *count, *position, dialect)?;
            Ok(None)
        }
        AlterAction::MergePartitions { from, into } => {
            let parent = current(writer, tables, name)?;
            crate::partition_alter::merge(writer, tables, &parent, from, into, dialect)?;
            Ok(None)
        }
        AlterAction::MergePartitionSpan { first, last, into } => {
            let parent = current(writer, tables, name)?;
            let from =
                crate::partition_alter::range_span(writer, tables, &parent, first, last)?;
            crate::partition_alter::merge(writer, tables, &parent, &from, into, dialect)?;
            Ok(None)
        }
        AlterAction::ModifyPartitionValues {
            partition,
            values,
            adding,
        } => {
            let parent = current(writer, tables, name)?;
            crate::partition_alter::modify_values(
                writer, tables, &parent, partition, values, *adding, dialect,
            )?;
            Ok(None)
        }
        AlterAction::RowMovement(enabled) => {
            let table = current(writer, tables, name)?;
            let mut record = crate::partitions::stored(writer, &table.name)?;
            if record.key.is_some() && record.row_movement_disabled == *enabled {
                record.row_movement_disabled = !enabled;
                crate::partitions::save(writer, &table.name, &record)?;
            }
            Ok(None)
        }
        AlterAction::AttachIndex(partition_index) => {
            let index = Name {
                text: name.clone(),
                position: partition_index.position,
            };
            crate::partition_indexes::attach_index(writer, tables, &index, partition_index, false)
                .map(|_| None)
        }
        AlterAction::MoveColumn { column, placement } => {
            move_column(writer, tables, name, column, placement).map(|()| None)
        }
        AlterAction::AddIndex(index) => {
            let create = CreateIndex {
                index: index.clone(),
                table: Name {
                    text: name.clone(),
                    position: index.position,
                },
                only: false,
                if_not_exists: false,
                sqlite_text: None,
            };
            indexes::create_index(writer, tables, &create, dialect).map(|_| None)
        }
        AlterAction::RenameIndex { from, to } => {
            rename_index(writer, tables, name, from, to).map(|()| None)
        }
        AlterAction::SetSchema(schema) => {
            if crate::temporary::is_temporary_name(name) || schema.text == crate::temporary::SCHEMA
            {
                return Err(SqlError::not_supported(
                    "cannot move objects into or out of temporary schemas",
                )
                .at(schema.position));
            }
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
        AlterAction::SetDefault { column, default } => set_default(
            writer,
            tables,
            name,
            column,
            default
                .as_ref()
                .map(|default| (default, Dialect::PostgreSql)),
            dialect,
        )
        .map(|()| None),
        AlterAction::AddDefault {
            column,
            default,
            source,
            position,
        } => add_default(
            writer,
            tables,
            name,
            column,
            WrittenDefault {
                expr: default,
                source,
                position: *position,
            },
            dialect,
        )
        .map(|()| None),
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
            generation,
            sequence,
            options,
        } => add_identity(
            writer,
            tables,
            name,
            IdentityRequest {
                column,
                generation: *generation,
                sequence: sequence.as_ref(),
                options,
            },
            dialect,
        )
        .map(|()| None),
        AlterAction::SetType {
            column,
            sql_type,
            using,
        } => set_type(writer, tables, name, column, *sql_type, using.as_ref(), dialect)
            .map(|()| None),
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
        AlterAction::ValidateConstraint(constraint) => {
            validate_constraint(writer, tables, name, constraint).map(|()| None)
        }
        AlterAction::RenameConstraint { from, to } => {
            rename_constraint(writer, tables, name, from, to).map(|()| None)
        }
        AlterAction::AlterIdentity {
            column,
            generation,
            options,
        } => alter_identity(writer, tables, name, column, *generation, options, dialect)
            .map(|()| None),
        AlterAction::DropIdentity { column, if_exists } => {
            drop_identity(writer, tables, name, column, *if_exists, dialect)
        }
        AlterAction::SetExpression { column, generation } => {
            set_expression(writer, tables, name, column, generation, dialect).map(|()| None)
        }
        AlterAction::DropExpression { column, if_exists } => {
            drop_expression(writer, tables, name, column, *if_exists, dialect)
        }
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
    new.id = if crate::temporary::is_temporary_name(&new.name) {
        crate::temporary::next_id(writer)?
    } else {
        next_table_id(writer)?
    };
    if has_rows(writer, &new)? {
        return Err(crate::encoding::corrupt("table numbering"));
    }
    let checks = PreparedChecks::of_validated(&new)?;
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
                    checks.generate(&new, &mut mapped)?;
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
fn unreadable_in_transact_sql(reader: &str, to: &Name) -> SqlError {
    SqlError::not_supported(format!(
        "renaming a column to \"{}\", which the Transact-SQL {reader} names",
        to.text
    ))
    .with_hint(
        "Transact-SQL reads every name in lower case, so the expression could no longer find the column: choose a name in lower case.",
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
    if definition.serial && !definition.primary_key && dialect == Dialect::PostgreSql {
        let existing = current(writer, tables, name)?;
        if existing.column_index(&definition.name.text).is_none() {
            return add_serial_column(writer, tables, name, definition, dialect);
        }
    }
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
    let (stored_type, domain) =
        execute::column_storage(definition.sql_type, definition.name.position)?;
    let mut column = column_of(definition);
    column.sql_type = stored_type;
    let old_columns = table.columns.clone();
    table.columns.push(column.clone());
    let added = table.columns.len() - 1;
    table.set_default_dialect(added, definition.default_dialect);
    table.set_domain(added, domain);
    table.set_on_update_current_timestamp(added, definition.on_update_current_timestamp);
    let generator = match &definition.generated {
        Some(spec) if dialect == Dialect::Sqlite && spec.stored => {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "Cannot add a STORED column",
            )
            .with_sqlite_code(1)
            .at(spec.position));
        }
        Some(spec) => {
            let catalog = crate::partitions::catalog(writer, tables)?;
            if catalog.key_of(&table.name).is_some() || catalog.parent_of(&table.name).is_some() {
                return Err(
                    SqlError::not_supported("generated columns in a partitioned table")
                        .at(spec.position),
                );
            }
            crate::generated_columns::define(&mut table, &[(added, spec)], dialect)?;
            crate::generated_columns::refuse_routine_calls(
                &*writer,
                tables,
                &table,
                &[(added, spec)],
                dialect,
            )?;
            if let Some(defined) = table.columns.get(added) {
                column.sql_type = defined.sql_type;
            }
            crate::generated_columns::Generator::of(&table)?
        }
        None => None,
    };
    let fills = (column.default.is_some() || generator.is_some()) && has_rows(writer, &table)?;
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
            row.push(execute::default_of(&table, added)?);
            if let Some(generator) = &generator {
                generator.fill(&table, &mut row)?;
            }
            let value = row.pop().unwrap_or(Value::Null);
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
    crate::roles::forget_column(writer, &table.name, &definition.name.text)?;
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
                using_index: None,
                not_valid: false,
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
    let catalog = crate::partitions::catalog(&*writer, tables)?;
    if catalog.is_empty() {
        return drop_column_here(writer, tables, name, column, if_exists, cascade, dialect);
    }
    if let Some((parent, _)) = catalog.parent_of(name)
        && current(writer, tables, parent)?
            .column_index(&column.text)
            .is_some()
    {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            format!("cannot drop inherited column \"{}\"", column.text),
        )
        .at(column.position));
    }
    let partitions = catalog.descendants_of(name);
    let notice = drop_column_here(writer, tables, name, column, if_exists, cascade, dialect)?;
    for partition in partitions {
        if current(writer, tables, &partition)?
            .column_index(&column.text)
            .is_some()
        {
            drop_column_here(writer, tables, &partition, column, false, cascade, dialect)?;
        }
    }
    Ok(notice)
}

fn drop_column_here(
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
    refuse_dropping_a_key_column(writer, tables, &table, &column.text, dialect)?;
    let mut also_dropped = Vec::new();
    for dependent in crate::generated_columns::dependents_of(&table, index) {
        if !cascade || dialect != Dialect::PostgreSql {
            return Err(crate::generated_columns::dropped_dependency(
                &table, index, dependent, dialect,
            ));
        }
        if let Some(found) = table.columns.get(dependent) {
            also_dropped.push(found.name.clone());
        }
    }
    let mut cascade_notices = Vec::new();
    for dependent in &also_dropped {
        cascade_notices.push(format!(
            "NOTICE:  drop cascades to column {dependent} of table {}",
            crate::schemas::base_name(&table.name)
        ));
        let dependent_name = Name {
            text: dependent.clone(),
            position: column.position,
        };
        drop_column_here(
            writer,
            tables,
            name,
            &dependent_name,
            false,
            cascade,
            dialect,
        )?;
    }
    let table = current(writer, tables, name)?;
    let Some(index) = table.column_index(&column.text) else {
        return Err(missing_column(&table, column, dialect));
    };
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
    let key_columns = new.primary_key.clone();
    for (position, column) in new.columns.iter_mut().enumerate() {
        if column.auto_increment && !key_columns.contains(&position) {
            column.auto_increment = false;
        }
    }
    new.identities = table.identities_without_column(index);
    new.default_dialects = table.default_dialects_without_column(index);
    new.generated = crate::encoding::generated_without_column(&table.generated, index);
    new.domains = table.domains_without_column(index);
    new.on_update_current_timestamp = table.on_update_without_column(index);
    if new.auto_increment_column().is_none() {
        new.identity_seed = None;
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
    new.forget_missing_statuses();
    let dropped_indexes: Vec<u32> = table
        .indexes
        .iter()
        .map(|definition| definition.id)
        .filter(|id| !new.indexes.iter().any(|kept| kept.id == *id))
        .collect();
    crate::partitions::forget_index_links(writer, &table.name, &dropped_indexes)?;
    constraints::unregister_references(writer, tables, &table)?;
    let mut map = |mut row: Vec<Value>| -> SqlResult<Vec<Value>> {
        if index < row.len() {
            row.remove(index);
        }
        Ok(row)
    };
    let rebuilt = rebuild(writer, &table, new, &mut map, None)?;
    constraints::register_references(writer, tables, &rebuilt)?;
    crate::sequences::drop_owned_by(writer, tables, &table.name, Some(column.text.as_str()))?;
    Ok(cascade_notices.into_iter().chain(notices).next())
}

fn unknown_mysql_column(table: &TableDefinition, name: &Name) -> SqlError {
    SqlError::new(
        SqlState::MYSQL_COLUMN_NOT_FOUND,
        format!("Unknown column '{}' in '{}'", name.text, table.name),
    )
    .with_vendor_code(1054)
    .at(name.position)
}

fn moved_order(count: usize, from: usize, to: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..count).collect();
    if from < count && to < count {
        let moved = order.remove(from);
        order.insert(to, moved);
    }
    order
}

fn move_column(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &Name,
    placement: &ColumnPlacement,
) -> SqlResult<()> {
    let table = current(writer, tables, name)?;
    let from = table
        .column_index(&column.text)
        .ok_or_else(|| unknown_mysql_column(&table, column))?;
    let to = match placement {
        ColumnPlacement::First => 0,
        ColumnPlacement::After(anchor) => {
            let after = table
                .column_index(&anchor.text)
                .filter(|&after| after != from)
                .ok_or_else(|| unknown_mysql_column(&table, anchor))?;
            if after < from { after + 1 } else { after }
        }
    };
    if to == from {
        return Ok(());
    }
    let order = moved_order(table.columns.len(), from, to);
    let mut new_position = vec![0; order.len()];
    for (position, &old) in order.iter().enumerate() {
        if let Some(slot) = new_position.get_mut(old) {
            *slot = position;
        }
    }
    let at = |old: usize| new_position.get(old).copied().unwrap_or(old);
    let mut new = table.clone();
    new.columns = order
        .iter()
        .filter_map(|&old| table.columns.get(old).cloned())
        .collect();
    new.primary_key = table.primary_key.iter().map(|&old| at(old)).collect();
    for index in &mut new.indexes {
        index.columns = index.columns.iter().map(|&old| at(old)).collect();
    }
    for key in &mut new.foreign_keys {
        key.columns = key.columns.iter().map(|&old| at(old)).collect();
    }
    for identity in &mut new.identities {
        identity.column = at(identity.column);
    }
    for entry in &mut new.default_dialects {
        entry.column = at(entry.column);
    }
    for entry in &mut new.domains {
        entry.column = at(entry.column);
    }
    for entry in &mut new.generated {
        entry.column = at(entry.column);
    }
    new.generated.sort_by_key(|entry| entry.column);
    for column in &mut new.on_update_current_timestamp {
        *column = at(*column);
    }
    new.on_update_current_timestamp.sort_unstable();
    if crate::constraints::current_dialect() == Dialect::MySql {
        crate::generated_columns::check_mysql_order(&new)?;
    }
    constraints::unregister_references(writer, tables, &table)?;
    let mut map = |row: Vec<Value>| -> SqlResult<Vec<Value>> {
        Ok(order
            .iter()
            .map(|&old| row.get(old).cloned().unwrap_or(Value::Null))
            .collect())
    };
    let rebuilt = rebuild(writer, &table, new, &mut map, None)?;
    constraints::register_references(writer, tables, &rebuilt)?;
    Ok(())
}

fn refuse_dropping_a_key_column(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &TableDefinition,
    column: &str,
    dialect: Dialect,
) -> SqlResult<()> {
    let catalog = crate::partitions::catalog(&*writer, tables)?;
    if catalog.is_empty() {
        return Ok(());
    }
    let keyed = std::iter::once(table.name.clone()).chain(catalog.descendants_of(&table.name));
    for name in keyed {
        let Some(key) = catalog.key_of(&name) else {
            continue;
        };
        let definition = current(writer, tables, &name)?;
        if crate::partitions::key_reads_column(&definition, key, column)? {
            return Err(key_column_drop_refused(column, &name, dialect));
        }
    }
    Ok(())
}

fn key_column_drop_refused(column: &str, name: &str, dialect: Dialect) -> SqlError {
    if dialect != Dialect::MySql {
        return SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            format!(
                "cannot drop column \"{column}\" because it is part of the partition key of \
                 relation \"{}\"",
                crate::schemas::base_name(name)
            ),
        );
    }
    SqlError::new(
        SqlState::MYSQL_COLUMN_NOT_FOUND,
        format!("Unknown column '{column}' in 'partition function'"),
    )
    .with_vendor_code(1054)
}

fn rename_partitioned_column(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    from: &Name,
    to: &Name,
    dialect: Dialect,
) -> SqlResult<()> {
    let catalog = crate::partitions::catalog(&*writer, tables)?;
    if catalog.is_empty() {
        return rename_column(writer, tables, name, from, to, dialect);
    }
    if let Some((parent, _)) = catalog.parent_of(name)
        && current(writer, tables, parent)?
            .column_index(&from.text)
            .is_some()
    {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            format!("cannot rename inherited column \"{}\"", from.text),
        )
        .at(from.position));
    }
    rename_column(writer, tables, name, from, to, dialect)?;
    crate::partitions::rename_key_column(writer, name, &from.text, &to.text)?;
    for partition in catalog.descendants_of(name) {
        if current(writer, tables, &partition)?
            .column_index(&from.text)
            .is_some()
        {
            rename_column(writer, tables, &partition, from, to, dialect)?;
            crate::partitions::rename_key_column(writer, &partition, &from.text, &to.text)?;
        }
    }
    Ok(())
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
            return Err(unreadable_in_transact_sql(
                &format!("check constraint \"{}\"", check.name),
                to,
            ));
        }
        check.expression = renamed;
    }
    let lower_case = to.text.to_lowercase() == to.text;
    for entry in &mut table.generated {
        let renamed =
            constraints::renamed_in_text(&entry.expression, entry.dialect, &from.text, &to.text);
        if entry.dialect == Dialect::SqlServer && renamed != entry.expression && !lower_case {
            let computed = table
                .columns
                .get(entry.column)
                .map_or("", |column| column.name.as_str());
            return Err(unreadable_in_transact_sql(
                &format!("computed column \"{computed}\""),
                to,
            ));
        }
        entry.expression = renamed;
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
    crate::triggers::move_table_triggers(writer, &old_name, &to.text)?;
    crate::roles::forget_relation_access(writer, &to.text)?;
    crate::roles::move_table(writer, &old_name, &to.text)?;
    crate::row_security::move_table(writer, &old_name, &to.text)?;
    crate::partitions::rename(writer, &old_name, &to.text)?;
    crate::statistics::rename(writer, &old_name, &to.text)?;
    constraints::refresh_flags(writer, tables, &to.text)
}

fn set_default(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &Name,
    default: Option<(&(crate::ast::Expr, String), Dialect)>,
    dialect: Dialect,
) -> SqlResult<()> {
    let mut table = current(writer, tables, name)?;
    let index = column_index(&table, column, dialect)?;
    if dialect == Dialect::PostgreSql && table.has_stored_identity(index) {
        let error = SqlError::new(
            SqlState::SYNTAX_ERROR,
            format!(
                "column \"{}\" of relation \"{}\" is an identity column",
                column.text,
                crate::schemas::base_name(name)
            ),
        );
        return Err(match default {
            Some(_) => error,
            None => error.with_hint("Use ALTER TABLE ... ALTER COLUMN ... DROP IDENTITY instead."),
        });
    }
    if let Some(((expr, text), _)) = default {
        execute::check_default_expr(expr)?;
        crate::sequences::check_default_sequences(writer, text)?;
    }
    let target = &mut table.columns[index];
    target.default = default.map(|((_, text), _)| text.clone());
    if target.auto_increment {
        target.auto_increment = false;
        table.identity_seed = None;
    }
    table.set_default_dialect(
        index,
        default.map_or(Dialect::PostgreSql, |(_, written_in)| written_in),
    );
    table.set_identity(index, None);
    save(writer, &table)
}

struct WrittenDefault<'a> {
    expr: &'a crate::ast::Expr,
    source: &'a str,
    position: usize,
}

fn add_default(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &Name,
    default: WrittenDefault<'_>,
    dialect: Dialect,
) -> SqlResult<()> {
    let table = current(writer, tables, name)?;
    let target = &table.columns[column_index(&table, column, dialect)?];
    if target.auto_increment {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            format!(
                "Defaults cannot be created on columns with an IDENTITY attribute. Table '{}', column '{}'.",
                crate::schemas::base_name(&table.name),
                target.name
            ),
        )
        .with_sqlserver_code(1754)
        .at(column.position));
    }
    let (source, written_in) =
        crate::parser::sqlserver_kept_default(default.expr, default.source, target.sql_type)
            .map_err(|error| error.at(default.position))?;
    set_default(
        writer,
        tables,
        name,
        column,
        Some((&(default.expr.clone(), source), written_in)),
        dialect,
    )
}

struct IdentityRequest<'a> {
    column: &'a Name,
    generation: IdentityGeneration,
    sequence: Option<&'a Name>,
    options: &'a SequenceOptions,
}

fn add_identity(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    request: IdentityRequest<'_>,
    dialect: Dialect,
) -> SqlResult<()> {
    let IdentityRequest {
        column,
        generation,
        sequence,
        options,
    } = request;
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
            if table.identity_of(index).is_some() {
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
        or_replace: false,
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
    table.set_default_dialect(index, Dialect::PostgreSql);
    table.set_identity(index, Some(generation));
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
    using: Option<&crate::ast::Expr>,
    dialect: Dialect,
) -> SqlResult<()> {
    let (sql_type, domain) = execute::column_storage(sql_type, column.position)?;
    let table = current(writer, tables, name)?;
    let index = column_index(&table, column, dialect)?;
    if using.is_none()
        && table.columns[index].sql_type == sql_type
        && table.domain_of(index) == domain
    {
        return Ok(());
    }
    if let Some(expr) = using {
        if crate::subquery::mentions_subquery(expr) {
            return Err(SqlError::not_supported("cannot use subquery in transform expression")
                .at(expr.position));
        }
        crate::eval::reject_aggregates(expr, "transform expressions")?;
        crate::eval::check_columns(expr, crate::partitions::table_scope(&table))?;
    }
    let old_table = table.clone();
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
    new.set_domain(index, domain);
    let target = new.columns[index].clone();
    let mut map = |mut row: Vec<Value>| -> SqlResult<Vec<Value>> {
        let transformed = match using {
            Some(expr) => Some(
                crate::eval::Evaluator {
                    scope: crate::partitions::table_scope(&old_table),
                    row: &row,
                    correlation: None,
                }
                .eval(expr, Some(sql_type))?,
            ),
            None => None,
        };
        if let Some(value) = row.get_mut(index) {
            let taken = match transformed {
                Some(found) => found,
                None => std::mem::replace(value, Value::Null),
            };
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
        crate::comments::move_part(
            writer,
            name,
            CommentKind::Column,
            &old_name.text,
            &column.name.text,
        )?;
        crate::roles::move_column(writer, name, &old_name.text, &column.name.text)?;
    }
    set_type(writer, tables, name, &column.name, column.sql_type, None, dialect)?;
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
        table.set_default_dialect(index, column.default_dialect);
    }
    table.set_identity(index, None);
    table.set_on_update_current_timestamp(index, column.on_update_current_timestamp);
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
                using_index: None,
                not_valid: false,
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
                using_index: None,
                not_valid: false,
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
    if dialect == Dialect::Oracle
        && let Some(given) = &spec.name
        && take_over_index(writer, tables, name, spec, given, dialect)?
    {
        return Ok(());
    }
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
    if dialect == Dialect::Oracle
        && let Some(using) = &spec.using_index
        && drop_index_taken_over(writer, tables, name, spec, using, dialect)?
    {
        table = current(writer, tables, name)?;
    }
    if spec.not_valid
        && !matches!(
            spec.kind,
            ConstraintKind::Check { .. } | ConstraintKind::ForeignKey(_)
        )
    {
        return Err(SqlError::not_supported(
            "only CHECK and FOREIGN KEY constraints can be marked NOT VALID",
        )
        .at(spec.position));
    }
    match &spec.kind {
        ConstraintKind::Check { expr, text } => {
            let position = constraints::define_check(&mut table, spec, expr, text, dialect)?;
            if spec.not_valid {
                let name = table.checks[position].name.clone();
                table.set_status(&name, false, false);
            } else {
                constraints::validate_existing(writer, tables, &table, Some(position), None)?;
            }
            save(writer, &table)
        }
        ConstraintKind::ForeignKey(key) => {
            let position =
                constraints::define_foreign_key(writer, tables, &mut table, spec, key, dialect)?;
            if spec.not_valid {
                let name = table.foreign_keys[position].name.clone();
                let deferred = table.is_initially_deferred(&name);
                table.set_status(&name, false, deferred);
            } else {
                constraints::validate_existing(writer, tables, &table, None, Some(position))?;
            }
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
                    method: IndexMethod::BTree,
                    sort_options: SortOptions::default(),
                },
                table: Name {
                    text: table.name.clone(),
                    position: spec.position,
                },
                only: false,
                if_not_exists: false,
                sqlite_text: None,
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

fn take_over_index(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    spec: &ConstraintSpec,
    given: &Name,
    dialect: Dialect,
) -> SqlResult<bool> {
    let table = current(writer, tables, name)?;
    let Some(index) = table.index_named(&given.text).cloned() else {
        return Ok(false);
    };
    let (ConstraintKind::PrimaryKey(columns) | ConstraintKind::Unique(columns)) = &spec.kind else {
        return Ok(false);
    };
    let mut wanted = Vec::with_capacity(columns.len());
    for column in columns {
        wanted.push(column_index(&table, column, dialect)?);
    }
    let same_columns = index.columns == wanted;
    match &spec.kind {
        ConstraintKind::Unique(_) if !index.unique => Err(SqlError::new(
            SqlState::DUPLICATE_TABLE,
            "name is already used by an existing object",
        )
        .with_oracle_code(955)
        .at(given.position)),
        ConstraintKind::Unique(_) if same_columns && !index.backs_unique_constraint => {
            let mut table = table;
            if let Some(found) = table
                .indexes
                .iter_mut()
                .find(|found| found.name == index.name)
            {
                found.backs_unique_constraint = true;
            }
            save(writer, &table)?;
            Ok(true)
        }
        ConstraintKind::Unique(_) => Ok(same_columns),
        ConstraintKind::PrimaryKey(_) if same_columns => {
            let drop = crate::ast::DropIndex {
                names: vec![given.clone()],
                table: Some(Name {
                    text: table.name.clone(),
                    position: given.position,
                }),
                if_exists: false,
            };
            indexes::drop_index(writer, tables, &drop, dialect)?;
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn drop_index_taken_over(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    spec: &ConstraintSpec,
    using: &Name,
    dialect: Dialect,
) -> SqlResult<bool> {
    let index_name = crate::schemas::base_name(&using.text);
    if spec
        .name
        .as_ref()
        .is_some_and(|given| given.text == index_name)
    {
        return Ok(false);
    }
    let (ConstraintKind::PrimaryKey(columns) | ConstraintKind::Unique(columns)) = &spec.kind else {
        return Ok(false);
    };
    let table = current(writer, tables, name)?;
    let Some(index) = table.index_named(index_name).cloned() else {
        return Err(
            SqlError::new(SqlState::UNDEFINED_OBJECT, "specified index does not exist")
                .with_oracle_code(1418)
                .at(using.position),
        );
    };
    let mut wanted = Vec::with_capacity(columns.len());
    for column in columns {
        wanted.push(column_index(&table, column, dialect)?);
    }
    let enforces = index
        .columns
        .get(..wanted.len())
        .is_some_and(|leading| wanted.iter().all(|column| leading.contains(column)));
    if !enforces {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "specified index cannot be used to enforce the constraint",
        )
        .with_oracle_code(14196)
        .at(using.position));
    }
    if index.columns.len() != wanted.len() || index.backs_unique_constraint {
        return Ok(false);
    }
    let drop = crate::ast::DropIndex {
        names: vec![Name {
            text: index_name.to_owned(),
            position: using.position,
        }],
        table: Some(Name {
            text: table.name.clone(),
            position: using.position,
        }),
        if_exists: false,
    };
    indexes::drop_index(writer, tables, &drop, dialect)?;
    Ok(true)
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
        table.forget_missing_statuses();
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
        table.forget_missing_statuses();
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
        .find(|index| {
            indexes::same_index_name(&index.name, &constraint.text, dialect)
                && (dialect != Dialect::PostgreSql || index.backs_unique_constraint)
        })
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
    if dialect == Dialect::Oracle
        && let Some(position) =
            crate::oracle_dictionary::not_null_constraint_column(&table, &constraint.text)
    {
        table.columns[position].not_null = false;
        save(writer, &table)?;
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

fn missing_constraint(table: &TableDefinition, constraint: &Name) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!(
            "constraint \"{}\" of relation \"{}\" does not exist",
            constraint.text,
            crate::schemas::display(&table.name)
        ),
    )
    .at(constraint.position)
}

fn validate_constraint(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    constraint: &Name,
) -> SqlResult<()> {
    let mut table = current(writer, tables, name)?;
    let check = table
        .checks
        .iter()
        .position(|check| check.name == constraint.text);
    let key = table
        .foreign_keys
        .iter()
        .position(|key| key.name == constraint.text);
    if check.is_none() && key.is_none() {
        if table.constraint_name_taken(&constraint.text) {
            return Err(SqlError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!(
                    "constraint \"{}\" of relation \"{}\" is not a foreign key or check constraint",
                    constraint.text,
                    crate::schemas::display(&table.name)
                ),
            )
            .at(constraint.position));
        }
        return Err(missing_constraint(&table, constraint));
    }
    if table.is_validated(&constraint.text) {
        return Ok(());
    }
    constraints::validate_existing(writer, tables, &table, check, key)?;
    let deferred = table.is_initially_deferred(&constraint.text);
    table.set_status(&constraint.text, true, deferred);
    save(writer, &table)
}

fn rename_constraint(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    from: &Name,
    to: &Name,
) -> SqlResult<()> {
    constraints::check_identifier(to)?;
    let mut table = current(writer, tables, name)?;
    if !table.constraint_name_taken(&from.text) {
        return Err(missing_constraint(&table, from));
    }
    if table.constraint_name_taken(&to.text) {
        return Err(SqlError::new(
            SqlState::DUPLICATE_OBJECT,
            format!(
                "constraint \"{}\" for relation \"{}\" already exists",
                to.text,
                crate::schemas::display(&table.name)
            ),
        )
        .at(to.position));
    }
    if let Some(check) = table
        .checks
        .iter_mut()
        .find(|check| check.name == from.text)
    {
        check.name = to.text.clone();
    } else if let Some(key) = table
        .foreign_keys
        .iter_mut()
        .find(|key| key.name == from.text)
    {
        key.name = to.text.clone();
    } else if !table.primary_key.is_empty() && table.primary_key_constraint_name() == from.text {
        table.primary_key_name = (to.text
            != format!("{}_pkey", crate::schemas::base_name(&table.name)))
        .then(|| to.text.clone());
    } else {
        let schema = crate::schemas::schema_of(&table.name).to_owned();
        check_index_name_free(writer, &schema, &to.text, to.position)?;
        if let Some(index) = table
            .indexes
            .iter_mut()
            .find(|index| index.name == from.text)
        {
            index.name = to.text.clone();
        }
    }
    table.rename_status(&from.text, &to.text);
    save(writer, &table)?;
    crate::comments::move_part(writer, name, CommentKind::Constraint, &from.text, &to.text)
}

fn rename_index(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    from: &Name,
    to: &Name,
) -> SqlResult<()> {
    let mut table = current(writer, tables, name)?;
    let Some(position) = table
        .indexes
        .iter()
        .position(|index| indexes::same_index_name(&index.name, &from.text, Dialect::MySql))
    else {
        return Err(SqlError::new(
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!(
                "Key '{}' doesn't exist in table '{}'",
                from.text,
                crate::schemas::base_name(&table.name)
            ),
        )
        .with_vendor_code(1176)
        .at(from.position));
    };
    let stored = table.indexes[position].name.clone();
    if stored == to.text {
        return Ok(());
    }
    if to.text.eq_ignore_ascii_case("primary") {
        return Err(SqlError::new(
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!("Incorrect index name '{}'", to.text),
        )
        .with_vendor_code(1280)
        .at(to.position));
    }
    let taken = table.indexes.iter().enumerate().any(|(at, index)| {
        at != position && indexes::same_index_name(&index.name, &to.text, Dialect::MySql)
    });
    if taken {
        return Err(SqlError::new(
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!("Duplicate key name '{}'", to.text),
        )
        .with_vendor_code(1061)
        .at(to.position));
    }
    let Some(index) = table.indexes.get_mut(position) else {
        return Err(crate::encoding::corrupt("index"));
    };
    index.name = to.text.clone();
    let index_id = index.id;
    table.rename_status(&stored, &to.text);
    save(writer, &table)?;
    crate::comments::move_part(writer, name, CommentKind::Index, &stored, &to.text)?;
    crate::partition_indexes::rename_linked(
        writer,
        tables,
        &table,
        index_id,
        (stored.as_str(), to.text.as_str()),
        0,
    )
}

fn check_index_name_free(
    writer: &mut dyn KeyValueWrite,
    schema: &str,
    index: &str,
    position: usize,
) -> SqlResult<()> {
    let taken = indexes::every_table(writer)?.iter().any(|other| {
        crate::schemas::schema_of(&other.name) == schema
            && (other.index_named(index).is_some()
                || crate::schemas::base_name(&other.name) == index)
    });
    if taken {
        return Err(SqlError::new(
            SqlState::DUPLICATE_TABLE,
            format!("relation \"{index}\" already exists"),
        )
        .at(position));
    }
    Ok(())
}

fn not_an_identity(table: &TableDefinition, column: &Name) -> SqlError {
    SqlError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!(
            "column \"{}\" of relation \"{}\" is not an identity column",
            column.text,
            crate::schemas::display(&table.name)
        ),
    )
    .at(column.position)
}

fn owned_sequence(
    writer: &mut dyn KeyValueWrite,
    table: &str,
    column: &str,
) -> SqlResult<Option<String>> {
    let table = crate::schemas::normalized(table);
    Ok(crate::sequences::every_sequence(writer)?
        .into_iter()
        .find(|(_, sequence)| {
            sequence
                .owned_by
                .as_ref()
                .is_some_and(|(owner, owner_column)| {
                    crate::schemas::normalized(owner) == table && owner_column == column
                })
        })
        .map(|(name, _)| name))
}

#[allow(clippy::too_many_arguments)]
fn alter_identity(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &Name,
    generation: Option<IdentityGeneration>,
    options: &SequenceOptions,
    dialect: Dialect,
) -> SqlResult<()> {
    let mut table = current(writer, tables, name)?;
    let index = column_index(&table, column, dialect)?;
    if table.identity_of(index).is_none() {
        return Err(not_an_identity(&table, column));
    }
    if let Some(generation) = generation {
        table.set_identity(index, Some(generation));
    }
    let changes_sequence = *options != SequenceOptions::default();
    if changes_sequence && table.columns[index].auto_increment {
        if options.min.is_some()
            || options.max.is_some()
            || options.cycle.is_some()
            || options.restart.is_some()
            || options.sql_type.is_some()
        {
            return Err(SqlError::not_supported(
                "MINVALUE, MAXVALUE, CYCLE, RESTART and AS for an identity column of a CREATE TABLE",
            )
            .with_hint("START WITH and INCREMENT BY can change; the next value follows the table's highest stored one."));
        }
        let current_seed = table.identity_seed();
        let seed = crate::encoding::IdentitySeed {
            seed: options.start.unwrap_or(current_seed.seed),
            increment: options.increment.unwrap_or(current_seed.increment),
        };
        if seed.increment == 0 {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "INCREMENT must not be zero",
            ));
        }
        table.identity_seed = Some(seed);
    }
    save(writer, &table)?;
    if changes_sequence && !table.columns[index].auto_increment {
        let Some(sequence) = owned_sequence(writer, &table.name, &column.text)? else {
            return Err(not_an_identity(&table, column));
        };
        let alter = crate::ast::AlterSequence {
            name: Name {
                text: sequence,
                position: column.position,
            },
            if_exists: false,
            options: options.clone(),
            rename: None,
        };
        crate::sequences::alter_sequence(writer, tables, &alter, dialect)?;
    }
    Ok(())
}

fn drop_identity(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &Name,
    if_exists: bool,
    dialect: Dialect,
) -> SqlResult<Option<String>> {
    let mut table = current(writer, tables, name)?;
    let index = column_index(&table, column, dialect)?;
    if table.identity_of(index).is_none() {
        if if_exists {
            return Ok(Some(format!(
                "NOTICE:  column \"{}\" of relation \"{}\" is not an identity column, skipping",
                column.text,
                crate::schemas::display(&table.name)
            )));
        }
        return Err(not_an_identity(&table, column));
    }
    let sequence_backed = !table.columns[index].auto_increment;
    table.set_identity(index, None);
    if sequence_backed {
        table.columns[index].default = None;
        table.set_default_dialect(index, Dialect::PostgreSql);
    } else {
        table.columns[index].auto_increment = false;
        table.identity_seed = None;
    }
    save(writer, &table)?;
    if sequence_backed {
        crate::sequences::drop_owned_by(writer, tables, &table.name, Some(column.text.as_str()))?;
    }
    Ok(None)
}

fn not_generated(table: &TableDefinition, column: &Name) -> SqlError {
    SqlError::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!(
            "column \"{}\" of relation \"{}\" is not a generated column",
            column.text,
            crate::schemas::display(&table.name)
        ),
    )
    .at(column.position)
}

fn set_expression(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &Name,
    generation: &crate::ast::GenerationSpec,
    dialect: Dialect,
) -> SqlResult<()> {
    let mut table = current(writer, tables, name)?;
    let index = column_index(&table, column, dialect)?;
    let Some(entry) = table.generation_of(index).cloned() else {
        return Err(not_generated(&table, column));
    };
    let keyed = table.primary_key.contains(&index)
        || table
            .indexes
            .iter()
            .any(|found| found.unique && found.columns.contains(&index));
    if keyed {
        return Err(SqlError::not_supported(
            "SET EXPRESSION on a generated column in a primary key or unique index",
        )
        .at(column.position));
    }
    let old = table.clone();
    table.generated.retain(|found| found.column != index);
    let spec = crate::ast::GenerationSpec {
        stored: entry.stored,
        ..generation.clone()
    };
    crate::generated_columns::define(&mut table, &[(index, &spec)], dialect)?;
    let generator = crate::generated_columns::Generator::of(&table)?;
    let checks = PreparedChecks::of(&table)?;
    let indexed = !table.indexes.is_empty();
    if indexed {
        indexes::delete_every_index_entry(writer, &old)?;
    }
    save(writer, &table)?;
    let mut map = |mut row: Vec<Value>| -> SqlResult<Vec<Value>> {
        if let Some(generator) = &generator {
            generator.fill(&table, &mut row)?;
        }
        execute::check_not_null_of(&table, &row)?;
        if !checks.is_empty() {
            checks.check(&table, &row)?;
        }
        Ok(row)
    };
    rewrite_in_place(writer, &table, &table.columns, &mut map)?;
    if indexed {
        for (key, row) in execute::every_row(&*writer, &table)? {
            indexes::put_entries(writer, &table, &row, &key)?;
        }
    }
    Ok(())
}

fn drop_expression(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    column: &Name,
    if_exists: bool,
    dialect: Dialect,
) -> SqlResult<Option<String>> {
    let mut table = current(writer, tables, name)?;
    let index = column_index(&table, column, dialect)?;
    if table.generation_of(index).is_none() {
        if if_exists {
            return Ok(Some(format!(
                "NOTICE:  column \"{}\" of relation \"{}\" is not a stored generated column, skipping",
                column.text,
                crate::schemas::display(&table.name)
            )));
        }
        return Err(not_generated(&table, column));
    }
    let dependents = crate::generated_columns::dependents_of(&table, index);
    if dependents.iter().any(|&other| other != index) {
        return Err(SqlError::not_supported(
            "DROP EXPRESSION on a generated column that other generated columns read",
        )
        .at(column.position));
    }
    table.generated.retain(|found| found.column != index);
    save(writer, &table)?;
    Ok(None)
}

fn serial_sequence_name(
    writer: &mut dyn KeyValueWrite,
    table: &str,
    column: &str,
) -> SqlResult<String> {
    let schema = crate::schemas::schema_of(table);
    let base = format!("{}_{column}_seq", crate::schemas::base_name(table));
    let mut candidate = crate::schemas::qualified(schema, &base);
    let mut suffix = 1u32;
    while crate::sequences::relation_exists(writer, &candidate)? {
        candidate = crate::schemas::qualified(schema, &format!("{base}{suffix}"));
        suffix = suffix.saturating_add(1);
        if suffix > 10_000 {
            return Err(SqlError::new(
                SqlState::DUPLICATE_TABLE,
                format!("no free name for the sequence of column \"{column}\""),
            ));
        }
    }
    Ok(candidate)
}

fn add_serial_column(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    name: &str,
    definition: &ColumnDefinition,
    dialect: Dialect,
) -> SqlResult<Option<String>> {
    let position = definition.name.position;
    let sequence = serial_sequence_name(writer, name, &definition.name.text)?;
    let sequence_name = Name {
        text: sequence.clone(),
        position,
    };
    crate::sequences::create_sequence(
        writer,
        tables,
        &crate::ast::CreateSequence {
            name: sequence_name.clone(),
            if_not_exists: false,
            or_replace: false,
            options: crate::ast::SequenceOptions {
                sql_type: Some((definition.sql_type, position)),
                ..crate::ast::SequenceOptions::default()
            },
        },
        dialect,
    )?;
    let shown = crate::schemas::display(&sequence);
    let text = format!("nextval('{shown}'::regclass)");
    let call = crate::ast::Expr {
        kind: crate::ast::ExprKind::Function {
            name: "nextval".to_owned(),
            arguments: vec![crate::ast::Expr {
                kind: crate::ast::ExprKind::Literal(crate::ast::Literal::String(shown)),
                position,
            }],
        },
        position,
    };
    let plain = ColumnDefinition {
        auto_increment: false,
        serial: false,
        not_null: true,
        default: Some((call, text)),
        ..definition.clone()
    };
    let notice = add_column(writer, tables, name, &plain, false, dialect)?;
    crate::sequences::alter_sequence(
        writer,
        tables,
        &crate::ast::AlterSequence {
            name: sequence_name,
            if_exists: false,
            options: crate::ast::SequenceOptions {
                owned_by: Some(Some((
                    Name {
                        text: name.to_owned(),
                        position,
                    },
                    definition.name.clone(),
                ))),
                ..crate::ast::SequenceOptions::default()
            },
            rename: None,
        },
        dialect,
    )?;
    Ok(notice)
}

