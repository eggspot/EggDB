use crate::ast::{
    ColumnDefinition, ConstraintKind, ConstraintSpec, CreateTable, Name, NamedPartition,
    NamedPartitionBound, PartitionBoundSpec, PartitionBoundValue, PartitionBy, PartitionKeyPart,
};
use crate::dialect::Dialect;
use crate::encoding::TableDefinition;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::partitions::{KeyPart, PartitionBound, PartitionKey, PartitionStrategy};
use crate::session::QueryResult;
use crate::store::KeyValueWrite;
use std::sync::Arc;

pub(crate) const MYSQL_PARTITION_MARKER: &str = "#P#";

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

fn copied_column(
    table: &TableDefinition,
    index: usize,
    position: usize,
) -> SqlResult<ColumnDefinition> {
    let column = &table.columns[index];
    let dialect = table.default_dialect(index);
    let default = match (
        &column.default,
        execute::default_expression(column, dialect)?,
    ) {
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
        serial: false,
        identity_seed: None,
        generated_always: false,
        on_update_current_timestamp: false,
        unique: None,
        constraints: Vec::new(),
        comment: None,
        generated: None,
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

pub(crate) fn copy_of(parent: &TableDefinition, name: Name) -> SqlResult<CreateTable> {
    let position = name.position;
    let columns = (0..parent.columns.len())
        .map(|index| copied_column(parent, index, position))
        .collect::<SqlResult<Vec<_>>>()?;
    let primary_key = (!parent.primary_key.is_empty())
        .then(|| (names_of(parent, &parent.primary_key, position), position));
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
        system_versioning: None,
        rowversion_column: None,
        name,
        temporary: None,
        if_not_exists: false,
        columns,
        primary_key,
        primary_key_name: None,
        indexes: Vec::new(),
        constraints,
        comment: None,
        partition_by: None,
        partition_of: None,
        sqlite_text: None,
    })
}

fn validated_key(
    table: &TableDefinition,
    by: &PartitionBy,
    dialect: Dialect,
    user_function_exists: &mut dyn FnMut(&str) -> bool,
) -> SqlResult<PartitionKey> {
    let mut parts: Vec<KeyPart> = Vec::with_capacity(by.key.len());
    let mut positions = Vec::with_capacity(by.key.len());
    for part in &by.key {
        match part {
            PartitionKeyPart::Column(column) => {
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
                let named = KeyPart::Column(column.text.clone());
                if parts.contains(&named) {
                    return Err(SqlError::new(
                        SqlState::DUPLICATE_COLUMN,
                        format!("column \"{}\" appears twice in partition key", column.text),
                    )
                    .at(column.position));
                }
                parts.push(named);
                positions.push(index);
            }
            PartitionKeyPart::Expression {
                expr,
                text,
                position,
            } => {
                crate::partitions::check_key_expression(
                    table,
                    expr,
                    dialect,
                    *position,
                    user_function_exists,
                )?;
                if dialect == Dialect::MySql {
                    let mut read = Vec::new();
                    crate::partitions::expression_columns(expr, table, &mut read);
                    for column in read {
                        if !positions.contains(&column) {
                            positions.push(column);
                        }
                    }
                }
                parts.push(KeyPart::Expression {
                    text: text.clone(),
                    dialect,
                });
            }
        }
    }
    if by.mysql_integer_key {
        check_mysql_integer_key(table, by)?;
    }
    if by.strategy == PartitionStrategy::List && parts.len() != 1 {
        return Err(SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            "cannot use \"list\" partition strategy with more than one column",
        )
        .at(by.position));
    }
    let key = PartitionKey {
        strategy: by.strategy,
        parts,
    };
    let has_unique_index = table.indexes.iter().any(|index| index.unique);
    if key.has_expressions()
        && dialect != Dialect::MySql
        && (!table.primary_key.is_empty() || has_unique_index)
    {
        let kind = if table.primary_key.is_empty() {
            "UNIQUE"
        } else {
            "PRIMARY KEY"
        };
        return Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("unsupported {kind} constraint with partition key definition"),
        )
        .with_detail(format!(
            "{kind} constraints cannot be used when partition keys include expressions."
        ))
        .at(by.position));
    }
    let covers = |unique: &[usize]| positions.iter().all(|key| unique.contains(key));
    let primary_covers = table.primary_key.is_empty() || covers(&table.primary_key);
    if !primary_covers
        || table
            .indexes
            .iter()
            .any(|index| index.unique && !covers(&index.columns))
    {
        return Err(unique_lacks_key(by.position));
    }
    Ok(key)
}

