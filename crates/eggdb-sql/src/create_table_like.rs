use crate::ast::{ColumnDefinition, CreateTable, CreateTableLike, Name, OnCommit};
use crate::dialect::Dialect;
use crate::encoding::{TableDefinition, table_key};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, Relation, TableCache};
use crate::name_resolution::Binding;
use crate::partitions::PartitionBound;
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite};
use std::sync::Arc;

const MYSQL_PARTITION_MARKER: &str = "#P#";
const MAX_PARTITION_DEPTH: usize = 32;

pub(crate) fn run(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    binding: &Binding<'_>,
    dialect: Dialect,
    like: &mut CreateTableLike,
) -> SqlResult<QueryResult> {
    binding.names(&*writer, tables, std::slice::from_mut(&mut like.source))?;
    let source = source_table(&*writer, tables, binding, &like.source)?;
    let partitioning = crate::partitions::stored(&*writer, &source.name)?;
    let partitioned = partitioning.key.is_some();
    if partitioned && like.temporary {
        return Err(SqlError::new(
            SqlState::MYSQL_GENERAL,
            "Cannot create temporary table with partitions",
        )
        .with_vendor_code(1562)
        .at(like.name.position));
    }
    let mut shell = shell_table(&*writer, &source, like.name.clone())?;
    shell.temporary = like.temporary.then_some(OnCommit::PreserveRows);
    shell.if_not_exists = like.if_not_exists;
    binding.create_table(&*writer, tables, &mut shell)?;
    let existed = crate::sequences::relation_exists(&*writer, &shell.name.text)?;
    let created = execute::create_table(writer, tables, &shell, dialect)?;
    if existed {
        return Ok(created);
    }
    let target = tables.require(&*writer, &shell.name)?;
    let copy = copied_definition(&source, &target);
    writer.put(&table_key(&copy.name), &copy.encode())?;
    if partitioned {
        let mut parent = partitioning;
        parent.parent = None;
        parent.index_links = Vec::new();
        crate::partitions::save(writer, &copy.name, &parent)?;
        copy_partitions(
            writer,
            tables,
            dialect,
            &source.name,
            &copy.name,
            like.name.position,
            0,
        )?;
    }
    Ok(created)
}

fn source_table(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    binding: &Binding<'_>,
    name: &Name,
) -> SqlResult<Arc<TableDefinition>> {
    match tables.relation(reader, &name.text)? {
        Some(Relation::Table(table)) => Ok(table),
        Some(Relation::View(_)) => Err(SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!(
                "'{}.{}' is not BASE TABLE",
                binding.settings.database,
                crate::schemas::base_name(&name.text)
            ),
        )
        .with_vendor_code(1347)
        .at(name.position)),
        None => Err(SqlError::new(
            SqlState::UNDEFINED_TABLE,
            format!(
                "relation \"{}\" does not exist",
                crate::schemas::base_name(&name.text)
            ),
        )
        .at(name.position)),
    }
}

fn shell_table(
    reader: &dyn KeyValueRead,
    source: &TableDefinition,
    name: Name,
) -> SqlResult<CreateTable> {
    let position = name.position;
    let columns = crate::comments::column_comments(reader, &source.name)?
        .into_iter()
        .filter(|(column, _)| source.column_index(column).is_some())
        .map(|(column, comment)| commented_column(source, column, comment, position))
        .collect();
    Ok(CreateTable {
        system_versioning: None,
        rowversion_column: None,
        lsm_storage: false,
        name,
        temporary: None,
        if_not_exists: false,
        columns,
        primary_key: None,
        primary_key_name: None,
        indexes: Vec::new(),
        constraints: Vec::new(),
        comment: crate::comments::relation_comment(reader, &source.name)?,
        partition_by: None,
        partition_of: None,
        sqlite_text: None,
    })
}

fn commented_column(
    source: &TableDefinition,
    column: String,
    comment: String,
    position: usize,
) -> ColumnDefinition {
    let sql_type = source
        .column_index(&column)
        .and_then(|index| source.columns.get(index))
        .map_or(crate::types::SqlType::Text, |found| found.sql_type);
    ColumnDefinition {
        name: Name {
            text: column,
            position,
        },
        sql_type,
        not_null: false,
        primary_key: false,
        default: None,
        default_dialect: Dialect::MySql,
        auto_increment: false,
        serial: false,
        identity_seed: None,
        generated_always: false,
        on_update_current_timestamp: false,
        unique: None,
        constraints: Vec::new(),
        comment: Some(comment),
        generated: None,
    }
}

fn renamed(name: &str, from: &str, to: &str) -> String {
    match name.strip_prefix(from) {
        Some(rest) if rest.starts_with('_') => format!("{to}{rest}"),
        _ => name.to_owned(),
    }
}

fn copied_definition(source: &TableDefinition, target: &TableDefinition) -> TableDefinition {
    let from = crate::schemas::base_name(&source.name);
    let to = crate::schemas::base_name(&target.name);
    let foreign: Vec<&str> = source
        .foreign_keys
        .iter()
        .map(|key| key.name.as_str())
        .collect();
    let mut copy = source.clone();
    copy.id = target.id;
    copy.name = target.name.clone();
    copy.foreign_keys.clear();
    copy.referenced = target.referenced;
    copy.owns_sequence = false;
    copy.constraint_statuses
        .retain(|status| !foreign.contains(&status.name.as_str()));
    for status in &mut copy.constraint_statuses {
        status.name = renamed(&status.name, from, to);
    }
    for check in &mut copy.checks {
        check.name = renamed(&check.name, from, to);
    }
    copy.primary_key_name = copy
        .primary_key_name
        .as_deref()
        .map(|name| renamed(name, from, to));
    copy
}

fn children(reader: &dyn KeyValueRead, parent: &str) -> SqlResult<Vec<(String, PartitionBound)>> {
    let catalog = crate::partitions::load(reader)?;
    Ok(catalog
        .children_of(parent)
        .into_iter()
        .map(|(child, bound)| (child.to_owned(), bound.clone()))
        .collect())
}

fn partition_name(source: &str, child: &str, target: &str) -> String {
    let marked = format!("{source}{MYSQL_PARTITION_MARKER}");
    let suffix = child
        .strip_prefix(marked.as_str())
        .unwrap_or_else(|| crate::schemas::base_name(child));
    format!("{target}{MYSQL_PARTITION_MARKER}{suffix}")
}

fn copy_partitions(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    dialect: Dialect,
    source: &str,
    target: &str,
    position: usize,
    depth: usize,
) -> SqlResult<()> {
    if depth > MAX_PARTITION_DEPTH {
        return Err(SqlError::new(
            SqlState::STATEMENT_TOO_COMPLEX,
            "partitions nest too deep to copy",
        )
        .at(position));
    }
    for (child, bound) in children(&*writer, source)? {
        let Some(partition) = tables.table(&*writer, &child)? else {
            continue;
        };
        let name = Name {
            text: partition_name(source, &child, target),
            position,
        };
        let shell = shell_table(&*writer, &partition, name.clone())?;
        execute::create_table(writer, tables, &shell, dialect)?;
        let created = tables.require(&*writer, &name)?;
        let copy = copied_definition(&partition, &created);
        writer.put(&table_key(&copy.name), &copy.encode())?;
        let mut record = crate::partitions::stored(&*writer, &child)?;
        record.parent = Some((target.to_owned(), bound));
        crate::partitions::save(writer, &copy.name, &record)?;
        if record.key.is_some() {
            copy_partitions(
                writer,
                tables,
                dialect,
                &child,
                &copy.name,
                position,
                depth + 1,
            )?;
        }
    }
    Ok(())
}
