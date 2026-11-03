use crate::aggregate::{bind, rebuild};
use crate::ast::{
    BinaryOperator, Expr, ExprKind, FromClause, JoinCondition, JoinKind, Select, SelectItem,
};
use crate::encoding::{
    TableDefinition, decode_row_into, encode_column_key, index_prefix, prefix_end, rows_prefix,
    write_prefix_end,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{
    Evaluator, Scope, Source, Sources, check_columns, children, compare_truth, reject_aggregates,
};
use crate::hash_index::HashIndex;
use crate::indexes::{write_entry_key, write_row_key_of_entry};
use crate::memory::WorkMemory;
use crate::store::{KeyValueRead, ScanDirection};
use crate::types::{Conversion, SqlType, Value, comparable, compare_values, convert, equality_key};
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) struct JoinedFrom<'a> {
    pub(crate) sources: Sources<'a>,
    shapes: Vec<StepShape<'a>>,
}

struct StepShape<'a> {
    kind: JoinKind,
    source: Source<'a>,
    on: Option<&'a Expr>,
    using: Vec<(usize, usize)>,
    where_keys: Vec<(usize, usize)>,
    visible: Sources<'a>,
}

fn sources_between<'a>(
    list: &[Source<'a>],
    visible: std::ops::Range<usize>,
    merged: &[usize],
) -> Sources<'a> {
    Sources {
        list: list[visible.clone()].to_vec(),
        merged: merged.to_vec(),
        out_of_reach: list[..visible.start]
            .iter()
            .map(|source| source.label)
            .collect(),
        star: Vec::new(),
    }
}

fn shown_columns_of<'s>(source: &'s Source<'_>) -> impl Iterator<Item = usize> + 's {
    source
        .table
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| !crate::temp_tables::is_hidden_column(&column.name))
        .map(move |(index, _)| source.offset + index)
}

pub(crate) fn type_family(sql_type: SqlType) -> u8 {
    match sql_type {
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt => 0,
        SqlType::Real | SqlType::Double => 1,
        SqlType::Text | SqlType::Varchar(_) | SqlType::Bpchar(_) => 2,
        SqlType::Boolean => 3,
        SqlType::Numeric(_) => 4,
        SqlType::Date | SqlType::Timestamp(_) => 5,
        SqlType::TimestampTz(_) => 6,
        SqlType::Json => 7,
        SqlType::Jsonb => 8,
        SqlType::Time(_) => 9,
        SqlType::TimeTz(_) => 10,
        SqlType::Interval { .. } => 11,
        SqlType::Bytea => 12,
        SqlType::Uuid => 13,
        SqlType::Array(_) => 14,
        SqlType::Catalog(kind) if kind.is_identifier() => 16,
        SqlType::Catalog(kind) if kind.is_text_like() => 2,
        SqlType::Catalog(_) => 15,
        SqlType::Defined(defined) => match defined.kind {
            crate::types::DefinedKind::Enum => 17,
            crate::types::DefinedKind::Composite => 18,
            crate::types::DefinedKind::Range => 19,
            crate::types::DefinedKind::Domain => 20,
            crate::types::DefinedKind::Base => 22,
        },
        SqlType::Dynamic(_) => 21,
    }
}

fn has_integer_keys(sql_type: SqlType) -> bool {
    matches!(type_family(sql_type), 0 | 16)
}

fn same_family(all: &Sources<'_>, outer: usize, inner: usize) -> bool {
    let scope = Scope::Many(all);
    match (scope.column_type(outer), scope.column_type(inner)) {
        (Some(x), Some(y)) => type_family(x) == type_family(y),
        _ => false,
    }
}

fn no_equality(left: SqlType, right: SqlType, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "operator does not exist: {} = {}",
            crate::pg_catalog::operator_operand_type_name(left),
            crate::pg_catalog::operator_operand_type_name(right)
        ),
    )
    .with_hint(
        "No operator matches the given name and argument types. You might need to add explicit type casts.",
    )
    .at(position)
}

pub(crate) fn resolve<'a>(
    from: &'a FromClause,
    tables: &'a [Arc<TableDefinition>],
    filter: Option<&Expr>,
) -> SqlResult<JoinedFrom<'a>> {
    let references: Vec<_> = std::iter::once(&from.first)
        .chain(from.joins.iter().map(|join| &join.table))
        .collect();
    let mut list: Vec<Source<'a>> = Vec::with_capacity(references.len());
    let mut offset = 0;
    for (reference, table) in references.iter().zip(tables) {
        let label = reference.alias.as_deref().unwrap_or(&table.name);
        if list.iter().any(|source| source.label == label) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_ALIAS,
                format!("table name \"{label}\" specified more than once"),
            )
            .at(reference.name.position));
        }
        list.push(Source {
            table,
            label,
            offset,
        });
        offset += table.columns.len();
    }
    let mut merged = Vec::new();
    let mut star = Vec::new();
    let mut group_start = 0;
    let mut group_star: Vec<usize> = shown_columns_of(&list[0]).collect();
    let mut shapes = Vec::with_capacity(from.joins.len());
    for (index, join) in from.joins.iter().enumerate() {
        let right_index = index + 1;
        let right = list[right_index];
        if join.kind == JoinKind::Comma {
            star.append(&mut group_star);
            group_start = right_index;
            group_star = shown_columns_of(&right).collect();
            shapes.push(StepShape {
                kind: join.kind,
                source: right,
                on: None,
                using: Vec::new(),
                where_keys: Vec::new(),
                visible: sources_between(&list, right_index..right_index + 1, &merged),
            });
            continue;
        }
        let visible = sources_between(&list, group_start..right_index + 1, &merged);
        let mut using = Vec::new();
        let mut on = None;
        match &join.condition {
            JoinCondition::On(expr) => {
                reject_aggregates(expr, "JOIN conditions")?;
                check_columns(expr, Scope::Many(&visible))?;
                on = Some(expr);
            }
            JoinCondition::Using(names) => {
                let left = sources_between(&list, group_start..right_index, &merged);
                using = using_pairs(&left, right, names)?;
            }
            JoinCondition::Natural => {
                let left = sources_between(&list, group_start..right_index, &merged);
                let names = natural_columns(&list, &group_star, right, join.table.name.position);
                using = using_pairs(&left, right, &names)?;
            }
            JoinCondition::Always => {}
        }
        if using.is_empty() {
            group_star.extend(shown_columns_of(&right));
        } else {
            let lefts: Vec<usize> = using.iter().map(|&(left, _)| left).collect();
            let rights: Vec<usize> = using.iter().map(|&(_, right)| right).collect();
            let (shown, hidden) = if join.kind == JoinKind::Right {
                (&rights, &lefts)
            } else {
                (&lefts, &rights)
            };
            let mut reordered = shown.clone();
            reordered.extend(group_star.iter().filter(|offset| !lefts.contains(offset)));
            reordered.extend(shown_columns_of(&right).filter(|offset| !rights.contains(offset)));
            group_star = reordered;
            merged.extend(hidden.iter().copied());
        }
        shapes.push(StepShape {
            kind: join.kind,
            source: right,
            on,
            using,
            where_keys: Vec::new(),
            visible,
        });
    }
    star.append(&mut group_star);
    let mut joined = JoinedFrom {
        sources: Sources {
            list,
            merged,
            out_of_reach: Vec::new(),
            star,
        },
        shapes,
    };
    if let Some(filter) = filter {
        joined.add_where_keys(filter)?;
    }
    Ok(joined)
}

