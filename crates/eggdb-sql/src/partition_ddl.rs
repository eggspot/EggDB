use crate::ast::{
    ColumnDefinition, ConstraintKind, ConstraintSpec, CreateTable, IndexSpec, Name,
    NamedPartitionBound, PartitionBoundSpec, PartitionBy, PartitionBoundValue,
};
use crate::dialect::Dialect;
use crate::encoding::TableDefinition;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::partitions::{PartitionBound, PartitionKey, PartitionStrategy};
use crate::session::QueryResult;
use crate::store::KeyValueWrite;
use crate::types::SqlType;
use std::sync::Arc;

const MYSQL_PARTITION_MARKER: &str = "#P#";

pub(crate) fn wants(create: &CreateTable) -> bool {
    create.partition_of.is_some() || create.partition_by.is_some()
}

fn not_partitioned(table: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!(
            "table \"{}\" is not partitioned",
            crate::schemas::display(table)
        ),
    )
    .at(position)
}

fn unsupported(what: &str, position: usize) -> SqlError {
    SqlError::not_supported(what).at(position)
}

fn copied_column(table: &TableDefinition, index: usize, position: usize) -> SqlResult<ColumnDefinition> {
    let column = &table.columns[index];
    let dialect = table.default_dialect(index);
    let default = match (&column.default, execute::default_expression(column, dialect)?) {
        (Some(text), Some(expr)) => Some((expr, text.clone())),
        _ => None,
    };
    Ok(ColumnDefinition {
        name: Name {
            text: column.name.clone(),
            position,
        },
        sql_type: column.sql_type,
        not_null: column.not_null,
        primary_key: false,
        default,
        default_dialect: dialect,
        auto_increment: false,
        identity_seed: None,
        generated_always: false,
        unique: None,
        constraints: Vec::new(),
        comment: None,
    })
}

fn names_of(table: &TableDefinition, columns: &[usize], position: usize) -> Vec<Name> {
    columns
        .iter()
        .map(|&column| Name {
            text: table.columns[column].name.clone(),
            position,
        })
        .collect()
}

fn copy_of(parent: &TableDefinition, name: Name) -> SqlResult<CreateTable> {
    let position = name.position;
    let columns = (0..parent.columns.len())
        .map(|index| copied_column(parent, index, position))
        .collect::<SqlResult<Vec<_>>>()?;
    let primary_key = (!parent.primary_key.is_empty())
        .then(|| (names_of(parent, &parent.primary_key, position), position));
    let indexes = parent
        .indexes
        .iter()
        .filter(|index| index.unique)
        .map(|index| IndexSpec {
            name: None,
            unique: true,
            columns: names_of(parent, &index.columns, position),
            position,
            method: index.method,
            sort_options: index.sort_options,
        })
        .collect();
    let mut constraints = Vec::with_capacity(parent.checks.len());
    for check in &parent.checks {
        let expr = crate::constraints::parse_check(check)?;
        constraints.push(ConstraintSpec {
            name: Some(Name {
                text: check.name.clone(),
                position,
            }),
            kind: ConstraintKind::Check {
                expr,
                text: check.expression.clone(),
            },
            position,
            column: None,
            using_index: None,
            not_valid: false,
        });
    }
    Ok(CreateTable {
        name,
        temporary: None,
        if_not_exists: false,
        columns,
        primary_key,
        primary_key_name: None,
        indexes,
        constraints,
        comment: None,
        partition_by: None,
        partition_of: None,
    })
}

fn key_types(table: &TableDefinition, key: &PartitionKey) -> SqlResult<Vec<SqlType>> {
    key.columns
        .iter()
        .map(|column| {
            table
                .column_index(column)
                .map(|index| table.columns[index].sql_type)
                .ok_or_else(|| crate::encoding::corrupt("partition key"))
        })
        .collect()
}

fn validated_key(table: &TableDefinition, by: &PartitionBy) -> SqlResult<PartitionKey> {
    let mut columns: Vec<String> = Vec::with_capacity(by.columns.len());
    let mut positions = Vec::with_capacity(by.columns.len());
    for column in &by.columns {
        let Some(index) = table.column_index(&column.text) else {
            return Err(SqlError::new(
                SqlState::UNDEFINED_COLUMN,
                format!(
                    "column \"{}\" named in partition key does not exist",
                    column.text
                ),
            )
            .at(column.position));
        };
        if columns.contains(&column.text) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column \"{}\" appears twice in partition key", column.text),
            )
            .at(column.position));
        }
        columns.push(column.text.clone());
        positions.push(index);
    }
    if by.strategy == PartitionStrategy::List && columns.len() != 1 {
        return Err(SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            "cannot use \"list\" partition strategy with more than one column",
        )
        .at(by.position));
    }
    if table.columns.iter().any(|column| column.auto_increment) {
        return Err(unsupported(
            "AUTO_INCREMENT and identity columns in a partitioned table",
            by.position,
        ));
    }
    let covers = |unique: &[usize]| positions.iter().all(|key| unique.contains(key));
    let primary_covers = table.primary_key.is_empty() || covers(&table.primary_key);
    if !primary_covers
        || table
            .indexes
            .iter()
            .any(|index| index.unique && !covers(&index.columns))
    {
        return Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "unique constraint on partitioned table must include all partitioning columns",
        )
        .at(by.position));
    }
    Ok(PartitionKey {
        strategy: by.strategy,
        columns,
    })
}

