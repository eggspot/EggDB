use crate::ast::{Name, NamedPartition, NamedPartitionBound, SplitPoint};
use crate::dialect::Dialect;
use crate::encoding::TableDefinition;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::partition_ddl::{attach_resolved, copy_of, named_child, same_columns};
use crate::partitions::{
    self, BoundText, PartitionBound, PartitionKey, PartitionStrategy, checked_range,
    highest_range_end, list_bound, list_values, range_bound_from, range_end, range_start,
};
use crate::store::KeyValueWrite;
use crate::types::{SqlType, Value};

struct Partitioned {
    key: PartitionKey,
    types: Vec<SqlType>,
    children: Vec<(String, PartitionBound)>,
}

fn partitioned(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    position: usize,
) -> SqlResult<Partitioned> {
    let catalog = partitions::catalog(writer, tables)?;
    let key = catalog.key_of(&parent.name).cloned().ok_or_else(|| {
        SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!(
                "Partition management on a not partitioned table \"{}\" is not possible",
                crate::schemas::display(&parent.name)
            ),
        )
        .with_vendor_code(1505)
        .at(position)
    })?;
    let types = partitions::key_types(parent, &key)?;
    let children = catalog
        .children_of(&parent.name)
        .into_iter()
        .map(|(name, bound)| (name.to_owned(), bound.clone()))
        .collect();
    Ok(Partitioned {
        key,
        types,
        children,
    })
}

fn unknown_partition(parent: &TableDefinition, partition: &Name) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!(
            "Unknown partition '{}' in table '{}'",
            partition.text,
            crate::schemas::display(&parent.name)
        ),
    )
    .with_vendor_code(1735)
    .with_oracle_code(2149)
    .at(partition.position)
}

fn existing<'p>(
    state: &'p Partitioned,
    parent: &TableDefinition,
    partition: &Name,
) -> SqlResult<&'p (String, PartitionBound)> {
    let wanted = named_child(parent, &partition.text, partition.position).text;
    state
        .children
        .iter()
        .find(|(name, _)| *name == wanted || name.eq_ignore_ascii_case(&wanted))
        .ok_or_else(|| unknown_partition(parent, partition))
}

fn table_name(text: &str, position: usize) -> Name {
    Name {
        text: text.to_owned(),
        position,
    }
}

pub(crate) fn create_child(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    name: &Name,
    bound: PartitionBound,
    dialect: Dialect,
) -> SqlResult<()> {
    let child = copy_of(parent, name.clone())?;
    execute::create_table(writer, tables, &child, dialect)?;
    attach_resolved(writer, tables, parent, name, bound)
}

fn not_for_hash(position: usize) -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "DROP PARTITION can only be used on RANGE/LIST partitions",
    )
    .with_vendor_code(1512)
    .at(position)
}

pub(crate) fn add(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    added: &[NamedPartition],
    hash_count: Option<u32>,
    position: usize,
    dialect: Dialect,
) -> SqlResult<()> {
    let state = partitioned(writer, tables, parent, position)?;
    if partitions::stored(writer, &parent.name)?.interval.is_some() {
        return Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "ADD PARTITION is not permitted on Interval partitioned objects",
        )
        .with_oracle_code(14760)
        .at(position));
    }
    match state.key.strategy {
        PartitionStrategy::Range => {
            let catalog = partitions::catalog(writer, tables)?;
            let mut from = highest_range_end(&catalog, &parent.name, &state.types)?
                .unwrap_or_else(|| vec![BoundText::MinValue; state.types.len()]);
            for partition in added {
                let NamedPartitionBound::LessThan(values) = &partition.bound else {
                    return Err(wrong_bound("VALUES LESS THAN", partition));
                };
                let bound = range_bound_from(from, values, &state.types, partition.name.position)?;
                from = range_end(&bound).unwrap_or_default().to_vec();
                let name = named_child(parent, &partition.name.text, partition.name.position);
                create_child(writer, tables, parent, &name, bound, dialect)?;
            }
            Ok(())
        }
        PartitionStrategy::List => {
            for partition in added {
                let NamedPartitionBound::In(values) = &partition.bound else {
                    return Err(wrong_bound("VALUES IN", partition));
                };
                let bound =
                    list_bound(values, &state.key, &state.types, partition.name.position)?;
                let name = named_child(parent, &partition.name.text, partition.name.position);
                create_child(writer, tables, parent, &name, bound, dialect)?;
            }
            Ok(())
        }
        PartitionStrategy::Hash => {
            let mut names: Vec<String> = hash_partition_names(&state, parent);
            let extra = hash_count.map_or(added.len(), |count| count as usize);
            for index in 0..extra {
                let name = added
                    .get(index)
                    .map(|partition| partition.name.text.clone())
                    .unwrap_or_else(|| format!("p{}", names.len()));
                names.push(name);
            }
            rehash(writer, tables, parent, &state, &names, position, dialect)
        }
    }
}