fn using_pairs(
    left: &Sources<'_>,
    right: Source<'_>,
    names: &[crate::ast::Name],
) -> SqlResult<Vec<(usize, usize)>> {
    let mut using = Vec::with_capacity(names.len());
    for (at, name) in names.iter().enumerate() {
        if names[..at].iter().any(|earlier| earlier.text == name.text) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_COLUMN,
                format!(
                    "column name \"{}\" appears more than once in USING clause",
                    name.text
                ),
            )
            .at(name.position));
        }
        let left_offset = Scope::Many(left)
            .column(None, &name.text, name.position)
            .map_err(|error| {
                if error.state() == SqlState::AMBIGUOUS_COLUMN {
                    SqlError::new(
                        SqlState::AMBIGUOUS_COLUMN,
                        format!(
                            "common column name \"{}\" appears more than once in left table",
                            name.text
                        ),
                    )
                    .at(name.position)
                } else {
                    SqlError::new(
                        SqlState::UNDEFINED_COLUMN,
                        format!(
                            "column \"{}\" specified in USING clause does not exist in left table",
                            name.text
                        ),
                    )
                    .at(name.position)
                }
            })?;
        let right_column = right.table.column_index(&name.text).ok_or_else(|| {
            SqlError::new(
                SqlState::UNDEFINED_COLUMN,
                format!(
                    "column \"{}\" specified in USING clause does not exist in right table",
                    name.text
                ),
            )
            .at(name.position)
        })?;
        let left_type = Scope::Many(left).column_type(left_offset);
        let right_type = right.table.columns[right_column].sql_type;
        if let Some(left_type) = left_type
            && !comparable(left_type, right_type)
        {
            return Err(no_equality(left_type, right_type, name.position));
        }
        using.push((left_offset, right.offset + right_column));
    }
    Ok(using)
}

fn column_name_at<'t>(list: &[Source<'t>], offset: usize) -> Option<&'t str> {
    list.iter().find_map(|source| {
        offset
            .checked_sub(source.offset)
            .and_then(|at| source.table.columns.get(at))
            .map(|column| column.name.as_str())
    })
}

fn natural_columns(
    list: &[Source<'_>],
    left_star: &[usize],
    right: Source<'_>,
    position: usize,
) -> Vec<crate::ast::Name> {
    let mut names: Vec<crate::ast::Name> = Vec::new();
    for &offset in left_star {
        let Some(name) = column_name_at(list, offset) else {
            continue;
        };
        let shared = shown_columns_of(&right).any(|at| {
            right
                .table
                .columns
                .get(at - right.offset)
                .is_some_and(|column| column.name == name)
        });
        if shared && !names.iter().any(|known| known.text == name) {
            names.push(crate::ast::Name {
                text: name.to_owned(),
                position,
            });
        }
    }
    names
}

pub(crate) fn has_full_using(from: &FromClause) -> bool {
    from.joins.iter().any(|join| {
        join.kind == JoinKind::Full
            && matches!(
                join.condition,
                JoinCondition::Using(_) | JoinCondition::Natural
            )
    })
}

struct MergedColumn {
    name: String,
    merged: Expr,
}

fn qualified_column(label: &str, name: &str, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Column {
            qualifier: Some(label.to_owned()),
            name: name.to_owned(),
        },
        position,
    }
}

fn with_merged_columns(expr: &Expr, merged: &[MergedColumn]) -> SqlResult<Expr> {
    if let ExprKind::Column {
        qualifier: None,
        name,
    } = &expr.kind
        && let Some(column) = merged.iter().find(|column| &column.name == name)
    {
        let mut replaced = column.merged.clone();
        replaced.position = expr.position;
        return Ok(replaced);
    }
    rebuild(expr, &mut |child| with_merged_columns(child, merged))
}

pub(crate) fn merge_full_using(
    select: &mut Select,
    tables: &[Arc<TableDefinition>],
) -> SqlResult<()> {
    let Some(from) = &select.from else {
        return Ok(());
    };
    let (merged, star, conditions) = {
        let joined = resolve(from, tables, None)?;
        let scope = Scope::Many(&joined.sources);
        let mut merged: Vec<(usize, MergedColumn)> = Vec::new();
        let mut conditions: Vec<(usize, Expr)> = Vec::new();
        for (index, shape) in joined.shapes.iter().enumerate() {
            if shape.kind != JoinKind::Full || shape.using.is_empty() {
                continue;
            }
            let position = from
                .joins
                .get(index)
                .map_or(0, |join| join.table.name.position);
            let mut equalities = Vec::with_capacity(shape.using.len());
            for &(left, right) in &shape.using {
                let (Some((left_source, left_index)), Some((right_source, right_index))) =
                    (scope.source_of(left), scope.source_of(right))
                else {
                    continue;
                };
                let left_name = &left_source.table.columns[left_index].name;
                let right_name = &right_source.table.columns[right_index].name;
                let left_column = qualified_column(left_source.label, left_name, position);
                let right_column = qualified_column(right_source.label, right_name, position);
                merged.push((
                    left,
                    MergedColumn {
                        name: crate::temp_tables::display_name(left_name).to_owned(),
                        merged: Expr {
                            kind: ExprKind::Function {
                                name: "coalesce".to_owned(),
                                arguments: vec![left_column.clone(), right_column.clone()],
                            },
                            position,
                        },
                    },
                ));
                equalities.push(Expr {
                    kind: ExprKind::Binary(
                        BinaryOperator::Equal,
                        Box::new(left_column),
                        Box::new(right_column),
                    ),
                    position,
                });
            }
            let condition = match equalities.len() {
                1 => equalities.pop(),
                _ => Some(Expr {
                    kind: ExprKind::And(equalities),
                    position,
                }),
            };
            if let Some(condition) = condition {
                conditions.push((index, condition));
            }
        }
        let star: Vec<SelectItem> = scope
            .star()
            .into_iter()
            .filter_map(|offset| {
                if let Some((_, column)) = merged.iter().find(|(at, _)| *at == offset) {
                    return Some(SelectItem::Expr {
                        expr: column.merged.clone(),
                        alias: Some(column.name.clone()),
                    });
                }
                let (source, index) = scope.source_of(offset)?;
                let name = &source.table.columns[index].name;
                if crate::temp_tables::is_hidden_column(name) {
                    return None;
                }
                Some(SelectItem::Expr {
                    expr: qualified_column(source.label, name, 0),
                    alias: Some(crate::temp_tables::display_name(name).to_owned()),
                })
            })
            .collect();
        let merged: Vec<MergedColumn> = merged.into_iter().map(|(_, column)| column).collect();
        (merged, star, conditions)
    };
    let mut items = Vec::with_capacity(select.items.len());
    for item in std::mem::take(&mut select.items) {
        match item {
            SelectItem::Wildcard {
                qualifier: None,
                position,
            } => items.extend(star.iter().cloned().map(|item| match item {
                SelectItem::Expr { mut expr, alias } => {
                    expr.position = position;
                    SelectItem::Expr { expr, alias }
                }
                other => other,
            })),
            SelectItem::Expr { expr, alias } => {
                let alias = match (&expr.kind, alias) {
                    (_, Some(alias)) => Some(alias),
                    (
                        ExprKind::Column {
                            qualifier: None,
                            name,
                        },
                        None,
                    ) if merged.iter().any(|column| &column.name == name) => Some(name.clone()),
                    (_, None) => None,
                };
                items.push(SelectItem::Expr {
                    expr: with_merged_columns(&expr, &merged)?,
                    alias,
                });
            }
            other => items.push(other),
        }
    }
    select.items = items;
    if let Some(filter) = &select.filter {
        select.filter = Some(with_merged_columns(filter, &merged)?);
    }
    for key in &mut select.group_by {
        *key = with_merged_columns(key, &merged)?;
    }
    if let Some(having) = &select.having {
        select.having = Some(with_merged_columns(having, &merged)?);
    }
    for key in &mut select.distinct_on {
        *key = with_merged_columns(key, &merged)?;
    }
    for item in &mut select.order_by {
        item.expr = with_merged_columns(&item.expr, &merged)?;
    }
    if let Some(from) = &mut select.from {
        for join in &mut from.joins {
            if let JoinCondition::On(on) = &join.condition {
                join.condition = JoinCondition::On(with_merged_columns(on, &merged)?);
            }
        }
        for (index, condition) in conditions {
            if let Some(join) = from.joins.get_mut(index) {
                join.condition = JoinCondition::On(condition);
            }
        }
    }
    Ok(())
}

