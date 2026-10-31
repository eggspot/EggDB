use crate::cypher::{Clause, Direction, Expr, NodePattern, Operator, Pattern, Projecting, RelationshipPattern, SetItem};
use crate::store::{GraphRead, GraphWrite};
use crate::value::{Node, Relationship, Value, equals, order};
use crate::{GraphError, GraphResult, QueryResult};
use std::cmp::Ordering;

const MAX_ROWS: usize = 1 << 22;
const AGGREGATES: [&str; 6] = ["count", "sum", "avg", "min", "max", "collect"];

type Row = Vec<(String, Value)>;

fn lookup<'r>(row: &'r Row, name: &str) -> Option<&'r Value> {
    row.iter().rev().find(|(known, _)| known == name).map(|(_, value)| value)
}

fn bind(row: &mut Row, name: &str, value: Value) {
    match row.iter_mut().find(|(known, _)| known == name) {
        Some((_, slot)) => *slot = value,
        None => row.push((name.to_owned(), value)),
    }
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
}

fn is_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Call { name, .. } if AGGREGATES.contains(&name.as_str()) => true,
        Expr::Call { arguments, .. } => arguments.iter().any(is_aggregate),
        Expr::Binary(_, left, right) => is_aggregate(left) || is_aggregate(right),
        Expr::Not(inner) | Expr::Negate(inner) | Expr::IsNull(inner, _) | Expr::Property(inner, _) => is_aggregate(inner),
        Expr::List(items) => items.iter().any(is_aggregate),
        _ => false,
    }
}

fn number_result(left: &Value, right: &Value, integer: fn(i64, i64) -> Option<i64>, float: fn(f64, f64) -> f64) -> GraphResult<Value> {
    match (left, right) {
        (Value::Integer(a), Value::Integer(b)) => integer(*a, *b).map(Value::Integer).ok_or_else(|| arithmetic_error("integer overflow")),
        (a, b) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => Ok(Value::Float(float(a, b))),
            _ => Err(type_error(format!("cannot combine {} and {}", a.type_name(), b.type_name()))),
        },
    }
}

fn three_valued(value: &Value) -> GraphResult<Option<bool>> {
    match value {
        Value::Null => Ok(None),
        Value::Boolean(flag) => Ok(Some(*flag)),
        other => Err(type_error(format!("expected a boolean, not {}", other.type_name()))),
    }
}

fn as_count(value: &Value, what: &str) -> GraphResult<usize> {
    match value {
        Value::Integer(number) if *number >= 0 => Ok(*number as usize),
        _ => Err(GraphError::new("Neo.ClientError.Statement.ArgumentError", format!("{what} must be a non-negative integer"))),
    }
}

