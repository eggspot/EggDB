use crate::ast::{Expr, ExprKind, Literal, OrderItem};
use crate::eval::children;
use crate::types::{
    SortDirection, Value, compare_in_directions, decode_directions, encode_directions,
};
use std::cmp::Ordering;

pub(crate) const WINDOW_CALL: &str = "window_call";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowFunction {
    RowNumber,
    Rank,
    DenseRank,
}

impl WindowFunction {
    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "row_number" => Self::RowNumber,
            "rank" => Self::Rank,
            "dense_rank" => Self::DenseRank,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::RowNumber => "row_number",
            Self::Rank => "rank",
            Self::DenseRank => "dense_rank",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WindowSpec {
    pub(crate) function: WindowFunction,
    pub(crate) partition: usize,
    pub(crate) order: Vec<SortDirection>,
}

pub(crate) fn call(
    function: WindowFunction,
    partition: Vec<Expr>,
    order: Vec<OrderItem>,
    position: usize,
) -> Expr {
    let literal = |literal: Literal| Expr {
        kind: ExprKind::Literal(literal),
        position,
    };
    let directions = order.iter().map(SortDirection::of).collect::<Vec<_>>();
    let mut arguments = vec![
        literal(Literal::String(function.name().to_owned())),
        literal(Literal::String(encode_directions(&directions))),
        literal(Literal::Integer(partition.len() as i64)),
    ];
    arguments.extend(partition);
    arguments.extend(order.into_iter().map(|item| item.expr));
    Expr {
        kind: ExprKind::Function {
            name: WINDOW_CALL.to_owned(),
            arguments,
        },
        position,
    }
}

pub(crate) fn is_call(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Function { name, .. } if name == WINDOW_CALL)
}

pub(crate) fn first_call(expr: &Expr) -> Option<&Expr> {
    if is_call(expr) {
        return Some(expr);
    }
    children(expr).into_iter().find_map(first_call)
}

pub(crate) fn split(expr: Expr) -> Option<(WindowSpec, Vec<Expr>)> {
    let ExprKind::Function { name, arguments } = expr.kind else {
        return None;
    };
    if name != WINDOW_CALL {
        return None;
    }
    let mut arguments = arguments.into_iter();
    let (
        Some(ExprKind::Literal(Literal::String(function))),
        Some(ExprKind::Literal(Literal::String(directions))),
        Some(ExprKind::Literal(Literal::Integer(partition))),
    ) = (
        arguments.next().map(|argument| argument.kind),
        arguments.next().map(|argument| argument.kind),
        arguments.next().map(|argument| argument.kind),
    )
    else {
        return None;
    };
    let spec = WindowSpec {
        function: WindowFunction::named(&function)?,
        partition: usize::try_from(partition).ok()?,
        order: decode_directions(&directions),
    };
    Some((spec, arguments.collect()))
}

fn same_partition(spec: &WindowSpec, left: &[Value], right: &[Value]) -> bool {
    let partition = spec.partition.min(left.len()).min(right.len());
    left.iter().zip(right).take(partition).all(|(left, right)| {
        match (left.is_null(), right.is_null()) {
            (true, true) => true,
            (false, false) => crate::types::compare_values(left, right) == Ordering::Equal,
            _ => false,
        }
    })
}

fn compare_rows(
    spec: &WindowSpec,
    partition: &[SortDirection],
    left: &[Value],
    right: &[Value],
) -> Ordering {
    compare_in_directions(partition, left, right).then_with(|| {
        compare_in_directions(
            &spec.order,
            left.get(spec.partition..).unwrap_or(&[]),
            right.get(spec.partition..).unwrap_or(&[]),
        )
    })
}

pub(crate) fn numbers(spec: &WindowSpec, keys: &[Vec<Value>]) -> Vec<i64> {
    let partition = vec![
        SortDirection {
            descending: false,
            nulls_first: false,
        };
        spec.partition
    ];
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by(|&left, &right| compare_rows(spec, &partition, &keys[left], &keys[right]));
    let mut numbers = vec![0; keys.len()];
    let mut previous: Option<usize> = None;
    let mut row_number: i64 = 0;
    let mut rank: i64 = 0;
    let mut dense_rank: i64 = 0;
    for &at in &order {
        let row = &keys[at];
        match previous.map(|before| &keys[before]) {
            Some(before) if same_partition(spec, before, row) => {
                row_number += 1;
                let peers = compare_in_directions(
                    &spec.order,
                    before.get(spec.partition..).unwrap_or(&[]),
                    row.get(spec.partition..).unwrap_or(&[]),
                ) == Ordering::Equal;
                if !peers {
                    rank = row_number;
                    dense_rank += 1;
                }
            }
            _ => {
                row_number = 1;
                rank = 1;
                dense_rank = 1;
            }
        }
        if let Some(slot) = numbers.get_mut(at) {
            *slot = match spec.function {
                WindowFunction::RowNumber => row_number,
                WindowFunction::Rank => rank,
                WindowFunction::DenseRank => dense_rank,
            };
        }
        previous = Some(at);
    }
    numbers
}

#[cfg(test)]
mod tests {
    use super::{WindowFunction, WindowSpec, numbers};
    use crate::types::{SortDirection, Value};

    fn spec(function: WindowFunction) -> WindowSpec {
        WindowSpec {
            function,
            partition: 1,
            order: vec![SortDirection {
                descending: false,
                nulls_first: false,
            }],
        }
    }

    fn keys() -> Vec<Vec<Value>> {
        [(1, 30), (2, 10), (1, 10), (1, 30), (2, 20), (1, 40)]
            .into_iter()
            .map(|(group, value)| vec![Value::Integer(group), Value::Integer(value)])
            .collect()
    }

    #[test]
    fn numbers_count_within_each_partition_in_window_order() {
        assert_eq!(
            numbers(&spec(WindowFunction::RowNumber), &keys()),
            [2, 1, 1, 3, 2, 4]
        );
        assert_eq!(
            numbers(&spec(WindowFunction::Rank), &keys()),
            [2, 1, 1, 2, 2, 4]
        );
        assert_eq!(
            numbers(&spec(WindowFunction::DenseRank), &keys()),
            [2, 1, 1, 2, 2, 3]
        );
    }
}