const MYSQL_PARTITION_FUNCTIONS: [&str; 23] = [
    "abs",
    "ceiling",
    "datediff",
    "day",
    "dayofmonth",
    "dayofweek",
    "dayofyear",
    "extract",
    "floor",
    "hour",
    "microsecond",
    "minute",
    "mod",
    "month",
    "quarter",
    "second",
    "time_to_sec",
    "to_days",
    "to_seconds",
    "unix_timestamp",
    "weekday",
    "year",
    "yearweek",
];

fn is_mysql_integer(sql_type: crate::types::SqlType) -> bool {
    use crate::types::SqlType;
    matches!(
        sql_type,
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt
    )
}

fn is_mysql_number(sql_type: crate::types::SqlType) -> bool {
    use crate::types::SqlType;
    is_mysql_integer(sql_type)
        || matches!(
            sql_type,
            SqlType::Numeric(_) | SqlType::Real | SqlType::Double
        )
}

fn check_mysql_integer_key(table: &TableDefinition, by: &PartitionBy) -> SqlResult<()> {
    for part in &by.key {
        match part {
            PartitionKeyPart::Column(column) => {
                let Some(index) = table.column_index(&column.text) else {
                    continue;
                };
                if !is_mysql_integer(table.columns[index].sql_type) {
                    return Err(SqlError::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        format!(
                            "Field '{}' is of a not allowed type for this type of partitioning",
                            column.text
                        ),
                    )
                    .with_vendor_code(1659)
                    .at(column.position));
                }
            }
            PartitionKeyPart::Expression { expr, position, .. } => {
                let refused_function = crate::partitions::any_part(expr, &|part| {
                    matches!(
                        &part.kind,
                        crate::ast::ExprKind::Function { name, .. }
                            if !MYSQL_PARTITION_FUNCTIONS
                                .contains(&name.to_ascii_lowercase().as_str())
                    )
                });
                if refused_function {
                    return Err(SqlError::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        "This partition function is not allowed",
                    )
                    .with_vendor_code(1564)
                    .at(*position));
                }
                let known_type =
                    crate::typing::static_type(expr, crate::partitions::table_scope(table));
                if known_type.is_some_and(|sql_type| !is_mysql_number(sql_type)) {
                    return Err(SqlError::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        "The PARTITION function returns the wrong type",
                    )
                    .with_vendor_code(1491)
                    .at(*position));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn unique_lacks_key(position: usize) -> SqlError {
    if crate::constraints::current_dialect() == Dialect::MySql {
        return SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "A UNIQUE INDEX must include all columns in the table's partitioning function",
        )
        .with_vendor_code(1503)
        .at(position);
    }
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "unique constraint on partitioned table must include all partitioning columns",
    )
    .at(position)
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
    let mut user_function_exists = |name: &str| {
        crate::routines::of_name(&*writer, tables, name)
            .ok()
            .flatten()
            .is_some()
    };
    let key = validated_key(&table, by, dialect, &mut user_function_exists)?;
    let mut record = crate::partitions::stored(writer, &table.name)?;
    record.key = Some(key.clone());
    record.row_movement_disabled = match by.row_movement {
        Some(enabled) => !enabled,
        None => dialect == Dialect::Oracle && create.partition_of.is_none(),
    };
    crate::partitions::save(writer, &table.name, &record)?;
    create_named_partitions(writer, tables, &table, &key, by, dialect)?;
    if let Some(text) = &by.interval {
        let interval =
            crate::partition_interval::validated(writer, tables, &table, &key, text, by.position)?;
        let mut record = crate::partitions::stored(writer, &table.name)?;
        record.interval = Some(interval);
        crate::partitions::save(writer, &table.name, &record)?;
    }
    Ok(result)
}

