use crate::ast::{BinaryOperator, Expr, ExprKind, PartitionBoundSpec, PartitionBoundValue};
pub(crate) use crate::ast::PartitionStrategy;
use crate::dialect::Dialect;
use crate::encoding::{
    TableDefinition, decode_partitioning, encode_key_value, encode_partitioning, partition_key,
    partitions_prefix, prefix_end,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope};
use crate::execute::TableCache;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::{Conversion, SqlType, Value, compare_values, convert};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const MAX_KEY_COLUMNS: usize = 32;
pub(crate) const MAX_BOUND_VALUES: usize = 65_536;
const MAX_DEPTH: usize = 32;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;
const HASH_COMBINE: u64 = 0x49a0_f4dd_15e5_a8e3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoundText {
    Value(String),
    Null,
    MinValue,
    MaxValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartitionKey {
    pub(crate) strategy: PartitionStrategy,
    pub(crate) columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PartitionBound {
    Range {
        from: Vec<BoundText>,
        to: Vec<BoundText>,
    },
    List(Vec<BoundText>),
    Hash {
        modulus: u32,
        remainder: u32,
    },
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PartitionRecord {
    pub(crate) key: Option<PartitionKey>,
    pub(crate) parent: Option<(String, PartitionBound)>,
}

impl PartitionRecord {
    fn is_empty(&self) -> bool {
        self.key.is_none() && self.parent.is_none()
    }
}

#[derive(Debug, Default)]
pub(crate) struct PartitionCatalog {
    records: BTreeMap<String, PartitionRecord>,
}

impl PartitionCatalog {
    pub(crate) fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub(crate) fn record(&self, table: &str) -> Option<&PartitionRecord> {
        self.records.get(table)
    }

    pub(crate) fn key_of(&self, table: &str) -> Option<&PartitionKey> {
        self.records.get(table)?.key.as_ref()
    }

    pub(crate) fn parent_of(&self, table: &str) -> Option<(&str, &PartitionBound)> {
        let (parent, bound) = self.records.get(table)?.parent.as_ref()?;
        Some((parent.as_str(), bound))
    }

    pub(crate) fn children_of(&self, parent: &str) -> Vec<(&str, &PartitionBound)> {
        self.records
            .iter()
            .filter_map(|(name, record)| match &record.parent {
                Some((owner, bound)) if owner == parent => Some((name.as_str(), bound)),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn descendants_of(&self, parent: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut pending = vec![parent.to_owned()];
        while let Some(next) = pending.pop() {
            if found.len() > self.records.len() {
                break;
            }
            for (child, _) in self.children_of(&next) {
                found.push(child.to_owned());
                pending.push(child.to_owned());
            }
        }
        found
    }

    pub(crate) fn every(&self) -> impl Iterator<Item = (&str, &PartitionRecord)> {
        self.records.iter().map(|(name, record)| (name.as_str(), record))
    }
}

#[derive(Debug, Default)]
pub(crate) struct PartitionCache {
    version: Option<u64>,
    catalog: Option<Arc<PartitionCatalog>>,
}

pub(crate) fn catalog(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
) -> SqlResult<Arc<PartitionCatalog>> {
    let version = reader.committed_version();
    let cache = &mut tables.partitions;
    if let Some(catalog) = &cache.catalog
        && version.is_some()
        && cache.version == version
    {
        return Ok(Arc::clone(catalog));
    }
    let loaded = Arc::new(load(reader)?);
    cache.version = version;
    cache.catalog = Some(Arc::clone(&loaded));
    Ok(loaded)
}

pub(crate) fn load(reader: &dyn KeyValueRead) -> SqlResult<PartitionCatalog> {
    let prefix = partitions_prefix();
    let mut records = BTreeMap::new();
    let mut failure = None;
    let mut memory = crate::memory::WorkMemory::new();
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            memory.charge(key.len() + value.len() + 64)?;
            let decoded = std::str::from_utf8(&key[prefix.len()..])
                .map_err(|_| crate::encoding::corrupt("partitioning"))
                .and_then(|table| Ok((table.to_owned(), decode_partitioning(table, value)?)));
            match decoded {
                Ok((table, record)) => {
                    records.insert(table, record);
                    Ok(true)
                }
                Err(error) => {
                    failure = Some(error);
                    Ok(false)
                }
            }
        },
    )?;
    match failure {
        Some(error) => Err(error),
        None => Ok(PartitionCatalog { records }),
    }
}

pub(crate) fn stored(reader: &dyn KeyValueRead, table: &str) -> SqlResult<PartitionRecord> {
    match reader.get(&partition_key(table))? {
        Some(bytes) => decode_partitioning(table, &bytes),
        None => Ok(PartitionRecord::default()),
    }
}

pub(crate) fn save(
    writer: &mut dyn KeyValueWrite,
    table: &str,
    record: &PartitionRecord,
) -> SqlResult<()> {
    let key = partition_key(table);
    if record.is_empty() {
        writer.delete(&key)?;
    } else {
        writer.put(&key, &encode_partitioning(record))?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
enum Edge {
    Value(Value),
    Min,
    Max,
}

#[derive(Debug, Clone)]
enum TypedBound {
    Range { from: Vec<Edge>, to: Vec<Edge> },
    List { values: Vec<Value>, null: bool },
    Hash { modulus: u32, remainder: u32 },
    Default,
}

fn typed_edge(text: &BoundText, sql_type: SqlType) -> SqlResult<Edge> {
    Ok(match text {
        BoundText::Value(text) => Edge::Value(convert(
            Value::Text(text.clone()),
            sql_type,
            Conversion::Explicit,
        )?),
        BoundText::Null => Edge::Value(Value::Null),
        BoundText::MinValue => Edge::Min,
        BoundText::MaxValue => Edge::Max,
    })
}

fn typed(bound: &PartitionBound, types: &[SqlType]) -> SqlResult<TypedBound> {
    let corrupt = || crate::encoding::corrupt("partition bound");
    Ok(match bound {
        PartitionBound::Range { from, to } => {
            if from.len() != types.len() || to.len() != types.len() {
                return Err(corrupt());
            }
            let edges = |texts: &[BoundText]| -> SqlResult<Vec<Edge>> {
                texts
                    .iter()
                    .zip(types)
                    .map(|(text, sql_type)| typed_edge(text, *sql_type))
                    .collect()
            };
            TypedBound::Range {
                from: edges(from)?,
                to: edges(to)?,
            }
        }
        PartitionBound::List(texts) => {
            let sql_type = *types.first().ok_or_else(corrupt)?;
            let mut values = Vec::with_capacity(texts.len());
            let mut null = false;
            for text in texts {
                match typed_edge(text, sql_type)? {
                    Edge::Value(Value::Null) => null = true,
                    Edge::Value(value) => values.push(value),
                    Edge::Min | Edge::Max => return Err(corrupt()),
                }
            }
            TypedBound::List { values, null }
        }
        PartitionBound::Hash { modulus, remainder } => TypedBound::Hash {
            modulus: *modulus,
            remainder: *remainder,
        },
        PartitionBound::Default => TypedBound::Default,
    })
}

fn compare_to_edges(key: &[Value], edges: &[Edge]) -> Ordering {
    for (value, edge) in key.iter().zip(edges) {
        match edge {
            Edge::Min => return Ordering::Greater,
            Edge::Max => return Ordering::Less,
            Edge::Value(bound) => match compare_values(value, bound) {
                Ordering::Equal => {}
                other => return other,
            },
        }
    }
    Ordering::Equal
}

fn hash_value(value: &Value) -> u64 {
    let mut bytes = Vec::new();
    encode_key_value(&mut bytes, value);
    bytes.iter().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

fn hash_key(key: &[Value]) -> u64 {
    key.iter()
        .filter(|value| !matches!(value, Value::Null))
        .fold(0u64, |combined, value| {
            combined
                ^ hash_value(value)
                    .wrapping_add(HASH_COMBINE)
                    .wrapping_add(combined << 54)
                    .wrapping_add(combined >> 7)
        })
}

impl TypedBound {
    fn holds(&self, key: &[Value]) -> bool {
        match self {
            Self::Range { from, to } => {
                !key.iter().any(|value| matches!(value, Value::Null))
                    && compare_to_edges(key, from) != Ordering::Less
                    && compare_to_edges(key, to) == Ordering::Less
            }
            Self::List { values, null } => match key.first() {
                None | Some(Value::Null) => *null,
                Some(value) => values
                    .iter()
                    .any(|listed| compare_values(listed, value) == Ordering::Equal),
            },
            Self::Hash { modulus, remainder } => {
                hash_key(key) % u64::from(*modulus) == u64::from(*remainder)
            }
            Self::Default => false,
        }
    }
}

fn key_values(row: &[Value], positions: &[usize]) -> Vec<Value> {
    positions
        .iter()
        .map(|&position| row.get(position).cloned().unwrap_or(Value::Null))
        .collect()
}

fn key_positions(table: &TableDefinition, columns: &[String]) -> SqlResult<Vec<usize>> {
    columns
        .iter()
        .map(|column| {
            table
                .column_index(column)
                .ok_or_else(|| crate::encoding::corrupt("partition key"))
        })
        .collect()
}

#[derive(Debug, Clone)]
struct TypedChild {
    name: String,
    bound: TypedBound,
}

#[derive(Debug, Clone)]
pub(crate) struct Partitioned {
    pub(crate) table: String,
    pub(crate) strategy: PartitionStrategy,
    pub(crate) key: Vec<usize>,
    key_names: Vec<String>,
    key_types: Vec<SqlType>,
    children: Vec<TypedChild>,
}

impl Partitioned {
    pub(crate) fn of(
        catalog: &PartitionCatalog,
        table: &TableDefinition,
    ) -> SqlResult<Option<Self>> {
        let Some(key) = catalog.key_of(&table.name) else {
            return Ok(None);
        };
        let positions = key_positions(table, &key.columns)?;
        let key_types: Vec<SqlType> = positions
            .iter()
            .map(|&position| table.columns[position].sql_type)
            .collect();
        let mut children = Vec::new();
        for (name, bound) in catalog.children_of(&table.name) {
            children.push(TypedChild {
                name: name.to_owned(),
                bound: typed(bound, &key_types)?,
            });
        }
        Ok(Some(Self {
            table: table.name.clone(),
            strategy: key.strategy,
            key: positions,
            key_names: key.columns.clone(),
            key_types,
            children,
        }))
    }

    pub(crate) fn key_names(&self) -> &[String] {
        &self.key_names
    }

    pub(crate) fn children(&self) -> impl Iterator<Item = &str> {
        self.children.iter().map(|child| child.name.as_str())
    }

    fn chosen(&self, key: &[Value]) -> Option<&str> {
        let mut default = None;
        for child in &self.children {
            match child.bound {
                TypedBound::Default => default = Some(child.name.as_str()),
                ref bound if bound.holds(key) => return Some(child.name.as_str()),
                _ => {}
            }
        }
        default
    }

    pub(crate) fn route(&self, row: &[Value]) -> Option<&str> {
        self.chosen(&key_values(row, &self.key))
    }

    pub(crate) fn no_partition(&self, row: &[Value]) -> SqlError {
        let key = key_values(row, &self.key);
        no_partition_error(&self.table, &self.key_names, &key)
    }

    pub(crate) fn surviving(&self, filter: Option<&Expr>, qualifiers: &[&str]) -> Vec<&str> {
        let restrictions = match filter {
            Some(filter) => restrictions_of(filter, qualifiers, &self.key_names, &self.key_types),
            None => vec![Vec::new(); self.key_names.len()],
        };
        let mut kept = Vec::new();
        for child in &self.children {
            let survives = match &child.bound {
                TypedBound::Default => self.default_survives(&restrictions),
                bound => bound_survives(self.strategy, bound, &restrictions),
            };
            if survives {
                kept.push(child.name.as_str());
            }
        }
        kept
    }

    fn default_survives(&self, restrictions: &[Vec<Restriction>]) -> bool {
        if self.strategy == PartitionStrategy::Hash {
            return true;
        }
        let Some(first) = restrictions.first() else {
            return true;
        };
        let pinned: Option<Vec<&Value>> = first.iter().find_map(|restriction| match restriction {
            Restriction::Equal(value) => Some(vec![value]),
            Restriction::In(values) => Some(values.iter().collect()),
            _ => None,
        });
        let Some(pinned) = pinned else {
            return true;
        };
        if self.key.len() > 1 {
            return true;
        }
        !pinned.iter().all(|value| {
            self.children.iter().any(|child| {
                !matches!(child.bound, TypedBound::Default)
                    && child.bound.holds(std::slice::from_ref(*value))
            })
        })
    }
}

#[derive(Debug, Clone)]
enum Restriction {
    Equal(Value),
    Less(Value),
    LessOrEqual(Value),
    Greater(Value),
    GreaterOrEqual(Value),
    In(Vec<Value>),
    IsNull,
}

fn constant_value(expr: &Expr, sql_type: SqlType) -> Option<Value> {
    let constant = match &expr.kind {
        ExprKind::Literal(_) => true,
        ExprKind::Cast { expr, .. } | ExprKind::Unary(_, expr) => {
            matches!(expr.kind, ExprKind::Literal(_))
        }
        _ => false,
    };
    if !constant {
        return None;
    }
    let evaluator = Evaluator {
        scope: Scope::empty(),
        row: &[],
        correlation: None,
    };
    let value = evaluator.eval(expr, Some(sql_type)).ok()?;
    let value = convert(value, sql_type, Conversion::Explicit).ok()?;
    (!matches!(value, Value::Null)).then_some(value)
}

fn names_key_column(expr: &Expr, qualifiers: &[&str], key_names: &[String]) -> Option<usize> {
    let ExprKind::Column { qualifier, name } = &expr.kind else {
        return None;
    };
    if let Some(qualifier) = qualifier
        && !qualifiers.iter().any(|allowed| allowed == qualifier)
    {
        return None;
    }
    key_names.iter().position(|key| key == name)
}

fn flipped(operator: BinaryOperator) -> Option<BinaryOperator> {
    Some(match operator {
        BinaryOperator::Equal => BinaryOperator::Equal,
        BinaryOperator::Less => BinaryOperator::Greater,
        BinaryOperator::LessOrEqual => BinaryOperator::GreaterOrEqual,
        BinaryOperator::Greater => BinaryOperator::Less,
        BinaryOperator::GreaterOrEqual => BinaryOperator::LessOrEqual,
        _ => return None,
    })
}

fn conjuncts(expr: &Expr) -> Vec<&Expr> {
    let mut found = Vec::new();
    let mut pending = vec![expr];
    while let Some(next) = pending.pop() {
        match &next.kind {
            ExprKind::And(parts) => pending.extend(parts.iter()),
            _ => found.push(next),
        }
    }
    found
}

fn restrictions_of(
    filter: &Expr,
    qualifiers: &[&str],
    key_names: &[String],
    key_types: &[SqlType],
) -> Vec<Vec<Restriction>> {
    let mut restrictions = vec![Vec::new(); key_names.len()];
    for conjunct in conjuncts(filter) {
        match &conjunct.kind {
            ExprKind::Binary(operator, left, right) => {
                let (column, value, operator) =
                    match names_key_column(left, qualifiers, key_names) {
                        Some(column) => (column, right, Some(*operator)),
                        None => match names_key_column(right, qualifiers, key_names) {
                            Some(column) => (column, left, flipped(*operator)),
                            None => continue,
                        },
                    };
                let Some(operator) = operator else {
                    continue;
                };
                let Some(value) = constant_value(value, key_types[column]) else {
                    continue;
                };
                let restriction = match operator {
                    BinaryOperator::Equal => Restriction::Equal(value),
                    BinaryOperator::Less => Restriction::Less(value),
                    BinaryOperator::LessOrEqual => Restriction::LessOrEqual(value),
                    BinaryOperator::Greater => Restriction::Greater(value),
                    BinaryOperator::GreaterOrEqual => Restriction::GreaterOrEqual(value),
                    _ => continue,
                };
                restrictions[column].push(restriction);
            }
            ExprKind::Between {
                expr,
                low,
                high,
                negated: false,
            } => {
                let Some(column) = names_key_column(expr, qualifiers, key_names) else {
                    continue;
                };
                if let Some(low) = constant_value(low, key_types[column]) {
                    restrictions[column].push(Restriction::GreaterOrEqual(low));
                }
                if let Some(high) = constant_value(high, key_types[column]) {
                    restrictions[column].push(Restriction::LessOrEqual(high));
                }
            }
            ExprKind::InList {
                expr,
                list,
                negated: false,
            } => {
                let Some(column) = names_key_column(expr, qualifiers, key_names) else {
                    continue;
                };
                let values: Option<Vec<Value>> = list
                    .iter()
                    .map(|item| constant_value(item, key_types[column]))
                    .collect();
                if let Some(values) = values {
                    restrictions[column].push(Restriction::In(values));
                }
            }
            ExprKind::IsNull {
                expr,
                negated: false,
            } => {
                if let Some(column) = names_key_column(expr, qualifiers, key_names) {
                    restrictions[column].push(Restriction::IsNull);
                }
            }
            _ => {}
        }
    }
    restrictions
}

fn edge_cmp(edge: &Edge, value: &Value) -> Ordering {
    match edge {
        Edge::Min => Ordering::Less,
        Edge::Max => Ordering::Greater,
        Edge::Value(bound) => compare_values(bound, value),
    }
}

fn range_allows(
    lower: &Edge,
    upper: &Edge,
    upper_inclusive: bool,
    restriction: &Restriction,
) -> bool {
    let at_least_lower = |value: &Value| edge_cmp(lower, value) != Ordering::Greater;
    let below_upper = |value: &Value| match edge_cmp(upper, value) {
        Ordering::Greater => true,
        Ordering::Equal => upper_inclusive,
        Ordering::Less => false,
    };
    match restriction {
        Restriction::Equal(value) => at_least_lower(value) && below_upper(value),
        Restriction::In(values) => values
            .iter()
            .any(|value| at_least_lower(value) && below_upper(value)),
        Restriction::Less(value) => edge_cmp(lower, value) == Ordering::Less,
        Restriction::LessOrEqual(value) => at_least_lower(value),
        Restriction::Greater(value) => edge_cmp(upper, value) == Ordering::Greater,
        Restriction::GreaterOrEqual(value) => below_upper(value),
        Restriction::IsNull => false,
    }
}

fn list_allows(values: &[Value], null: bool, restriction: &Restriction) -> bool {
    let any = |test: &dyn Fn(Ordering) -> bool, value: &Value| {
        values
            .iter()
            .any(|listed| test(compare_values(listed, value)))
    };
    match restriction {
        Restriction::Equal(value) => any(&|order| order == Ordering::Equal, value),
        Restriction::In(wanted) => wanted
            .iter()
            .any(|value| any(&|order| order == Ordering::Equal, value)),
        Restriction::Less(value) => any(&|order| order == Ordering::Less, value),
        Restriction::LessOrEqual(value) => any(&|order| order != Ordering::Greater, value),
        Restriction::Greater(value) => any(&|order| order == Ordering::Greater, value),
        Restriction::GreaterOrEqual(value) => any(&|order| order != Ordering::Less, value),
        Restriction::IsNull => null,
    }
}

fn bound_survives(
    strategy: PartitionStrategy,
    bound: &TypedBound,
    restrictions: &[Vec<Restriction>],
) -> bool {
    match bound {
        TypedBound::Range { from, to } => {
            let (Some(lower), Some(upper), Some(first)) =
                (from.first(), to.first(), restrictions.first())
            else {
                return true;
            };
            let upper_inclusive = to.len() > 1;
            first
                .iter()
                .all(|restriction| range_allows(lower, upper, upper_inclusive, restriction))
        }
        TypedBound::List { values, null } => restrictions.first().is_none_or(|first| {
            first
                .iter()
                .all(|restriction| list_allows(values, *null, restriction))
        }),
        TypedBound::Hash { .. } if strategy == PartitionStrategy::Hash => {
            let pinned: Option<Vec<Value>> = restrictions
                .iter()
                .map(|column| {
                    column.iter().find_map(|restriction| match restriction {
                        Restriction::Equal(value) => Some(value.clone()),
                        Restriction::IsNull => Some(Value::Null),
                        _ => None,
                    })
                })
                .collect();
            pinned.is_none_or(|key| bound.holds(&key))
        }
        TypedBound::Hash { .. } | TypedBound::Default => true,
    }
}

#[derive(Debug)]
struct ConstraintLevel {
    parent: String,
    key: Vec<usize>,
    bound: TypedBound,
    siblings: Vec<TypedBound>,
}

#[derive(Debug)]
pub(crate) struct PartitionConstraint {
    levels: Vec<ConstraintLevel>,
}

impl PartitionConstraint {
    pub(crate) fn holds(&self, row: &[Value]) -> bool {
        self.levels.iter().all(|level| {
            let key = key_values(row, &level.key);
            match level.bound {
                TypedBound::Default => !level.siblings.iter().any(|sibling| sibling.holds(&key)),
                ref bound => bound.holds(&key),
            }
        })
    }

    pub(crate) fn violation(&self, table: &str, row: &[Value]) -> SqlError {
        let dialect = crate::constraints::current_dialect();
        match dialect {
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
                format!("new row for relation \"{table}\" violates partition constraint"),
            )
            .with_detail(format!(
                "Failing row contains ({}).",
                crate::constraints::shown_row(row)
            ))
            .with_table(table),
        }
    }

    pub(crate) fn parent(&self) -> Option<&str> {
        self.levels.first().map(|level| level.parent.as_str())
    }
}

pub(crate) fn no_partition_error(table: &str, key_names: &[String], key: &[Value]) -> SqlError {
    match crate::constraints::current_dialect() {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!(
                "Table has no partition for value {}",
                crate::constraints::shown_row(key)
            ),
        )
        .with_vendor_code(1526),
        Dialect::Oracle => SqlError::new(
            SqlState::CHECK_VIOLATION,
            "inserted partition key does not map to any partition",
        )
        .with_oracle_code(14400),
        _ => SqlError::new(
            SqlState::CHECK_VIOLATION,
            format!("no partition of relation \"{table}\" found for row"),
        )
        .with_detail(format!(
            "Partition key of the failing row contains ({}) = ({}).",
            key_names.join(", "),
            crate::constraints::shown_row(key)
        ))
        .with_table(table),
    }
}

pub(crate) fn constraint_of(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    table: &TableDefinition,
) -> SqlResult<Option<Arc<PartitionConstraint>>> {
    let catalog = catalog(reader, tables)?;
    if catalog.is_empty() || catalog.parent_of(&table.name).is_none() {
        return Ok(None);
    }
    let mut levels = Vec::new();
    let mut current = table.name.clone();
    while let Some((parent, bound)) = catalog.parent_of(&current) {
        if levels.len() >= MAX_DEPTH {
            return Err(crate::encoding::corrupt("partition tree"));
        }
        let Some(key) = catalog.key_of(parent) else {
            return Err(crate::encoding::corrupt("partition tree"));
        };
        let positions = key_positions(table, &key.columns)?;
        let types: Vec<SqlType> = positions
            .iter()
            .map(|&position| table.columns[position].sql_type)
            .collect();
        let siblings = if matches!(bound, PartitionBound::Default) {
            catalog
                .children_of(parent)
                .into_iter()
                .filter(|(name, sibling)| *name != current && !matches!(sibling, PartitionBound::Default))
                .map(|(_, sibling)| typed(sibling, &types))
                .collect::<SqlResult<Vec<_>>>()?
        } else {
            Vec::new()
        };
        levels.push(ConstraintLevel {
            parent: parent.to_owned(),
            key: positions,
            bound: typed(bound, &types)?,
            siblings,
        });
        current = parent.to_owned();
    }
    Ok(Some(Arc::new(PartitionConstraint { levels })))
}

pub(crate) fn partitioned(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    table: &TableDefinition,
) -> SqlResult<Option<Partitioned>> {
    let catalog = catalog(reader, tables)?;
    if catalog.is_empty() {
        return Ok(None);
    }
    Partitioned::of(&catalog, table)
}

fn bound_value_text(
    value: &PartitionBoundValue,
    sql_type: SqlType,
    position: usize,
) -> SqlResult<BoundText> {
    Ok(match value {
        PartitionBoundValue::MinValue => BoundText::MinValue,
        PartitionBoundValue::MaxValue => BoundText::MaxValue,
        PartitionBoundValue::Value(expr) => {
            let evaluator = Evaluator {
                scope: Scope::empty(),
                row: &[],
                correlation: None,
            };
            let value = evaluator
                .eval(expr, Some(sql_type))
                .and_then(|value| convert(value, sql_type, Conversion::Assignment))
                .map_err(|error| {
                    SqlError::new(error.state(), error.message().to_owned()).at(position)
                })?;
            match convert(value, SqlType::Text, Conversion::Explicit)? {
                Value::Null => BoundText::Null,
                Value::Text(text) => BoundText::Value(text),
                _ => return Err(invalid_bound("a partition bound value has no text form", position)),
            }
        }
    })
}

fn invalid_bound(message: &str, position: usize) -> SqlError {
    SqlError::new(SqlState::INVALID_TABLE_DEFINITION, message.to_owned()).at(position)
}

pub(crate) fn bound_of(
    spec: &PartitionBoundSpec,
    key: &PartitionKey,
    types: &[SqlType],
    position: usize,
) -> SqlResult<PartitionBound> {
    let wrong_strategy = |written: &str| {
        invalid_bound(
            &format!(
                "invalid bound specification for a {} partition",
                strategy_name(key.strategy).to_ascii_lowercase()
            ),
            position,
        )
        .with_detail(format!("The bound is written as for a {written} partition."))
    };
    Ok(match spec {
        PartitionBoundSpec::Default => PartitionBound::Default,
        PartitionBoundSpec::Range { from, to } => {
            if key.strategy != PartitionStrategy::Range {
                return Err(wrong_strategy("range"));
            }
            if from.len() != types.len() || to.len() != types.len() {
                return Err(invalid_bound(
                    "FROM and TO must give one value for each partition key column",
                    position,
                ));
            }
            let texts = |values: &[PartitionBoundValue]| -> SqlResult<Vec<BoundText>> {
                values
                    .iter()
                    .zip(types)
                    .map(|(value, sql_type)| bound_value_text(value, *sql_type, position))
                    .collect()
            };
            let from = texts(from)?;
            let to = texts(to)?;
            if from.iter().chain(&to).any(|text| *text == BoundText::Null) {
                return Err(invalid_bound(
                    "cannot specify NULL in range bound",
                    position,
                ));
            }
            let bound = PartitionBound::Range { from, to };
            let typed_bound = typed(&bound, types)?;
            if let TypedBound::Range { from, to } = &typed_bound
                && compare_edge_lists(from, to) != Ordering::Less
            {
                return Err(invalid_bound(
                    "empty range bound specified for partition",
                    position,
                ));
            }
            bound
        }
        PartitionBoundSpec::List(values) => {
            if key.strategy != PartitionStrategy::List {
                return Err(wrong_strategy("list"));
            }
            let sql_type = *types.first().ok_or_else(|| invalid_bound("no key", position))?;
            let mut texts = Vec::with_capacity(values.len());
            for value in values {
                if !matches!(value, PartitionBoundValue::Value(_)) {
                    return Err(invalid_bound(
                        "MINVALUE and MAXVALUE are only allowed in range bounds",
                        position,
                    ));
                }
                let text = bound_value_text(value, sql_type, position)?;
                if !texts.contains(&text) {
                    texts.push(text);
                }
            }
            PartitionBound::List(texts)
        }
        PartitionBoundSpec::Hash { modulus, remainder } => {
            if key.strategy != PartitionStrategy::Hash {
                return Err(wrong_strategy("hash"));
            }
            if remainder >= modulus {
                return Err(invalid_bound(
                    "remainder for hash partition must be less than modulus",
                    position,
                ));
            }
            PartitionBound::Hash {
                modulus: *modulus,
                remainder: *remainder,
            }
        }
    })
}

fn compare_edge_lists(left: &[Edge], right: &[Edge]) -> Ordering {
    for (left, right) in left.iter().zip(right) {
        let order = match (left, right) {
            (Edge::Min, Edge::Min) | (Edge::Max, Edge::Max) => return Ordering::Equal,
            (Edge::Min, _) | (_, Edge::Max) => Ordering::Less,
            (Edge::Max, _) | (_, Edge::Min) => Ordering::Greater,
            (Edge::Value(left), Edge::Value(right)) => compare_values(left, right),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
}

pub(crate) fn check_no_overlap(
    catalog: &PartitionCatalog,
    parent: &str,
    child: &str,
    bound: &PartitionBound,
    types: &[SqlType],
    position: usize,
) -> SqlResult<()> {
    let overlap = |other: &str| {
        SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            format!("partition \"{child}\" would overlap partition \"{other}\""),
        )
        .at(position)
    };
    let new = typed(bound, types)?;
    for (other, existing) in catalog.children_of(parent) {
        if other == child {
            continue;
        }
        let existing = typed(existing, types)?;
        let overlaps = match (&new, &existing) {
            (TypedBound::Default, TypedBound::Default) => {
                return Err(SqlError::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    format!(
                        "partition \"{child}\" conflicts with existing default partition \"{other}\""
                    ),
                )
                .at(position));
            }
            (
                TypedBound::Range { from, to },
                TypedBound::Range {
                    from: other_from,
                    to: other_to,
                },
            ) => {
                compare_edge_lists(from, other_to) == Ordering::Less
                    && compare_edge_lists(other_from, to) == Ordering::Less
            }
            (
                TypedBound::List { values, null },
                TypedBound::List {
                    values: other_values,
                    null: other_null,
                },
            ) => {
                (*null && *other_null)
                    || values.iter().any(|value| {
                        other_values
                            .iter()
                            .any(|other| compare_values(value, other) == Ordering::Equal)
                    })
            }
            (
                TypedBound::Hash { modulus, remainder },
                TypedBound::Hash {
                    modulus: other_modulus,
                    remainder: other_remainder,
                },
            ) => {
                let (larger, smaller) = if modulus >= other_modulus {
                    (*modulus, *other_modulus)
                } else {
                    (*other_modulus, *modulus)
                };
                if larger % smaller != 0 {
                    return Err(SqlError::new(
                        SqlState::INVALID_OBJECT_DEFINITION,
                        "every hash partition modulus must be a factor of the next larger modulus",
                    )
                    .at(position));
                }
                remainder % smaller == other_remainder % smaller
            }
            _ => false,
        };
        if overlaps {
            return Err(overlap(other));
        }
    }
    Ok(())
}

pub(crate) fn strategy_name(strategy: PartitionStrategy) -> &'static str {
    match strategy {
        PartitionStrategy::Range => "RANGE",
        PartitionStrategy::List => "LIST",
        PartitionStrategy::Hash => "HASH",
    }
}

pub(crate) fn key_definition(key: &PartitionKey) -> String {
    let columns: Vec<String> = key
        .columns
        .iter()
        .map(|column| crate::pg_keywords::quote_ident(column))
        .collect();
    format!("{} ({})", strategy_name(key.strategy), columns.join(", "))
}

fn bound_literal(text: &BoundText, numeric: bool) -> String {
    match text {
        BoundText::Value(text) if numeric => text.clone(),
        BoundText::Value(text) => format!("'{}'", text.replace('\'', "''")),
        BoundText::Null => "NULL".to_owned(),
        BoundText::MinValue => "MINVALUE".to_owned(),
        BoundText::MaxValue => "MAXVALUE".to_owned(),
    }
}

pub(crate) fn bound_definition(bound: &PartitionBound, numeric: &[bool]) -> String {
    let list = |texts: &[BoundText]| -> String {
        texts
            .iter()
            .enumerate()
            .map(|(index, text)| bound_literal(text, numeric.get(index).copied().unwrap_or(false)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match bound {
        PartitionBound::Range { from, to } => {
            format!("FOR VALUES FROM ({}) TO ({})", list(from), list(to))
        }
        PartitionBound::List(values) => {
            let numeric = numeric.first().copied().unwrap_or(false);
            let items: Vec<String> = values
                .iter()
                .map(|text| bound_literal(text, numeric))
                .collect();
            format!("FOR VALUES IN ({})", items.join(", "))
        }
        PartitionBound::Hash { modulus, remainder } => {
            format!("FOR VALUES WITH (modulus {modulus}, remainder {remainder})")
        }
        PartitionBound::Default => "DEFAULT".to_owned(),
    }
}

pub(crate) fn numeric_key_columns(table: &TableDefinition, key: &PartitionKey) -> Vec<bool> {
    key.columns
        .iter()
        .map(|column| {
            table
                .column_index(column)
                .is_some_and(|index| table.columns[index].sql_type.is_numeric())
        })
        .collect()
}
