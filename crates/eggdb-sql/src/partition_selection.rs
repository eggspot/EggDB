use crate::ast::{Expr, PartitionSelection, TableReference};
use crate::dialect::Dialect;
use crate::encoding::TableDefinition;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope};
use crate::partition_ddl::MYSQL_PARTITION_MARKER;
use crate::partitions::{PartitionCatalog, Partitioned};
use crate::types::{Conversion, convert};
use std::collections::BTreeSet;

pub(crate) fn not_partitioned(table: &str) -> SqlError {
    match crate::constraints::current_dialect() {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            "PARTITION () clause on non partitioned table",
        )
        .with_vendor_code(1747),
        Dialect::Oracle => SqlError::new(SqlState::WRONG_OBJECT_TYPE, "object is not partitioned")
            .with_oracle_code(14501),
        _ => SqlError::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!(
                "table \"{}\" is not partitioned",
                crate::schemas::display(table)
            ),
        ),
    }
}

fn unknown_partition(name: &str, table: &str) -> SqlError {
    match crate::constraints::current_dialect() {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!(
                "Unknown partition '{name}' in table '{}'",
                crate::schemas::base_name(table)
            ),
        )
        .with_vendor_code(1735),
        Dialect::Oracle => SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            "Specified partition does not exist",
        )
        .with_oracle_code(2149),
        _ => SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!(
                "partition \"{name}\" of relation \"{}\" does not exist",
                crate::schemas::display(table)
            ),
        ),
    }
}

pub(crate) fn row_outside_selection() -> SqlError {
    match crate::constraints::current_dialect() {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            "Found a row not matching the given partition set",
        )
        .with_vendor_code(1748),
        Dialect::Oracle => SqlError::new(
            SqlState::CHECK_VIOLATION,
            "inserted partition key is outside specified partition",
        )
        .with_oracle_code(14401),
        _ => SqlError::new(
            SqlState::CHECK_VIOLATION,
            "new row is outside the partitions the statement names",
        ),
    }
}

pub(crate) fn refuse_on_unpartitioned(reference: &TableReference) -> SqlResult<()> {
    match reference.partition_selection {
        Some(_) => Err(not_partitioned(&reference.name.text).at(reference.name.position)),
        None => Ok(()),
    }
}

fn partition_name(table: &str) -> &str {
    let base = crate::schemas::base_name(table);
    base.rsplit_once(MYSQL_PARTITION_MARKER)
        .map_or(base, |(_, partition)| partition)
}

fn partition_holding_key(
    catalog: &PartitionCatalog,
    table: &TableDefinition,
    values: &[Expr],
) -> SqlResult<String> {
    let Some(partitioned) = Partitioned::of(catalog, table, None)? else {
        return Err(not_partitioned(&table.name));
    };
    let types = partitioned.key_types();
    if values.len() != types.len() {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!(
                "PARTITION FOR takes {} value(s), one for each partition key column",
                types.len()
            ),
        ));
    }
    let evaluator = Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    };
    let mut key = Vec::with_capacity(values.len());
    for (expr, &sql_type) in values.iter().zip(types) {
        let value = evaluator.eval(expr, Some(sql_type))?;
        key.push(
            convert(value, sql_type, Conversion::Assignment)
                .map_err(|error| error.at_if_unplaced(expr.position))?,
        );
    }
    match partitioned.partition_holding(&key) {
        Some(child) => Ok(child.to_owned()),
        None => Err(partitioned.no_partition_for_key(&key)),
    }
}

/// The partitions a `PARTITION (...)` or `PARTITION FOR (...)` after a table name selects,
/// with every partition below them.
#[derive(Debug, Default)]
pub(crate) struct SelectedPartitions {
    tables: BTreeSet<String>,
}

impl SelectedPartitions {
    pub(crate) fn of(
        catalog: &PartitionCatalog,
        table: &TableDefinition,
        selection: &PartitionSelection,
    ) -> SqlResult<Self> {
        let named = match selection {
            PartitionSelection::Named(names) => {
                let descendants = catalog.descendants_of(&table.name);
                let mut named = Vec::with_capacity(names.len());
                for name in names {
                    let Some(found) = descendants
                        .iter()
                        .find(|partition| partition_name(partition).eq_ignore_ascii_case(&name.text))
                    else {
                        return Err(unknown_partition(&name.text, &table.name).at(name.position));
                    };
                    named.push(found.clone());
                }
                named
            }
            PartitionSelection::HoldingKey { values, position } => {
                vec![
                    partition_holding_key(catalog, table, values)
                        .map_err(|error| error.at_if_unplaced(*position))?,
                ]
            }
        };
        let mut tables = BTreeSet::new();
        for partition in named {
            if tables.contains(&partition) {
                continue;
            }
            tables.extend(catalog.descendants_of(&partition));
            tables.insert(partition);
        }
        Ok(Self { tables })
    }

    pub(crate) fn of_reference(
        catalog: &PartitionCatalog,
        table: &TableDefinition,
        reference: &TableReference,
    ) -> SqlResult<Option<Self>> {
        match &reference.partition_selection {
            Some(selection) => Self::of(catalog, table, selection)
                .map(Some)
                .map_err(|error| error.at_if_unplaced(reference.name.position)),
            None => Ok(None),
        }
    }

    pub(crate) fn contains(&self, table: &str) -> bool {
        self.tables.contains(table)
    }

    pub(crate) fn narrowed(&self, catalog: &PartitionCatalog, leaves: Vec<String>) -> Vec<String> {
        let kept: Vec<String> = leaves
            .into_iter()
            .filter(|leaf| self.contains(leaf))
            .collect();
        if !kept.is_empty() {
            return kept;
        }
        let selected_leaves: Vec<String> = self
            .tables
            .iter()
            .filter(|table| catalog.key_of(table).is_none())
            .cloned()
            .collect();
        if !selected_leaves.is_empty() {
            return selected_leaves;
        }
        self.tables.iter().cloned().collect()
    }
}
