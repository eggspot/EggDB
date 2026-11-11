use crate::ast::{Expr, ExprKind, FromClause, JoinCondition, JoinKind, TableReference};
use crate::error::{SqlError, SqlResult, SqlState};

/// The internal function that marks a column written with Oracle's `(+)`; its NUL-led name can't
/// be written in SQL, and the outer-join rewrite removes every one it accepts.
pub(super) const MARKER: &str = "\u{0}oracle_outer_join";

pub(super) fn marked(column: Expr) -> Expr {
    let position = column.position;
    Expr {
        kind: ExprKind::Function {
            name: MARKER.to_owned(),
            arguments: vec![column],
        },
        position,
    }
}

fn is_marker(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Function { name, .. } if name == MARKER)
}

pub(super) fn mentions_marker(expr: &Expr) -> bool {
    is_marker(expr) || crate::eval::children(expr).into_iter().any(mentions_marker)
}

fn not_allowed_here(position: usize) -> SqlError {
    SqlError::syntax("outer join operator (+) is not allowed here", position).with_oracle_code(30563)
}

fn marked_labels(expr: &Expr, labels: &mut Vec<String>) -> SqlResult<()> {
    if let ExprKind::Function { name, arguments } = &expr.kind
        && name == MARKER
    {
        return match arguments.first().map(|column| &column.kind) {
            Some(ExprKind::Column {
                qualifier: Some(qualifier),
                ..
            }) => {
                if !labels.contains(qualifier) {
                    labels.push(qualifier.clone());
                }
                Ok(())
            }
            _ => Err(SqlError::not_supported(
                "(+) on a column without its table's name or alias",
            )
            .at(expr.position)),
        };
    }
    for child in crate::eval::children(expr) {
        marked_labels(child, labels)?;
    }
    Ok(())
}

fn stripped(mut expr: Expr) -> Expr {
    if let ExprKind::Function { name, arguments } = &mut expr.kind
        && name == MARKER
        && let Some(column) = arguments.pop()
    {
        return column;
    }
    for child in crate::eval::children_mut(&mut expr) {
        let taken = std::mem::replace(
            child,
            Expr {
                kind: ExprKind::Literal(crate::ast::Literal::Null),
                position: 0,
            },
        );
        *child = stripped(taken);
    }
    expr
}

fn conjuncts(filter: Expr) -> Vec<Expr> {
    match filter.kind {
        ExprKind::And(operands) => operands.into_iter().flat_map(conjuncts).collect(),
        kind => vec![Expr {
            kind,
            position: filter.position,
        }],
    }
}

fn joined(mut conditions: Vec<Expr>) -> Option<Expr> {
    match conditions.len() {
        0 => None,
        1 => conditions.pop(),
        _ => {
            let position = conditions[0].position;
            Some(Expr {
                kind: ExprKind::And(conditions),
                position,
            })
        }
    }
}

fn labelled(reference: &TableReference, label: &str) -> bool {
    match &reference.alias {
        Some(alias) => alias == label,
        None => crate::schemas::base_name(&reference.name.text) == label,
    }
}

/// Turns Oracle's `(+)` outer joins into joins: each `WHERE` conjunct that marks columns of one
/// table moves into that table's join condition, and its comma join becomes a `LEFT JOIN` (a
/// `RIGHT JOIN` when the marked table is the first of two).
pub(super) fn rewrite(
    from: Option<FromClause>,
    filter: Option<Expr>,
) -> SqlResult<(Option<FromClause>, Option<Expr>)> {
    let (mut from, filter) = match (from, filter) {
        (Some(from), Some(filter)) => (from, filter),
        (from, filter) => {
            if let Some(filter) = &filter
                && mentions_marker(filter)
            {
                return Err(not_allowed_here(filter.position));
            }
            return Ok((from, filter));
        }
    };
    if !mentions_marker(&filter) {
        return Ok((Some(from), Some(filter)));
    }
    let mut kept = Vec::new();
    let mut outer: Vec<(String, Vec<Expr>)> = Vec::new();
    for conjunct in conjuncts(filter) {
        let mut labels = Vec::new();
        marked_labels(&conjunct, &mut labels)?;
        match labels.as_slice() {
            [] => kept.push(conjunct),
            [label] => {
                let condition = stripped(conjunct);
                match outer.iter_mut().find(|(known, _)| known == label) {
                    Some((_, conditions)) => conditions.push(condition),
                    None => outer.push((label.clone(), vec![condition])),
                }
            }
            _ => {
                return Err(SqlError::new(
                    SqlState::SYNTAX_ERROR,
                    "a predicate may reference only one outer-joined table",
                )
                .with_oracle_code(1468)
                .at(conjunct.position));
            }
        }
    }
    for (label, conditions) in outer {
        let Some(condition) = joined(conditions) else {
            continue;
        };
        let position = condition.position;
        if let Some(join) = from
            .joins
            .iter_mut()
            .find(|join| labelled(&join.table, &label))
        {
            if join.kind != JoinKind::Comma {
                return Err(SqlError::syntax(
                    "old style outer join (+) cannot be used with ANSI joins",
                    position,
                )
                .with_oracle_code(25156));
            }
            join.kind = JoinKind::Left;
            join.condition = JoinCondition::On(condition);
            continue;
        }
        if labelled(&from.first, &label) {
            if from.joins.len() == 1 && from.joins[0].kind == JoinKind::Comma {
                from.joins[0].kind = JoinKind::Right;
                from.joins[0].condition = JoinCondition::On(condition);
                continue;
            }
            return Err(SqlError::not_supported(
                "(+) on the first of more than two tables: list the outer-joined table later",
            )
            .at(position));
        }
        return Err(SqlError::new(
            SqlState::UNDEFINED_TABLE,
            format!("missing FROM-clause entry for table \"{label}\""),
        )
        .with_oracle_code(904)
        .at(position));
    }
    Ok((Some(from), joined(kept)))
}