fn source_at(list: &[Source<'_>], offset: usize) -> Option<usize> {
    list.iter().rposition(|source| source.offset <= offset)
}

impl JoinedFrom<'_> {
    fn first_filtered_level(&self) -> usize {
        self.shapes
            .iter()
            .rposition(|shape| matches!(shape.kind, JoinKind::Right | JoinKind::Full))
            .map_or(0, |index| index + 1)
    }

    fn add_where_keys(&mut self, filter: &Expr) -> SqlResult<()> {
        let lowest = self.first_filtered_level();
        let mut found: Vec<(usize, usize, usize)> = Vec::new();
        let converts_by_precedence =
            crate::constraints::current_dialect() == crate::Dialect::SqlServer;
        {
            let scope = Scope::Many(&self.sources);
            let list = &self.sources.list;
            for conjunct in conjuncts_of(filter) {
                let ExprKind::Binary(BinaryOperator::Equal, left, right) = &conjunct.kind else {
                    continue;
                };
                let offset = |expr: &Expr| match &expr.kind {
                    ExprKind::Column { qualifier, name } => {
                        scope.column(qualifier.as_deref(), name, expr.position).ok()
                    }
                    _ => None,
                };
                let (Some(a), Some(b)) = (offset(left), offset(right)) else {
                    continue;
                };
                if let (Some(left_type), Some(right_type)) =
                    (scope.column_type(a), scope.column_type(b))
                    && !converts_by_precedence
                    && !comparable(left_type, right_type)
                {
                    return Err(no_equality(left_type, right_type, conjunct.position));
                }
                let (Some(source_a), Some(source_b)) = (source_at(list, a), source_at(list, b))
                else {
                    continue;
                };
                let (outer, inner, step) = match source_a.cmp(&source_b) {
                    Ordering::Less => (a, b, source_b),
                    Ordering::Greater => (b, a, source_a),
                    Ordering::Equal => continue,
                };
                if step < lowest || !same_family(&self.sources, outer, inner) {
                    continue;
                }
                found.push((step - 1, outer, inner));
            }
        }
        for (shape_at, outer, inner) in found {
            let Some(shape) = self.shapes.get_mut(shape_at) else {
                continue;
            };
            if matches!(
                shape.kind,
                JoinKind::Comma | JoinKind::Cross | JoinKind::Inner
            ) && !shape.where_keys.contains(&(outer, inner))
            {
                shape.where_keys.push((outer, inner));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinMethod {
    PrimaryKey,
    Index(usize),
    Hash,
    Loop,
}

fn primary_key_lefts(
    table: &TableDefinition,
    offset: usize,
    pairs: &[(usize, usize)],
) -> Option<Vec<usize>> {
    if table.primary_key.is_empty() {
        return None;
    }
    table
        .primary_key
        .iter()
        .map(|&index| {
            pairs
                .iter()
                .find(|&&(_, inner)| inner == offset + index)
                .map(|&(outer, _)| outer)
        })
        .collect()
}

fn index_lefts(
    table: &TableDefinition,
    offset: usize,
    pairs: &[(usize, usize)],
) -> Option<(usize, Vec<usize>)> {
    let mut best: Option<(usize, Vec<usize>)> = None;
    for (position, index) in table.indexes.iter().enumerate() {
        let lefts: Vec<usize> = index
            .columns
            .iter()
            .map_while(|&column| {
                pairs
                    .iter()
                    .find(|&&(_, inner)| inner == offset + column)
                    .map(|&(outer, _)| outer)
            })
            .collect();
        let better = match &best {
            None => !lefts.is_empty(),
            Some((_, known)) => lefts.len() > known.len(),
        };
        if better {
            best = Some((position, lefts));
        }
    }
    best
}

impl JoinedFrom<'_> {
    pub(crate) fn methods(&self) -> Vec<JoinMethod> {
        self.shapes
            .iter()
            .map(|shape| {
                let pairs = equality_pairs(shape, &self.sources).pairs;
                let table = shape.source.table;
                let offset = shape.source.offset;
                if primary_key_lefts(table, offset, &pairs).is_some() {
                    JoinMethod::PrimaryKey
                } else if let Some((position, _)) = index_lefts(table, offset, &pairs) {
                    JoinMethod::Index(position)
                } else if !pairs.is_empty() {
                    JoinMethod::Hash
                } else {
                    JoinMethod::Loop
                }
            })
            .collect()
    }
}

enum Access {
    Lookup(Vec<usize>),
    IndexLookup {
        position: usize,
        lefts: Vec<usize>,
        probe: RefCell<IndexProbe>,
    },
    Hash {
        probe: Vec<usize>,
        index: HashIndex,
    },
    Scan(Vec<Vec<Value>>),
    Preserved {
        rows: Vec<Vec<Value>>,
        probe: Vec<usize>,
        ids: HashMap<Vec<u8>, Vec<usize>>,
        matched: RefCell<Vec<bool>>,
    },
}

struct IndexProbe {
    start: Vec<u8>,
    prefix_length: usize,
    end: Vec<u8>,
    row_key: Vec<u8>,
    expected_entry: Vec<u8>,
}

impl IndexProbe {
    fn new(table_id: u32, index_id: u32) -> Self {
        let start = index_prefix(table_id, index_id);
        Self {
            prefix_length: start.len(),
            start,
            end: Vec::new(),
            row_key: Vec::new(),
            expected_entry: Vec::new(),
        }
    }

    fn restart(&mut self) -> &mut Vec<u8> {
        self.start.truncate(self.prefix_length);
        &mut self.start
    }

    fn close_range(&mut self) {
        write_prefix_end(&mut self.end, &self.start);
    }
}

fn equality_bytes(row: &[Value], offsets: &[usize], key: &mut Vec<u8>) -> bool {
    key.clear();
    for &offset in offsets {
        match row.get(offset) {
            Some(value) if !value.is_null() => equality_key(key, value),
            _ => return false,
        }
    }
    true
}

fn preserved_rows(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    needed: &[bool],
    pairs: &[(usize, usize)],
    offset: usize,
    memory: &mut WorkMemory,
) -> SqlResult<Access> {
    let build: Vec<usize> = pairs.iter().map(|&(_, inner)| inner - offset).collect();
    let mut rows = Vec::new();
    let mut ids: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    let mut key = Vec::new();
    scan_table(reader, table, needed, &mut |row| {
        memory.charge_row(&row)?;
        memory.charge(1)?;
        if !build.is_empty() && equality_bytes(&row, &build, &mut key) {
            memory.charge(key.len() + 40)?;
            match ids.get_mut(key.as_slice()) {
                Some(same_key) => same_key.push(rows.len()),
                None => {
                    ids.insert(key.clone(), vec![rows.len()]);
                }
            }
        }
        rows.push(row);
        Ok(true)
    })?;
    Ok(Access::Preserved {
        matched: RefCell::new(vec![false; rows.len()]),
        rows,
        probe: pairs.iter().map(|&(outer, _)| outer).collect(),
        ids,
    })
}

struct Step<'a> {
    key: RefCell<Vec<u8>>,
    prefix_length: usize,
    offset: usize,
    preserves_left: bool,
    preserves_right: bool,
    table: &'a TableDefinition,
    needed: Vec<bool>,
    on: Option<Expr>,
    using: &'a [(usize, usize)],
    scope: Scope<'a>,
    access: Access,
}

pub(crate) struct JoinedRows<'a> {
    reader: &'a dyn KeyValueRead,
    watch: crate::cancel::Watch,
    first: &'a TableDefinition,
    first_needed: Vec<bool>,
    first_key: Option<Vec<u8>>,
    width: usize,
    steps: Vec<Step<'a>>,
    filters: Vec<Vec<Expr>>,
}

struct KeyPairs {
    pairs: Vec<(usize, usize)>,
    hashed: usize,
}

impl KeyPairs {
    fn hash_keys(&self) -> &[(usize, usize)] {
        self.pairs.get(..self.hashed).unwrap_or(&self.pairs)
    }
}

fn equality_pairs(shape: &StepShape<'_>, all: &Sources<'_>) -> KeyPairs {
    with_where_keys(condition_pairs(shape, all), &shape.where_keys, |pair| {
        integer_pair(all, pair)
    })
}

fn with_where_keys(
    mut pairs: Vec<(usize, usize)>,
    where_keys: &[(usize, usize)],
    is_integer: impl Fn((usize, usize)) -> bool,
) -> KeyPairs {
    let lone_integer_condition = matches!(pairs.as_slice(), [only] if is_integer(*only));
    for &pair in where_keys {
        if !pairs.contains(&pair) {
            pairs.push(pair);
        }
    }
    let hashed = if lone_integer_condition {
        1
    } else {
        pairs.len()
    };
    KeyPairs { pairs, hashed }
}

fn integer_pair(all: &Sources<'_>, (outer, inner): (usize, usize)) -> bool {
    let scope = Scope::Many(all);
    let integer = |offset: usize| scope.column_type(offset).is_some_and(has_integer_keys);
    integer(outer) && integer(inner)
}

fn condition_pairs(shape: &StepShape<'_>, all: &Sources<'_>) -> Vec<(usize, usize)> {
    let scope = Scope::Many(&shape.visible);
    let same_family = |outer: usize, inner: usize| same_family(all, outer, inner);
    let mut pairs: Vec<(usize, usize)> = shape
        .using
        .iter()
        .copied()
        .filter(|&(outer, inner)| same_family(outer, inner))
        .collect();
    let Some(on) = shape.on else {
        return pairs;
    };
    let right = shape.source.offset..shape.source.offset + shape.source.table.columns.len();
    let conjuncts: Vec<&Expr> = match &on.kind {
        ExprKind::And(operands) => operands.iter().collect(),
        _ => vec![on],
    };
    for conjunct in conjuncts {
        let ExprKind::Binary(BinaryOperator::Equal, left_expr, right_expr) = &conjunct.kind else {
            continue;
        };
        let offset = |expr: &Expr| match &expr.kind {
            ExprKind::Column { qualifier, name } => {
                scope.column(qualifier.as_deref(), name, expr.position).ok()
            }
            _ => None,
        };
        let (Some(a), Some(b)) = (offset(left_expr), offset(right_expr)) else {
            continue;
        };
        let (outer, inner) = match (right.contains(&a), right.contains(&b)) {
            (false, true) => (a, b),
            (true, false) => (b, a),
            _ => continue,
        };
        if same_family(outer, inner) && !pairs.contains(&(outer, inner)) {
            pairs.push((outer, inner));
        }
    }
    pairs
}

fn scan_table(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    needed: &[bool],
    visit: &mut dyn FnMut(Vec<Value>) -> SqlResult<bool>,
) -> SqlResult<()> {
    let prefix = rows_prefix(table.id);
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |_, bytes| {
            let mut row = Vec::with_capacity(table.columns.len());
            decode_row_into(bytes, &table.columns, Some(needed), &mut row)?;
            visit(row)
        },
    )
}

