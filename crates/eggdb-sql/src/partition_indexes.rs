use crate::ast::{CreateIndex, IndexSpec, Name};
use crate::dialect::Dialect;
use crate::encoding::{IndexDefinition, TableDefinition};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::partitions::{IndexLink, KeyPart, PRIMARY_KEY_LINK, PartitionKey};
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite};
use std::sync::Arc;

const MAX_DEPTH: usize = 32;

fn column_names<'t>(table: &'t TableDefinition, index: &IndexDefinition) -> Vec<&'t str> {
    index
        .columns
        .iter()
        .filter_map(|&column| table.columns.get(column))
        .map(|column| column.name.as_str())
        .collect()
}

fn equivalent(
    parent: &TableDefinition,
    parent_index: &IndexDefinition,
    child: &TableDefinition,
    child_index: &IndexDefinition,
) -> bool {
    parent_index.unique == child_index.unique
        && parent_index.method == child_index.method
        && parent_index.sort_options == child_index.sort_options
        && (!parent_index.unique
            || parent_index.backs_unique_constraint == child_index.backs_unique_constraint)
        && column_names(parent, parent_index) == column_names(child, child_index)
}

pub(crate) fn unique_index_links(
    parent: &TableDefinition,
    partition: &TableDefinition,
) -> Vec<IndexLink> {
    let mut links: Vec<IndexLink> = Vec::new();
    for parent_index in parent.indexes.iter().filter(|index| index.unique) {
        let found = partition.indexes.iter().find(|candidate| {
            !links.iter().any(|link| link.index == candidate.id)
                && equivalent(parent, parent_index, partition, candidate)
        });
        if let Some(found) = found {
            links.push(IndexLink {
                index: found.id,
                parent_index: parent_index.id,
            });
        }
    }
    if primary_keys_match(parent, partition) {
        links.push(IndexLink {
            index: PRIMARY_KEY_LINK,
            parent_index: PRIMARY_KEY_LINK,
        });
    }
    links
}

fn key_column_names(table: &TableDefinition) -> Vec<&str> {
    table
        .primary_key
        .iter()
        .filter_map(|&column| table.columns.get(column).map(|found| found.name.as_str()))
        .collect()
}

pub(crate) fn primary_keys_match(parent: &TableDefinition, partition: &TableDefinition) -> bool {
    !parent.primary_key.is_empty() && key_column_names(parent) == key_column_names(partition)
}

fn spec_for(parent: &TableDefinition, index: &IndexDefinition, dialect: Dialect) -> IndexSpec {
    IndexSpec {
        name: (dialect == Dialect::MySql).then(|| Name {
            text: index.name.clone(),
            position: 0,
        }),
        unique: index.unique,
        columns: column_names(parent, index)
            .into_iter()
            .map(|name| Name {
                text: name.to_owned(),
                position: 0,
            })
            .collect(),
        position: 0,
        method: index.method,
        sort_options: index.sort_options,
    }
}

fn check_unique_covers_key(
    table: &TableDefinition,
    key: &PartitionKey,
    columns: &[&str],
    position: usize,
) -> SqlResult<()> {
    if key.has_expressions()
        && let Some(read) = crate::partitions::mysql_key_columns(table, key)
    {
        return if read
            .iter()
            .all(|column| columns.iter().any(|named| named.eq_ignore_ascii_case(column)))
        {
            Ok(())
        } else {
            Err(crate::partition_ddl::unique_lacks_key(position))
        };
    }
    if key.has_expressions() {
        return Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "unsupported UNIQUE constraint with partition key definition",
        )
        .with_detail("UNIQUE constraints cannot be used when partition keys include expressions.")
        .at(position));
    }
    let covered = key.parts.iter().all(|part| match part {
        KeyPart::Column(name) => columns.contains(&name.as_str()),
        KeyPart::Expression { .. } => false,
    });
    if covered {
        Ok(())
    } else {
        Err(crate::partition_ddl::unique_lacks_key(position))
    }
}

fn table_named(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    name: &str,
    position: usize,
) -> SqlResult<Arc<TableDefinition>> {
    tables.require_target(
        reader,
        &Name {
            text: name.to_owned(),
            position,
        },
        "index",
    )
}

fn children(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    parent: &str,
) -> SqlResult<Vec<String>> {
    Ok(crate::partitions::catalog(reader, tables)?
        .children_of(parent)
        .into_iter()
        .map(|(child, _)| child.to_owned())
        .collect())
}

