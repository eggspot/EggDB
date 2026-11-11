use super::Parser;
use crate::ast::{
    BinaryOperator, DerivedTable, Expr, ExprKind, FromClause, Join, JoinCondition, JoinKind,
    Literal, Name, OrderItem, Query, Select, SelectItem, TableReference,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult};

fn mentions_label(expr: &Expr, label: &str) -> bool {
    if let ExprKind::Column {
        qualifier: Some(qualifier),
        ..
    } = &expr.kind
        && qualifier == label
    {
        return true;
    }
    crate::eval::children(expr)
        .into_iter()
        .any(|child| mentions_label(child, label))
}

fn conjuncts(expr: &Expr) -> Vec<&Expr> {
    match &expr.kind {
        ExprKind::And(operands) => operands.iter().flat_map(conjuncts).collect(),
        _ => vec![expr],
    }
}

fn ordering(condition: &Expr, label: &str) -> SqlResult<(Expr, bool)> {
    let mut found = None;
    for conjunct in conjuncts(condition) {
        let ExprKind::Binary(operator, left, right) = &conjunct.kind else {
            continue;
        };
        if !matches!(
            operator,
            BinaryOperator::Less
                | BinaryOperator::LessOrEqual
                | BinaryOperator::Greater
                | BinaryOperator::GreaterOrEqual
        ) {
            continue;
        }
        let (inner, left_side_is_inner) = match (mentions_label(left, label), mentions_label(right, label)) {
            (false, true) => ((**right).clone(), false),
            (true, false) => ((**left).clone(), true),
            _ => continue,
        };
        let inner_below = matches!(
            (operator, left_side_is_inner),
            (BinaryOperator::Greater | BinaryOperator::GreaterOrEqual, false)
                | (BinaryOperator::Less | BinaryOperator::LessOrEqual, true)
        );
        if found.is_some() {
            return Err(SqlError::syntax(
                "an ASOF join takes exactly one inequality",
                conjunct.position,
            ));
        }
        found = Some((inner, inner_below));
    }
    found.ok_or_else(|| {
        SqlError::syntax(
            "an ASOF join needs one inequality between the two tables",
            condition.position,
        )
    })
}

fn qualified(label: &str, column: &Name) -> Expr {
    Expr {
        kind: ExprKind::Column {
            qualifier: Some(label.to_owned()),
            name: column.text.clone(),
        },
        position: column.position,
    }
}

fn using_condition(columns: &[Name], left: &str, right: &str, position: usize) -> SqlResult<Expr> {
    let Some((time, keys)) = columns.split_last() else {
        return Err(SqlError::syntax("ASOF JOIN ... USING needs a column", position));
    };
    if left == right {
        return Err(SqlError::syntax(
            "ASOF JOIN ... USING needs an alias for one of the two tables",
            position,
        ));
    }
    let compared = |operator: BinaryOperator, column: &Name| Expr {
        kind: ExprKind::Binary(
            operator,
            Box::new(qualified(left, column)),
            Box::new(qualified(right, column)),
        ),
        position: column.position,
    };
    let mut conjuncts: Vec<Expr> = keys
        .iter()
        .map(|column| compared(BinaryOperator::Equal, column))
        .collect();
    conjuncts.push(compared(BinaryOperator::GreaterOrEqual, time));
    Ok(if conjuncts.len() == 1 {
        conjuncts.remove(0)
    } else {
        Expr {
            kind: ExprKind::And(conjuncts),
            position,
        }
    })
}

impl Parser<'_> {
    pub(super) fn at_asof_join(&self) -> bool {
        self.postgresql()
            && self.is_word("asof")
            && (self.is_word_at(1, "join")
                || (self.is_word_at(1, "left")
                    && (self.is_word_at(2, "join") || self.is_word_at(2, "outer"))))
    }

    /// DuckDB's `ASOF [LEFT] JOIN table ON equalities AND one inequality`, or its `USING (keys,
    /// time)` form, as a lateral subquery taking, for each row, the one row of `table` nearest on
    /// the inequality's side. `left_label` names the table on the left, which `USING` compares
    /// with.
    pub(super) fn asof_join(&mut self, left_label: &str) -> SqlResult<Join> {
        let position = self.position();
        self.at += 1;
        let keep_unmatched = self.take_word("left");
        if keep_unmatched {
            self.take_word("outer");
        }
        self.expect_word("join")?;
        let table = self.table_or_function()?;
        if table.derived.is_some() || table.generated.is_some() {
            return Err(self.not_supported("ASOF JOIN of a subquery or function"));
        }
        let label = table
            .alias
            .clone()
            .unwrap_or_else(|| crate::schemas::base_name(&table.name.text).to_owned());
        let condition = if self.take_word("using") {
            let columns = self.name_list()?;
            using_condition(&columns, left_label, &label, position)?
        } else {
            self.expect_word("on")?;
            self.expression()?
        };
        let condition = self.condition(condition);
        let (nearest, inner_below) = ordering(&condition, &label)?;
        let inner = TableReference {
            alias: Some(label.clone()),
            ..table
        };
        let select = Select {
            distinct: false,
            items: vec![SelectItem::Wildcard {
                qualifier: None,
                position,
            }],
            from: Some(FromClause {
                first: inner,
                joins: Vec::new(),
            }),
            filter: Some(condition),
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            source_row_limit: None,
            grouping_sets: None,
            distinct_on: Vec::new(),
        };
        let mut query = Query::of_select(select, position);
        query.order_by = vec![OrderItem {
            expr: nearest,
            descending: inner_below,
            nulls_first: Some(false),
        }];
        query.limit = Some(Expr {
            kind: ExprKind::Literal(Literal::Integer(1)),
            position,
        });
        let derived = TableReference {
            name: Name {
                text: label.clone(),
                position,
            },
            alias: Some(label),
            derived: Some(Box::new(DerivedTable {
                query,
                column_names: Vec::new(),
                dialect: Dialect::PostgreSql,
                lateral: true,
                hierarchy: None,
            })),
            generated: None,
            partition_selection: None,
        };
        Ok(if keep_unmatched {
            Join {
                kind: JoinKind::Left,
                table: derived,
                condition: JoinCondition::On(Expr {
                    kind: ExprKind::Literal(Literal::Boolean(true)),
                    position,
                }),
            }
        } else {
            Join {
                kind: JoinKind::Cross,
                table: derived,
                condition: JoinCondition::Always,
            }
        })
    }
}
