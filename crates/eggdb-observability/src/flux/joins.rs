//! Flux's `join` package (D1064): `join.inner`, `join.left`, `join.right`, `join.full` and
//! `join.time`. Each pairs the rows of two streams for which `on: (l, r) => ...` is true and
//! builds each output row with `as: (l, r) => ({...})`; the outer joins also keep the unmatched
//! rows of one side or both, with the other side's columns null. When `on` is a conjunction of
//! equalities between a left and a right column, rows are paired through a hash of those
//! columns; any other `on` compares every pair, within a bound.

use super::{Argument, Call, Expr, Flux, MOST_ROWS, Row, Stream, Table, evaluate, regroup, value_of};
use eggdb_sql::Value;
use std::collections::BTreeMap;

const MOST_COMPARISONS: usize = 50_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Inner,
    Left,
    Right,
    Full,
}

struct Pairing<'c> {
    left: &'c str,
    right: &'c str,
    on: Expr,
    output: &'c Expr,
}

fn stream_argument<'s>(call: &Call, name: &str, streams: &'s BTreeMap<String, Stream>) -> Result<&'s Stream, String> {
    match call.argument(name) {
        Some(Argument::Name(variable)) => streams.get(variable).ok_or_else(|| format!("undefined identifier {variable}")),
        _ => Err(format!("{}() needs {name}: a stream named by a variable", call.name)),
    }
}