pub(crate) fn create_on_partitioned(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &TableDefinition,
    create: &CreateIndex,
    dialect: Dialect,
    constraint: bool,
) -> SqlResult<QueryResult> {
    let position = create.index.position;
    if create.index.unique
        && let Some(key) = crate::partitions::catalog(writer, tables)?.key_of(&table.name)
    {
        let columns: Vec<&str> = create
            .index
            .columns
            .iter()
            .map(|name| name.text.as_str())
            .collect();
        check_unique_covers_key(table, key, &columns, position)?;
    }
    let (definition, at) =
        crate::indexes::add_index(writer, table, &create.index, dialect, constraint)?;
    let Some(index) = definition.indexes.get(at).cloned() else {
        return Err(crate::encoding::corrupt("index"));
    };
    if !create.only {
        for child in children(writer, tables, &table.name)? {
            reach(
                writer,
                tables,
                &definition,
                &index,
                &child,
                position,
                dialect,
                0,
            )?;
        }
    }
    Ok(QueryResult::command("CREATE INDEX"))
}

pub(crate) fn reach_partition(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &str,
    child: &str,
    position: usize,
) -> SqlResult<()> {
    let parent = table_named(writer, tables, parent, position)?;
    let dialect = crate::constraints::current_dialect();
    for index in &parent.indexes {
        reach(writer, tables, &parent, index, child, position, dialect, 0)?;
    }
    let partition = table_named(writer, tables, child, position)?;
    if primary_keys_match(&parent, &partition) {
        link(writer, child, PRIMARY_KEY_LINK, PRIMARY_KEY_LINK)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn reach(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    parent_index: &IndexDefinition,
    child_name: &str,
    position: usize,
    dialect: Dialect,
    depth: usize,
) -> SqlResult<()> {
    if depth > MAX_DEPTH {
        return Err(crate::encoding::corrupt("partition tree"));
    }
    let catalog = crate::partitions::catalog(writer, tables)?;
    if catalog
        .child_index_of(child_name, parent_index.id)
        .is_some()
    {
        return Ok(());
    }
    let child = table_named(writer, tables, child_name, position)?;
    let linked: Vec<u32> = catalog
        .index_links_of(child_name)
        .iter()
        .map(|link| link.index)
        .collect();
    let matching = child
        .indexes
        .iter()
        .find(|candidate| {
            !linked.contains(&candidate.id) && equivalent(parent, parent_index, &child, candidate)
        })
        .cloned();
    let child_index = match matching {
        Some(adopted) => {
            if catalog.key_of(child_name).is_some() {
                for grandchild in children(writer, tables, child_name)? {
                    reach(
                        writer,
                        tables,
                        &child,
                        &adopted,
                        &grandchild,
                        position,
                        dialect,
                        depth + 1,
                    )?;
                }
            }
            adopted.id
        }
        None => {
            let spec = spec_for(parent, parent_index, dialect);
            let child_key = catalog.key_of(child_name).cloned();
            if parent_index.unique
                && let Some(key) = &child_key
            {
                let columns: Vec<&str> =
                    spec.columns.iter().map(|name| name.text.as_str()).collect();
                check_unique_covers_key(&child, key, &columns, position)?;
            }
            let (definition, at) = crate::indexes::add_index(
                writer,
                &child,
                &spec,
                dialect,
                parent_index.backs_unique_constraint,
            )?;
            let Some(created) = definition.indexes.get(at).cloned() else {
                return Err(crate::encoding::corrupt("index"));
            };
            if child_key.is_some() {
                for grandchild in children(writer, tables, child_name)? {
                    reach(
                        writer,
                        tables,
                        &definition,
                        &created,
                        &grandchild,
                        position,
                        dialect,
                        depth + 1,
                    )?;
                }
            }
            created.id
        }
    };
    link(writer, child_name, child_index, parent_index.id)
}

fn link(
    writer: &mut dyn KeyValueWrite,
    child: &str,
    index: u32,
    parent_index: u32,
) -> SqlResult<()> {
    let mut record = crate::partitions::stored(writer, child)?;
    if record.parent.is_none() {
        return Ok(());
    }
    record
        .index_links
        .retain(|link| link.index != index && link.parent_index != parent_index);
    if record.index_links.len() >= crate::partitions::MAX_INDEX_LINKS {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            "a partition can have at most 65536 indexes attached to its parent's",
        ));
    }
    record.index_links.push(IndexLink {
        index,
        parent_index,
    });
    crate::partitions::save(writer, child, &record)
}

