use crate::cypher::{
    Clause, Direction, Expr, Imports, LabelExpression, NodePattern, Operator, Pattern, Projecting,
    Query, RelationshipPattern, SetItem, Shortest, Subquery,
};
use crate::store::{GraphRead, GraphWrite};
use crate::value::{Node, Path, Relationship, Value, equals, order};
use crate::{GraphError, GraphResult, QueryResult};
use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

const MAX_ROWS: usize = 1 << 22;
const MAX_SHORTEST_PATH_STEPS: usize = 1 << 20;
const MAX_SHORTEST_PATH_RELATIONSHIPS_RETURNED: usize = 1 << 20;
const AGGREGATES: [&str; 6] = ["count", "sum", "avg", "min", "max", "collect"];

type Row = Vec<(String, Value)>;
type Change = Box<dyn Fn(&mut Vec<String>, &mut Vec<(String, Value)>)>;

fn lookup<'r>(row: &'r Row, name: &str) -> Option<&'r Value> {
    row.iter()
        .rev()
        .find(|(known, _)| known == name)
        .map(|(_, value)| value)
}

fn distinct_key<'v>(values: impl Iterator<Item = &'v Value>) -> String {
    let mut key = String::new();
    for value in values {
        push_distinct_key(&mut key, value);
        key.push('\u{1f}');
    }
    key
}

fn push_distinct_key(key: &mut String, value: &Value) {
    match value {
        Value::Float(number) if *number == 0.0 => key.push_str("Float(0.0)"),
        Value::List(items) => {
            key.push('[');
            for item in items {
                push_distinct_key(key, item);
                key.push(',');
            }
            key.push(']');
        }
        Value::Map(entries) => {
            key.push('{');
            for (name, item) in entries {
                let _ = write!(key, "{name:?}:");
                push_distinct_key(key, item);
                key.push(',');
            }
            key.push('}');
        }
        other => {
            let _ = write!(key, "{other:?}");
        }
    }
}

fn values_held(value: &Value) -> usize {
    let mut count = 1usize;
    let mut containers = vec![value];
    while let Some(container) = containers.pop() {
        let (items, entries): (&[Value], &[(String, Value)]) = match container {
            Value::List(items) => (items, &[]),
            Value::Map(entries) => (&[], entries),
            _ => continue,
        };
        for child in items.iter().chain(entries.iter().map(|(_, item)| item)) {
            count = count.saturating_add(1);
            if matches!(child, Value::List(_) | Value::Map(_)) {
                containers.push(child);
            }
        }
    }
    count
}

fn push_comprehension_output(
    out: &mut Vec<Value>,
    held: &mut usize,
    value: Value,
) -> GraphResult<()> {
    *held = held.saturating_add(values_held(&value));
    if *held > MAX_ROWS {
        return Err(out_of_memory(format!(
            "a list or pattern comprehension builds more than {MAX_ROWS} values"
        )));
    }
    out.push(value);
    Ok(())
}

fn bind_path(row: &mut Row, name: &str, path: Value) -> GraphResult<()> {
    if lookup(row, name).is_some() {
        return Err(syntax_error(format!("Variable `{name}` already declared")));
    }
    row.push((name.to_owned(), path));
    Ok(())
}

fn bind(row: &mut Row, name: &str, value: Value) {
    match row.iter_mut().find(|(known, _)| known == name) {
        Some((_, slot)) => *slot = value,
        None => row.push((name.to_owned(), value)),
    }
}

#[derive(Clone)]
struct Walk {
    origin: u64,
    used: Vec<u64>,
    relationships: Vec<Relationship>,
}

struct Reached {
    node: u64,
    distance: u32,
    before: Vec<(usize, u64)>,
}

fn out_of_memory(message: impl Into<String>) -> GraphError {
    GraphError::new(
        "Neo.TransientError.General.MemoryPoolOutOfMemoryError",
        message,
    )
}

fn syntax_error(message: impl Into<String>) -> GraphError {
    GraphError::new("Neo.ClientError.Statement.SyntaxError", message)
}

fn semantic_error(message: impl Into<String>) -> GraphError {
    GraphError::new("Neo.ClientError.Statement.SemanticError", message)
}

fn literal_text(value: &Value) -> String {
    match value {
        Value::String(text) => format!("'{text}'"),
        other => display(other),
    }
}

fn pattern_element_text(names: String, properties: &[(String, Value)]) -> String {
    let mut text = names;
    if !properties.is_empty() {
        if !text.is_empty() {
            text.push(' ');
        }
        let entries: Vec<String> = properties
            .iter()
            .map(|(name, value)| format!("{name}: {}", literal_text(value)))
            .collect();
        let _ = write!(text, "{{{}}}", entries.join(", "));
    }
    text
}

fn null_property(properties: &[(String, Value)]) -> Option<&str> {
    properties
        .iter()
        .find(|(_, value)| matches!(value, Value::Null))
        .map(|(name, _)| name.as_str())
}

fn imported_row(imports: &Imports, row: &Row) -> GraphResult<Row> {
    match imports {
        Imports::Everything => Ok(row.clone()),
        Imports::Variables(names) => names
            .iter()
            .map(|name| {
                lookup(row, name)
                    .map(|value| (name.clone(), value.clone()))
                    .ok_or_else(|| syntax_error(format!("Variable `{name}` not defined")))
            })
            .collect(),
    }
}

fn walks_back(
    reached: &[Reached],
    end: usize,
    all: bool,
    returned: &Cell<usize>,
) -> GraphResult<Vec<Vec<u64>>> {
    let mut walks = Vec::new();
    let mut stack: Vec<(usize, Vec<u64>)> = vec![(end, Vec::new())];
    while let Some((at, tail)) = stack.pop() {
        let Some(step) = reached.get(at) else {
            continue;
        };
        if step.before.is_empty() {
            let mut walk = tail;
            walk.reverse();
            let total = returned.get().saturating_add(walk.len().max(1));
            returned.set(total);
            if total > MAX_SHORTEST_PATH_RELATIONSHIPS_RETURNED {
                return Err(out_of_memory(format!(
                    "the query's shortest paths hold more than {MAX_SHORTEST_PATH_RELATIONSHIPS_RETURNED} relationships together; bound them with fewer hops, fewer start nodes or a more selective pattern"
                )));
            }
            walks.push(walk);
            continue;
        }
        let choices = if all { step.before.len() } else { 1 };
        for (previous, relationship) in step.before.iter().take(choices) {
            let mut longer = tail.clone();
            longer.push(*relationship);
            stack.push((*previous, longer));
        }
    }
    Ok(walks)
}

fn type_error(message: impl Into<String>) -> GraphError {
    GraphError::new("Neo.ClientError.Statement.TypeError", message)
}

fn arithmetic_error(message: impl Into<String>) -> GraphError {
    GraphError::new("Neo.ClientError.Statement.ArithmeticError", message)
}

pub(crate) enum Access<'g> {
    Read(GraphRead<'g>),
    Write(GraphWrite<'g>),
}

pub(crate) struct Executor<'g, 'p> {
    pub(crate) access: Access<'g>,
    pub(crate) parameters: &'p [(String, Value)],
    pub(crate) foreach_runs: usize,
    pub(crate) shortest_path_steps: Cell<usize>,
    pub(crate) shortest_path_relationships_returned: Cell<usize>,
}

fn is_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Call { name, .. } if AGGREGATES.contains(&name.as_str()) => true,
        Expr::Call { arguments, .. } => arguments.iter().any(is_aggregate),
        Expr::Binary(_, left, right) => is_aggregate(left) || is_aggregate(right),
        Expr::Not(inner)
        | Expr::Negate(inner)
        | Expr::IsNull(inner, _)
        | Expr::Property(inner, _)
        | Expr::LabelPredicate(inner, _) => is_aggregate(inner),
        Expr::List(items) => items.iter().any(is_aggregate),
        _ => false,
    }
}

fn number_result(
    left: &Value,
    right: &Value,
    integer: fn(i64, i64) -> Option<i64>,
    float: fn(f64, f64) -> f64,
) -> GraphResult<Value> {
    match (left, right) {
        (Value::Integer(a), Value::Integer(b)) => integer(*a, *b)
            .map(Value::Integer)
            .ok_or_else(|| arithmetic_error("integer overflow")),
        (a, b) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => Ok(Value::Float(float(a, b))),
            _ => Err(type_error(format!(
                "cannot combine {} and {}",
                a.type_name(),
                b.type_name()
            ))),
        },
    }
}