fn wrong_bound(expected: &str, partition: &NamedPartition) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TABLE_DEFINITION,
        format!("Only {expected} can be used for this table's partitions"),
    )
    .with_vendor_code(1480)
    .at(partition.name.position)
}

fn hash_partition_names(state: &Partitioned, parent: &TableDefinition) -> Vec<String> {
    let prefix = format!("{}{}", parent.name, crate::partition_ddl::MYSQL_PARTITION_MARKER);
    let mut ordered: Vec<(u32, String)> = state
        .children
        .iter()
        .map(|(name, bound)| {
            let remainder = match bound {
                PartitionBound::Hash { remainder, .. } => *remainder,
                _ => u32::MAX,
            };
            let short = name.strip_prefix(&prefix).unwrap_or(name).to_owned();
            (remainder, short)
        })
        .collect();
    ordered.sort();
    ordered.into_iter().map(|(_, name)| name).collect()
}

fn take_rows(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    children: &[String],
    position: usize,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut rows = Vec::new();
    for child in children {
        let table = tables.require_target(writer, &table_name(child, position), "reorganize")?;
        rows.extend(execute::every_row(writer, &table)?.into_iter().map(|(_, row)| row));
    }
    Ok(rows)
}

fn drop_children(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    children: &[String],
    position: usize,
    dialect: Dialect,
) -> SqlResult<()> {
    let names: Vec<Name> = children
        .iter()
        .map(|child| table_name(child, position))
        .collect();
    execute::drop_tables(writer, tables, &names, false, false, dialect)?;
    Ok(())
}

fn rebuild(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    old: &[String],
    new: Vec<(Name, PartitionBound)>,
    position: usize,
    dialect: Dialect,
) -> SqlResult<()> {
    let rows = take_rows(writer, tables, old, position)?;
    drop_children(writer, tables, old, position, dialect)?;
    for (name, bound) in new {
        create_child(writer, tables, parent, &name, bound, dialect)?;
    }
    let parent = tables.require_target(writer, &table_name(&parent.name, position), "reorganize")?;
    crate::partition_dml::reinsert_rows(writer, tables, &parent, rows)
}

fn rehash(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    state: &Partitioned,
    names: &[String],
    position: usize,
    dialect: Dialect,
) -> SqlResult<()> {
    let modulus = u32::try_from(names.len()).unwrap_or(u32::MAX);
    let old: Vec<String> = state.children.iter().map(|(name, _)| name.clone()).collect();
    let new = names
        .iter()
        .zip(0..)
        .map(|(name, remainder)| {
            (
                named_child(parent, name, position),
                PartitionBound::Hash { modulus, remainder },
            )
        })
        .collect();
    rebuild(writer, tables, parent, &old, new, position, dialect)
}

pub(crate) fn drop(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    dropped: &[Name],
    dialect: Dialect,
) -> SqlResult<()> {
    let position = dropped.first().map_or(0, |name| name.position);
    let state = partitioned(writer, tables, parent, position)?;
    if state.key.strategy == PartitionStrategy::Hash {
        return Err(not_for_hash(position));
    }
    let mut children = Vec::with_capacity(dropped.len());
    for partition in dropped {
        children.push(existing(&state, parent, partition)?.0.clone());
    }
    if children.len() == state.children.len() {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "Cannot remove all partitions, use DROP TABLE instead",
        )
        .with_vendor_code(1508)
        .with_oracle_code(14083)
        .at(position));
    }
    drop_children(writer, tables, &children, position, dialect)
}