pub(crate) fn before_drop(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    table: &TableDefinition,
    index: u32,
    position: usize,
) -> SqlResult<()> {
    let catalog = crate::partitions::catalog(writer, tables)?;
    if catalog.is_empty() {
        return Ok(());
    }
    if let Some((parent, parent_index)) = catalog.parent_index_of(&table.name, index)
        && let Some(parent_table) = tables.table(writer, parent)?
        && let Some(required_by) = parent_table
            .indexes
            .iter()
            .find(|candidate| candidate.id == parent_index)
    {
        let dropped = table
            .indexes
            .iter()
            .find(|candidate| candidate.id == index)
            .map_or("", |candidate| candidate.name.as_str());
        return Err(SqlError::new(
            SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
            format!(
                "cannot drop index {dropped} because index {} requires it",
                required_by.name
            ),
        )
        .with_hint(format!("You can drop index {} instead.", required_by.name))
        .at(position));
    }
    drop_partition_indexes(writer, tables, &table.name, index, 0)
}

fn drop_partition_indexes(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &str,
    parent_index: u32,
    depth: usize,
) -> SqlResult<()> {
    if depth > MAX_DEPTH {
        return Err(crate::encoding::corrupt("partition tree"));
    }
    let catalog = crate::partitions::catalog(writer, tables)?;
    let linked: Vec<(String, u32)> = catalog
        .children_of(parent)
        .into_iter()
        .filter_map(|(child, _)| {
            catalog
                .child_index_of(child, parent_index)
                .map(|index| (child.to_owned(), index))
        })
        .collect();
    for (child, child_index) in linked {
        let mut record = crate::partitions::stored(writer, &child)?;
        record.index_links.retain(|link| link.index != child_index);
        crate::partitions::save(writer, &child, &record)?;
        drop_partition_indexes(writer, tables, &child, child_index, depth + 1)?;
        let Some(definition) = tables.table(writer, &child)? else {
            continue;
        };
        let mut definition = (*definition).clone();
        if let Some(at) = definition
            .indexes
            .iter()
            .position(|candidate| candidate.id == child_index)
        {
            crate::indexes::remove_index(writer, &mut definition, at)?;
        }
    }
    Ok(())
}

pub(crate) fn rename_linked(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    parent: &TableDefinition,
    parent_index: u32,
    (from, to): (&str, &str),
    depth: usize,
) -> SqlResult<()> {
    if depth > MAX_DEPTH {
        return Err(crate::encoding::corrupt("partition tree"));
    }
    let catalog = crate::partitions::catalog(writer, tables)?;
    let linked: Vec<(String, u32)> = catalog
        .children_of(&parent.name)
        .into_iter()
        .filter_map(|(child, _)| {
            catalog
                .child_index_of(child, parent_index)
                .map(|index| (child.to_owned(), index))
        })
        .collect();
    for (child, child_index) in linked {
        let Some(definition) = tables.table(writer, &child)? else {
            continue;
        };
        let mut definition = (*definition).clone();
        if definition.index_named(to).is_some() {
            continue;
        }
        let Some(index) = definition
            .indexes
            .iter_mut()
            .find(|candidate| candidate.id == child_index && candidate.name == from)
        else {
            continue;
        };
        index.name = to.to_owned();
        writer.put(
            &crate::encoding::table_key(&definition.name),
            &definition.encode(),
        )?;
        crate::comments::move_part(
            writer,
            &definition.name,
            crate::encoding::CommentKind::Index,
            from,
            to,
        )?;
        rename_linked(
            writer,
            tables,
            &definition,
            child_index,
            (from, to),
            depth + 1,
        )?;
    }
    Ok(())
}

enum Found<'t> {
    Index(&'t TableDefinition, &'t IndexDefinition),
    PrimaryKey(&'t TableDefinition),
}

fn found_in<'t>(every: &'t [TableDefinition], schemas: &[String], name: &str) -> Option<Found<'t>> {
    let base = crate::schemas::base_name(name);
    for schema in schemas {
        let mut in_schema = every
            .iter()
            .filter(|table| crate::schemas::schema_of(&table.name) == schema);
        if let Some(found) = in_schema.clone().find_map(|table| {
            table
                .index_named(base)
                .map(|index| Found::Index(table, index))
        }) {
            return Some(found);
        }
        if let Some(table) = in_schema.find(|table| {
            !table.primary_key.is_empty() && table.primary_key_constraint_name() == base
        }) {
            return Some(Found::PrimaryKey(table));
        }
    }
    None
}