fn equalities<'e>(on: &'e Expr, left: &str, right: &str, found: &mut Vec<(&'e str, &'e str)>) -> bool {
    match on {
        Expr::And(first, second) => equalities(first, left, right, found) && equalities(second, left, right, found),
        Expr::Compare(first, "==", second) => match (first.as_ref(), second.as_ref()) {
            (Expr::Column(a), Expr::Column(b)) => {
                let side = |column: &'e str, owner: &str| column.strip_prefix(owner).and_then(|rest| rest.strip_prefix('.'));
                match (side(a, left), side(b, right), side(a, right), side(b, left)) {
                    (Some(l), Some(r), _, _) | (_, _, Some(r), Some(l)) => {
                        found.push((l, r));
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        },
        _ => false,
    }
}

fn merged(pairing: &Pairing<'_>, left: Option<&Row>, right: Option<&Row>) -> Row {
    let mut row = Row::new();
    for (owner, side) in [(pairing.left, left), (pairing.right, right)] {
        for (column, value) in side.into_iter().flatten() {
            row.insert(format!("{owner}.{column}"), value.clone());
        }
    }
    row
}

fn output_row(pairing: &Pairing<'_>, left: Option<&Row>, right: Option<&Row>, columns: &[String]) -> Result<Row, String> {
    let row = merged(pairing, left, right);
    let Expr::Record { base, fields } = pairing.output else {
        return Err("as: must build a record, (l, r) => ({...})".to_owned());
    };
    let mut out = match base.as_deref() {
        Some(owner) if owner == pairing.left => left.cloned().unwrap_or_else(|| columns.iter().map(|column| (column.clone(), Value::Null)).collect()),
        Some(owner) if owner == pairing.right => right.cloned().unwrap_or_default(),
        Some(owner) => return Err(format!("{owner} is neither side of the join")),
        None => Row::new(),
    };
    for (name, expr) in fields {
        out.insert(name.clone(), value_of(evaluate(expr, &row)?));
    }
    Ok(out)
}

fn rows_of(stream: &Stream) -> Vec<&Row> {
    stream.tables.iter().flat_map(|table| table.rows.iter()).collect()
}

fn key_of(row: &Row, columns: &[&str]) -> Option<Vec<String>> {
    columns.iter().map(|column| row.get(*column).filter(|value| !matches!(value, Value::Null)).and_then(Value::to_text)).collect()
}

fn pair_rows(pairing: &Pairing<'_>, left: &[&Row], right: &[&Row]) -> Result<Vec<(Option<usize>, Option<usize>)>, String> {
    let mut keys = Vec::new();
    let mut pairs = Vec::new();
    if equalities(&pairing.on, pairing.left, pairing.right, &mut keys) && !keys.is_empty() {
        let (left_columns, right_columns): (Vec<&str>, Vec<&str>) = keys.into_iter().unzip();
        let mut by_key: BTreeMap<Vec<String>, Vec<usize>> = BTreeMap::new();
        for (at, row) in right.iter().enumerate() {
            if let Some(key) = key_of(row, &right_columns) {
                by_key.entry(key).or_default().push(at);
            }
        }
        for (left_at, row) in left.iter().enumerate() {
            for right_at in key_of(row, &left_columns).and_then(|key| by_key.get(&key)).map(Vec::as_slice).unwrap_or_default() {
                pairs.push((Some(left_at), Some(*right_at)));
            }
        }
        return Ok(pairs);
    }
    if left.len().saturating_mul(right.len()) > MOST_COMPARISONS {
        return Err(format!("this join's on: compares more than {MOST_COMPARISONS} pairs of rows; compare columns with == instead"));
    }
    for (left_at, left_row) in left.iter().enumerate() {
        for (right_at, right_row) in right.iter().enumerate() {
            if evaluate(&pairing.on, &merged(pairing, Some(left_row), Some(right_row)))? == Flux::Boolean(true) {
                pairs.push((Some(left_at), Some(right_at)));
            }
        }
    }
    Ok(pairs)
}

fn columns_of(rows: &[&Row]) -> Vec<String> {
    let mut columns: Vec<String> = rows.iter().flat_map(|row| row.keys().cloned()).collect();
    columns.sort();
    columns.dedup();
    columns
}

/// Runs one of the `join` package's functions on the streams it names.
pub(super) fn joined(call: &Call, streams: &BTreeMap<String, Stream>) -> Result<Stream, String> {
    let left_stream = stream_argument(call, "left", streams)?;
    let right_stream = stream_argument(call, "right", streams)?;
    let method = match call.name.as_str() {
        "join.inner" => Method::Inner,
        "join.left" => Method::Left,
        "join.right" => Method::Right,
        "join.full" => Method::Full,
        "join.time" => match call.argument("method") {
            None => Method::Inner,
            Some(Argument::Text(method)) if method == "inner" => Method::Inner,
            Some(Argument::Text(method)) if method == "left" => Method::Left,
            Some(Argument::Text(method)) if method == "right" => Method::Right,
            Some(Argument::Text(method)) if method == "full" => Method::Full,
            Some(other) => return Err(format!("join.time() takes method: inner, left, right or full, not {other:?}")),
        },
        other => return Err(format!("function {other}() is not supported yet")),
    };
    let Some(Argument::PairFunction { left, right, body: output }) = call.argument("as") else {
        return Err(format!("{}() needs as: (l, r) => ({{...}})", call.name));
    };
    let on = if call.name == "join.time" {
        Expr::Compare(
            Box::new(Expr::Column(format!("{left}._time"))),
            "==",
            Box::new(Expr::Column(format!("{right}._time"))),
        )
    } else {
        match call.argument("on") {
            Some(Argument::PairFunction { left: on_left, right: on_right, body }) if on_left == left && on_right == right => body.clone(),
            Some(Argument::PairFunction { .. }) => return Err("on: and as: must name the left and right rows the same way".to_owned()),
            _ => return Err(format!("{}() needs on: (l, r) => ...", call.name)),
        }
    };
    let pairing = Pairing { left, right, on, output };
    let (left_rows, right_rows) = (rows_of(left_stream), rows_of(right_stream));
    let left_columns = columns_of(&left_rows);
    let mut pairs = pair_rows(&pairing, &left_rows, &right_rows)?;
    let mut left_matched = vec![false; left_rows.len()];
    let mut right_matched = vec![false; right_rows.len()];
    for (left_at, right_at) in &pairs {
        if let Some(at) = left_at {
            left_matched[*at] = true;
        }
        if let Some(at) = right_at {
            right_matched[*at] = true;
        }
    }
    if matches!(method, Method::Left | Method::Full) {
        pairs.extend(left_matched.iter().enumerate().filter(|(_, found)| !**found).map(|(at, _)| (Some(at), None)));
    }
    if matches!(method, Method::Right | Method::Full) {
        pairs.extend(right_matched.iter().enumerate().filter(|(_, found)| !**found).map(|(at, _)| (None, Some(at))));
    }
    if pairs.len() > MOST_ROWS {
        return Err(format!("join makes more than {MOST_ROWS} rows"));
    }
    let mut rows = Vec::with_capacity(pairs.len());
    for (left_at, right_at) in pairs {
        rows.push(output_row(&pairing, left_at.map(|at| left_rows[at]), right_at.map(|at| right_rows[at]), &left_columns)?);
    }
    let key = left_stream.tables.first().map(|table| table.key.clone()).unwrap_or_default();
    let key: Vec<String> = key.into_iter().filter(|column| rows.iter().all(|row| row.contains_key(column))).collect();
    let mut tables = regroup(vec![Table { key: Vec::new(), rows }], key);
    tables.retain(|table| !table.rows.is_empty());
    Ok(Stream {
        tables,
        start: left_stream.start.min(right_stream.start),
        stop: left_stream.stop.max(right_stream.stop),
    })
}
