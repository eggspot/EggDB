pub(crate) use crate::ast::PartitionStrategy;
use crate::ast::{BinaryOperator, Expr, ExprKind, PartitionBoundSpec, PartitionBoundValue};
use crate::dialect::Dialect;
use crate::encoding::{
    TableDefinition, decode_partitioning, encode_key_value, encode_partitioning, partition_key,
    partitions_prefix, prefix_end,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, Source};
use crate::execute::TableCache;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::{Conversion, SqlType, Value, compare_values, convert};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const MAX_KEY_COLUMNS: usize = 32;
pub(crate) const MAX_BOUND_VALUES: usize = 65_536;
pub(crate) const MAX_INDEX_LINKS: usize = 65_536;
const CHANGING_FUNCTIONS: &[&str] = &[
    "random",
    "rand",
    "now",
    "current_timestamp",
    "current_date",
    "current_time",
    "localtime",
    "localtimestamp",
    "clock_timestamp",
    "statement_timestamp",
    "transaction_timestamp",
    "timeofday",
    "nextval",
    "currval",
    "lastval",
    "setval",
    "gen_random_uuid",
    "uuid",
    "uuid_generate_v4",
    "uuid_short",
    "sysdate",
    "systimestamp",
    "sysdatetime",
    "getdate",
    "getutcdate",
    "newid",
    "unix_timestamp",
    "utc_timestamp",
    "curdate",
    "curtime",
    crate::routines::ROUTINE_CALL,
];
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
pub(crate) enum KeyPart {
    Column(String),
    Expression { text: String, dialect: Dialect },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartitionKey {
    pub(crate) strategy: PartitionStrategy,
    pub(crate) parts: Vec<KeyPart>,
}

impl PartitionKey {
    pub(crate) fn has_expressions(&self) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, KeyPart::Expression { .. }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexLink {
    pub(crate) index: u32,
    pub(crate) parent_index: u32,
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
    pub(crate) index_links: Vec<IndexLink>,
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
        self.records
            .iter()
            .map(|(name, record)| (name.as_str(), record))
    }

    pub(crate) fn index_links_of(&self, table: &str) -> &[IndexLink] {
        self.records
            .get(table)
            .map_or(&[], |record| record.index_links.as_slice())
    }

    pub(crate) fn parent_index_of(&self, table: &str, index: u32) -> Option<(&str, u32)> {
        let (parent, _) = self.parent_of(table)?;
        self.index_links_of(table)
            .iter()
            .find(|link| link.index == index)
            .map(|link| (parent, link.parent_index))
    }

    pub(crate) fn child_index_of(&self, child: &str, parent_index: u32) -> Option<u32> {
        self.index_links_of(child)
            .iter()
            .find(|link| link.parent_index == parent_index)
            .map(|link| link.index)
    }