fn schemas_to_search(reader: &dyn KeyValueRead, name: &str) -> SqlResult<Vec<String>> {
    if crate::schemas::is_qualified(name) {
        Ok(vec![crate::schemas::schema_of(name).to_owned()])
    } else {
        crate::schemas::relation_path(reader)
    }
}

fn missing_relation(name: &Name) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_TABLE,
        format!("relation \"{}\" does not exist", name.text),
    )
    .at(name.position)
}

fn cannot_attach(partition_index: &str, index: &str, detail: String, position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_OBJECT_DEFINITION,
        format!("cannot attach index \"{partition_index}\" as a partition of index \"{index}\""),
    )
    .with_detail(detail)
    .at(position)
}

pub(crate) fn attach_index(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    index: &Name,
    partition_index: &Name,
    if_exists: bool,
) -> SqlResult<QueryResult> {
    let result = QueryResult::command("ALTER INDEX");
    let every = crate::indexes::every_table(writer)?;
    let parent_found = found_in(
        &every,
        &schemas_to_search(writer, &index.text)?,
        &index.text,
    );
    let Some(parent_found) = parent_found else {
        if if_exists {
            return Ok(result.with_notice(format!(
                "NOTICE:  relation \"{}\" does not exist, skipping",
                index.text
            )));
        }
        return Err(missing_relation(index));
    };
    let child_found = found_in(
        &every,
        &schemas_to_search(writer, &partition_index.text)?,
        &partition_index.text,
    )
    .ok_or_else(|| missing_relation(partition_index))?;
    let catalog = crate::partitions::catalog(writer, tables)?;
    let index_name = crate::schemas::base_name(&index.text);
    let partition_index_name = crate::schemas::base_name(&partition_index.text);
    let position = partition_index.position;
    let (parent_table, child_table) = match (&parent_found, &child_found) {
        (Found::Index(parent, _), Found::Index(child, _))
        | (Found::PrimaryKey(parent), Found::PrimaryKey(child)) => (*parent, *child),
        (Found::Index(..), Found::PrimaryKey(_)) | (Found::PrimaryKey(_), Found::Index(..)) => {
            return Err(cannot_attach(
                partition_index_name,
                index_name,
                "The index definitions do not match.".to_owned(),
                position,
            ));
        }
    };
    if catalog.key_of(&parent_table.name).is_none() {
        return Err(SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("\"{index_name}\" is not a partitioned index"),
        )
        .at(index.position));
    }
    if catalog
        .parent_of(&child_table.name)
        .is_none_or(|(owner, _)| owner != parent_table.name)
    {
        return Err(cannot_attach(
            partition_index_name,
            index_name,
            format!(
                "Index \"{partition_index_name}\" is not an index on any partition of table \"{}\".",
                crate::schemas::display(&parent_table.name)
            ),
            position,
        ));
    }
    let (Found::Index(_, parent_index), Found::Index(_, child_index)) = (parent_found, child_found)
    else {
        if !primary_keys_match(parent_table, child_table) {
            return Err(cannot_attach(
                partition_index_name,
                index_name,
                "The index definitions do not match.".to_owned(),
                position,
            ));
        }
        link(writer, &child_table.name, PRIMARY_KEY_LINK, PRIMARY_KEY_LINK)?;
        return Ok(result);
    };
    if catalog.parent_index_of(&child_table.name, child_index.id)
        == Some((parent_table.name.as_str(), parent_index.id))
    {
        return Ok(result);
    }
    if catalog
        .parent_index_of(&child_table.name, child_index.id)
        .is_some()
    {
        return Err(cannot_attach(
            partition_index_name,
            index_name,
            format!("Index \"{partition_index_name}\" is already attached to another index."),
            position,
        ));
    }
    if catalog
        .child_index_of(&child_table.name, parent_index.id)
        .is_some()
    {
        return Err(cannot_attach(
            partition_index_name,
            index_name,
            format!(
                "Another index is already attached for partition \"{}\".",
                crate::schemas::display(&child_table.name)
            ),
            position,
        ));
    }
    if !equivalent(parent_table, parent_index, child_table, child_index) {
        return Err(cannot_attach(
            partition_index_name,
            index_name,
            "The index definitions do not match.".to_owned(),
            position,
        ));
    }
    let child_name = child_table.name.clone();
    let (child_id, parent_id) = (child_index.id, parent_index.id);
    link(writer, &child_name, child_id, parent_id)?;
    Ok(result)
}