impl Executor<'_, '_> {
    fn read(&self) -> GraphRead<'_> {
        match &self.access {
            Access::Read(read) => GraphRead { keys: read.keys },
            Access::Write(write) => write.read(),
        }
    }

    fn writer(&mut self) -> GraphResult<&mut GraphWrite<'_>> {
        match &mut self.access {
            Access::Write(write) => Ok(write),
            Access::Read(_) => Err(GraphError::new("Neo.ClientError.Statement.AccessMode", "Writing in read access mode not allowed")),
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
                .ok_or_else(|| GraphError::new("Neo.ClientError.Statement.ParameterMissing", format!("Expected parameter(s): {name}")))?,
            Expr::Variable(name) => lookup(row, name)
                .cloned()
                .ok_or_else(|| GraphError::new("Neo.ClientError.Statement.SyntaxError", format!("Variable `{name}` not defined")))?,
            Expr::Property(target, property) => match self.evaluate(target, row)? {
                Value::Node(node) => node.properties.iter().find(|(name, _)| name == property).map_or(Value::Null, |(_, value)| value.clone()),
                Value::Relationship(relationship) => relationship.properties.iter().find(|(name, _)| name == property).map_or(Value::Null, |(_, value)| value.clone()),
                Value::Map(entries) => entries.iter().find(|(name, _)| name == property).map_or(Value::Null, |(_, value)| value.clone()),
                Value::Null => Value::Null,
                other => return Err(type_error(format!("Type mismatch: expected a map, node or relationship but was {}", other.type_name()))),
            },
            Expr::Index(target, index) => match (self.evaluate(target, row)?, self.evaluate(index, row)?) {
                (Value::List(items), Value::Integer(at)) => {
                    let at = if at < 0 { items.len() as i64 + at } else { at };
                    usize::try_from(at).ok().and_then(|at| items.get(at).cloned()).unwrap_or(Value::Null)
                }
                (Value::Map(entries), Value::String(key)) => entries.into_iter().find(|(name, _)| *name == key).map_or(Value::Null, |(_, value)| value),
                (Value::Null, _) | (_, Value::Null) => Value::Null,
                _ => return Err(type_error("cannot index this value")),
            },
            Expr::List(items) => Value::List(items.iter().map(|item| self.evaluate(item, row)).collect::<GraphResult<_>>()?),
            Expr::Map(entries) => Value::Map(entries.iter().map(|(key, value)| Ok((key.clone(), self.evaluate(value, row)?))).collect::<GraphResult<_>>()?),
            Expr::Not(inner) => match three_valued(&self.evaluate(inner, row)?)? {
                Some(flag) => Value::Boolean(!flag),
                None => Value::Null,
            },
            Expr::Negate(inner) => match self.evaluate(inner, row)? {
                Value::Integer(number) => Value::Integer(number.checked_neg().ok_or_else(|| arithmetic_error("integer overflow"))?),
                Value::Float(number) => Value::Float(-number),
                Value::Null => Value::Null,
                other => return Err(type_error(format!("cannot negate {}", other.type_name()))),
            },
            Expr::IsNull(inner, negated) => Value::Boolean(matches!(self.evaluate(inner, row)?, Value::Null) != *negated),
            Expr::Binary(operator, left, right) => self.binary(*operator, left, right, row)?,
            Expr::Case { subject, arms, otherwise } => {
                let subject = subject.as_ref().map(|subject| self.evaluate(subject, row)).transpose()?;
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
            Expr::Call { name, arguments, star, .. } => {
                if *star || AGGREGATES.contains(&name.as_str()) {
                    return Err(GraphError::new("Neo.ClientError.Statement.SyntaxError", format!("Invalid use of aggregating function {name}(...) in this context")));
                }
                let values: Vec<Value> = arguments.iter().map(|argument| self.evaluate(argument, row)).collect::<GraphResult<_>>()?;
                self.function(name, values)?
            }
        })
    }

    fn binary(&self, operator: Operator, left: &Expr, right: &Expr, row: &Row) -> GraphResult<Value> {
        if matches!(operator, Operator::And | Operator::Or | Operator::Xor) {
            let a = three_valued(&self.evaluate(left, row)?)?;
            let b = three_valued(&self.evaluate(right, row)?)?;
            return Ok(match (operator, a, b) {
                (Operator::And, Some(false), _) | (Operator::And, _, Some(false)) => Value::Boolean(false),
                (Operator::And, Some(true), Some(true)) => Value::Boolean(true),
                (Operator::Or, Some(true), _) | (Operator::Or, _, Some(true)) => Value::Boolean(true),
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
            Operator::Less | Operator::LessOrEqual | Operator::Greater | Operator::GreaterOrEqual => {
                let comparable = a.kind_rank() == b.kind_rank() && matches!(a, Value::Integer(_) | Value::Float(_) | Value::String(_) | Value::Boolean(_));
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
                    if unknown { Value::Null } else { Value::Boolean(false) }
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
        let argument_error = || GraphError::new("Neo.ClientError.Statement.ArgumentError", format!("wrong arguments for {name}()"));
        Ok(match (name, values.as_slice()) {
            ("coalesce", items) => items.iter().find(|value| !matches!(value, Value::Null)).cloned().unwrap_or(Value::Null),
            (_, [Value::Null, ..]) if name != "range" => Value::Null,
            ("id", [Value::Node(node)]) => Value::Integer(node.id as i64),
            ("id", [Value::Relationship(relationship)]) => Value::Integer(relationship.id as i64),
            ("labels", [Value::Node(node)]) => Value::List(node.labels.iter().cloned().map(Value::String).collect()),
            ("type", [Value::Relationship(relationship)]) => Value::String(relationship.kind.clone()),
            ("properties", [Value::Node(node)]) => Value::Map(node.properties.clone()),
            ("properties", [Value::Relationship(relationship)]) => Value::Map(relationship.properties.clone()),
            ("properties", [Value::Map(entries)]) => Value::Map(entries.clone()),
            ("keys", [Value::Node(node)]) => Value::List(node.properties.iter().map(|(name, _)| Value::String(name.clone())).collect()),
            ("keys", [Value::Relationship(relationship)]) => Value::List(relationship.properties.iter().map(|(name, _)| Value::String(name.clone())).collect()),
            ("keys", [Value::Map(entries)]) => Value::List(entries.iter().map(|(name, _)| Value::String(name.clone())).collect()),
            ("startnode", [Value::Relationship(relationship)]) => self.read().node(relationship.start)?.map_or(Value::Null, |node| Value::Node(Box::new(node))),
            ("endnode", [Value::Relationship(relationship)]) => self.read().node(relationship.end)?.map_or(Value::Null, |node| Value::Node(Box::new(node))),
            ("size" | "length", [Value::List(items)]) => Value::Integer(items.len() as i64),
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
            ("tointeger", [Value::Float(number)]) if number.is_finite() => Value::Integer(number.trunc() as i64),
            ("tointeger", [Value::String(text)]) => text.trim().parse::<i64>().map_or(Value::Null, Value::Integer),
            ("tofloat", [value]) => match value {
                Value::String(text) => text.trim().parse::<f64>().map_or(Value::Null, Value::Float),
                other => other.as_f64().map_or(Value::Null, Value::Float),
            },
            ("abs", [Value::Integer(number)]) => Value::Integer(number.checked_abs().ok_or_else(|| arithmetic_error("integer overflow"))?),
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
                let count = if (step > 0 && end >= start) || (step < 0 && end <= start) { ((end - start) / step + 1) as u64 } else { 0 };
                if count > MAX_ROWS as u64 {
                    return Err(argument_error());
                }
                Value::List((0..count as i64).map(|at| Value::Integer(start + at * step)).collect())
            }
            _ => return Err(GraphError::new("Neo.ClientError.Statement.SyntaxError", format!("Unknown function '{name}' or wrong arguments"))),
        })
    }

    fn node_matches(&self, node: &Node, pattern: &NodePattern, row: &Row) -> GraphResult<bool> {
        if !pattern.labels.iter().all(|label| node.labels.contains(label)) {
            return Ok(false);
        }
        for (name, expected) in &pattern.properties {
            let expected = self.evaluate(expected, row)?;
            let actual = node.properties.iter().find(|(key, _)| key == name).map_or(Value::Null, |(_, value)| value.clone());
            if equals(&actual, &expected) != Some(true) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn relationship_matches(&self, relationship: &Relationship, pattern: &RelationshipPattern, row: &Row) -> GraphResult<bool> {
        if !pattern.kinds.is_empty() && !pattern.kinds.contains(&relationship.kind) {
            return Ok(false);
        }
        for (name, expected) in &pattern.properties {
            let expected = self.evaluate(expected, row)?;
            let actual = relationship.properties.iter().find(|(key, _)| key == name).map_or(Value::Null, |(_, value)| value.clone());
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
            Some(other) => return Err(type_error(format!("Variable `{name}` already declared as {}", other.type_name()))),
            None => None,
        })
    }

    fn steps_from(&self, from: u64, pattern: &RelationshipPattern, row: &Row, used: &[u64]) -> GraphResult<Vec<(Relationship, u64)>> {
        let read = self.read();
        let mut ids = Vec::new();
        if matches!(pattern.direction, Direction::Outgoing | Direction::Either) {
            ids.extend(read.touching(from, true)?.into_iter().map(|id| (id, true)));
        }
        if matches!(pattern.direction, Direction::Incoming | Direction::Either) {
            ids.extend(read.touching(from, false)?.into_iter().map(|id| (id, false)));
        }
        let mut out = Vec::new();
        for (id, outgoing) in ids {
            if used.contains(&id) || (pattern.direction == Direction::Either && !outgoing && out.iter().any(|(known, _): &(Relationship, u64)| known.id == id)) {
                continue;
            }
            let Some(relationship) = read.relationship(id)? else {
                continue;
            };
            if !self.relationship_matches(&relationship, pattern, row)? {
                continue;
            }
            let next = if outgoing { relationship.end } else { relationship.start };
            out.push((relationship, next));
        }
        Ok(out)
    }

    fn extend(&self, row: Row, pattern: &Pattern, step: usize, at: u64, used: Vec<u64>, out: &mut Vec<Row>) -> GraphResult<()> {
        if out.len() > MAX_ROWS {
            return Err(GraphError::new("Neo.TransientError.General.MemoryPoolOutOfMemoryError", "the query matches too many rows"));
        }
        let Some((relationship_pattern, node_pattern)) = pattern.steps.get(step) else {
            out.push(row);
            return Ok(());
        };
        let bound_relationship = relationship_pattern.variable.as_ref().and_then(|name| lookup(&row, name).cloned());
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
                let mut taken = used.clone();
                taken.extend(path.iter().map(|relationship| relationship.id));
                for (relationship, next) in self.steps_from(*node, relationship_pattern, &row, &taken)? {
                    let mut extended = path.clone();
                    extended.push(relationship);
                    if hop >= min {
                        reached.push((next, extended.clone()));
                    }
                    next_frontier.push((next, extended));
                }
            }
            if next_frontier.len() > MAX_ROWS {
                return Err(GraphError::new("Neo.TransientError.General.MemoryPoolOutOfMemoryError", "the variable-length pattern reaches too many paths"));
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
                Value::List(path.iter().cloned().map(|relationship| Value::Relationship(Box::new(relationship))).collect())
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
            let mut next_used = used.clone();
            next_used.extend(path.iter().map(|relationship| relationship.id));
            self.extend(next_row, pattern, step + 1, node.id, next_used, out)?;
        }
        Ok(())
    }

    fn match_pattern(&self, row: &Row, pattern: &Pattern) -> GraphResult<Vec<Row>> {
        let starts: Vec<Node> = match self.bound_node(&pattern.start, row)? {
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
        };
        let mut out = Vec::new();
        for node in starts {
            if !self.node_matches(&node, &pattern.start, row)? {
                continue;
            }
            let mut start_row = row.clone();
            if let Some(name) = &pattern.start.variable {
                bind(&mut start_row, name, Value::Node(Box::new(node.clone())));
            }
            self.extend(start_row, pattern, 0, node.id, Vec::new(), &mut out)?;
        }
        Ok(out)
    }

    fn variables_of(patterns: &[Pattern]) -> Vec<String> {
        let mut names = Vec::new();
        for pattern in patterns {
            names.extend(pattern.start.variable.clone());
            for (relationship, node) in &pattern.steps {
                names.extend(relationship.variable.clone());
                names.extend(node.variable.clone());
            }
        }
        names
    }

    fn properties(&self, entries: &[(String, Expr)], row: &Row) -> GraphResult<Vec<(String, Value)>> {
        entries.iter().map(|(name, value)| Ok((name.clone(), self.evaluate(value, row)?))).collect()
    }

    fn create_node(&mut self, pattern: &NodePattern, row: &mut Row) -> GraphResult<u64> {
        if let Some(Some(node)) = self.bound_node(pattern, row)? {
            if !pattern.labels.is_empty() || !pattern.properties.is_empty() {
                return Err(GraphError::new("Neo.ClientError.Statement.SyntaxError", "Can't create a node that is already bound"));
            }
            return Ok(node.id);
        }
        let properties = self.properties(&pattern.properties, row)?;
        let id = self.writer()?.put_node(None, &pattern.labels, &properties)?;
        if let Some(name) = &pattern.variable {
            let node = self.read().node(id)?.ok_or_else(|| GraphError::new("Neo.DatabaseError.General.UnknownError", "a created node vanished"))?;
            bind(row, name, Value::Node(Box::new(node)));
        }
        Ok(id)
    }

    fn create(&mut self, pattern: &Pattern, row: &mut Row) -> GraphResult<()> {
        let mut at = self.create_node(&pattern.start, row)?;
        for (relationship, node) in &pattern.steps {
            let next = self.create_node(node, row)?;
            let [kind] = relationship.kinds.as_slice() else {
                return Err(GraphError::new("Neo.ClientError.Statement.SyntaxError", "A relationship created needs exactly one type"));
            };
            if relationship.hops.is_some() {
                return Err(GraphError::new("Neo.ClientError.Statement.SyntaxError", "Variable length relationships cannot be created"));
            }
            let (start, end) = match relationship.direction {
                Direction::Outgoing => (at, next),
                Direction::Incoming => (next, at),
                Direction::Either => return Err(GraphError::new("Neo.ClientError.Statement.SyntaxError", "Only directed relationships are supported in CREATE")),
            };
            let properties = self.properties(&relationship.properties, row)?;
            let id = self.writer()?.put_relationship(None, start, end, kind, &properties)?;
            if let Some(name) = &relationship.variable {
                let created = self.read().relationship(id)?.ok_or_else(|| GraphError::new("Neo.DatabaseError.General.UnknownError", "a created relationship vanished"))?;
                bind(row, name, Value::Relationship(Box::new(created)));
            }
            at = next;
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
                _ => {}
            }
        }
        Ok(())
    }

    fn set(&mut self, item: &SetItem, row: &Row, removing: bool) -> GraphResult<()> {
        let (variable, change): (String, Box<dyn Fn(&mut Vec<String>, &mut Vec<(String, Value)>)>) = match item {
            SetItem::Property(Expr::Variable(variable), property, value) => {
                let value = if removing { Value::Null } else { self.evaluate(value, row)? };
                let property = property.clone();
                (variable.clone(), Box::new(move |_, properties| {
                    properties.retain(|(name, _)| *name != property);
                    if !matches!(value, Value::Null) {
                        properties.push((property.clone(), value.clone()));
                    }
                }))
            }
            SetItem::Replace(variable, value, merge) => {
                let entries = match self.evaluate(value, row)? {
                    Value::Map(entries) => entries,
                    Value::Node(node) => node.properties,
                    Value::Relationship(relationship) => relationship.properties,
                    other => return Err(type_error(format!("expected a map, not {}", other.type_name()))),
                };
                let merge = *merge;
                (variable.clone(), Box::new(move |_, properties| {
                    if !merge {
                        properties.clear();
                    }
                    for (name, value) in &entries {
                        properties.retain(|(known, _)| known != name);
                        if !matches!(value, Value::Null) {
                            properties.push((name.clone(), value.clone()));
                        }
                    }
                }))
            }
            SetItem::Labels(variable, labels) => {
                let labels = labels.clone();
                (variable.clone(), Box::new(move |current, _| {
                    if removing {
                        current.retain(|label| !labels.contains(label));
                    } else {
                        current.extend(labels.iter().cloned());
                    }
                }))
            }
            SetItem::Property(..) => return Err(GraphError::not_supported("SET on a property of an expression")),
        };
        match lookup(row, &variable).cloned() {
            Some(Value::Node(node)) => {
                let mut labels = node.labels.clone();
                let mut properties = node.properties.clone();
                change(&mut labels, &mut properties);
                self.writer()?.put_node(Some(node.id), &labels, &properties)?;
            }
            Some(Value::Relationship(relationship)) => {
                if matches!(item, SetItem::Labels(..)) {
                    return Err(type_error("relationships have a type, not labels"));
                }
                let mut properties = relationship.properties.clone();
                change(&mut Vec::new(), &mut properties);
                let writer = self.writer()?;
                writer.delete_relationship(relationship.id)?;
                writer.put_relationship(Some(relationship.id), relationship.start, relationship.end, &relationship.kind, &properties)?;
            }
            Some(Value::Null) => {}
            Some(other) => return Err(type_error(format!("cannot set properties on {}", other.type_name()))),
            None => return Err(GraphError::new("Neo.ClientError.Statement.SyntaxError", format!("Variable `{variable}` not defined"))),
        }
        Ok(())
    }

    fn project(&self, rows: Vec<Row>, projecting: &Projecting) -> GraphResult<(Vec<String>, Vec<Row>)> {
        let mut items = projecting.items.clone();
        if projecting.star {
            let mut names: Vec<String> = rows.first().map(|row| row.iter().map(|(name, _)| name.clone()).collect()).unwrap_or_default();
            names.sort();
            items.splice(0..0, names.into_iter().map(|name| crate::cypher::Projection { expr: Expr::Variable(name.clone()), name }));
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
                match groups.iter_mut().find(|(known, _)| known.len() == key.len() && known.iter().zip(&key).all(|(a, b)| a == b)) {
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
                    let value = if is_aggregate(&item.expr) { self.aggregate(&item.expr, &members)? } else { self.evaluate(&item.expr, &first)? };
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
            let mut unique: Vec<(Row, Row)> = Vec::new();
            for (out, source) in projected {
                if !unique.iter().any(|(known, _)| *known == out) {
                    unique.push((out, source));
                }
            }
            projected = unique;
        }
        if !projecting.order.is_empty() {
            let mut keyed = Vec::with_capacity(projected.len());
            for (out, source) in projected {
                let mut scope = source.clone();
                for (name, value) in &out {
                    bind(&mut scope, name, value.clone());
                }
                let keys = projecting.order.iter().map(|(expr, _)| self.evaluate(expr, &scope)).collect::<GraphResult<Vec<_>>>()?;
                keyed.push((keys, out, source));
            }
            keyed.sort_by(|(a, _, _), (b, _, _)| {
                for ((x, y), (_, descending)) in a.iter().zip(b).zip(&projecting.order) {
                    let ordering = order(x, y);
                    if ordering != Ordering::Equal {
                        return if *descending { ordering.reverse() } else { ordering };
                    }
                }
                Ordering::Equal
            });
            projected = keyed.into_iter().map(|(_, out, source)| (out, source)).collect();
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
        let rows: Vec<Row> = projected.into_iter().skip(skip).take(limit).map(|(out, _)| out).collect();
        Ok((names, rows))
    }

    fn aggregate(&self, expr: &Expr, rows: &[Row]) -> GraphResult<Value> {
        let Expr::Call { name, distinct, arguments, star } = expr else {
            return Err(GraphError::not_supported("aggregates inside expressions"));
        };
        if *star {
            return Ok(Value::Integer(rows.len() as i64));
        }
        let argument = arguments.first().ok_or_else(|| GraphError::new("Neo.ClientError.Statement.SyntaxError", format!("{name} needs an argument")))?;
        let mut values = Vec::new();
        for row in rows {
            let value = self.evaluate(argument, row)?;
            if matches!(value, Value::Null) {
                continue;
            }
            if *distinct && values.iter().any(|known| equals(known, &value) == Some(true)) {
                continue;
            }
            values.push(value);
        }
        Ok(match name.as_str() {
            "count" => Value::Integer(values.len() as i64),
            "collect" => Value::List(values),
            "min" => values.into_iter().min_by(order).unwrap_or(Value::Null),
            "max" => values.into_iter().max_by(order).unwrap_or(Value::Null),
            "sum" => values.iter().try_fold(Value::Integer(0), |total, value| number_result(&total, value, i64::checked_add, |a, b| a + b))?,
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

    pub(crate) fn run(&mut self, clauses: &[Clause]) -> GraphResult<QueryResult> {
        let mut rows: Vec<Row> = vec![Row::new()];
        let mut result = QueryResult::default();
        for clause in clauses {
            match clause {
                Clause::Match { optional, patterns, filter } => {
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
                            return Err(GraphError::new("Neo.TransientError.General.MemoryPoolOutOfMemoryError", "the query matches too many rows"));
                        }
                    }
                    rows = next;
                }
                Clause::Create(patterns) => {
                    for row in &mut rows {
                        for pattern in patterns {
                            self.create(pattern, row)?;
                        }
                    }
                }
                Clause::Merge(pattern) => {
                    let mut next = Vec::new();
                    for row in rows {
                        let found = self.match_pattern(&row, pattern)?;
                        if found.is_empty() {
                            let mut created = row;
                            self.create(pattern, &mut created)?;
                            next.push(created);
                        } else {
                            next.extend(found);
                        }
                    }
                    rows = next;
                }
                Clause::Set(items) | Clause::Remove(items) => {
                    let removing = matches!(clause, Clause::Remove(_));
                    for row in &mut rows {
                        for item in items {
                            self.set(item, row, removing)?;
                            self.refresh(row)?;
                        }
                    }
                }
                Clause::Delete { detach, targets } => {
                    for row in &rows {
                        for target in targets {
                            match self.evaluate(target, row)? {
                                Value::Node(node) => {
                                    self.writer()?.delete_node(node.id, *detach)?;
                                }
                                Value::Relationship(relationship) => {
                                    self.writer()?.delete_relationship(relationship.id)?;
                                }
                                Value::List(items) => {
                                    for item in items {
                                        if let Value::Relationship(relationship) = item {
                                            self.writer()?.delete_relationship(relationship.id)?;
                                        }
                                    }
                                }
                                Value::Null => {}
                                other => return Err(type_error(format!("cannot delete {}", other.type_name()))),
                            }
                        }
                    }
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
                            return Err(GraphError::new("Neo.TransientError.General.MemoryPoolOutOfMemoryError", "UNWIND makes too many rows"));
                        }
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
                    result.rows = projected.into_iter().map(|row| row.into_iter().map(|(_, value)| value).collect()).collect();
                    return Ok(result);
                }
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
        Value::Float(number) if number.fract() == 0.0 && number.is_finite() => format!("{number:.1}"),
        Value::Float(number) => number.to_string(),
        Value::String(text) => text.clone(),
        Value::List(items) => format!("[{}]", items.iter().map(display).collect::<Vec<_>>().join(", ")),
        Value::Map(entries) => format!("{{{}}}", entries.iter().map(|(name, value)| format!("{name}: {}", display(value))).collect::<Vec<_>>().join(", ")),
        Value::Node(node) => format!("({}{})", node.id, node.labels.iter().map(|label| format!(":{label}")).collect::<String>()),
        Value::Relationship(relationship) => format!("[{}:{}]", relationship.id, relationship.kind),
    }
}
