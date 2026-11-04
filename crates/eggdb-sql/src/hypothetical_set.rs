use crate::ast::{AggregateFunction, Expr, ExprKind, Literal, OrderItem};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::Evaluator;
use crate::types::{
    SortDirection, Value, compare_in_directions, decode_directions, encode_directions, equality_key,
};
use std::cmp::Ordering;
use std::collections::HashSet;

pub(crate) const HYPOTHETICAL_ARGUMENTS: &str = "hypothetical_set_arguments";

pub(crate) fn arguments(direct: Vec<Expr>, order: Vec<OrderItem>, position: usize) -> Expr {
    let directions = encode_directions(&order.iter().map(SortDirection::of).collect::<Vec<_>>());
    let mut arguments = Vec::with_capacity(direct.len() + order.len() + 1);
    arguments.push(Expr {
        kind: ExprKind::Literal(Literal::String(directions)),
        position,
    });
    arguments.extend(direct);
    arguments.extend(order.into_iter().map(|item| item.expr));
    Expr {
        kind: ExprKind::Function {
            name: HYPOTHETICAL_ARGUMENTS.to_owned(),
            arguments,
        },
        position,
    }
}

struct Parts<'a> {
    directions: Vec<SortDirection>,
    hypothetical: &'a [Expr],
    keys: &'a [Expr],
}

fn parts(argument: &Expr) -> Option<Parts<'_>> {
    let ExprKind::Function { name, arguments } = &argument.kind else {
        return None;
    };
    if name != HYPOTHETICAL_ARGUMENTS {
        return None;
    }
    let [directions, rest @ ..] = arguments.as_slice() else {
        return None;
    };
    let ExprKind::Literal(Literal::String(directions)) = &directions.kind else {
        return None;
    };
    let width = rest.len() / 2;
    let (hypothetical, keys) = rest.split_at(width);
    Some(Parts {
        directions: decode_directions(directions),
        hypothetical,
        keys,
    })
}

#[derive(Clone, Default)]
pub(crate) struct Standing {
    rows: i64,
    before: i64,
    peers: i64,
    distinct_before: HashSet<Vec<u8>>,
    held: usize,
}

impl Standing {
    pub(crate) fn held_bytes(&self) -> usize {
        self.held
    }

    pub(crate) fn add_row(
        &mut self,
        function: AggregateFunction,
        argument: &Expr,
        evaluator: &Evaluator<'_>,
    ) -> SqlResult<()> {
        let Some(parts) = parts(argument) else {
            return Ok(());
        };
        let mut keys = Vec::with_capacity(parts.keys.len());
        let mut hypothetical = Vec::with_capacity(parts.hypothetical.len());
        for (key, value) in parts.keys.iter().zip(parts.hypothetical) {
            let key = evaluator.eval(key, None)?;
            let given = evaluator.eval(value, key.sql_type())?;
            unmatched_types(&key, &given, value.position)?;
            hypothetical.push(given);
            keys.push(key);
        }
        self.rows = self.rows.saturating_add(1);
        match compare_in_directions(&parts.directions, &keys, &hypothetical) {
            Ordering::Less => {
                self.before = self.before.saturating_add(1);
                if function == AggregateFunction::DenseRank {
                    let key = distinct_key(&keys);
                    let bytes = key.len() + 32;
                    if self.distinct_before.insert(key) {
                        self.held = self.held.saturating_add(bytes);
                    }
                }
            }
            Ordering::Equal => self.peers = self.peers.saturating_add(1),
            Ordering::Greater => {}
        }
        Ok(())
    }

    pub(crate) fn result(&self, function: AggregateFunction) -> Value {
        match function {
            AggregateFunction::Rank => Value::BigInt(self.before.saturating_add(1)),
            AggregateFunction::DenseRank => Value::BigInt(
                i64::try_from(self.distinct_before.len())
                    .unwrap_or(i64::MAX)
                    .saturating_add(1),
            ),
            AggregateFunction::PercentRank if self.rows == 0 => Value::Double(0.0),
            AggregateFunction::PercentRank => Value::Double(self.before as f64 / self.rows as f64),
            AggregateFunction::CumeDist => Value::Double(
                self.before.saturating_add(self.peers).saturating_add(1) as f64
                    / self.rows.saturating_add(1) as f64,
            ),
            _ => Value::Null,
        }
    }
}

fn unmatched_types(key: &Value, given: &Value, position: usize) -> SqlResult<()> {
    if crate::constraints::current_dialect() != crate::Dialect::PostgreSql {
        return Ok(());
    }
    let (Some(key_type), Some(given_type)) = (key.sql_type(), given.sql_type()) else {
        return Ok(());
    };
    if crate::set_operations::common_type(key_type, given_type).is_some() {
        return Ok(());
    }
    Err(SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!(
            "WITHIN GROUP types {} and {} cannot be matched",
            key_type.name(),
            given_type.name()
        ),
    )
    .at(position))
}

fn distinct_key(keys: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut part = Vec::new();
    for key in keys {
        part.clear();
        equality_key(&mut part, key);
        out.extend_from_slice(&(part.len() as u64).to_le_bytes());
        out.extend_from_slice(&part);
    }
    out
}