fn three_valued(value: &Value) -> GraphResult<Option<bool>> {
    match value {
        Value::Null => Ok(None),
        Value::Boolean(flag) => Ok(Some(*flag)),
        other => Err(type_error(format!(
            "expected a boolean, not {}",
            other.type_name()
        ))),
    }
}

fn as_count(value: &Value, what: &str) -> GraphResult<usize> {
    match value {
        Value::Integer(number) if *number >= 0 => Ok(*number as usize),
        _ => Err(GraphError::new(
            "Neo.ClientError.Statement.ArgumentError",
            format!("{what} must be a non-negative integer"),
        )),
    }
}

impl<'g> Executor<'g, '_> {
    fn read(&self) -> GraphRead<'_> {
        match &self.access {
            Access::Read(read) => GraphRead { keys: read.keys },
            Access::Write(write) => write.read(),
        }
    }

    fn writer(&mut self) -> GraphResult<&mut GraphWrite<'g>> {
        match &mut self.access {
            Access::Write(write) => Ok(write),
            Access::Read(_) => Err(GraphError::new(
                "Neo.ClientError.Statement.AccessMode",
                "Writing in read access mode not allowed",
            )),
        }
    }

    fn evaluate(&self, expr: &Expr, row: &Row) -> GraphResult<Value> {
        Ok(match expr {
            Expr::Literal(value) => value.clone(),
            Expr::Parameter(name) => self
                .parameters
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, value)| value.clone())
                .ok_or_else(|| {
                    GraphError::new(
                        "Neo.ClientError.Statement.ParameterMissing",
                        format!("Expected parameter(s): {name}"),
                    )
                })?,
            Expr::Variable(name) => lookup(row, name).cloned().ok_or_else(|| {
                GraphError::new(
                    "Neo.ClientError.Statement.SyntaxError",
                    format!("Variable `{name}` not defined"),
                )
            })?,
            Expr::Property(target, property) => match self.evaluate(target, row)? {
                Value::Node(node) => node
                    .properties
                    .iter()
                    .find(|(name, _)| name == property)
                    .map_or(Value::Null, |(_, value)| value.clone()),
                Value::Relationship(relationship) => relationship
                    .properties
                    .iter()
                    .find(|(name, _)| name == property)
                    .map_or(Value::Null, |(_, value)| value.clone()),
                Value::Map(entries) => entries
                    .iter()
                    .find(|(name, _)| name == property)
                    .map_or(Value::Null, |(_, value)| value.clone()),
                Value::Null => Value::Null,
                other => {
                    return Err(type_error(format!(
                        "Type mismatch: expected a map, node or relationship but was {}",
                        other.type_name()
                    )));
                }
            },
            Expr::Index(target, index) => {
                match (self.evaluate(target, row)?, self.evaluate(index, row)?) {
                    (Value::List(items), Value::Integer(at)) => {
                        let at = if at < 0 { items.len() as i64 + at } else { at };
                        usize::try_from(at)
                            .ok()
                            .and_then(|at| items.get(at).cloned())
                            .unwrap_or(Value::Null)
                    }
                    (Value::Map(entries), Value::String(key)) => entries
                        .into_iter()
                        .find(|(name, _)| *name == key)
                        .map_or(Value::Null, |(_, value)| value),
                    (Value::Null, _) | (_, Value::Null) => Value::Null,
                    _ => return Err(type_error("cannot index this value")),
                }
            }
            Expr::List(items) => Value::List(
                items
                    .iter()
                    .map(|item| self.evaluate(item, row))
                    .collect::<GraphResult<_>>()?,
            ),
            Expr::Map(entries) => Value::Map(
                entries
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), self.evaluate(value, row)?)))
                    .collect::<GraphResult<_>>()?,
            ),
            Expr::Not(inner) => match three_valued(&self.evaluate(inner, row)?)? {
                Some(flag) => Value::Boolean(!flag),
                None => Value::Null,
            },
            Expr::Negate(inner) => match self.evaluate(inner, row)? {
                Value::Integer(number) => Value::Integer(
                    number
                        .checked_neg()
                        .ok_or_else(|| arithmetic_error("integer overflow"))?,
                ),
                Value::Float(number) => Value::Float(-number),
                Value::Null => Value::Null,
                other => return Err(type_error(format!("cannot negate {}", other.type_name()))),
            },
            Expr::IsNull(inner, negated) => {
                Value::Boolean(matches!(self.evaluate(inner, row)?, Value::Null) != *negated)
            }
            Expr::Binary(operator, left, right) => self.binary(*operator, left, right, row)?,
            Expr::Case {
                subject,
                arms,
                otherwise,
            } => {
                let subject = subject
                    .as_ref()
                    .map(|subject| self.evaluate(subject, row))
                    .transpose()?;
                for (when, then) in arms {
                    let candidate = self.evaluate(when, row)?;
                    let chosen = match &subject {
                        Some(subject) => equals(subject, &candidate) == Some(true),
                        None => candidate.is_true(),
                    };
                    if chosen {
                        return self.evaluate(then, row);
                    }
                }
                match otherwise {
                    Some(otherwise) => self.evaluate(otherwise, row)?,
                    None => Value::Null,
                }
            }
            Expr::Call {
                name,
                arguments,
                star,
                ..
            } => {
                if *star || AGGREGATES.contains(&name.as_str()) {
                    return Err(GraphError::new(
                        "Neo.ClientError.Statement.SyntaxError",
                        format!("Invalid use of aggregating function {name}(...) in this context"),
                    ));
                }
                let values: Vec<Value> = arguments
                    .iter()
                    .map(|argument| self.evaluate(argument, row))
                    .collect::<GraphResult<_>>()?;
                self.function(name, values)?
            }
            Expr::ListComprehension {
                variable,
                list,
                filter,
                map,
            } => {
                let items = match self.evaluate(list, row)? {
                    Value::List(items) => items,
                    Value::Null => return Ok(Value::Null),
                    other => {
                        return Err(type_error(format!(
                            "Type mismatch: expected a list but was {}",
                            other.type_name()
                        )));
                    }
                };
                let mut scope = row.clone();
                let mut out = Vec::new();
                let mut held = 0;
                for item in items {
                    bind(&mut scope, variable, item.clone());
                    if let Some(filter) = filter
                        && !self.evaluate(filter, &scope)?.is_true()
                    {
                        continue;
                    }
                    let value = match map {
                        Some(map) => self.evaluate(map, &scope)?,
                        None => item,
                    };
                    push_comprehension_output(&mut out, &mut held, value)?;
                }
                Value::List(out)
            }
            Expr::PatternComprehension {
                pattern,
                filter,
                map,
            } => {
                let mut out = Vec::new();
                let mut held = 0;
                for matched in self.match_pattern(row, pattern)? {
                    if let Some(filter) = filter
                        && !self.evaluate(filter, &matched)?.is_true()
                    {
                        continue;
                    }
                    let value = self.evaluate(map, &matched)?;
                    push_comprehension_output(&mut out, &mut held, value)?;
                }
                Value::List(out)
            }
            Expr::LabelPredicate(target, labels) => match self.evaluate(target, row)? {
                Value::Node(node) => Value::Boolean(labels.matches(&node.labels)),
                Value::Relationship(relationship) => {
                    Value::Boolean(labels.matches(std::slice::from_ref(&relationship.kind)))
                }
                Value::Null => Value::Null,
                other => {
                    return Err(type_error(format!(
                        "Type mismatch: expected Node or Relationship but was {}",
                        other.type_name()
                    )));
                }
            },
            Expr::ShortestPath(pattern) => self.shortest_path_value(pattern, row)?,
        })
    }

    fn shortest_path_value(&self, pattern: &Pattern, row: &Row) -> GraphResult<Value> {
        let (Some(shortest), Some((_, end))) = (pattern.shortest, pattern.steps.first()) else {
            return Err(syntax_error(
                "shortestPath(...) requires a pattern containing a single relationship",
            ));
        };
        for node in [&pattern.start, end] {
            match self.bound_node(node, row)? {
                Some(Some(_)) => {}
                Some(None) => return Ok(Value::Null),
                None => {
                    return Err(semantic_error(
                        "shortestPath(...) used as an expression needs both of its end nodes bound to variables",
                    ));
                }
            }
        }
        let mut paths = Vec::new();
        for (_, origin, relationships) in self.shortest_walks(row, pattern, shortest, true)? {
            paths.push(self.path_value(origin, &relationships)?);
        }
        Ok(match shortest {
            Shortest::One => paths.into_iter().next().unwrap_or(Value::Null),
            Shortest::All => Value::List(paths),
        })
    }

    fn binary(
        &self,
        operator: Operator,
        left: &Expr,
        right: &Expr,
        row: &Row,
    ) -> GraphResult<Value> {
        if matches!(operator, Operator::And | Operator::Or | Operator::Xor) {
            let a = three_valued(&self.evaluate(left, row)?)?;
            let b = three_valued(&self.evaluate(right, row)?)?;
            return Ok(match (operator, a, b) {
                (Operator::And, Some(false), _) | (Operator::And, _, Some(false)) => {
                    Value::Boolean(false)
                }
                (Operator::And, Some(true), Some(true)) => Value::Boolean(true),
                (Operator::Or, Some(true), _) | (Operator::Or, _, Some(true)) => {
                    Value::Boolean(true)
                }
                (Operator::Or, Some(false), Some(false)) => Value::Boolean(false),
                (Operator::Xor, Some(a), Some(b)) => Value::Boolean(a != b),
                _ => Value::Null,
            });
        }
        let (a, b) = (self.evaluate(left, row)?, self.evaluate(right, row)?);
        if matches!(a, Value::Null) || matches!(b, Value::Null) {
            if operator == Operator::In
                && let Value::List(items) = &b
                && items.is_empty()
            {
                return Ok(Value::Boolean(false));
            }
            return Ok(Value::Null);
        }
        Ok(match operator {
            Operator::Equal => Value::Boolean(equals(&a, &b) == Some(true)),
            Operator::NotEqual => Value::Boolean(equals(&a, &b) == Some(false)),
            Operator::Less
            | Operator::LessOrEqual
            | Operator::Greater
            | Operator::GreaterOrEqual => {
                let comparable = a.kind_rank() == b.kind_rank()
                    && matches!(
                        a,
                        Value::Integer(_) | Value::Float(_) | Value::String(_) | Value::Boolean(_)
                    );
                if !comparable {
                    return Ok(Value::Null);
                }
                let ordering = order(&a, &b);
                Value::Boolean(match operator {
                    Operator::Less => ordering == Ordering::Less,
                    Operator::LessOrEqual => ordering != Ordering::Greater,
                    Operator::Greater => ordering == Ordering::Greater,
                    _ => ordering != Ordering::Less,
                })
            }
            Operator::Add => match (&a, &b) {
                (Value::String(x), y) => Value::String(format!("{x}{}", display(y))),
                (x, Value::String(y)) => Value::String(format!("{}{y}", display(x))),
                (Value::List(x), Value::List(y)) => Value::List([x.clone(), y.clone()].concat()),
                (Value::List(x), y) => Value::List([x.clone(), vec![y.clone()]].concat()),
                _ => number_result(&a, &b, i64::checked_add, |x, y| x + y)?,
            },
            Operator::Subtract => number_result(&a, &b, i64::checked_sub, |x, y| x - y)?,
            Operator::Multiply => number_result(&a, &b, i64::checked_mul, |x, y| x * y)?,
            Operator::Divide => {
                if matches!(b, Value::Integer(0)) {
                    return Err(arithmetic_error("/ by zero"));
                }
                number_result(&a, &b, i64::checked_div, |x, y| x / y)?
            }
            Operator::Modulo => {
                if matches!(b, Value::Integer(0)) {
                    return Err(arithmetic_error("/ by zero"));
                }
                number_result(&a, &b, i64::checked_rem, |x, y| x % y)?
            }
            Operator::Power => match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => Value::Float(x.powf(y)),
                _ => return Err(type_error("^ needs numbers")),
            },
            Operator::In => match b {
                Value::List(items) => {
                    let mut unknown = false;
                    for item in &items {
                        match equals(&a, item) {
                            Some(true) => return Ok(Value::Boolean(true)),
                            None => unknown = true,
                            Some(false) => {}
                        }
                    }
                    if unknown {
                        Value::Null
                    } else {
                        Value::Boolean(false)
                    }
                }
                _ => return Err(type_error("IN needs a list")),
            },
            Operator::StartsWith | Operator::EndsWith | Operator::Contains => match (&a, &b) {
                (Value::String(text), Value::String(part)) => Value::Boolean(match operator {
                    Operator::StartsWith => text.starts_with(part.as_str()),
                    Operator::EndsWith => text.ends_with(part.as_str()),
                    _ => text.contains(part.as_str()),
                }),
                _ => Value::Null,
            },
            Operator::And | Operator::Or | Operator::Xor => Value::Null,
        })
    }

    fn function(&self, name: &str, values: Vec<Value>) -> GraphResult<Value> {
        let argument_error = || {
            GraphError::new(
                "Neo.ClientError.Statement.ArgumentError",
                format!("wrong arguments for {name}()"),
            )
        };
        Ok(match (name, values.as_slice()) {
            ("coalesce", items) => items
                .iter()
                .find(|value| !matches!(value, Value::Null))
                .cloned()
                .unwrap_or(Value::Null),
            (_, [Value::Null, ..]) if name != "range" => Value::Null,
            ("id", [Value::Node(node)]) => Value::Integer(node.id as i64),
            ("id", [Value::Relationship(relationship)]) => Value::Integer(relationship.id as i64),
            ("labels", [Value::Node(node)]) => {
                Value::List(node.labels.iter().cloned().map(Value::String).collect())
            }
            ("type", [Value::Relationship(relationship)]) => {
                Value::String(relationship.kind.clone())
            }
            ("properties", [Value::Node(node)]) => Value::Map(node.properties.clone()),
            ("properties", [Value::Relationship(relationship)]) => {
                Value::Map(relationship.properties.clone())
            }
            ("properties", [Value::Map(entries)]) => Value::Map(entries.clone()),
            ("keys", [Value::Node(node)]) => Value::List(
                node.properties
                    .iter()
                    .map(|(name, _)| Value::String(name.clone()))
                    .collect(),
            ),
            ("keys", [Value::Relationship(relationship)]) => Value::List(
                relationship
                    .properties
                    .iter()
                    .map(|(name, _)| Value::String(name.clone()))
                    .collect(),
            ),
            ("keys", [Value::Map(entries)]) => Value::List(
                entries
                    .iter()
                    .map(|(name, _)| Value::String(name.clone()))
                    .collect(),
            ),
            ("startnode", [Value::Relationship(relationship)]) => self
                .read()
                .node(relationship.start)?
                .map_or(Value::Null, |node| Value::Node(Box::new(node))),
            ("endnode", [Value::Relationship(relationship)]) => self
                .read()
                .node(relationship.end)?
                .map_or(Value::Null, |node| Value::Node(Box::new(node))),
            ("size" | "length", [Value::List(items)]) => Value::Integer(items.len() as i64),
            ("length", [Value::Path(path)]) => Value::Integer(path.relationships.len() as i64),
            ("nodes", [Value::Path(path)]) => Value::List(
                path.nodes
                    .iter()
                    .cloned()
                    .map(|node| Value::Node(Box::new(node)))
                    .collect(),
            ),
            ("relationships", [Value::Path(path)]) => Value::List(
                path.relationships
                    .iter()
                    .cloned()
                    .map(|relationship| Value::Relationship(Box::new(relationship)))
                    .collect(),
            ),
            ("size", [Value::String(text)]) => Value::Integer(text.chars().count() as i64),
            ("head", [Value::List(items)]) => items.first().cloned().unwrap_or(Value::Null),
            ("last", [Value::List(items)]) => items.last().cloned().unwrap_or(Value::Null),
            ("tail", [Value::List(items)]) => Value::List(items.iter().skip(1).cloned().collect()),
            ("reverse", [Value::List(items)]) => Value::List(items.iter().rev().cloned().collect()),
            ("reverse", [Value::String(text)]) => Value::String(text.chars().rev().collect()),
            ("toupper", [Value::String(text)]) => Value::String(text.to_uppercase()),
            ("tolower", [Value::String(text)]) => Value::String(text.to_lowercase()),
            ("trim", [Value::String(text)]) => Value::String(text.trim().to_owned()),
            ("tostring", [value]) => Value::String(display(value)),
            ("tointeger", [Value::Integer(number)]) => Value::Integer(*number),
            ("tointeger", [Value::Float(number)]) if number.is_finite() => {
                Value::Integer(number.trunc() as i64)
            }
            ("tointeger", [Value::String(text)]) => text
                .trim()
                .parse::<i64>()
                .map_or(Value::Null, Value::Integer),
            ("tofloat", [value]) => match value {
                Value::String(text) => text.trim().parse::<f64>().map_or(Value::Null, Value::Float),
                other => other.as_f64().map_or(Value::Null, Value::Float),
            },
            ("abs", [Value::Integer(number)]) => Value::Integer(
                number
                    .checked_abs()
                    .ok_or_else(|| arithmetic_error("integer overflow"))?,
            ),
            ("abs", [Value::Float(number)]) => Value::Float(number.abs()),
            ("range", [start, end]) | ("range", [start, end, _]) => {
                let (Value::Integer(start), Value::Integer(end)) = (start, end) else {
                    return Err(argument_error());
                };
                let step = match values.get(2) {
                    Some(Value::Integer(step)) if *step != 0 => *step,
                    None => 1,
                    _ => return Err(argument_error()),
                };
                let count = if (step > 0 && end >= start) || (step < 0 && end <= start) {
                    let span = (i128::from(*end) - i128::from(*start)) / i128::from(step) + 1;
                    u64::try_from(span).unwrap_or(u64::MAX)
                } else {
                    0
                };
                if count > MAX_ROWS as u64 {
                    return Err(argument_error());
                }
                Value::List(
                    (0..count as i64)
                        .map(|at| Value::Integer(start + at * step))
                        .collect(),
                )
            }
            _ => {
                return Err(GraphError::new(
                    "Neo.ClientError.Statement.SyntaxError",
                    format!("Unknown function '{name}' or wrong arguments"),
                ));
            }
        })
    }

    fn node_matches(&self, node: &Node, pattern: &NodePattern, row: &Row) -> GraphResult<bool> {
        if !pattern.labels.matches(&node.labels) {
            return Ok(false);
        }
        for (name, expected) in &pattern.properties {
            let expected = self.evaluate(expected, row)?;
            let actual = node
                .properties
                .iter()
                .find(|(key, _)| key == name)
                .map_or(Value::Null, |(_, value)| value.clone());
            if equals(&actual, &expected) != Some(true) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn relationship_matches(
        &self,
        relationship: &Relationship,
        pattern: &RelationshipPattern,
        row: &Row,
    ) -> GraphResult<bool> {
        if !pattern.kinds.is_empty() && !pattern.kinds.contains(&relationship.kind) {
            return Ok(false);
        }
        for (name, expected) in &pattern.properties {
            let expected = self.evaluate(expected, row)?;
            let actual = relationship
                .properties
                .iter()
                .find(|(key, _)| key == name)
                .map_or(Value::Null, |(_, value)| value.clone());
            if equals(&actual, &expected) != Some(true) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn bound_node(&self, pattern: &NodePattern, row: &Row) -> GraphResult<Option<Option<Node>>> {
        let Some(name) = &pattern.variable else {
            return Ok(None);
        };
        Ok(match lookup(row, name) {
            Some(Value::Node(node)) => Some(Some((**node).clone())),
            Some(Value::Null) => Some(None),
            Some(other) => {
                return Err(type_error(format!(
                    "Variable `{name}` already declared as {}",
                    other.type_name()
                )));
            }
            None => None,
        })
    }

    fn steps_from(
        &self,
        from: u64,
        pattern: &RelationshipPattern,
        row: &Row,
        used: &[u64],
    ) -> GraphResult<Vec<(Relationship, u64)>> {
        let read = self.read();
        let mut ids = Vec::new();
        if matches!(pattern.direction, Direction::Outgoing | Direction::Either) {
            ids.extend(read.touching(from, true)?.into_iter().map(|id| (id, true)));
        }
        if matches!(pattern.direction, Direction::Incoming | Direction::Either) {
            ids.extend(
                read.touching(from, false)?
                    .into_iter()
                    .map(|id| (id, false)),
            );
        }
        let mut out = Vec::new();
        for (id, outgoing) in ids {
            if used.contains(&id)
                || (pattern.direction == Direction::Either
                    && !outgoing
                    && out
                        .iter()
                        .any(|(known, _): &(Relationship, u64)| known.id == id))
            {
                continue;
            }
            let Some(relationship) = read.relationship(id)? else {
                continue;
            };
            if !self.relationship_matches(&relationship, pattern, row)? {
                continue;
            }
            let next = if outgoing {
                relationship.end
            } else {
                relationship.start
            };
            out.push((relationship, next));
        }
        Ok(out)
    }

    fn extend(
        &self,
        row: Row,
        pattern: &Pattern,
        step: usize,
        at: u64,
        walk: Walk,
        out: &mut Vec<Row>,
    ) -> GraphResult<()> {
        if out.len() > MAX_ROWS {
            return Err(out_of_memory("the query matches too many rows"));
        }
        let Some((relationship_pattern, node_pattern)) = pattern.steps.get(step) else {
            let mut row = row;
            if let Some(name) = &pattern.path {
                let path = self.path_value(walk.origin, &walk.relationships)?;
                bind_path(&mut row, name, path)?;
            }
            out.push(row);
            return Ok(());
        };
        let bound_relationship = relationship_pattern
            .variable
            .as_ref()
            .and_then(|name| lookup(&row, name).cloned());
        let bound_node = self.bound_node(node_pattern, &row)?;
        let (min, max) = relationship_pattern.hops.unwrap_or((1, 1));
        let mut frontier: Vec<(u64, Vec<Relationship>)> = vec![(at, Vec::new())];
        let mut reached: Vec<(u64, Vec<Relationship>)> = Vec::new();
        if min == 0 {
            reached.push((at, Vec::new()));
        }
        for hop in 1..=max {
            let mut next_frontier = Vec::new();
            for (node, path) in &frontier {
                let mut taken = walk.used.clone();
                taken.extend(path.iter().map(|relationship| relationship.id));
                for (relationship, next) in
                    self.steps_from(*node, relationship_pattern, &row, &taken)?
                {
                    let mut extended = path.clone();
                    extended.push(relationship);
                    if hop >= min {
                        reached.push((next, extended.clone()));
                    }
                    next_frontier.push((next, extended));
                }
            }
            if next_frontier.len() > MAX_ROWS {
                return Err(GraphError::new(
                    "Neo.TransientError.General.MemoryPoolOutOfMemoryError",
                    "the variable-length pattern reaches too many paths",
                ));
            }
            frontier = next_frontier;
            if frontier.is_empty() {
                break;
            }
        }
        for (node_id, path) in reached {
            let Some(node) = self.read().node(node_id)? else {
                continue;
            };
            match &bound_node {
                Some(Some(bound)) if bound.id != node.id => continue,
                Some(None) => continue,
                _ => {}
            }
            if !self.node_matches(&node, node_pattern, &row)? {
                continue;
            }
            let relationship_value = if relationship_pattern.hops.is_some() {
                Value::List(
                    path.iter()
                        .cloned()
                        .map(|relationship| Value::Relationship(Box::new(relationship)))
                        .collect(),
                )
            } else {
                match path.first() {
                    Some(relationship) => Value::Relationship(Box::new(relationship.clone())),
                    None => continue,
                }
            };
            if let Some(bound) = &bound_relationship
                && equals(bound, &relationship_value) != Some(true)
            {
                continue;
            }
            let mut next_row = row.clone();
            if let Some(name) = &relationship_pattern.variable {
                bind(&mut next_row, name, relationship_value);
            }
            let node_value = Value::Node(Box::new(node.clone()));
            if let Some(name) = &node_pattern.variable {
                bind(&mut next_row, name, node_value);
            }
            let mut next_walk = walk.clone();
            next_walk
                .used
                .extend(path.iter().map(|relationship| relationship.id));
            if pattern.path.is_some() {
                next_walk.relationships.extend(path);
            }
            self.extend(next_row, pattern, step + 1, node.id, next_walk, out)?;
        }
        Ok(())
    }

    fn start_nodes(&self, pattern: &NodePattern, row: &Row) -> GraphResult<Vec<Node>> {
        Ok(match self.bound_node(pattern, row)? {
            Some(Some(node)) => vec![node],
            Some(None) => Vec::new(),
            None => {
                let read = self.read();
                let mut nodes = Vec::new();
                for id in read.node_ids()? {
                    if let Some(node) = read.node(id)? {
                        nodes.push(node);
                    }
                }
                nodes
            }
        })
    }

    fn existing_node(&self, id: u64) -> GraphResult<Node> {
        self.read().node(id)?.ok_or_else(|| {
            GraphError::new(
                "Neo.DatabaseError.General.UnknownError",
                format!("node {id} on the path vanished"),
            )
        })
    }

    fn existing_relationship(&self, id: u64) -> GraphResult<Relationship> {
        self.read().relationship(id)?.ok_or_else(|| {
            GraphError::new(
                "Neo.DatabaseError.General.UnknownError",
                format!("relationship {id} on the path vanished"),
            )
        })
    }

    fn path_value(&self, origin: u64, relationships: &[Relationship]) -> GraphResult<Value> {
        let mut nodes = Vec::with_capacity(relationships.len() + 1);
        let mut at = origin;
        nodes.push(self.existing_node(at)?);
        for relationship in relationships {
            at = if relationship.start == at {
                relationship.end
            } else {
                relationship.start
            };
            nodes.push(self.existing_node(at)?);
        }
        Ok(Value::Path(Box::new(Path {
            nodes,
            relationships: relationships.to_vec(),
        })))
    }

    fn match_pattern(&self, row: &Row, pattern: &Pattern) -> GraphResult<Vec<Row>> {
        if let Some(shortest) = pattern.shortest {
            return self.match_shortest(row, pattern, shortest);
        }
        let mut out = Vec::new();
        for node in self.start_nodes(&pattern.start, row)? {
            if !self.node_matches(&node, &pattern.start, row)? {
                continue;
            }
            let mut start_row = row.clone();
            if let Some(name) = &pattern.start.variable {
                bind(&mut start_row, name, Value::Node(Box::new(node.clone())));
            }
            let walk = Walk {
                origin: node.id,
                used: Vec::new(),
                relationships: Vec::new(),
            };
            self.extend(start_row, pattern, 0, node.id, walk, &mut out)?;
        }
        Ok(out)
    }

    fn breadth_first(
        &self,
        origin: u64,
        pattern: &RelationshipPattern,
        row: &Row,
        max: u32,
        target: Option<u64>,
        all: bool,
    ) -> GraphResult<Vec<Reached>> {
        let mut reached = vec![Reached {
            node: origin,
            distance: 0,
            before: Vec::new(),
        }];
        let mut index: HashMap<u64, usize> = HashMap::from([(origin, 0)]);
        let mut frontier = vec![0usize];
        for hop in 1..=max {
            let mut next_frontier = Vec::new();
            for &at in &frontier {
                let Some(from) = reached.get(at).map(|step| step.node) else {
                    continue;
                };
                for (relationship, next) in self.steps_from(from, pattern, row, &[])? {
                    let steps = self.shortest_path_steps.get() + 1;
                    self.shortest_path_steps.set(steps);
                    if steps > MAX_SHORTEST_PATH_STEPS {
                        return Err(out_of_memory(format!(
                            "the query's shortest path searches expand more than {MAX_SHORTEST_PATH_STEPS} relationships; bound them with fewer hops, fewer start nodes or a more selective pattern"
                        )));
                    }
                    match index.get(&next) {
                        None => {
                            index.insert(next, reached.len());
                            next_frontier.push(reached.len());
                            reached.push(Reached {
                                node: next,
                                distance: hop,
                                before: vec![(at, relationship.id)],
                            });
                        }
                        Some(&known) if all => {
                            if let Some(step) = reached.get_mut(known)
                                && step.distance == hop
                            {
                                step.before.push((at, relationship.id));
                            }
                        }
                        Some(_) => {}
                    }
                }
            }
            frontier = next_frontier;
            let target_reached =
                target.is_some_and(|target| target != origin && index.contains_key(&target));
            if frontier.is_empty() || target_reached {
                break;
            }
        }
        Ok(reached)
    }

    fn match_shortest(
        &self,
        row: &Row,
        pattern: &Pattern,
        shortest: Shortest,
    ) -> GraphResult<Vec<Row>> {
        let mut out = Vec::new();
        for (mut matched, origin, relationships) in
            self.shortest_walks(row, pattern, shortest, pattern.path.is_some())?
        {
            if let Some(name) = &pattern.path {
                let path = self.path_value(origin, &relationships)?;
                bind_path(&mut matched, name, path)?;
            }
            out.push(matched);
        }
        Ok(out)
    }

    fn shortest_walks(
        &self,
        row: &Row,
        pattern: &Pattern,
        shortest: Shortest,
        keep_relationships: bool,
    ) -> GraphResult<Vec<(Row, u64, Vec<Relationship>)>> {
        let Some((relationship_pattern, end_pattern)) = pattern.steps.first() else {
            return Ok(Vec::new());
        };
        let target = match self.bound_node(end_pattern, row)? {
            Some(Some(node)) => Some(node.id),
            Some(None) => return Ok(Vec::new()),
            None => None,
        };
        let (min, max) = relationship_pattern.hops.unwrap_or((1, 1));
        let all = shortest == Shortest::All;
        let mut out = Vec::new();
        for start in self.start_nodes(&pattern.start, row)? {
            if !self.node_matches(&start, &pattern.start, row)? {
                continue;
            }
            let mut start_row = row.clone();
            if let Some(name) = &pattern.start.variable {
                bind(&mut start_row, name, Value::Node(Box::new(start.clone())));
            }
            let reached =
                self.breadth_first(start.id, relationship_pattern, &start_row, max, target, all)?;
            for (end, step) in reached.iter().enumerate() {
                if step.distance < min || target.is_some_and(|target| target != step.node) {
                    continue;
                }
                let end_node = self.existing_node(step.node)?;
                if !self.node_matches(&end_node, end_pattern, &start_row)? {
                    continue;
                }
                for walk in walks_back(
                    &reached,
                    end,
                    all,
                    &self.shortest_path_relationships_returned,
                )? {
                    let relationships = walk
                        .iter()
                        .map(|id| self.existing_relationship(*id))
                        .collect::<GraphResult<Vec<_>>>()?;
                    let mut next_row = start_row.clone();
                    if let Some(name) = &relationship_pattern.variable {
                        let value = if relationship_pattern.hops.is_some() {
                            Value::List(
                                relationships
                                    .iter()
                                    .cloned()
                                    .map(|relationship| Value::Relationship(Box::new(relationship)))
                                    .collect(),
                            )
                        } else {
                            match relationships.first() {
                                Some(relationship) => {
                                    Value::Relationship(Box::new(relationship.clone()))
                                }
                                None => Value::Null,
                            }
                        };
                        bind(&mut next_row, name, value);
                    }
                    if let Some(name) = &end_pattern.variable {
                        bind(&mut next_row, name, Value::Node(Box::new(end_node.clone())));
                    }
                    let kept = if keep_relationships {
                        relationships
                    } else {
                        Vec::new()
                    };
                    out.push((next_row, start.id, kept));
                    if out.len() > MAX_ROWS {
                        return Err(out_of_memory("the query matches too many rows"));
                    }
                }
            }
        }
        Ok(out)
    }

    fn variables_of(patterns: &[Pattern]) -> Vec<String> {
        let mut names = Vec::new();
        for pattern in patterns {
            names.extend(pattern.path.clone());
            names.extend(pattern.start.variable.clone());
            for (relationship, node) in &pattern.steps {
                names.extend(relationship.variable.clone());
                names.extend(node.variable.clone());
            }
        }
        names
    }

    fn properties(
        &self,
        entries: &[(String, Expr)],
        row: &Row,
    ) -> GraphResult<Vec<(String, Value)>> {
        entries
            .iter()
            .map(|(name, value)| Ok((name.clone(), self.evaluate(value, row)?)))
            .collect()
    }

    fn create_node(&mut self, pattern: &NodePattern, row: &mut Row) -> GraphResult<u64> {
        if let Some(Some(node)) = self.bound_node(pattern, row)? {
            if !pattern.labels.is_empty() || !pattern.properties.is_empty() {
                return Err(GraphError::new(
                    "Neo.ClientError.Statement.SyntaxError",
                    "Can't create a node that is already bound",
                ));
            }
            return Ok(node.id);
        }
        let LabelExpression::AllOf(labels) = &pattern.labels else {
            return Err(syntax_error(
                "Label expressions with '|' are not allowed in CREATE and MERGE patterns; join the labels of a node with ':'",
            ));
        };
        let properties = self.properties(&pattern.properties, row)?;
        let id = self.writer()?.put_node(None, labels, &properties)?;
        if let Some(name) = &pattern.variable {
            let node = self.read().node(id)?.ok_or_else(|| {
                GraphError::new(
                    "Neo.DatabaseError.General.UnknownError",
                    "a created node vanished",
                )
            })?;
            bind(row, name, Value::Node(Box::new(node)));
        }
        Ok(id)
    }

    fn create(&mut self, pattern: &Pattern, row: &mut Row, merging: bool) -> GraphResult<()> {
        if let Some(name) = &pattern.path
            && lookup(row, name).is_some()
        {
            return Err(syntax_error(format!("Variable `{name}` already declared")));
        }
        let origin = self.create_node(&pattern.start, row)?;
        let mut at = origin;
        let mut walked = Vec::new();
        for (relationship, node) in &pattern.steps {
            let next = self.create_node(node, row)?;
            let [kind] = relationship.kinds.as_slice() else {
                return Err(GraphError::new(
                    "Neo.ClientError.Statement.SyntaxError",
                    "A relationship created needs exactly one type",
                ));
            };
            if relationship.hops.is_some() {
                return Err(GraphError::new(
                    "Neo.ClientError.Statement.SyntaxError",
                    "Variable length relationships cannot be created",
                ));
            }
            let (start, end) = match relationship.direction {
                Direction::Outgoing => (at, next),
                Direction::Incoming => (next, at),
                Direction::Either if merging => (at, next),
                Direction::Either => {
                    return Err(GraphError::new(
                        "Neo.ClientError.Statement.SyntaxError",
                        "Only directed relationships are supported in CREATE",
                    ));
                }
            };
            let properties = self.properties(&relationship.properties, row)?;
            let id = self
                .writer()?
                .put_relationship(None, start, end, kind, &properties)?;
            if relationship.variable.is_some() || pattern.path.is_some() {
                let created = self.existing_relationship(id)?;
                if let Some(name) = &relationship.variable {
                    bind(row, name, Value::Relationship(Box::new(created.clone())));
                }
                if pattern.path.is_some() {
                    walked.push(created);
                }
            }
            at = next;
        }
        if let Some(name) = &pattern.path {
            let path = self.path_value(origin, &walked)?;
            bind_path(row, name, path)?;
        }
        Ok(())
    }

    fn refresh(&self, row: &mut Row) -> GraphResult<()> {
        for (_, value) in row.iter_mut() {
            match value {
                Value::Node(node) => {
                    if let Some(current) = self.read().node(node.id)? {
                        **node = current;
                    }
                }
                Value::Relationship(relationship) => {
                    if let Some(current) = self.read().relationship(relationship.id)? {
                        **relationship = current;
                    }
                }
                Value::Path(path) => {
                    for node in &mut path.nodes {
                        if let Some(current) = self.read().node(node.id)? {
                            *node = current;
                        }
                    }
                    for relationship in &mut path.relationships {
                        if let Some(current) = self.read().relationship(relationship.id)? {
                            *relationship = current;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn apply_set(&mut self, items: &[SetItem], row: &mut Row, removing: bool) -> GraphResult<()> {
        for item in items {
            self.refresh(row)?;
            self.set(item, row, removing)?;
        }
        self.refresh(row)
    }

    fn set(&mut self, item: &SetItem, row: &Row, removing: bool) -> GraphResult<()> {
        let (variable, change): (String, Change) = match item {
            SetItem::Property(Expr::Variable(variable), property, value) => {
                let value = if removing {
                    Value::Null
                } else {
                    self.evaluate(value, row)?
                };
                let property = property.clone();
                (
                    variable.clone(),
                    Box::new(move |_, properties| {
                        properties.retain(|(name, _)| *name != property);
                        if !matches!(value, Value::Null) {
                            properties.push((property.clone(), value.clone()));
                        }
                    }),
                )
            }
            SetItem::Replace(variable, value, merge) => {
                let entries = match self.evaluate(value, row)? {
                    Value::Map(entries) => entries,
                    Value::Node(node) => node.properties,
                    Value::Relationship(relationship) => relationship.properties,
                    other => {
                        return Err(type_error(format!(
                            "expected a map, not {}",
                            other.type_name()
                        )));
                    }
                };
                let merge = *merge;
                (
                    variable.clone(),
                    Box::new(move |_, properties| {
                        if !merge {
                            properties.clear();
                        }
                        for (name, value) in &entries {
                            properties.retain(|(known, _)| known != name);
                            if !matches!(value, Value::Null) {
                                properties.push((name.clone(), value.clone()));
                            }
                        }
                    }),
                )
            }
            SetItem::Labels(variable, labels) => {
                let labels = labels.clone();
                (
                    variable.clone(),
                    Box::new(move |current, _| {
                        if removing {
                            current.retain(|label| !labels.contains(label));
                        } else {
                            current.extend(labels.iter().cloned());
                        }
                    }),
                )
            }
            SetItem::Property(..) => {
                return Err(GraphError::not_supported(
                    "SET on a property of an expression",
                ));
            }
        };
        match lookup(row, &variable).cloned() {
            Some(Value::Node(node)) => {
                let mut labels = node.labels.clone();
                let mut properties = node.properties.clone();
                change(&mut labels, &mut properties);
                self.writer()?
                    .put_node(Some(node.id), &labels, &properties)?;
            }
            Some(Value::Relationship(relationship)) => {
                if matches!(item, SetItem::Labels(..)) {
                    return Err(type_error("relationships have a type, not labels"));
                }
                let mut properties = relationship.properties.clone();
                change(&mut Vec::new(), &mut properties);
                let writer = self.writer()?;
                writer.delete_relationship(relationship.id)?;
                writer.put_relationship(
                    Some(relationship.id),
                    relationship.start,
                    relationship.end,
                    &relationship.kind,
                    &properties,
                )?;
            }
            Some(Value::Null) => {}
            Some(other) => {
                return Err(type_error(format!(
                    "cannot set properties on {}",
                    other.type_name()
                )));
            }
            None => {
                return Err(GraphError::new(
                    "Neo.ClientError.Statement.SyntaxError",
                    format!("Variable `{variable}` not defined"),
                ));
            }
        }
        Ok(())
    }

    fn project(
        &self,
        rows: Vec<Row>,
        projecting: &Projecting,
    ) -> GraphResult<(Vec<String>, Vec<Row>)> {
        let mut items = projecting.items.clone();
        if projecting.star {
            let mut names: Vec<String> = rows
                .first()
                .map(|row| row.iter().map(|(name, _)| name.clone()).collect())
                .unwrap_or_default();
            names.sort();
            items.splice(
                0..0,
                names.into_iter().map(|name| crate::cypher::Projection {
                    expr: Expr::Variable(name.clone()),
                    name,
                }),
            );
        }
        let names: Vec<String> = items.iter().map(|item| item.name.clone()).collect();
        let aggregating = items.iter().any(|item| is_aggregate(&item.expr));
        let mut projected: Vec<(Row, Row)> = Vec::new();
        if aggregating {
            let mut groups: Vec<(Vec<Value>, Vec<Row>)> = Vec::new();
            for row in rows {
                let key = items
                    .iter()
                    .filter(|item| !is_aggregate(&item.expr))
                    .map(|item| self.evaluate(&item.expr, &row))
                    .collect::<GraphResult<Vec<_>>>()?;
                match groups.iter_mut().find(|(known, _)| {
                    known.len() == key.len() && known.iter().zip(&key).all(|(a, b)| a == b)
                }) {
                    Some((_, members)) => members.push(row),
                    None => groups.push((key, vec![row])),
                }
            }
            if groups.is_empty() && items.iter().all(|item| is_aggregate(&item.expr)) {
                groups.push((Vec::new(), Vec::new()));
            }
            for (_, members) in groups {
                let first = members.first().cloned().unwrap_or_default();
                let mut out = Row::new();
                for item in &items {
                    let value = if is_aggregate(&item.expr) {
                        self.aggregate(&item.expr, &members)?
                    } else {
                        self.evaluate(&item.expr, &first)?
                    };
                    out.push((item.name.clone(), value));
                }
                projected.push((out, first));
            }
        } else {
            for row in rows {
                let mut out = Row::new();
                for item in &items {
                    out.push((item.name.clone(), self.evaluate(&item.expr, &row)?));
                }
                projected.push((out, row));
            }
        }
        if projecting.distinct {
            let mut seen = HashSet::new();
            projected
                .retain(|(out, _)| seen.insert(distinct_key(out.iter().map(|(_, value)| value))));
        }
        if !projecting.order.is_empty() {
            let mut keyed = Vec::with_capacity(projected.len());
            for (out, source) in projected {
                let mut scope = source.clone();
                for (name, value) in &out {
                    bind(&mut scope, name, value.clone());
                }
                let keys = projecting
                    .order
                    .iter()
                    .map(|(expr, _)| self.evaluate(expr, &scope))
                    .collect::<GraphResult<Vec<_>>>()?;
                keyed.push((keys, out, source));
            }
            keyed.sort_by(|(a, _, _), (b, _, _)| {
                for ((x, y), (_, descending)) in a.iter().zip(b).zip(&projecting.order) {
                    let ordering = order(x, y);
                    if ordering != Ordering::Equal {
                        return if *descending {
                            ordering.reverse()
                        } else {
                            ordering
                        };
                    }
                }
                Ordering::Equal
            });
            projected = keyed
                .into_iter()
                .map(|(_, out, source)| (out, source))
                .collect();
        }
        let empty = Row::new();
        let skip = match &projecting.skip {
            Some(expr) => as_count(&self.evaluate(expr, &empty)?, "SKIP")?,
            None => 0,
        };
        let limit = match &projecting.limit {
            Some(expr) => as_count(&self.evaluate(expr, &empty)?, "LIMIT")?,
            None => usize::MAX,
        };
        let rows: Vec<Row> = projected
            .into_iter()
            .skip(skip)
            .take(limit)
            .map(|(out, _)| out)
            .collect();
        Ok((names, rows))
    }

    fn aggregate(&self, expr: &Expr, rows: &[Row]) -> GraphResult<Value> {
        let Expr::Call {
            name,
            distinct,
            arguments,
            star,
        } = expr
        else {
            return Err(GraphError::not_supported("aggregates inside expressions"));
        };
        if *star {
            return Ok(Value::Integer(rows.len() as i64));
        }
        let argument = arguments.first().ok_or_else(|| {
            GraphError::new(
                "Neo.ClientError.Statement.SyntaxError",
                format!("{name} needs an argument"),
            )
        })?;
        let mut values = Vec::new();
        for row in rows {
            let value = self.evaluate(argument, row)?;
            if matches!(value, Value::Null) {
                continue;
            }
            if *distinct
                && values
                    .iter()
                    .any(|known| equals(known, &value) == Some(true))
            {
                continue;
            }
            values.push(value);
        }
        Ok(match name.as_str() {
            "count" => Value::Integer(values.len() as i64),
            "collect" => Value::List(values),
            "min" => values.into_iter().min_by(order).unwrap_or(Value::Null),
            "max" => values.into_iter().max_by(order).unwrap_or(Value::Null),
            "sum" => values.iter().try_fold(Value::Integer(0), |total, value| {
                number_result(&total, value, i64::checked_add, |a, b| a + b)
            })?,
            _ => {
                if values.is_empty() {
                    Value::Null
                } else {
                    let total: f64 = values.iter().filter_map(Value::as_f64).sum();
                    Value::Float(total / values.len() as f64)
                }
            }
        })
    }

    pub(crate) fn run_query(&mut self, query: &Query) -> GraphResult<QueryResult> {
        let parts = query
            .parts
            .iter()
            .map(|clauses| (clauses.as_slice(), Row::new()))
            .collect();
        self.run_union(parts, query.union_all)
    }

    fn run_union(
        &mut self,
        parts: Vec<(&[Clause], Row)>,
        union_all: bool,
    ) -> GraphResult<QueryResult> {
        let several = parts.len() > 1;
        let mut parts = parts.into_iter();
        let Some((first, start)) = parts.next() else {
            return Ok(QueryResult::default());
        };
        let mut result = self.run_rows(first, vec![start])?;
        for (part, start) in parts {
            let next = self.run_rows(part, vec![start])?;
            if next.columns != result.columns {
                return Err(syntax_error(
                    "All sub queries in an UNION must have the same return column names",
                ));
            }
            result.rows.extend(next.rows);
            if result.rows.len() > MAX_ROWS {
                return Err(out_of_memory("the UNION returns too many rows"));
            }
        }
        if several && !union_all {
            let mut seen = HashSet::new();
            result
                .rows
                .retain(|row| seen.insert(distinct_key(row.iter())));
        }
        Ok(result)
    }

    fn update(&mut self, clause: &Clause, mut rows: Vec<Row>) -> GraphResult<Vec<Row>> {
        match clause {
            Clause::Create(patterns) => {
                for row in &mut rows {
                    for pattern in patterns {
                        self.create(pattern, row, false)?;
                    }
                }
            }
            Clause::Merge {
                pattern,
                on_create,
                on_match,
            } => {
                let mut next = Vec::new();
                for row in rows {
                    self.refuse_null_merge_properties(pattern, &row)?;
                    let found = self.match_pattern(&row, pattern)?;
                    if found.is_empty() {
                        let mut created = row;
                        self.create(pattern, &mut created, true)?;
                        self.apply_set(on_create, &mut created, false)?;
                        next.push(created);
                    } else {
                        for mut matched in found {
                            self.apply_set(on_match, &mut matched, false)?;
                            next.push(matched);
                        }
                    }
                    if next.len() > MAX_ROWS {
                        return Err(out_of_memory("MERGE makes too many rows"));
                    }
                }
                rows = next;
            }
            Clause::Set(items) | Clause::Remove(items) => {
                let removing = matches!(clause, Clause::Remove(_));
                for row in &mut rows {
                    self.apply_set(items, row, removing)?;
                }
            }
            Clause::Delete { detach, targets } => {
                let mut nodes = Vec::new();
                for row in &rows {
                    for target in targets {
                        let value = self.evaluate(target, row)?;
                        self.delete_relationships_of(value, &mut nodes)?;
                    }
                }
                for node in nodes {
                    self.writer()?.delete_node(node, *detach)?;
                }
            }
            Clause::Foreach {
                variable,
                list,
                body,
            } => {
                for row in &mut rows {
                    let items = match self.evaluate(list, row)? {
                        Value::List(items) => items,
                        Value::Null => Vec::new(),
                        other => {
                            return Err(type_error(format!(
                                "Type mismatch: FOREACH expected a list but was {}",
                                other.type_name()
                            )));
                        }
                    };
                    for item in items {
                        self.foreach_runs += 1;
                        if self.foreach_runs > MAX_ROWS {
                            return Err(out_of_memory(format!(
                                "FOREACH runs its body more than {MAX_ROWS} times in one query"
                            )));
                        }
                        let mut scoped = row.clone();
                        bind(&mut scoped, variable, item);
                        let mut inner = vec![scoped];
                        for clause in body {
                            inner = self.update(clause, inner)?;
                        }
                    }
                    self.refresh(row)?;
                }
            }
            Clause::Subquery(subquery) => {
                rows = self.run_subquery(subquery, rows)?;
            }
            Clause::Match { .. }
            | Clause::Unwind(..)
            | Clause::Call { .. }
            | Clause::With(_)
            | Clause::Return(_) => {}
        }
        Ok(rows)
    }

    fn refuse_null_merge_properties(&self, pattern: &Pattern, row: &Row) -> GraphResult<()> {
        for node in pattern.nodes() {
            if self.bound_node(node, row)?.is_some() {
                continue;
            }
            let properties = self.properties(&node.properties, row)?;
            if let Some(name) = null_property(&properties) {
                return Err(semantic_error(format!(
                    "Cannot merge the following node because of null property value for '{name}': ({})",
                    pattern_element_text(node.labels.written(), &properties)
                )));
            }
        }
        for (relationship, _) in &pattern.steps {
            let properties = self.properties(&relationship.properties, row)?;
            if let Some(name) = null_property(&properties) {
                let kinds = if relationship.kinds.is_empty() {
                    String::new()
                } else {
                    format!(":{}", relationship.kinds.join("|"))
                };
                let element = pattern_element_text(kinds, &properties);
                let written = match relationship.direction {
                    Direction::Outgoing => format!("()-[{element}]->()"),
                    Direction::Incoming => format!("()<-[{element}]-()"),
                    Direction::Either => format!("()-[{element}]-()"),
                };
                return Err(semantic_error(format!(
                    "Cannot merge the following relationship because of null property value for '{name}': {written}"
                )));
            }
        }
        Ok(())
    }

    fn run_subquery(&mut self, subquery: &Subquery, rows: Vec<Row>) -> GraphResult<Vec<Row>> {
        let returns = subquery.returns();
        let writes = subquery.writes();
        let mut next = Vec::new();
        for mut row in rows {
            let mut parts = Vec::with_capacity(subquery.parts.len());
            for part in &subquery.parts {
                parts.push((
                    part.clauses.as_slice(),
                    imported_row(&part.imports, &row)?,
                ));
            }
            let result = self.run_union(parts, subquery.union_all)?;
            if writes {
                self.refresh(&mut row)?;
            }
            if !returns {
                next.push(row);
            } else {
                if let Some(name) = result
                    .columns
                    .iter()
                    .find(|name| lookup(&row, name).is_some())
                {
                    return Err(semantic_error(format!(
                        "Variable `{name}` already declared in outer scope"
                    )));
                }
                for values in result.rows {
                    let mut extended = row.clone();
                    extended.extend(result.columns.iter().cloned().zip(values));
                    next.push(extended);
                    if next.len() > MAX_ROWS {
                        return Err(out_of_memory("CALL { ... } makes too many rows"));
                    }
                }
            }
            if next.len() > MAX_ROWS {
                return Err(out_of_memory("CALL { ... } makes too many rows"));
            }
        }
        Ok(next)
    }

    fn delete_relationships_of(&mut self, value: Value, nodes: &mut Vec<u64>) -> GraphResult<()> {
        match value {
            Value::Node(node) => nodes.push(node.id),
            Value::Relationship(relationship) => {
                self.writer()?.delete_relationship(relationship.id)?;
            }
            Value::Path(path) => {
                for relationship in &path.relationships {
                    self.writer()?.delete_relationship(relationship.id)?;
                }
                nodes.extend(path.nodes.iter().map(|node| node.id));
            }
            Value::List(items) => {
                for item in items {
                    if matches!(item, Value::Relationship(_) | Value::Path(_)) {
                        self.delete_relationships_of(item, nodes)?;
                    }
                }
            }
            Value::Null => {}
            other => {
                return Err(type_error(format!("cannot delete {}", other.type_name())));
            }
        }
        Ok(())
    }

    fn run_rows(&mut self, clauses: &[Clause], mut rows: Vec<Row>) -> GraphResult<QueryResult> {
        let mut result = QueryResult::default();
        for clause in clauses {
            if let Clause::Subquery(subquery) = clause {
                rows = self.run_subquery(subquery, rows)?;
                continue;
            }
            if clause.writes() {
                rows = self.update(clause, rows)?;
                continue;
            }
            match clause {
                Clause::Match {
                    optional,
                    patterns,
                    filter,
                } => {
                    let mut next = Vec::new();
                    for row in rows {
                        let mut matched = vec![row.clone()];
                        for pattern in patterns {
                            let mut extended = Vec::new();
                            for partial in matched {
                                extended.extend(self.match_pattern(&partial, pattern)?);
                            }
                            matched = extended;
                        }
                        if let Some(filter) = filter {
                            let mut kept = Vec::new();
                            for candidate in matched {
                                if self.evaluate(filter, &candidate)?.is_true() {
                                    kept.push(candidate);
                                }
                            }
                            matched = kept;
                        }
                        if matched.is_empty() && *optional {
                            let mut padded = row;
                            for name in Self::variables_of(patterns) {
                                if lookup(&padded, &name).is_none() {
                                    padded.push((name, Value::Null));
                                }
                            }
                            matched.push(padded);
                        }
                        next.extend(matched);
                        if next.len() > MAX_ROWS {
                            return Err(GraphError::new(
                                "Neo.TransientError.General.MemoryPoolOutOfMemoryError",
                                "the query matches too many rows",
                            ));
                        }
                    }
                    rows = next;
                }
                Clause::Unwind(list, name) => {
                    let mut next = Vec::new();
                    for row in rows {
                        match self.evaluate(list, &row)? {
                            Value::List(items) => {
                                for item in items {
                                    let mut extended = row.clone();
                                    bind(&mut extended, name, item);
                                    next.push(extended);
                                }
                            }
                            Value::Null => {}
                            single => {
                                let mut extended = row;
                                bind(&mut extended, name, single);
                                next.push(extended);
                            }
                        }
                        if next.len() > MAX_ROWS {
                            return Err(GraphError::new(
                                "Neo.TransientError.General.MemoryPoolOutOfMemoryError",
                                "UNWIND makes too many rows",
                            ));
                        }
                    }
                    rows = next;
                }
                Clause::Call {
                    procedure,
                    arguments,
                    yields,
                } => {
                    let mut next = Vec::new();
                    let mut columns = Vec::new();
                    for row in rows {
                        let values = arguments
                            .iter()
                            .map(|argument| self.evaluate(argument, &row))
                            .collect::<GraphResult<Vec<_>>>()?;
                        let (names, produced) =
                            crate::procedures::call(&self.read(), procedure, &values)?;
                        let picked: Vec<(usize, String)> = if yields.is_empty() {
                            names
                                .iter()
                                .enumerate()
                                .map(|(at, name)| (at, (*name).to_owned()))
                                .collect()
                        } else {
                            yields
                                .iter()
                                .map(|(column, alias)| {
                                    names
                                        .iter()
                                        .position(|name| name == column)
                                        .map(|at| (at, alias.clone()))
                                        .ok_or_else(|| {
                                            GraphError::new(
                                                "Neo.ClientError.Statement.SyntaxError",
                                                format!("{procedure} does not yield {column}"),
                                            )
                                        })
                                })
                                .collect::<GraphResult<_>>()?
                        };
                        columns = picked.iter().map(|(_, alias)| alias.clone()).collect();
                        for record in produced {
                            let mut extended = row.clone();
                            for (at, alias) in &picked {
                                bind(
                                    &mut extended,
                                    alias,
                                    record.get(*at).cloned().unwrap_or(Value::Null),
                                );
                            }
                            next.push(extended);
                            if next.len() > MAX_ROWS {
                                return Err(GraphError::new(
                                    "Neo.TransientError.General.MemoryPoolOutOfMemoryError",
                                    "CALL makes too many rows",
                                ));
                            }
                        }
                    }
                    if std::ptr::eq(clause, clauses.last().unwrap_or(clause)) {
                        result.columns = columns.clone();
                        result.rows = next
                            .into_iter()
                            .map(|row| {
                                columns
                                    .iter()
                                    .map(|column| {
                                        lookup(&row, column).cloned().unwrap_or(Value::Null)
                                    })
                                    .collect()
                            })
                            .collect();
                        return Ok(result);
                    }
                    rows = next;
                }
                Clause::With(projecting) => {
                    let (_, projected) = self.project(rows, projecting)?;
                    rows = match &projecting.filter {
                        Some(filter) => {
                            let mut kept = Vec::new();
                            for row in projected {
                                if self.evaluate(filter, &row)?.is_true() {
                                    kept.push(row);
                                }
                            }
                            kept
                        }
                        None => projected,
                    };
                }
                Clause::Return(projecting) => {
                    let (columns, projected) = self.project(rows, projecting)?;
                    result.columns = columns;
                    result.rows = projected
                        .into_iter()
                        .map(|row| row.into_iter().map(|(_, value)| value).collect())
                        .collect();
                    return Ok(result);
                }
                Clause::Create(_)
                | Clause::Merge { .. }
                | Clause::Set(_)
                | Clause::Remove(_)
                | Clause::Delete { .. }
                | Clause::Foreach { .. }
                | Clause::Subquery(_) => {}
            }
        }
        Ok(result)
    }
}

/// The text `toString` and string concatenation give a value.
pub fn display(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Boolean(flag) => flag.to_string(),
        Value::Integer(number) => number.to_string(),
        Value::Float(number) if number.fract() == 0.0 && number.is_finite() => {
            format!("{number:.1}")
        }
        Value::Float(number) => number.to_string(),
        Value::String(text) => text.clone(),
        Value::List(items) => format!(
            "[{}]",
            items.iter().map(display).collect::<Vec<_>>().join(", ")
        ),
        Value::Map(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(name, value)| format!("{name}: {}", display(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Node(node) => format!(
            "({}{})",
            node.id,
            node.labels
                .iter()
                .map(|label| format!(":{label}"))
                .collect::<String>()
        ),
        Value::Relationship(relationship) => format!("[{}:{}]", relationship.id, relationship.kind),
        Value::Path(path) => {
            let mut text = path
                .nodes
                .first()
                .map(|node| display(&Value::Node(Box::new(node.clone()))))
                .unwrap_or_default();
            for (relationship, node) in path.relationships.iter().zip(path.nodes.iter().skip(1)) {
                let link = display(&Value::Relationship(Box::new(relationship.clone())));
                if relationship.end == node.id {
                    text.push_str(&format!("-{link}->"));
                } else {
                    text.push_str(&format!("<-{link}-"));
                }
                text.push_str(&display(&Value::Node(Box::new(node.clone()))));
            }
            text
        }
    }
}