fn mark_column(list: &[Source<'_>], needed: &mut [Vec<bool>], qualifier: Option<&str>, name: &str) {
    for (source, columns) in list.iter().zip(needed.iter_mut()) {
        if qualifier.is_some_and(|qualifier| qualifier != source.label) {
            continue;
        }
        match name {
            "*" => columns.iter_mut().for_each(|column| *column = true),
            _ => {
                if let Some(slot) = source
                    .table
                    .column_index(name)
                    .and_then(|index| columns.get_mut(index))
                {
                    *slot = true;
                }
            }
        }
    }
}

fn mark_exprs(list: &[Source<'_>], needed: &mut [Vec<bool>], exprs: &[&Expr]) {
    for expr in exprs {
        if crate::subquery::mentions_subquery(expr) {
            mark_column(list, needed, None, "*");
            continue;
        }
        visit_exprs(expr, &mut |node| {
            if let ExprKind::Column { qualifier, name } = &node.kind {
                mark_column(list, needed, qualifier.as_deref(), name);
            }
        });
    }
}

/// The columns of each source a joined `SELECT` decodes, and the subset read once the join
/// conditions and `WHERE` have been applied: the select list, `GROUP BY`, `HAVING`, `ORDER BY`
/// and the correlated part of `WHERE` evaluated after the join.
pub(crate) struct NeededColumns {
    read: Vec<Vec<bool>>,
    output: Vec<Vec<bool>>,
}

struct SlotMarks {
    marked: Vec<bool>,
    unknown: bool,
}

impl SlotMarks {
    fn new(width: usize) -> Self {
        Self {
            marked: vec![false; width],
            unknown: false,
        }
    }

    fn slot(&mut self, index: usize) {
        match self.marked.get_mut(index) {
            Some(marked) => *marked = true,
            None => self.unknown = true,
        }
    }

    fn expr(&mut self, expr: &Expr) {
        if crate::subquery::mentions_subquery(expr) {
            self.unknown = true;
            return;
        }
        let marked = &mut self.marked;
        let unknown = &mut self.unknown;
        visit_exprs(expr, &mut |node| match node.kind {
            ExprKind::Slot { index, .. } => match marked.get_mut(index) {
                Some(slot) => *slot = true,
                None => *unknown = true,
            },
            ExprKind::Column { .. } => *unknown = true,
            _ => {}
        });
    }

    fn stored_columns(
        &self,
        own: &std::ops::Range<usize>,
        on: Option<&Expr>,
        using: &[(usize, usize)],
    ) -> Option<Vec<usize>> {
        let mut marks = Self {
            marked: self.marked.clone(),
            unknown: self.unknown,
        };
        if let Some(on) = on {
            marks.expr(on);
        }
        for &(outer, inner) in using {
            marks.slot(outer);
            marks.slot(inner);
        }
        if marks.unknown {
            return None;
        }
        Some(
            own.clone()
                .enumerate()
                .filter(|&(_, slot)| marks.marked.get(slot).copied().unwrap_or(true))
                .map(|(column, _)| column)
                .collect(),
        )
    }
}

fn read_after_step(
    joined: &JoinedFrom<'_>,
    index: usize,
    output: &[bool],
    filters: &[Vec<Expr>],
    later_ons: &[Option<Expr>],
) -> SlotMarks {
    let mut marks = SlotMarks::new(Scope::Many(&joined.sources).width());
    let Some(shape) = joined.shapes.get(index) else {
        marks.unknown = true;
        return marks;
    };
    let offset = shape.source.offset;
    let own = offset..offset + shape.source.table.columns.len();
    for (column, &shown) in output.iter().enumerate() {
        if shown {
            marks.slot(offset + column);
        }
    }
    let consumes_own_filters = matches!(
        shape.kind,
        JoinKind::Inner | JoinKind::Comma | JoinKind::Cross
    );
    for (level, level_filters) in filters.iter().enumerate().skip(index + 1) {
        for filter in level_filters {
            if level == index + 1 && consumes_own_filters && touches_only(filter, &own) {
                continue;
            }
            marks.expr(filter);
        }
    }
    for on in later_ons.iter().flatten() {
        marks.expr(on);
    }
    for later in joined.shapes.iter().skip(index + 1) {
        for &(outer, inner) in &later.using {
            marks.slot(outer);
            marks.slot(inner);
        }
    }
    marks
}

fn touches_only(expr: &Expr, own: &std::ops::Range<usize>) -> bool {
    let mut inside = true;
    let mut any = false;
    visit_exprs(expr, &mut |node| {
        if let ExprKind::Slot { index, .. } = node.kind {
            any = true;
            inside &= own.contains(&index);
        }
    });
    any && inside
}

fn shift_slots(expr: &Expr, offset: usize) -> Expr {
    match &expr.kind {
        ExprKind::Slot { index, sql_type } => Expr {
            kind: ExprKind::Slot {
                index: index - offset,
                sql_type: *sql_type,
            },
            position: expr.position,
        },
        _ => rebuild(expr, &mut |child| Ok(shift_slots(child, offset)))
            .unwrap_or_else(|_| expr.clone()),
    }
}

const MOST_EXPECTED_ROWS: usize = 1 << 20;

#[derive(Default)]
pub(crate) struct RowCountHints {
    counts: HashMap<u32, (u64, usize)>,
}

impl RowCountHints {
    fn expected(&self, table: u32, version: Option<u64>, most: Option<usize>) -> usize {
        let Some(version) = version else {
            return 0;
        };
        match self.counts.get(&table) {
            Some(&(seen, rows)) if seen == version => rows
                .min(most.unwrap_or(MOST_EXPECTED_ROWS))
                .min(MOST_EXPECTED_ROWS),
            _ => 0,
        }
    }

    fn record(&mut self, table: u32, version: Option<u64>, rows: usize) {
        if let Some(version) = version {
            if self.counts.len() >= 256 {
                self.counts.clear();
            }
            self.counts.insert(table, (version, rows));
        }
    }
}

const LOOKUP_HASH_LIMIT: usize = 64 << 20;

struct HashBuild<'b> {
    pairs: &'b [(usize, usize)],
    needed: &'b [bool],
    stored: Option<Vec<usize>>,
    keep: &'b [Expr],
    limit: Option<usize>,
}

fn build_hash(
    reader: &dyn KeyValueRead,
    shape: &StepShape<'_>,
    all: &Sources<'_>,
    plan: HashBuild<'_>,
    hints: &mut RowCountHints,
    memory: &mut WorkMemory,
) -> SqlResult<Option<HashIndex>> {
    let HashBuild {
        pairs,
        needed,
        stored,
        keep,
        limit,
    } = plan;
    let integer_keys = matches!(pairs, [only] if integer_pair(all, *only));
    let offset = shape.source.offset;
    let build: Vec<usize> = pairs.iter().map(|&(_, inner)| inner - offset).collect();
    let version = reader.committed_version();
    let table_id = shape.source.table.id;
    let expected = hints.expected(table_id, version, limit.map(|bytes| bytes / 256));
    let mut index = HashIndex::new(
        integer_keys,
        shape.source.table.columns.len(),
        stored,
        expected,
    );
    let mut bounded = limit.map(WorkMemory::with_limit);
    let mut over = false;
    let table = shape.source.table;
    let prefix = rows_prefix(table.id);
    let mut row = Vec::with_capacity(table.columns.len());
    let outcome = reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |_, bytes| {
            row.clear();
            decode_row_into(bytes, &table.columns, Some(needed), &mut row)?;
            let evaluator = Evaluator {
                scope: Scope::empty(),
                row: &row,
                correlation: None,
            };
            for condition in keep {
                if evaluator.boolean(condition, "WHERE")? != Some(true) {
                    return Ok(true);
                }
            }
            match &mut bounded {
                Some(local) => {
                    if index.insert(&mut row, &build, local).is_err() {
                        over = true;
                        return Ok(false);
                    }
                }
                None => index.insert(&mut row, &build, memory)?,
            }
            Ok(true)
        },
    );
    outcome?;
    if over {
        return Ok(None);
    }
    hints.record(table_id, version, index.len());
    if let Some(local) = bounded {
        memory.charge(local.used())?;
    }
    Ok(Some(index))
}