    pub(crate) fn index_reaches_every_partition(&self, table: &str, index: u32) -> bool {
        let mut pending = vec![(table.to_owned(), index, 0usize)];
        while let Some((parent, parent_index, depth)) = pending.pop() {
            if depth > MAX_DEPTH {
                return false;
            }
            for (child, _) in self.children_of(&parent) {
                let Some(child_index) = self.child_index_of(child, parent_index) else {
                    return false;
                };
                pending.push((child.to_owned(), child_index, depth + 1));
            }
        }
        true
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

fn table_scope(table: &TableDefinition) -> Scope<'_> {
    Scope::One(Source {
        table,
        label: &table.name,
        offset: 0,
    })
}

pub(crate) fn parse_key_expression(text: &str, dialect: Dialect) -> SqlResult<Expr> {
    let damaged = || crate::encoding::corrupt("partition key");
    let tokens = crate::lexer::tokenize_in(text, dialect).map_err(|_| damaged())?;
    let mut parser = crate::parser::Parser::new(text, &tokens).in_dialect(dialect);
    let expr = parser.expression().map_err(|_| damaged())?;
    if !parser.at_end() {
        return Err(damaged());
    }
    Ok(expr)
}

fn expression_type(expr: &Expr, table: &TableDefinition, dialect: Dialect) -> Option<SqlType> {
    match crate::typing::static_type(expr, table_scope(table)) {
        Some(sql_type) => Some(sql_type),
        None if dialect == Dialect::MySql => Some(SqlType::BigInt),
        None => None,
    }
}

fn any_part(expr: &Expr, test: &dyn Fn(&Expr) -> bool) -> bool {
    test(expr)
        || crate::eval::children(expr)
            .into_iter()
            .any(|child| any_part(child, test))
}

fn changes_between_calls(expr: &Expr) -> bool {
    let changing = |name: &str| CHANGING_FUNCTIONS.contains(&name.to_ascii_lowercase().as_str());
    match &expr.kind {
        ExprKind::SessionValue { .. } => true,
        ExprKind::Function { name, .. } => changing(name),
        ExprKind::OracleFunction { name, .. } | ExprKind::SqliteFunction { name, .. } => {
            changing(name)
        }
        _ => false,
    }
}

fn expression_columns(expr: &Expr, table: &TableDefinition, found: &mut Vec<usize>) {
    if let ExprKind::Column { name, .. } = &expr.kind
        && let Some(position) = table.column_index(name)
        && !found.contains(&position)
    {
        found.push(position);
    }
    for child in crate::eval::children(expr) {
        expression_columns(child, table, found);
    }
}

pub(crate) fn check_key_expression(
    table: &TableDefinition,
    expr: &Expr,
    dialect: Dialect,
    position: usize,
) -> SqlResult<()> {
    crate::eval::check_columns(expr, table_scope(table))?;
    if any_part(expr, &|part| matches!(part.kind, ExprKind::Subquery(_))) {
        return Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "cannot use subquery in partition key expression",
        )
        .at(position));
    }
    crate::eval::reject_aggregates(expr, "partition key expressions")?;
    if any_part(expr, &changes_between_calls) {
        return Err(SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            "functions in partition key expression must be marked IMMUTABLE",
        )
        .at(position));
    }
    let mut columns = Vec::new();
    expression_columns(expr, table, &mut columns);
    if columns.is_empty() {
        return Err(SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            "cannot use constant expression as partition key",
        )
        .at(position));
    }
    if expression_type(expr, table, dialect).is_none() {
        return Err(SqlError::not_supported(
            "a partition key expression whose type EggDB cannot work out",
        )
        .with_hint("Cast the expression to the type it gives, as in ((expr)::integer).")
        .at(position));
    }
    Ok(())
}

#[derive(Debug, Clone)]
enum KeySource {
    Column(usize),
    Expression(Expr),
}

#[derive(Debug, Clone)]
struct KeyReader {
    sources: Vec<KeySource>,
    types: Vec<SqlType>,
    table: Option<Arc<TableDefinition>>,
}

impl KeyReader {
    fn of(table: &TableDefinition, key: &PartitionKey) -> SqlResult<Self> {
        let damaged = || crate::encoding::corrupt("partition key");
        let mut sources = Vec::with_capacity(key.parts.len());
        let mut types = Vec::with_capacity(key.parts.len());
        let mut owner = None;
        for part in &key.parts {
            match part {
                KeyPart::Column(name) => {
                    let position = table.column_index(name).ok_or_else(damaged)?;
                    types.push(table.columns[position].sql_type);
                    sources.push(KeySource::Column(position));
                }
                KeyPart::Expression { text, dialect } => {
                    let expr = parse_key_expression(text, *dialect)?;
                    crate::eval::check_columns(&expr, table_scope(table)).map_err(|_| damaged())?;
                    types.push(expression_type(&expr, table, *dialect).ok_or_else(damaged)?);
                    sources.push(KeySource::Expression(expr));
                    if owner.is_none() {
                        owner = Some(Arc::new(table.clone()));
                    }
                }
            }
        }
        Ok(Self {
            sources,
            types,
            table: owner,
        })
    }

    fn read(&self, row: &[Value]) -> SqlResult<Vec<Value>> {
        self.sources
            .iter()
            .zip(&self.types)
            .map(|(source, sql_type)| match source {
                KeySource::Column(position) => {
                    Ok(row.get(*position).cloned().unwrap_or(Value::Null))
                }
                KeySource::Expression(expr) => {
                    let table = self
                        .table
                        .as_deref()
                        .ok_or_else(|| crate::encoding::corrupt("partition key"))?;
                    let evaluator = Evaluator {
                        scope: table_scope(table),
                        row,
                        correlation: None,
                    };
                    let value = evaluator.eval(expr, Some(*sql_type))?;
                    convert(value, *sql_type, Conversion::Explicit)
                }
            })
            .collect()
    }

    fn columns_read(&self) -> Vec<usize> {
        let mut found = Vec::new();
        for source in &self.sources {
            match source {
                KeySource::Column(position) => {
                    if !found.contains(position) {
                        found.push(*position);
                    }
                }
                KeySource::Expression(expr) => {
                    if let Some(table) = self.table.as_deref() {
                        expression_columns(expr, table, &mut found);
                    }
                }
            }
        }
        found
    }