pub(crate) fn named_child(table: &TableDefinition, partition: &str, position: usize) -> Name {
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
    let width = key.parts.len();
    let mut previous: Vec<PartitionBoundValue> = vec![PartitionBoundValue::MinValue; width];
    let mut planned: Vec<(Name, PartitionBoundSpec, Option<PartitionBy>)> = Vec::new();
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
        let subpartitioned = subpartitioning(by, &partition.name.text, &partition.subpartitions)?;
        planned.push((name, bound, subpartitioned));
    }
    if planned.is_empty()
        && let Some(count) = by.hash_partitions
    {
        for remainder in 0..count {
            let short = format!("p{remainder}");
            let subpartitioned = subpartitioning(by, &short, &[])?;
            planned.push((
                named_child(table, &short, position),
                PartitionBoundSpec::Hash {
                    modulus: count,
                    remainder,
                },
                subpartitioned,
            ));
        }
    }
    if by.subpartition_by.is_none()
        && let Some(partition) = by
            .partitions
            .iter()
            .find(|partition| !partition.subpartitions.is_empty())
    {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "Subpartitions can only be defined with SUBPARTITION BY",
        )
        .with_vendor_code(1500)
        .at(partition.name.position));
    }
    for (name, bound, subpartitioned) in planned {
        let mut child = copy_of(table, name.clone())?;
        match subpartitioned {
            Some(sub) => {
                child.partition_by = Some(sub);
                create_table(writer, tables, &child, dialect)?;
            }
            None => {
                execute::create_table(writer, tables, &child, dialect)?;
            }
        }
        attach(writer, tables, table, &name, &bound)?;
    }
    Ok(())
}

fn subpartitioning(
    by: &PartitionBy,
    partition: &str,
    named: &[NamedPartition],
) -> SqlResult<Option<PartitionBy>> {
    let Some(sub) = &by.subpartition_by else {
        return Ok(None);
    };
    let partitions = if named.is_empty() {
        if sub.strategy != PartitionStrategy::Hash {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "range and list subpartitions must be named in each partition",
            )
            .with_oracle_code(14160)
            .at(sub.position));
        }
        let count = sub.count.unwrap_or(1);
        (0..count)
            .map(|number| NamedPartition {
                name: Name {
                    text: format!("{partition}sp{number}"),
                    position: sub.position,
                },
                bound: NamedPartitionBound::Hash,
                subpartitions: Vec::new(),
            })
            .collect()
    } else {
        if let Some(count) = sub.count
            && usize::try_from(count).ok() != Some(named.len())
        {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "Wrong number of subpartitions defined, mismatch with previous setting",
            )
            .with_vendor_code(1485)
            .at(named.first().map_or(sub.position, |first| first.name.position)));
        }
        named.to_vec()
    };
    Ok(Some(PartitionBy {
        strategy: sub.strategy,
        key: sub.key.clone(),
        partitions,
        hash_partitions: None,
        mysql_integer_key: sub.mysql_integer_key,
        subpartition_by: None,
        row_movement: Some(true),
        interval: None,
        position: sub.position,
    }))
}

pub(crate) fn same_columns(
    parent: &TableDefinition,
    child: &TableDefinition,
    position: usize,
) -> SqlResult<()> {
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
            format!("child table is missing column \"{}\"", missing.name),
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
    attach_with(writer, tables, parent, partition, &|key, types| {
        crate::partitions::bound_of(bound, key, types, position)
    })
}

pub(crate) fn attach_resolved(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    partition: &Name,
    bound: PartitionBound,
) -> SqlResult<()> {
    attach_with(writer, tables, parent, partition, &|_, _| Ok(bound.clone()))
}

type BoundResolver<'b> =
    dyn Fn(&PartitionKey, &[crate::types::SqlType]) -> SqlResult<PartitionBound> + 'b;

fn attach_with(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    partition: &Name,
    resolve: &BoundResolver<'_>,
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
    if !child.generated.is_empty() || !parent.generated.is_empty() {
        return Err(
            SqlError::not_supported("generated columns in a partitioned table").at(position),
        );
    }
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
    let types = crate::partitions::key_types(parent, &key)?;
    let bound = resolve(&key, &types)?;
    crate::partitions::check_no_overlap(
        &catalog,
        &parent.name,
        &child.name,
        &bound,
        &types,
        position,
    )?;
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
    crate::partition_indexes::reach_partition(writer, tables, &parent.name, &child.name, position)
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
    record.index_links.clear();
    crate::partitions::save(writer, &child_name, &record)
}
