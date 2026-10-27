use crate::ast::{BinaryOperator, Expr, ExprKind, Literal};
use crate::encoding::{
    Column, NULL_IN_KEY, TableDefinition, decode_row, decode_row_into, encode_key_value,
    index_prefix, prefix_end, rows_prefix,
};
use crate::error::SqlResult;
use crate::eval::{Evaluator, Scope, check_columns};
use crate::indexes::row_key_of_entry;
use crate::store::{KeyValueRead, ScanDirection};
use crate::types::{Conversion, SqlType, Value, compare_values, convert};
use std::cmp::Ordering;

const MOST_RANGES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyChoice {
    Primary,
    Index(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Path {
    Scan,
    Point(Vec<u8>),
    Ranges {
        key: KeyChoice,
        ranges: Vec<(Vec<u8>, Vec<u8>)>,
        backward: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Use {
    Scan,
    Point,
    Equal,
    Range,
    Order,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Access {
    pub(crate) path: Path,
    pub(crate) ordered: bool,
    pub(crate) used: Use,
    pub(crate) conditions: Vec<(usize, &'static str)>,
}

impl Access {
    pub(crate) fn scan() -> Self {
        Self {
            path: Path::Scan,
            ordered: false,
            used: Use::Scan,
            conditions: Vec::new(),
        }
    }

    pub(crate) fn point(key: Vec<u8>) -> Self {
        Self {
            path: Path::Point(key),
            ordered: true,
            used: Use::Point,
            conditions: Vec::new(),
        }
    }

    pub(crate) fn key(&self) -> Option<KeyChoice> {
        match &self.path {
            Path::Scan => None,
            Path::Point(_) => Some(KeyChoice::Primary),
            Path::Ranges { key, .. } => Some(*key),
        }
    }

    pub(crate) fn backward(&self) -> bool {
        matches!(self.path, Path::Ranges { backward: true, .. })
    }
}

pub(crate) struct OrderRequest {
    pub(crate) columns: Vec<(usize, bool)>,
    pub(crate) limited: bool,
}

#[derive(Debug, Clone)]
enum Bound {
    Equal(Vec<Value>),
    Lower(Value, bool),
    Upper(Value, bool),
    Prefix(String),
}

struct Predicate {
    column: usize,
    bound: Bound,
}

fn conjuncts<'e>(expr: &'e Expr, out: &mut Vec<&'e Expr>) {
    match &expr.kind {
        ExprKind::And(operands) => {
            for operand in operands {
                conjuncts(operand, out);
            }
        }
        _ => out.push(expr),
    }
}

fn column_of(expr: &Expr, scope: Scope<'_>) -> Option<usize> {
    match &expr.kind {
        ExprKind::Column { qualifier, name } => {
            scope.column(qualifier.as_deref(), name, expr.position).ok()
        }
        _ => None,
    }
}

fn same_family(value: &Value, sql_type: SqlType) -> bool {
    match value {
        Value::Text(_) => sql_type.is_string(),
        Value::Boolean(_) => sql_type == SqlType::Boolean,
        Value::Null => false,
        _ => sql_type.is_numeric(),
    }
}

fn constant(expr: &Expr, column: &Column) -> Option<Value> {
    if check_columns(expr, Scope::empty()).is_err() {
        return None;
    }
    let evaluator = Evaluator {
        scope: Scope::empty(),
        row: &[],
    };
    let original = evaluator.eval(expr, Some(column.sql_type)).ok()?;
    if !same_family(&original, column.sql_type) {
        return None;
    }
    let converted = convert(original.clone(), column.sql_type, Conversion::Assignment).ok()?;
    if converted.is_null() || compare_values(&converted, &original) != Ordering::Equal {
        return None;
    }
    Some(converted)
}

fn mirrored(operator: BinaryOperator) -> BinaryOperator {
    match operator {
        BinaryOperator::Less => BinaryOperator::Greater,
        BinaryOperator::LessOrEqual => BinaryOperator::GreaterOrEqual,
        BinaryOperator::Greater => BinaryOperator::Less,
        BinaryOperator::GreaterOrEqual => BinaryOperator::LessOrEqual,
        other => other,
    }
}

fn comparison(operator: BinaryOperator, value: Value) -> Option<Bound> {
    Some(match operator {
        BinaryOperator::Equal => Bound::Equal(vec![value]),
        BinaryOperator::Less => Bound::Upper(value, false),
        BinaryOperator::LessOrEqual => Bound::Upper(value, true),
        BinaryOperator::Greater => Bound::Lower(value, false),
        BinaryOperator::GreaterOrEqual => Bound::Lower(value, true),
        _ => return None,
    })
}

fn like_prefix(pattern: &str) -> Option<String> {
    if pattern.contains('\\') {
        return None;
    }
    let prefix: String = pattern
        .chars()
        .take_while(|&character| character != '%' && character != '_')
        .collect();
    (!prefix.is_empty()).then_some(prefix)
}

fn predicates(expr: &Expr, table: &TableDefinition, scope: Scope<'_>, out: &mut Vec<Predicate>) {
    let found = |column: usize, bound: Bound| Predicate { column, bound };
    match &expr.kind {
        ExprKind::Binary(operator, left, right) => {
            let (column, value, operator) = match (column_of(left, scope), column_of(right, scope))
            {
                (Some(column), None) => (column, right, *operator),
                (None, Some(column)) => (column, left, mirrored(*operator)),
                _ => return,
            };
            let Some(value) = constant(value, &table.columns[column]) else {
                return;
            };
            if let Some(bound) = comparison(operator, value) {
                out.push(found(column, bound));
            }
        }
        ExprKind::Between {
            expr,
            low,
            high,
            negated: false,
        } => {
            let Some(column) = column_of(expr, scope) else {
                return;
            };
            let definition = &table.columns[column];
            if let Some(low) = constant(low, definition) {
                out.push(found(column, Bound::Lower(low, true)));
            }
            if let Some(high) = constant(high, definition) {
                out.push(found(column, Bound::Upper(high, true)));
            }
        }
        ExprKind::InList {
            expr,
            list,
            negated: false,
        } => {
            let Some(column) = column_of(expr, scope) else {
                return;
            };
            if list.len() > MOST_RANGES {
                return;
            }
            let mut values = Vec::with_capacity(list.len());
            for item in list {
                match constant(item, &table.columns[column]) {
                    Some(value) => values.push(value),
                    None if matches!(item.kind, ExprKind::Literal(Literal::Null)) => {}
                    None => return,
                }
            }
            if !values.is_empty() {
                out.push(found(column, Bound::Equal(values)));
            }
        }
        ExprKind::IsNull {
            expr,
            negated: false,
        } => {
            if let Some(column) = column_of(expr, scope) {
                out.push(found(column, Bound::Equal(vec![Value::Null])));
            }
        }
        ExprKind::Like {
            expr,
            pattern,
            negated: false,
            case_insensitive: false,
        } => {
            let Some(column) = column_of(expr, scope) else {
                return;
            };
            if !table.columns[column].sql_type.is_string() {
                return;
            }
            if let Some(Value::Text(pattern)) = constant(pattern, &table.columns[column])
                && let Some(prefix) = like_prefix(&pattern)
            {
                out.push(found(column, Bound::Prefix(prefix)));
            }
        }
        _ => {}
    }
}

struct Candidate {
    key: KeyChoice,
    equal: usize,
    ranged: bool,
    single: bool,
    point: bool,
    width: usize,
    ranges: Vec<(Vec<u8>, Vec<u8>)>,
    conditions: Vec<(usize, &'static str)>,
    order_columns: Vec<usize>,
}

impl Candidate {
    fn score(&self) -> (bool, usize, bool, bool, std::cmp::Reverse<usize>) {
        (
            self.point,
            self.equal,
            self.ranged,
            self.key == KeyChoice::Primary,
            std::cmp::Reverse(self.width),
        )
    }
}

fn encoded(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_key_value(&mut out, value);
    out
}

fn with(prefix: &[u8], tail: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + tail.len());
    key.extend_from_slice(prefix);
    key.extend_from_slice(tail);
    key
}

fn candidate(
    key: KeyChoice,
    prefix: Vec<u8>,
    columns: &[usize],
    unique: bool,
    order_columns: Vec<usize>,
    found: &[Predicate],
) -> Option<Candidate> {
    let mut points = vec![prefix];
    let mut equal = 0;
    let mut single = true;
    let mut has_null = false;
    let mut conditions = Vec::new();
    for &column in columns {
        let Some(values) = found.iter().find_map(|predicate| match &predicate.bound {
            Bound::Equal(values) if predicate.column == column => Some(values),
            _ => None,
        }) else {
            break;
        };
        let mut keys: Vec<Vec<u8>> = values.iter().map(encoded).collect();
        keys.sort();
        keys.dedup();
        if points.len().saturating_mul(keys.len()) > MOST_RANGES {
            break;
        }
        has_null |= values.iter().any(Value::is_null);
        single &= keys.len() == 1;
        points = points
            .iter()
            .flat_map(|point| keys.iter().map(move |key| with(point, key)))
            .collect();
        conditions.push((column, "="));
        equal += 1;
    }
    let mut lower = None;
    let mut upper = None;
    let mut like = None;
    if let Some(&column) = columns.get(equal) {
        for predicate in found.iter().filter(|predicate| predicate.column == column) {
            match &predicate.bound {
                Bound::Lower(value, inclusive) if lower.is_none() => {
                    lower = Some((encoded(value), *inclusive));
                    conditions.push((column, if *inclusive { ">=" } else { ">" }));
                }
                Bound::Upper(value, inclusive) if upper.is_none() => {
                    upper = Some((encoded(value), *inclusive));
                    conditions.push((column, if *inclusive { "<=" } else { "<" }));
                }
                Bound::Prefix(text) if like.is_none() => like = Some(text.clone()),
                _ => {}
            }
        }
        if lower.is_none()
            && upper.is_none()
            && let Some(text) = &like
        {
            let mut start = encoded(&Value::Text(text.clone()));
            start.truncate(start.len().saturating_sub(2));
            lower = Some((start.clone(), true));
            upper = Some((prefix_end(&start), false));
            conditions.push((column, ">="));
            conditions.push((column, "<"));
        }
    }
    let ranged = lower.is_some() || upper.is_some();
    if equal == 0 && !ranged {
        return None;
    }
    let mut ranges = Vec::with_capacity(points.len());
    for point in &points {
        let start = match &lower {
            Some((value, true)) => with(point, value),
            Some((value, false)) => prefix_end(&with(point, value)),
            None => point.clone(),
        };
        let end = match &upper {
            Some((value, true)) => prefix_end(&with(point, value)),
            Some((value, false)) => with(point, value),
            None if ranged => with(point, &[NULL_IN_KEY]),
            None => prefix_end(point),
        };
        if start < end {
            ranges.push((start, end));
        }
    }
    ranges.sort();
    Some(Candidate {
        key,
        point: unique && !has_null && single && equal == columns.len(),
        equal,
        ranged,
        single,
        width: columns.len(),
        ranges,
        conditions,
        order_columns,
    })
}

fn order_direction(
    columns: &[usize],
    equal: usize,
    single: bool,
    order: &OrderRequest,
) -> Option<bool> {
    let bound = &columns[..equal.min(columns.len())];
    let remaining: Vec<(usize, bool)> = order
        .columns
        .iter()
        .copied()
        .filter(|(column, _)| !bound.contains(column))
        .collect();
    let Some(&(_, descending)) = remaining.first() else {
        return Some(false);
    };
    if !single && equal > 0 {
        return None;
    }
    let rest = columns.get(equal..)?;
    if remaining.len() > rest.len() {
        return None;
    }
    for ((column, direction), &key_column) in remaining.iter().zip(rest) {
        if *column != key_column || *direction != descending {
            return None;
        }
    }
    Some(descending)
}

fn key_columns(table: &TableDefinition, key: KeyChoice) -> Vec<usize> {
    match key {
        KeyChoice::Primary => table.primary_key.clone(),
        KeyChoice::Index(position) => {
            let mut columns = table.indexes[position].columns.clone();
            columns.extend(table.primary_key.iter().copied());
            columns
        }
    }
}

fn key_prefix(table: &TableDefinition, key: KeyChoice) -> Vec<u8> {
    match key {
        KeyChoice::Primary => rows_prefix(table.id),
        KeyChoice::Index(position) => index_prefix(table.id, table.indexes[position].id),
    }
}

pub(crate) fn plan(
    table: &TableDefinition,
    scope: Scope<'_>,
    filter: Option<&Expr>,
    order: Option<&OrderRequest>,
) -> Access {
    let mut found = Vec::new();
    if let Some(filter) = filter {
        let mut parts = Vec::new();
        conjuncts(filter, &mut parts);
        for part in parts {
            predicates(part, table, scope, &mut found);
        }
    }
    let mut keys = Vec::new();
    if !table.primary_key.is_empty() {
        keys.push((KeyChoice::Primary, table.primary_key.clone(), true));
    }
    for (position, index) in table.indexes.iter().enumerate() {
        keys.push((
            KeyChoice::Index(position),
            index.columns.clone(),
            index.unique,
        ));
    }
    let best = keys
        .iter()
        .filter_map(|(key, columns, unique)| {
            candidate(
                *key,
                key_prefix(table, *key),
                columns,
                *unique,
                key_columns(table, *key),
                &found,
            )
        })
        .max_by(|left, right| left.score().cmp(&right.score()));
    if let Some(best) = best {
        let direction = order
            .and_then(|order| order_direction(&best.order_columns, best.equal, best.single, order));
        let used = if best.point {
            Use::Point
        } else if best.ranged {
            Use::Range
        } else {
            Use::Equal
        };
        let mut ranges = best.ranges;
        if direction == Some(true) {
            ranges.reverse();
        }
        return Access {
            path: Path::Ranges {
                key: best.key,
                ranges,
                backward: direction == Some(true),
            },
            ordered: direction.is_some(),
            used,
            conditions: best.conditions,
        };
    }
    let Some(order) = order else {
        return Access::scan();
    };
    for (key, _, _) in &keys {
        let columns = key_columns(table, *key);
        let Some(backward) = order_direction(&columns, 0, true, order) else {
            continue;
        };
        if *key != KeyChoice::Primary && !order.limited {
            continue;
        }
        let prefix = key_prefix(table, *key);
        let end = prefix_end(&prefix);
        return Access {
            path: Path::Ranges {
                key: *key,
                ranges: vec![(prefix, end)],
                backward,
            },
            ordered: true,
            used: Use::Order,
            conditions: Vec::new(),
        };
    }
    Access::scan()
}

pub(crate) type KeyedRowVisitor<'v> = dyn FnMut(&[u8], Vec<Value>) -> SqlResult<bool> + 'v;

pub(crate) fn read_ranges(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    key: KeyChoice,
    ranges: &[(Vec<u8>, Vec<u8>)],
    backward: bool,
    visit: &mut KeyedRowVisitor<'_>,
) -> SqlResult<()> {
    let direction = if backward {
        ScanDirection::Backward
    } else {
        ScanDirection::Forward
    };
    let mut going = true;
    for (start, end) in ranges {
        if !going {
            break;
        }
        match key {
            KeyChoice::Primary => reader.scan(start, end, direction, &mut |row_key, bytes| {
                going = visit(row_key, decode_row(bytes, &table.columns)?)?;
                Ok(going)
            })?,
            KeyChoice::Index(position) => {
                let index = &table.indexes[position];
                reader.scan(start, end, direction, &mut |entry, _| {
                    let row_key = row_key_of_entry(table, index, entry)?;
                    let mut row = Vec::with_capacity(table.columns.len());
                    let found = reader.get_with(&row_key, &mut |bytes| {
                        decode_row_into(bytes, &table.columns, None, &mut row)
                    })?;
                    if !found {
                        return Err(crate::encoding::corrupt("index entry"));
                    }
                    going = visit(&row_key, row)?;
                    Ok(going)
                })?
            }
        }
    }
    Ok(())
}