    fn matchers(&self, table: &TableDefinition) -> Vec<KeyMatcher> {
        self.sources
            .iter()
            .map(|source| match source {
                KeySource::Column(position) => KeyMatcher::Column(
                    table
                        .columns
                        .get(*position)
                        .map(|column| column.name.clone())
                        .unwrap_or_default(),
                ),
                KeySource::Expression(expr) => KeyMatcher::Expression(normalized(expr, &[])),
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
enum KeyMatcher {
    Column(String),
    Expression(Expr),
}

fn normalize(expr: &mut Expr, qualifiers: &[&str]) {
    expr.position = 0;
    if let ExprKind::Column { qualifier, .. } = &mut expr.kind
        && qualifier
            .as_deref()
            .is_some_and(|written| qualifiers.contains(&written))
    {
        *qualifier = None;
    }
    for child in crate::eval::children_mut(expr) {
        normalize(child, qualifiers);
    }
}

fn normalized(expr: &Expr, qualifiers: &[&str]) -> Expr {
    let mut copy = expr.clone();
    normalize(&mut copy, qualifiers);
    copy
}

pub(crate) fn key_types(table: &TableDefinition, key: &PartitionKey) -> SqlResult<Vec<SqlType>> {
    Ok(KeyReader::of(table, key)?.types)
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
    key: KeyReader,
    key_names: Vec<String>,
    matchers: Vec<KeyMatcher>,
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
        let reader = KeyReader::of(table, key)?;
        let mut children = Vec::new();
        for (name, bound) in catalog.children_of(&table.name) {
            children.push(TypedChild {
                name: name.to_owned(),
                bound: typed(bound, &reader.types)?,
            });
        }
        Ok(Some(Self {
            table: table.name.clone(),
            strategy: key.strategy,
            matchers: reader.matchers(table),
            key: reader,
            key_names: key.parts.iter().map(part_definition).collect(),
            children,
        }))
    }

    pub(crate) fn columns_read(&self) -> Vec<usize> {
        self.key.columns_read()
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

    pub(crate) fn route(&self, row: &[Value]) -> SqlResult<Option<&str>> {
        Ok(self.chosen(&self.key.read(row)?))
    }

    pub(crate) fn no_partition(&self, row: &[Value]) -> SqlError {
        match self.key.read(row) {
            Ok(key) => no_partition_error(&self.table, &self.key_names, &key),
            Err(error) => error,
        }
    }

    pub(crate) fn surviving(&self, filter: Option<&Expr>, qualifiers: &[&str]) -> Vec<&str> {
        let restrictions = match filter {
            Some(filter) => restrictions_of(filter, qualifiers, &self.matchers, &self.key.types),
            None => vec![Vec::new(); self.matchers.len()],
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
        if self.key.types.len() > 1 {
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

fn names_key_part(expr: &Expr, qualifiers: &[&str], matchers: &[KeyMatcher]) -> Option<usize> {
    if let ExprKind::Column { qualifier, name } = &expr.kind {
        if let Some(qualifier) = qualifier
            && !qualifiers.contains(&qualifier.as_str())
        {
            return None;
        }
        return matchers
            .iter()
            .position(|matcher| matches!(matcher, KeyMatcher::Column(key) if key == name));
    }
    if !matchers
        .iter()
        .any(|matcher| matches!(matcher, KeyMatcher::Expression(_)))
    {
        return None;
    }
    let candidate = normalized(expr, qualifiers);
    matchers
        .iter()
        .position(|matcher| matches!(matcher, KeyMatcher::Expression(key) if *key == candidate))
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
    matchers: &[KeyMatcher],
    key_types: &[SqlType],
) -> Vec<Vec<Restriction>> {
    let mut restrictions = vec![Vec::new(); matchers.len()];
    for conjunct in conjuncts(filter) {
        match &conjunct.kind {
            ExprKind::Binary(operator, left, right) => {
                let (column, value, operator) = match names_key_part(left, qualifiers, matchers)
                {
                    Some(column) => (column, right, Some(*operator)),
                    None => match names_key_part(right, qualifiers, matchers) {
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
                let Some(column) = names_key_part(expr, qualifiers, matchers) else {
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
                let Some(column) = names_key_part(expr, qualifiers, matchers) else {
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
                if let Some(column) = names_key_part(expr, qualifiers, matchers) {
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
    key: KeyReader,
    bound: TypedBound,
    siblings: Vec<TypedBound>,
}

#[derive(Debug)]
pub(crate) struct PartitionConstraint {
    levels: Vec<ConstraintLevel>,
}

impl PartitionConstraint {
    fn fits(&self, row: &[Value]) -> SqlResult<bool> {
        for level in &self.levels {
            let key = level.key.read(row)?;
            let fits = match level.bound {
                TypedBound::Default => !level.siblings.iter().any(|sibling| sibling.holds(&key)),
                ref bound => bound.holds(&key),
            };
            if !fits {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn holds(&self, row: &[Value]) -> bool {
        self.fits(row).unwrap_or(false)
    }

    pub(crate) fn check(&self, table: &str, row: &[Value]) -> SqlResult<()> {
        if self.fits(row)? {
            Ok(())
        } else {
            Err(self.violation(table, row))
        }
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

    pub(crate) fn root(&self) -> Option<&str> {
        self.levels.last().map(|level| level.parent.as_str())
    }

    pub(crate) fn uses_hash(&self) -> bool {
        self.levels
            .iter()
            .any(|level| matches!(level.bound, TypedBound::Hash { .. }))
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
        let reader = KeyReader::of(table, key)?;
        let types = reader.types.clone();
        let siblings = if matches!(bound, PartitionBound::Default) {
            catalog
                .children_of(parent)
                .into_iter()
                .filter(|(name, sibling)| {
                    *name != current && !matches!(sibling, PartitionBound::Default)
                })
                .map(|(_, sibling)| typed(sibling, &types))
                .collect::<SqlResult<Vec<_>>>()?
        } else {
            Vec::new()
        };
        levels.push(ConstraintLevel {
            parent: parent.to_owned(),
            key: reader,
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
                _ => {
                    return Err(invalid_bound(
                        "a partition bound value has no text form",
                        position,
                    ));
                }
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
        .with_detail(format!(
            "The bound is written as for a {written} partition."
        ))
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
            let sql_type = *types
                .first()
                .ok_or_else(|| invalid_bound("no key", position))?;
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

fn looks_like_function(text: &str, dialect: Dialect) -> bool {
    parse_key_expression(text, dialect).is_ok_and(|expr| {
        matches!(
            expr.kind,
            ExprKind::Function { .. }
                | ExprKind::OracleFunction { .. }
                | ExprKind::SqlServerFunction { .. }
                | ExprKind::SqliteFunction { .. }
        )
    })
}

fn part_definition(part: &KeyPart) -> String {
    match part {
        KeyPart::Column(column) => crate::pg_keywords::quote_ident(column),
        KeyPart::Expression { text, dialect } if looks_like_function(text, *dialect) => {
            text.clone()
        }
        KeyPart::Expression { text, .. } => format!("({text})"),
    }
}

pub(crate) fn key_definition(key: &PartitionKey) -> String {
    let parts: Vec<String> = key.parts.iter().map(part_definition).collect();
    format!("{} ({})", strategy_name(key.strategy), parts.join(", "))
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
    match key_types(table, key) {
        Ok(types) => types.iter().map(|sql_type| sql_type.is_numeric()).collect(),
        Err(_) => vec![false; key.parts.len()],
    }
}

pub(crate) fn forget(writer: &mut dyn KeyValueWrite, table: &str) -> SqlResult<()> {
    writer.delete(&partition_key(table)).map(|_| ())
}

pub(crate) fn rename(writer: &mut dyn KeyValueWrite, from: &str, to: &str) -> SqlResult<()> {
    let catalog = load(writer)?;
    if catalog.is_empty() {
        return Ok(());
    }
    if let Some(record) = catalog.record(from) {
        forget(writer, from)?;
        save(writer, to, record)?;
    }
    for (child, record) in catalog.every() {
        if let Some((parent, bound)) = &record.parent
            && parent == from
        {
            let mut moved = record.clone();
            moved.parent = Some((to.to_owned(), bound.clone()));
            save(writer, child, &moved)?;
        }
    }
    Ok(())
}

pub(crate) fn with_descendants(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    names: &[crate::ast::Name],
) -> SqlResult<Option<Vec<crate::ast::Name>>> {
    let catalog = catalog(reader, tables)?;
    if catalog.is_empty() {
        return Ok(None);
    }
    let mut expanded: Vec<crate::ast::Name> = Vec::with_capacity(names.len());
    for name in names {
        for descendant in catalog.descendants_of(&name.text).into_iter().rev() {
            if !expanded
                .iter()
                .chain(names)
                .any(|known| known.text == descendant)
            {
                expanded.push(crate::ast::Name {
                    text: descendant,
                    position: name.position,
                });
            }
        }
        expanded.push(name.clone());
    }
    Ok(Some(expanded))
}