pub(crate) fn create_table(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreateTable,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let position = create.name.position;
    if create.temporary.is_some() {
        return Err(unsupported("partitioned temporary tables", position));
    }
    let existed = tables.table(writer, &create.name.text)?.is_some();
    if existed && create.if_not_exists {
        let plain = CreateTable {
            partition_by: None,
            partition_of: None,
            ..create.clone()
        };
        return execute::create_table(writer, tables, &plain, dialect);
    }
    if let Some(of) = &create.partition_of {
        let parent_name = crate::schemas::resolve_relation(writer, &of.parent.text)?;
        let parent = tables.require_target(
            writer,
            &Name {
                text: parent_name,
                position: of.parent.position,
            },
            "create a partition of",
        )?;
        let mut child = copy_of(&parent, create.name.clone())?;
        child.partition_by = create.partition_by.clone();
        let result = create_table(writer, tables, &child, dialect)?;
        attach(writer, tables, &parent, &create.name, &of.bound)?;
        return Ok(result);
    }
    let Some(by) = &create.partition_by else {
        return execute::create_table(writer, tables, create, dialect);
    };
    let plain = CreateTable {
        partition_by: None,
        partition_of: None,
        ..create.clone()
    };
    let result = execute::create_table(writer, tables, &plain, dialect)?;
    let table = tables.require_target(writer, &create.name, "partition")?;
    let key = validated_key(&table, by)?;
    let mut record = crate::partitions::stored(writer, &table.name)?;
    record.key = Some(key.clone());
    crate::partitions::save(writer, &table.name, &record)?;
    create_named_partitions(writer, tables, &table, &key, by, dialect)?;
    Ok(result)
}

fn named_child(table: &TableDefinition, partition: &str, position: usize) -> Name {
    Name {
        text: format!("{}{MYSQL_PARTITION_MARKER}{partition}", table.name),
        position,
    }
}

fn create_named_partitions(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &Arc<TableDefinition>,
    key: &PartitionKey,
    by: &PartitionBy,
    dialect: Dialect,
) -> SqlResult<()> {
    let position = by.position;
    let width = key.columns.len();
    let mut previous: Vec<PartitionBoundValue> = vec![PartitionBoundValue::MinValue; width];
    let mut planned: Vec<(Name, PartitionBoundSpec)> = Vec::new();
    for (number, partition) in by.partitions.iter().enumerate() {
        let name = named_child(table, &partition.name.text, partition.name.position);
        let bound = match &partition.bound {
            NamedPartitionBound::LessThan(values) => {
                let mut to = values.clone();
                if to.len() == 1 && to[0] == PartitionBoundValue::MaxValue {
                    to = vec![PartitionBoundValue::MaxValue; width];
                }
                if to.len() != width {
                    return Err(SqlError::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        "VALUES LESS THAN must give one value for each partition key column",
                    )
                    .at(partition.name.position));
                }
                let from = std::mem::replace(&mut previous, to.clone());
                PartitionBoundSpec::Range { from, to }
            }
            NamedPartitionBound::In(values) if values.is_empty() => PartitionBoundSpec::Default,
            NamedPartitionBound::In(values) => PartitionBoundSpec::List(values.clone()),
            NamedPartitionBound::Hash => PartitionBoundSpec::Hash {
                modulus: u32::try_from(by.partitions.len()).unwrap_or(u32::MAX),
                remainder: u32::try_from(number).unwrap_or(u32::MAX),
            },
        };
        planned.push((name, bound));
    }
    if planned.is_empty()
        && let Some(count) = by.hash_partitions
    {
        for remainder in 0..count {
            planned.push((
                named_child(table, &format!("p{remainder}"), position),
                PartitionBoundSpec::Hash {
                    modulus: count,
                    remainder,
                },
            ));
        }
    }
    for (name, bound) in planned {
        let child = copy_of(table, name.clone())?;
        execute::create_table(writer, tables, &child, dialect)?;
        attach(writer, tables, table, &name, &bound)?;
    }
    Ok(())
}