pub(crate) fn truncate(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    truncated: &[Name],
) -> SqlResult<()> {
    let position = truncated.first().map_or(0, |name| name.position);
    let state = partitioned(writer, tables, parent, position)?;
    let every = truncated.len() == 1 && truncated[0].text.eq_ignore_ascii_case("all");
    let children: Vec<String> = if every {
        state.children.iter().map(|(name, _)| name.clone()).collect()
    } else {
        truncated
            .iter()
            .map(|partition| existing(&state, parent, partition).map(|(name, _)| name.clone()))
            .collect::<SqlResult<_>>()?
    };
    for child in children {
        let table = tables.require_target(writer, &table_name(&child, position), "truncate")?;
        let rows = execute::every_row(writer, &table)?;
        execute::delete_rows(writer, tables, &table, rows, &mut |_| Ok(()))?;
    }
    Ok(())
}

/// Checks that the table is partitioned and that each named partition (or `ALL`) exists, for
/// MySQL's `ANALYZE`, `CHECK`, `OPTIMIZE`, `REPAIR` and `REBUILD PARTITION`: EggDB keeps no
/// statistics, fragmentation or damage per partition for them to act on, so naming the
/// partitions correctly is all there is to check.
pub(crate) fn check_named(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    named: &[Name],
) -> SqlResult<()> {
    let position = named.first().map_or(0, |name| name.position);
    let state = partitioned(writer, tables, parent, position)?;
    if named.len() == 1 && named[0].text.eq_ignore_ascii_case("all") {
        return Ok(());
    }
    for partition in named {
        existing(&state, parent, partition)?;
    }
    Ok(())
}

pub(crate) fn reorganize(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    from: &[Name],
    into: &[NamedPartition],
    position: usize,
    dialect: Dialect,
) -> SqlResult<()> {
    let state = partitioned(writer, tables, parent, position)?;
    if from.is_empty() {
        let names: Vec<String> = into
            .iter()
            .map(|partition| partition.name.text.clone())
            .collect();
        if state.key.strategy != PartitionStrategy::Hash || names.is_empty() {
            return Err(SqlError::not_supported("REORGANIZE PARTITION without partitions to reorganize").at(position));
        }
        return rehash(writer, tables, parent, &state, &names, position, dialect);
    }
    let mut old: Vec<&(String, PartitionBound)> = Vec::with_capacity(from.len());
    for partition in from {
        old.push(existing(&state, parent, partition)?);
    }
    let old_names: Vec<String> = old.iter().map(|(name, _)| name.clone()).collect();
    let new = match state.key.strategy {
        PartitionStrategy::Range => {
            let start = lowest_start(&old, &state.types)?;
            let mut from_edge = start;
            let mut new = Vec::with_capacity(into.len());
            for partition in into {
                let NamedPartitionBound::LessThan(values) = &partition.bound else {
                    return Err(wrong_bound("VALUES LESS THAN", partition));
                };
                let bound =
                    range_bound_from(from_edge, values, &state.types, partition.name.position)?;
                from_edge = range_end(&bound).unwrap_or_default().to_vec();
                new.push((
                    named_child(parent, &partition.name.text, partition.name.position),
                    bound,
                ));
            }
            new
        }
        PartitionStrategy::List => into
            .iter()
            .map(|partition| {
                let NamedPartitionBound::In(values) = &partition.bound else {
                    return Err(wrong_bound("VALUES IN", partition));
                };
                Ok((
                    named_child(parent, &partition.name.text, partition.name.position),
                    list_bound(values, &state.key, &state.types, partition.name.position)?,
                ))
            })
            .collect::<SqlResult<Vec<_>>>()?,
        PartitionStrategy::Hash => {
            return Err(SqlError::not_supported(
                "REORGANIZE PARTITION of named hash partitions",
            )
            .at(position));
        }
    };
    rebuild(writer, tables, parent, &old_names, new, position, dialect)
}

fn lowest_start(
    old: &[&(String, PartitionBound)],
    types: &[SqlType],
) -> SqlResult<Vec<BoundText>> {
    let mut starts: Vec<Vec<BoundText>> = old
        .iter()
        .filter_map(|(_, bound)| range_start(bound).map(<[BoundText]>::to_vec))
        .collect();
    starts.sort_by(|left, right| partitions::compare_bound_texts(left, right, types));
    starts
        .into_iter()
        .next()
        .ok_or_else(|| crate::encoding::corrupt("partition bound"))
}