fn same_key_encoding(left: Option<SqlType>, right: SqlType) -> bool {
    match left {
        Some(SqlType::Varchar(_) | SqlType::Text) => right.is_string(),
        Some(left) => left == right,
        None => false,
    }
}

fn conjuncts_of(expr: &Expr) -> Vec<&Expr> {
    match &expr.kind {
        ExprKind::And(operands) => operands.iter().collect(),
        _ => vec![expr],
    }
}

fn is_lookup_equality(expr: &Expr, exact: &[(usize, usize)]) -> bool {
    let ExprKind::Binary(BinaryOperator::Equal, left, right) = &expr.kind else {
        return false;
    };
    let (ExprKind::Slot { index: a, .. }, ExprKind::Slot { index: b, .. }) =
        (&left.kind, &right.kind)
    else {
        return false;
    };
    exact
        .iter()
        .any(|&(outer, inner)| (outer, inner) == (*a, *b) || (outer, inner) == (*b, *a))
}

impl<'a> JoinedRows<'a> {
    pub(crate) fn new(
        reader: &'a dyn KeyValueRead,
        joined: &'a JoinedFrom<'a>,
        first_key: Option<Vec<u8>>,
        needed: NeededColumns,
        mut filters: Vec<Vec<Expr>>,
        hints: &mut RowCountHints,
        memory: &mut WorkMemory,
    ) -> SqlResult<Self> {
        let NeededColumns {
            read: mut needed,
            output,
        } = needed;
        let outer_is_scanned = first_key.is_none();
        let mut steps = Vec::with_capacity(joined.shapes.len());
        let mut bound_ons: Vec<Option<Expr>> = joined
            .shapes
            .iter()
            .map(|shape| {
                shape
                    .on
                    .map(|on| bind(on, Scope::Many(&shape.visible)))
                    .transpose()
            })
            .collect::<SqlResult<_>>()?;
        for (index, shape) in joined.shapes.iter().enumerate() {
            let table = shape.source.table;
            let offset = shape.source.offset;
            let mut step_needed = std::mem::take(&mut needed[index + 1]);
            let scope = Scope::Many(&shape.visible);
            let key_pairs = equality_pairs(shape, &joined.sources);
            let pairs = key_pairs.pairs.as_slice();
            let hash_keys = key_pairs.hash_keys();
            let own = offset..offset + table.columns.len();
            let keep: Vec<Expr> = if matches!(
                shape.kind,
                JoinKind::Inner | JoinKind::Comma | JoinKind::Cross
            ) {
                filters[index + 1]
                    .iter()
                    .filter(|filter| touches_only(filter, &own))
                    .map(|filter| shift_slots(filter, offset))
                    .collect()
            } else {
                Vec::new()
            };
            let lookup = primary_key_lefts(table, offset, pairs);
            let index_lookup = if lookup.is_none() {
                index_lefts(table, offset, pairs)
            } else {
                None
            };
            let read_after = read_after_step(
                joined,
                index,
                output.get(index + 1).map(Vec::as_slice).unwrap_or(&[]),
                &filters,
                bound_ons.get(index + 1..).unwrap_or(&[]),
            );
            let mut on = bound_ons.get_mut(index).and_then(Option::take);
            let mut using = shape.using.as_slice();
            let preserves_right = matches!(shape.kind, JoinKind::Right | JoinKind::Full);
            let access = if preserves_right {
                preserved_rows(reader, table, &step_needed, pairs, offset, memory)?
            } else if let Some(lefts) = lookup {
                let exact: Vec<(usize, usize)> = table
                    .primary_key
                    .iter()
                    .zip(&lefts)
                    .filter(|&(&index, &outer)| {
                        same_key_encoding(scope.column_type(outer), table.columns[index].sql_type)
                    })
                    .map(|(&index, &outer)| (outer, offset + index))
                    .collect();
                if exact.len() == lefts.len() {
                    if on.as_ref().is_some_and(|on| {
                        conjuncts_of(on)
                            .iter()
                            .all(|conjunct| is_lookup_equality(conjunct, &exact))
                    }) {
                        on = None;
                    }
                    if using.iter().all(|pair| exact.contains(pair)) {
                        using = &[];
                    }
                }
                let probe_build: Vec<(usize, usize)> = lefts
                    .iter()
                    .zip(&table.primary_key)
                    .map(|(&outer, &index)| (outer, offset + index))
                    .collect();
                let bounded = if outer_is_scanned {
                    build_hash(
                        reader,
                        shape,
                        &joined.sources,
                        HashBuild {
                            pairs: &probe_build,
                            needed: &step_needed,
                            stored: read_after.stored_columns(&own, on.as_ref(), using),
                            keep: &keep,
                            limit: Some(LOOKUP_HASH_LIMIT),
                        },
                        hints,
                        memory,
                    )?
                } else {
                    None
                };
                if bounded.is_some() && !keep.is_empty() {
                    filters[index + 1].retain(|filter| !touches_only(filter, &own));
                }
                match bounded {
                    Some(index) => Access::Hash {
                        probe: lefts,
                        index,
                    },
                    None => Access::Lookup(lefts),
                }
            } else if let Some((position, lefts)) = index_lookup {
                let bounded = if outer_is_scanned {
                    build_hash(
                        reader,
                        shape,
                        &joined.sources,
                        HashBuild {
                            pairs: hash_keys,
                            needed: &step_needed,
                            stored: read_after.stored_columns(&own, on.as_ref(), using),
                            keep: &keep,
                            limit: Some(LOOKUP_HASH_LIMIT),
                        },
                        hints,
                        memory,
                    )?
                } else {
                    None
                };
                if bounded.is_some() && !keep.is_empty() {
                    filters[index + 1].retain(|filter| !touches_only(filter, &own));
                }
                match bounded {
                    Some(index) => Access::Hash {
                        probe: hash_keys.iter().map(|&(outer, _)| outer).collect(),
                        index,
                    },
                    None => Access::IndexLookup {
                        position,
                        lefts,
                        probe: RefCell::new(IndexProbe::new(
                            table.id,
                            table.indexes[position].id,
                        )),
                    },
                }
            } else if !pairs.is_empty() {
                let built = build_hash(
                    reader,
                    shape,
                    &joined.sources,
                    HashBuild {
                        pairs: hash_keys,
                        needed: &step_needed,
                        stored: read_after.stored_columns(&own, on.as_ref(), using),
                        keep: &keep,
                        limit: None,
                    },
                    hints,
                    memory,
                )?
                .unwrap_or_else(|| HashIndex::new(false, table.columns.len(), None, 0));
                if !keep.is_empty() {
                    filters[index + 1].retain(|filter| !touches_only(filter, &own));
                }
                let index = built;
                Access::Hash {
                    probe: hash_keys.iter().map(|&(outer, _)| outer).collect(),
                    index,
                }
            } else {
                let mut rows = Vec::new();
                scan_table(reader, table, &step_needed, &mut |row| {
                    memory.charge_row(&row)?;
                    rows.push(row);
                    Ok(true)
                })?;
                Access::Scan(rows)
            };
            if let Access::IndexLookup { position, .. } = &access {
                for &column in &table.indexes[*position].columns {
                    if let Some(slot) = step_needed.get_mut(column) {
                        *slot = true;
                    }
                }
            }
            let prefix = rows_prefix(table.id);
            steps.push(Step {
                prefix_length: prefix.len(),
                key: RefCell::new(prefix),
                offset,
                preserves_left: matches!(shape.kind, JoinKind::Left | JoinKind::Full),
                preserves_right,
                table,
                needed: step_needed,
                on,
                using,
                scope,
                access,
            });
        }
        Ok(Self {
            reader,
            watch: crate::cancel::watch(),
            first: joined.sources.list[0].table,
            first_needed: std::mem::take(&mut needed[0]),
            first_key,
            width: Scope::Many(&joined.sources).width(),
            steps,
            filters,
        })
    }