fn same_columns(parent: &TableDefinition, child: &TableDefinition, position: usize) -> SqlResult<()> {
    for column in &child.columns {
        match parent.column_index(&column.name) {
            Some(index) if parent.columns[index].sql_type == column.sql_type => {}
            Some(_) => {
                return Err(SqlError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "child table \"{}\" has different type for column \"{}\"",
                        crate::schemas::display(&child.name),
                        column.name
                    ),
                )
                .at(position));
            }
            None => {
                return Err(SqlError::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "table \"{}\" contains column \"{}\" not found in parent \"{}\"",
                        crate::schemas::display(&child.name),
                        column.name,
                        crate::schemas::display(&parent.name)
                    ),
                )
                .with_detail("The new partition may contain only the columns present in parent.")
                .at(position));
            }
        }
    }
    if let Some(missing) = parent
        .columns
        .iter()
        .find(|column| child.column_index(&column.name).is_none())
    {
        return Err(SqlError::new(
            SqlState::DATATYPE_MISMATCH,
            format!(
                "child table is missing column \"{}\"",
                missing.name
            ),
        )
        .at(position));
    }
    Ok(())
}

pub(crate) fn attach(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    partition: &Name,
    bound: &PartitionBoundSpec,
) -> SqlResult<()> {
    let position = partition.position;
    let catalog = crate::partitions::catalog(writer, tables)?;
    let Some(key) = catalog.key_of(&parent.name).cloned() else {
        return Err(not_partitioned(&parent.name, position));
    };
    let child_name = crate::schemas::resolve_relation(writer, &partition.text)?;
    let child = tables.require_target(
        writer,
        &Name {
            text: child_name.clone(),
            position,
        },
        "attach",
    )?;
    if child.name == parent.name || catalog.descendants_of(&child.name).contains(&parent.name) {
        return Err(SqlError::new(
            SqlState::DUPLICATE_TABLE,
            format!(
                "cannot attach \"{}\" as a partition of itself or of one of its partitions",
                crate::schemas::display(&child.name)
            ),
        )
        .at(position));
    }
    if let Some((owner, _)) = catalog.parent_of(&child.name) {
        return Err(SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            format!(
                "\"{}\" is already a partition of \"{}\"",
                crate::schemas::display(&child.name),
                crate::schemas::display(owner)
            ),
        )
        .at(position));
    }
    same_columns(parent, &child, position)?;
    let types = key_types(parent, &key)?;
    let bound = crate::partitions::bound_of(bound, &key, &types, position)?;
    crate::partitions::check_no_overlap(&catalog, &parent.name, &child.name, &bound, &types, position)?;
    let mut record = crate::partitions::stored(writer, &child.name)?;
    record.parent = Some((parent.name.clone(), bound));
    crate::partitions::save(writer, &child.name, &record)?;
    check_existing_rows(writer, tables, &child, position)?;
    for (sibling, sibling_bound) in catalog.children_of(&parent.name) {
        if matches!(sibling_bound, PartitionBound::Default) && sibling != child.name {
            let sibling = tables.require_target(
                writer,
                &Name {
                    text: sibling.to_owned(),
                    position,
                },
                "attach",
            )?;
            check_existing_rows(writer, tables, &sibling, position)?;
        }
    }
    Ok(())
}

fn check_existing_rows(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &TableDefinition,
    position: usize,
) -> SqlResult<()> {
    let Some(constraint) = crate::partitions::constraint_of(writer, tables, table)? else {
        return Ok(());
    };
    for (_, row) in execute::every_row(writer, table)? {
        if !constraint.holds(&row) {
            return Err(SqlError::new(
                SqlState::CHECK_VIOLATION,
                format!(
                    "partition constraint of relation \"{}\" is violated by some row",
                    crate::schemas::display(&table.name)
                ),
            )
            .at(position));
        }
    }
    Ok(())
}

pub(crate) fn detach(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    partition: &Name,
) -> SqlResult<()> {
    let child_name = crate::schemas::resolve_relation(writer, &partition.text)?;
    let catalog = crate::partitions::catalog(writer, tables)?;
    match catalog.parent_of(&child_name) {
        Some((owner, _)) if owner == parent.name => {}
        _ => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_TABLE,
                format!(
                    "relation \"{}\" is not a partition of relation \"{}\"",
                    crate::schemas::display(&child_name),
                    crate::schemas::display(&parent.name)
                ),
            )
            .at(partition.position));
        }
    }
    let mut record = crate::partitions::stored(writer, &child_name)?;
    record.parent = None;
    crate::partitions::save(writer, &child_name, &record)
}