pub(crate) fn split(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    partition: &Name,
    point: &SplitPoint,
    into: &[Name; 2],
    dialect: Dialect,
) -> SqlResult<()> {
    let position = partition.position;
    let state = partitioned(writer, tables, parent, position)?;
    let (old_name, old_bound) = existing(&state, parent, partition)?.clone();
    let (first, second) = match (point, &old_bound) {
        (SplitPoint::At(values), PartitionBound::Range { from, to }) => {
            let first = range_bound_from(from.clone(), values, &state.types, position)?;
            let middle = range_end(&first).unwrap_or_default().to_vec();
            let second = checked_range(middle, to.clone(), &state.types, position)?;
            (first, second)
        }
        (SplitPoint::Values(values), PartitionBound::List(kept)) => {
            let first = list_bound(values, &state.key, &state.types, position)?;
            let moved = list_values(&first).unwrap_or_default();
            if moved.iter().any(|value| !kept.contains(value)) {
                return Err(SqlError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "the split values must be values of the partition being split",
                )
                .with_oracle_code(14313)
                .at(position));
            }
            let rest: Vec<BoundText> = kept
                .iter()
                .filter(|value| !moved.contains(value))
                .cloned()
                .collect();
            if rest.is_empty() {
                return Err(SqlError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "splitting would leave the second partition with no values",
                )
                .with_oracle_code(14314)
                .at(position));
            }
            (first, PartitionBound::List(rest))
        }
        (SplitPoint::Values(_), PartitionBound::Default) => {
            let PartitionBound::List(values) = list_bound(
                match point {
                    SplitPoint::Values(values) => values,
                    SplitPoint::At(values) => values,
                },
                &state.key,
                &state.types,
                position,
            )?
            else {
                return Err(SqlError::not_supported("splitting a default partition by no values").at(position));
            };
            (PartitionBound::List(values), PartitionBound::Default)
        }
        _ => {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "SPLIT PARTITION AT is for range partitions and VALUES for list partitions",
            )
            .with_oracle_code(14306)
            .at(position));
        }
    };
    let new = vec![
        (named_child(parent, &into[0].text, into[0].position), first),
        (named_child(parent, &into[1].text, into[1].position), second),
    ];
    rebuild(writer, tables, parent, &[old_name], new, position, dialect)
}

pub(crate) fn exchange(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    partition: &Name,
    table: &Name,
    validate: bool,
    dialect: Dialect,
) -> SqlResult<()> {
    let position = partition.position;
    let state = partitioned(writer, tables, parent, position)?;
    let (child, bound) = existing(&state, parent, partition)?.clone();
    let other_name = crate::schemas::resolve_relation(writer, &table.text)?;
    let other = tables.require_target(writer, &table_name(&other_name, table.position), "exchange")?;
    let catalog = partitions::catalog(writer, tables)?;
    if catalog.key_of(&other.name).is_some() || catalog.parent_of(&other.name).is_some() {
        return Err(SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            "Table to exchange with partition is partitioned or is a partition",
        )
        .with_vendor_code(1729)
        .at(table.position));
    }
    same_columns(parent, &other, table.position)?;
    crate::partition_ddl::detach(writer, tables, parent, &table_name(&child, position))?;
    let parked = format!("{child}#exchange");
    crate::alter::move_table(writer, tables, &child, &table_name(&parked, position), dialect)?;
    crate::alter::move_table(writer, tables, &other.name, &table_name(&child, position), dialect)?;
    crate::alter::move_table(writer, tables, &parked, &table_name(&other.name, position), dialect)?;
    let attached = attach_resolved(writer, tables, parent, &table_name(&child, position), bound);
    match attached {
        Err(error) if !validate && error.state() == SqlState::CHECK_VIOLATION => Err(error
            .with_detail("EggDB always checks the exchanged rows; WITHOUT VALIDATION does not skip it")),
        other => other,
    }
}

pub(crate) fn coalesce(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    count: u32,
    position: usize,
    dialect: Dialect,
) -> SqlResult<()> {
    let state = partitioned(writer, tables, parent, position)?;
    if state.key.strategy != PartitionStrategy::Hash {
        return Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "COALESCE PARTITION can only be used on HASH/KEY partitions",
        )
        .with_vendor_code(1509)
        .at(position));
    }
    let mut names = hash_partition_names(&state, parent);
    let removed = usize::try_from(count).unwrap_or(usize::MAX);
    if removed >= names.len() {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "Cannot remove all partitions, use DROP TABLE instead",
        )
        .with_vendor_code(1508)
        .at(position));
    }
    names.truncate(names.len() - removed);
    rehash(writer, tables, parent, &state, &names, position, dialect)
}