    pub(crate) fn for_each(
        &self,
        visit: &mut dyn FnMut(&[Value]) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        let mut row = Vec::with_capacity(self.width);
        let columns = &self.first.columns;
        if let Some(key) = &self.first_key {
            let mut found = false;
            self.reader.get_with(key, &mut |bytes| {
                decode_row_into(bytes, columns, Some(&self.first_needed), &mut row)?;
                found = true;
                Ok(())
            })?;
            if found {
                self.extend(0, &mut row, visit)?;
            }
            return Ok(());
        }
        let prefix = rows_prefix(self.first.id);
        let mut going = true;
        self.reader.scan(
            &prefix,
            &prefix_end(&prefix),
            ScanDirection::Forward,
            &mut |_, bytes| {
                row.clear();
                decode_row_into(bytes, columns, Some(&self.first_needed), &mut row)?;
                going = self.extend(0, &mut row, visit)?;
                Ok(going)
            },
        )?;
        if going {
            self.unmatched_right_rows(visit)?;
        }
        Ok(())
    }

    fn unmatched_right_rows(
        &self,
        visit: &mut dyn FnMut(&[Value]) -> SqlResult<bool>,
    ) -> SqlResult<()> {
        for (level, step) in self.steps.iter().enumerate() {
            let Access::Preserved { rows, matched, .. } = &step.access else {
                continue;
            };
            if !step.preserves_right {
                continue;
            }
            let unmatched: Vec<usize> = matched
                .try_borrow()
                .map_err(|_| SqlError::new(SqlState::INTERNAL_ERROR, "a join was re-entered"))?
                .iter()
                .enumerate()
                .filter(|(_, matched)| !**matched)
                .map(|(id, _)| id)
                .collect();
            let mut row = Vec::with_capacity(self.width);
            for id in unmatched {
                row.clear();
                row.resize(step.offset, Value::Null);
                row.extend_from_slice(&rows[id]);
                if !self.extend(level + 1, &mut row, visit)? {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn passes(&self, level: usize, row: &[Value]) -> SqlResult<bool> {
        let Some(filters) = self.filters.get(level).filter(|filters| !filters.is_empty()) else {
            return Ok(true);
        };
        let evaluator = Evaluator {
            scope: Scope::empty(),
            row,
            correlation: None,
        };
        for filter in filters {
            if evaluator.boolean(filter, "WHERE")? != Some(true) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn extend(
        &self,
        level: usize,
        row: &mut Vec<Value>,
        visit: &mut dyn FnMut(&[Value]) -> SqlResult<bool>,
    ) -> SqlResult<bool> {
        self.watch.check()?;
        if !self.passes(level, row)? {
            return Ok(true);
        }
        let Some(step) = self.steps.get(level) else {
            return visit(row);
        };
        let base = row.len();
        let mut matched = false;
        let mut attempt = |row: &mut Vec<Value>| -> SqlResult<bool> {
            let keep_going = if step.matches(row)? {
                matched = true;
                self.extend(level + 1, row, visit)?
            } else {
                true
            };
            row.truncate(base);
            Ok(keep_going)
        };
        if let Access::Preserved {
            rows,
            probe,
            ids,
            matched: flags,
        } = &step.access
        {
            let mut found_any = false;
            let mut try_row = |id: usize, row: &mut Vec<Value>| -> SqlResult<bool> {
                let Some(right) = rows.get(id) else {
                    return Ok(true);
                };
                row.extend_from_slice(right);
                let keep_going = if step.matches(row)? {
                    found_any = true;
                    if let Ok(mut flags) = flags.try_borrow_mut()
                        && let Some(flag) = flags.get_mut(id)
                    {
                        *flag = true;
                    }
                    self.extend(level + 1, row, visit)?
                } else {
                    true
                };
                row.truncate(base);
                Ok(keep_going)
            };
            if probe.is_empty() {
                for id in 0..rows.len() {
                    if !try_row(id, row)? {
                        return Ok(false);
                    }
                }
            } else {
                let mut key = step.key.borrow_mut();
                let candidates = if equality_bytes(row, probe, &mut key) {
                    ids.get(key.as_slice())
                } else {
                    None
                };
                drop(key);
                for &id in candidates.into_iter().flatten() {
                    if !try_row(id, row)? {
                        return Ok(false);
                    }
                }
            }
            if !found_any && step.preserves_left {
                row.resize(base + step.table.columns.len(), Value::Null);
                let keep_going = self.extend(level + 1, row, visit)?;
                row.truncate(base);
                return Ok(keep_going);
            }
            return Ok(true);
        }
        match &step.access {
            Access::Lookup(lefts) => {
                if self.lookup(step, lefts, row)? && !attempt(row)? {
                    return Ok(false);
                }
            }
            Access::IndexLookup {
                position,
                lefts,
                probe,
            } => {
                if !self.index_lookup(step, *position, lefts, probe, row, &mut attempt)? {
                    return Ok(false);
                }
            }
            Access::Hash { probe, index } => {
                let mut key = step.key.borrow_mut();
                let mut at = index.first(row, probe, &mut key);
                drop(key);
                while !HashIndex::is_end(at) {
                    index.push_row(at, row);
                    if !attempt(row)? {
                        return Ok(false);
                    }
                    at = index.following(at);
                }
            }
            Access::Scan(rows) => {
                for right in rows {
                    row.extend_from_slice(right);
                    if !attempt(row)? {
                        return Ok(false);
                    }
                }
            }
            Access::Preserved { .. } => {}
        }
        if !matched && step.preserves_left {
            row.resize(base + step.table.columns.len(), Value::Null);
            let keep_going = self.extend(level + 1, row, visit)?;
            row.truncate(base);
            return Ok(keep_going);
        }
        Ok(true)
    }

    fn index_lookup(
        &self,
        step: &Step<'_>,
        position: usize,
        lefts: &[usize],
        probe: &RefCell<IndexProbe>,
        row: &mut Vec<Value>,
        attempt: &mut dyn FnMut(&mut Vec<Value>) -> SqlResult<bool>,
    ) -> SqlResult<bool> {
        let table = step.table;
        let index = &table.indexes[position];
        let mut probe = probe.borrow_mut();
        let start = probe.restart();
        for (&column, &left) in index.columns.iter().zip(lefts) {
            let sql_type = table.columns[column].sql_type;
            let Some(value) = key_value(&row[left], sql_type) else {
                return Ok(true);
            };
            encode_column_key(start, &value, sql_type);
        }
        probe.close_range();
        let IndexProbe {
            start,
            end,
            row_key,
            expected_entry,
            ..
        } = &mut *probe;
        let mut going = true;
        self.reader
            .scan(start, end, ScanDirection::Forward, &mut |entry, _| {
                write_row_key_of_entry(row_key, table, index, entry)?;
                let base = row.len();
                let mut found = false;
                self.reader.get_with(row_key, &mut |bytes| {
                    decode_row_into(bytes, &table.columns, Some(&step.needed), row)?;
                    found = true;
                    Ok(())
                })?;
                let matches = found
                    && row.get(base..).is_some_and(|inner| {
                        write_entry_key(expected_entry, table, index, inner, row_key);
                        expected_entry.as_slice() == entry
                    });
                if !matches {
                    return Err(crate::encoding::corrupt("index entry"));
                }
                going = attempt(row)?;
                Ok(going)
            })?;
        Ok(going)
    }

    fn lookup(&self, step: &Step<'_>, lefts: &[usize], row: &mut Vec<Value>) -> SqlResult<bool> {
        let table = step.table;
        let mut key = step.key.borrow_mut();
        key.truncate(step.prefix_length);
        for (&index, &left) in table.primary_key.iter().zip(lefts) {
            let sql_type = table.columns[index].sql_type;
            let Some(value) = key_value(&row[left], sql_type) else {
                return Ok(false);
            };
            encode_column_key(&mut key, &value, sql_type);
        }
        let mut found = false;
        self.reader.get_with(key.as_slice(), &mut |bytes| {
            decode_row_into(bytes, &table.columns, Some(&step.needed), row)?;
            found = true;
            Ok(())
        })?;
        Ok(found)
    }
}

fn key_value(original: &Value, sql_type: SqlType) -> Option<Cow<'_, Value>> {
    if original.is_null() {
        return None;
    }
    if original.sql_type() == Some(sql_type)
        || (sql_type.is_string() && matches!(original, Value::Text(_)))
    {
        return Some(Cow::Borrowed(original));
    }
    let converted = convert(original.clone(), sql_type, Conversion::Assignment).ok()?;
    (compare_values(&converted, original) == Ordering::Equal).then_some(Cow::Owned(converted))
}

impl Step<'_> {
    fn matches(&self, row: &[Value]) -> SqlResult<bool> {
        for &(left, right) in self.using {
            if compare_truth(&row[left], &row[right], "=")? != Some(true) {
                return Ok(false);
            }
        }
        let Some(on) = &self.on else {
            return Ok(true);
        };
        let evaluator = Evaluator {
            scope: self.scope,
            row,
            correlation: None,
        };
        Ok(evaluator.boolean(on, "JOIN/ON")? == Some(true))
    }
}

fn visit_exprs<'e>(expr: &'e Expr, visit: &mut dyn FnMut(&'e Expr)) {
    visit(expr);
    for child in children(expr) {
        visit_exprs(child, visit);
    }
}

impl JoinedFrom<'_> {
    pub(crate) fn needed_columns(&self, select: &Select, late: Option<&Expr>) -> NeededColumns {
        let list = &self.sources.list;
        let mut needed: Vec<Vec<bool>> = list
            .iter()
            .map(|source| vec![false; source.table.columns.len()])
            .collect();
        let mut output_exprs: Vec<&Expr> = Vec::new();
        for item in &select.items {
            match item {
                SelectItem::Wildcard { qualifier, .. } => {
                    mark_column(list, &mut needed, qualifier.as_deref(), "*");
                }
                SelectItem::Expr { expr, .. } => output_exprs.push(expr),
            }
        }
        output_exprs.extend(select.group_by.iter());
        output_exprs.extend(select.having.iter());
        output_exprs.extend(select.distinct_on.iter());
        output_exprs.extend(select.order_by.iter().map(|item| &item.expr));
        output_exprs.extend(late);
        mark_exprs(list, &mut needed, &output_exprs);
        let output = needed.clone();
        let mut condition_exprs: Vec<&Expr> = select.filter.iter().collect();
        if let Some(from) = &select.from {
            for (join, shape) in from.joins.iter().zip(&self.shapes) {
                match &join.condition {
                    JoinCondition::On(expr) => condition_exprs.push(expr),
                    JoinCondition::Using(names) => {
                        for name in names {
                            mark_column(list, &mut needed, None, &name.text);
                        }
                    }
                    JoinCondition::Natural => {
                        for &(_, right) in &shape.using {
                            if let Some(name) = column_name_at(list, right) {
                                mark_column(list, &mut needed, None, name);
                            }
                        }
                    }
                    JoinCondition::Always => {}
                }
            }
        }
        mark_exprs(list, &mut needed, &condition_exprs);
        NeededColumns {
            read: needed,
            output,
        }
    }

    pub(crate) fn preserves_right(&self) -> bool {
        self.shapes
            .iter()
            .any(|shape| matches!(shape.kind, JoinKind::Right | JoinKind::Full))
    }

    pub(crate) fn filters_by_level(&self, filter: Option<&Expr>) -> Vec<Vec<Expr>> {
        let list = &self.sources.list;
        let mut levels: Vec<Vec<Expr>> = vec![Vec::new(); list.len()];
        let Some(filter) = filter else {
            return levels;
        };
        let lowest = self.first_filtered_level();
        for conjunct in conjuncts_of(filter) {
            let mut level = 0;
            visit_exprs(conjunct, &mut |node| {
                if let ExprKind::Slot { index, .. } = node.kind
                    && let Some(source) = list.iter().rposition(|source| source.offset <= index)
                {
                    level = level.max(source);
                }
            });
            levels[level.max(lowest)].push(conjunct.clone());
        }
        levels
    }
}

#[cfg(test)]
mod tests {
    use super::{IndexProbe, key_value, with_where_keys};
    use crate::encoding::{encode_column_key, index_prefix, prefix_end};
    use crate::types::{SqlType, Value};
    use std::borrow::Cow;

    #[test]
    fn a_reused_index_probe_holds_only_the_latest_value_after_its_prefix() {
        let mut probe = IndexProbe::new(7, 3);
        let mut ranges = Vec::new();
        for value in [Value::BigInt(123_456_789), Value::BigInt(5), Value::BigInt(-1)] {
            encode_column_key(probe.restart(), &value, SqlType::BigInt);
            probe.close_range();
            ranges.push((probe.start.clone(), probe.end.clone()));
        }
        for ((start, end), value) in ranges.iter().zip([123_456_789, 5, -1]) {
            let mut fresh = index_prefix(7, 3);
            encode_column_key(&mut fresh, &Value::BigInt(value), SqlType::BigInt);
            assert_eq!(start, &fresh);
            assert_eq!(end, &prefix_end(&fresh));
        }
    }

    #[test]
    fn a_key_value_of_the_column_type_is_borrowed_and_others_are_converted_or_refused() {
        let same = Value::BigInt(42);
        assert!(matches!(
            key_value(&same, SqlType::BigInt),
            Some(Cow::Borrowed(Value::BigInt(42)))
        ));
        let text = Value::Text("k".into());
        assert!(matches!(
            key_value(&text, SqlType::Text),
            Some(Cow::Borrowed(Value::Text(_)))
        ));
        assert!(matches!(
            key_value(&Value::Integer(42), SqlType::BigInt),
            Some(Cow::Owned(Value::BigInt(42)))
        ));
        assert!(key_value(&Value::Null, SqlType::BigInt).is_none());
        assert!(key_value(&Value::Double(1.5), SqlType::BigInt).is_none());
    }

    #[test]
    fn a_single_integer_condition_pair_keeps_the_integer_hash_without_where_keys() {
        let integer = |pair: (usize, usize)| pair == (0, 3);
        let keys = |conditions: Vec<(usize, usize)>, where_keys: &[(usize, usize)]| {
            let found = with_where_keys(conditions, where_keys, integer);
            (found.pairs.clone(), found.hash_keys().to_vec())
        };
        assert_eq!(
            keys(vec![(0, 3)], &[(1, 4)]),
            (vec![(0, 3), (1, 4)], vec![(0, 3)])
        );
        assert_eq!(
            keys(vec![(1, 4)], &[(0, 3)]),
            (vec![(1, 4), (0, 3)], vec![(1, 4), (0, 3)])
        );
        assert_eq!(
            keys(vec![(0, 3), (1, 4)], &[(2, 5), (1, 4)]),
            (vec![(0, 3), (1, 4), (2, 5)], vec![(0, 3), (1, 4), (2, 5)])
        );
        assert_eq!(
            keys(Vec::new(), &[(1, 4), (0, 3)]),
            (vec![(1, 4), (0, 3)], vec![(1, 4), (0, 3)])
        );
    }
}