pub(crate) fn merge(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    from: &[Name],
    into: &Name,
    dialect: Dialect,
) -> SqlResult<()> {
    let position = from.first().map_or(into.position, |name| name.position);
    let state = partitioned(writer, tables, parent, position)?;
    if from.len() < 2 {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "MERGE PARTITIONS needs at least two partitions",
        )
        .with_oracle_code(14004)
        .at(position));
    }
    let mut old: Vec<(String, PartitionBound)> = Vec::with_capacity(from.len());
    for partition in from {
        let found = existing(&state, parent, partition)?;
        if old.iter().any(|(name, _)| *name == found.0) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_OBJECT,
                format!("partition \"{}\" is named twice", partition.text),
            )
            .with_oracle_code(14004)
            .at(partition.position));
        }
        old.push(found.clone());
    }
    let merged = match state.key.strategy {
        PartitionStrategy::Range => merged_range(&mut old, &state.types, position)?,
        PartitionStrategy::List => merged_list(&old),
        PartitionStrategy::Hash => {
            return Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "MERGE PARTITIONS is for range and list partitions",
            )
            .with_oracle_code(14255)
            .at(position));
        }
    };
    let old_names: Vec<String> = old.into_iter().map(|(name, _)| name).collect();
    let new = vec![(named_child(parent, &into.text, into.position), merged)];
    rebuild(writer, tables, parent, &old_names, new, position, dialect)
}

pub(crate) fn range_span(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    first: &Name,
    last: &Name,
) -> SqlResult<Vec<Name>> {
    let state = partitioned(writer, tables, parent, first.position)?;
    if state.key.strategy != PartitionStrategy::Range {
        return Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "MERGE PARTITIONS ... TO is for range partitions",
        )
        .with_oracle_code(14255)
        .at(first.position));
    }
    let first_child = existing(&state, parent, first)?.0.clone();
    let last_child = existing(&state, parent, last)?.0.clone();
    let mut ordered: Vec<&(String, PartitionBound)> = state.children.iter().collect();
    ordered.sort_by(|(_, left), (_, right)| {
        partitions::compare_bound_texts(
            range_start(left).unwrap_or_default(),
            range_start(right).unwrap_or_default(),
            &state.types,
        )
    });
    let index_of = |child: &str| ordered.iter().position(|(name, _)| name == child);
    let (Some(start), Some(end)) = (index_of(&first_child), index_of(&last_child)) else {
        return Err(unknown_partition(parent, first));
    };
    if start >= end {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            format!(
                "partition \"{}\" must come before partition \"{}\"",
                first.text, last.text
            ),
        )
        .with_oracle_code(14274)
        .at(last.position));
    }
    let prefix = format!("{}{}", parent.name, crate::partition_ddl::MYSQL_PARTITION_MARKER);
    Ok(ordered[start..=end]
        .iter()
        .map(|(name, _)| Name {
            text: name.strip_prefix(&prefix).unwrap_or(name).to_owned(),
            position: first.position,
        })
        .collect())
}

fn merged_range(
    old: &mut [(String, PartitionBound)],
    types: &[SqlType],
    position: usize,
) -> SqlResult<PartitionBound> {
    old.sort_by(|(_, left), (_, right)| {
        partitions::compare_bound_texts(
            range_start(left).unwrap_or_default(),
            range_start(right).unwrap_or_default(),
            types,
        )
    });
    let not_adjacent = || {
        SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "partitions being merged are not adjacent",
        )
        .with_oracle_code(14274)
        .at(position)
    };
    for pair in old.windows(2) {
        let (Some(end), Some(start)) = (range_end(&pair[0].1), range_start(&pair[1].1)) else {
            return Err(not_adjacent());
        };
        if partitions::compare_bound_texts(end, start, types) != std::cmp::Ordering::Equal {
            return Err(not_adjacent());
        }
    }
    let from = old
        .first()
        .and_then(|(_, bound)| range_start(bound))
        .ok_or_else(|| crate::encoding::corrupt("partition bound"))?
        .to_vec();
    let to = old
        .last()
        .and_then(|(_, bound)| range_end(bound))
        .ok_or_else(|| crate::encoding::corrupt("partition bound"))?
        .to_vec();
    checked_range(from, to, types, position)
}

fn merged_list(old: &[(String, PartitionBound)]) -> PartitionBound {
    if old
        .iter()
        .any(|(_, bound)| matches!(bound, PartitionBound::Default))
    {
        return PartitionBound::Default;
    }
    let mut values: Vec<BoundText> = Vec::new();
    for value in old
        .iter()
        .filter_map(|(_, bound)| list_values(bound))
        .flatten()
    {
        if !values.contains(value) {
            values.push(value.clone());
        }
    }
    PartitionBound::List(values)
}

pub(crate) fn modify_values(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    partition: &Name,
    values: &[crate::ast::PartitionBoundValue],
    adding: bool,
    dialect: Dialect,
) -> SqlResult<()> {
    let position = partition.position;
    let state = partitioned(writer, tables, parent, position)?;
    let (child, bound) = existing(&state, parent, partition)?.clone();
    let PartitionBound::List(kept) = &bound else {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "ADD VALUES and DROP VALUES are for list partitions other than DEFAULT",
        )
        .with_oracle_code(14457)
        .at(position));
    };
    let PartitionBound::List(changed) = list_bound(values, &state.key, &state.types, position)?
    else {
        return Err(SqlError::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "ADD VALUES and DROP VALUES need at least one value",
        )
        .at(position));
    };
    let new_values = if adding {
        if let Some(present) = changed.iter().find(|value| kept.contains(value)) {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("value {} already exists in partition", bound_text_shown(present)),
            )
            .with_oracle_code(14312)
            .at(position));
        }
        let mut values = kept.clone();
        values.extend(changed.iter().cloned());
        values
    } else {
        if let Some(missing) = changed.iter().find(|value| !kept.contains(value)) {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("value {} does not exist in partition", bound_text_shown(missing)),
            )
            .with_oracle_code(14313)
            .at(position));
        }
        let rest: Vec<BoundText> = kept
            .iter()
            .filter(|value| !changed.contains(value))
            .cloned()
            .collect();
        if rest.is_empty() {
            return Err(SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "resulting list partition must contain at least one value",
            )
            .with_oracle_code(14314)
            .at(position));
        }
        rest
    };
    let default = state
        .children
        .iter()
        .find(|(_, bound)| matches!(bound, PartitionBound::Default))
        .map(|(name, _)| name.clone());
    let watched = if adding { default.clone() } else { Some(child.clone()) };
    let rows_before = match &watched {
        Some(name) => row_count(writer, tables, name, position)?,
        None => 0,
    };
    let mut old = vec![child.clone()];
    let mut new = vec![(table_name(&child, position), PartitionBound::List(new_values))];
    if let Some(default) = default {
        old.push(default.clone());
        new.push((table_name(&default, position), PartitionBound::Default));
    }
    let rebuilt = rebuild(writer, tables, parent, &old, new, position, dialect);
    let moved_out = || {
        if adding {
            SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "values being added already exist in DEFAULT partition",
            )
            .with_oracle_code(14324)
            .at(position)
        } else {
            SqlError::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "partition contains rows corresponding to values being dropped",
            )
            .with_oracle_code(14518)
            .at(position)
        }
    };
    match rebuilt {
        Err(error) if !adding && error.vendor_code() == Some(14400) => return Err(moved_out()),
        other => other?,
    }
    if let Some(name) = &watched
        && row_count(writer, tables, name, position)? < rows_before
    {
        return Err(moved_out());
    }
    Ok(())
}

fn row_count(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    child: &str,
    position: usize,
) -> SqlResult<usize> {
    let table = tables.require_target(writer, &table_name(child, position), "modify")?;
    Ok(execute::every_row(writer, &table)?.len())
}

fn bound_text_shown(value: &BoundText) -> String {
    match value {
        BoundText::Value(text) => text.clone(),
        BoundText::Null => "NULL".to_owned(),
        BoundText::MinValue => "MINVALUE".to_owned(),
        BoundText::MaxValue => "MAXVALUE".to_owned(),
    }
}
