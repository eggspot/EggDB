use crate::ast::{Expr, ExprKind};

/// The operands of one expression, in the order they appear in it: the fixed operands, then a
/// list (the values of `IN (...)`, the arguments of a call, the parts of `AND` and `OR`), then
/// each `WHEN` condition with its result, then `ELSE`. Gathering them allocates nothing.
pub(crate) struct Children<'e> {
    fixed: [Option<&'e Expr>; 3],
    list: &'e [Expr],
    branches: &'e [(Expr, Expr)],
    otherwise: Option<&'e Expr>,
}

/// [`Children`] borrowed mutably.
pub(crate) struct ChildrenMut<'e> {
    fixed: [Option<&'e mut Expr>; 3],
    list: &'e mut [Expr],
    branches: &'e mut [(Expr, Expr)],
    otherwise: Option<&'e mut Expr>,
}

impl<'e> Children<'e> {
    fn fixed(fixed: [Option<&'e Expr>; 3]) -> Self {
        Self {
            fixed,
            list: &[],
            branches: &[],
            otherwise: None,
        }
    }

    fn list(first: Option<&'e Expr>, list: &'e [Expr]) -> Self {
        Self {
            list,
            ..Self::fixed([first, None, None])
        }
    }
}

impl<'e> ChildrenMut<'e> {
    fn fixed(fixed: [Option<&'e mut Expr>; 3]) -> Self {
        Self {
            fixed,
            list: &mut [],
            branches: &mut [],
            otherwise: None,
        }
    }

    fn list(first: Option<&'e mut Expr>, list: &'e mut [Expr]) -> Self {
        Self {
            list,
            ..Self::fixed([first, None, None])
        }
    }
}

/// Walks [`Children`] in order.
pub(crate) struct ChildIter<'e> {
    fixed: std::array::IntoIter<Option<&'e Expr>, 3>,
    list: std::slice::Iter<'e, Expr>,
    branches: std::slice::Iter<'e, (Expr, Expr)>,
    result: Option<&'e Expr>,
    otherwise: Option<&'e Expr>,
}

/// Walks [`ChildrenMut`] in order.
pub(crate) struct ChildIterMut<'e> {
    fixed: std::array::IntoIter<Option<&'e mut Expr>, 3>,
    list: std::slice::IterMut<'e, Expr>,
    branches: std::slice::IterMut<'e, (Expr, Expr)>,
    result: Option<&'e mut Expr>,
    otherwise: Option<&'e mut Expr>,
}

impl<'e> IntoIterator for Children<'e> {
    type Item = &'e Expr;
    type IntoIter = ChildIter<'e>;

    fn into_iter(self) -> ChildIter<'e> {
        ChildIter {
            fixed: self.fixed.into_iter(),
            list: self.list.iter(),
            branches: self.branches.iter(),
            result: None,
            otherwise: self.otherwise,
        }
    }
}

impl<'e> IntoIterator for ChildrenMut<'e> {
    type Item = &'e mut Expr;
    type IntoIter = ChildIterMut<'e>;

    fn into_iter(self) -> ChildIterMut<'e> {
        ChildIterMut {
            fixed: self.fixed.into_iter(),
            list: self.list.iter_mut(),
            branches: self.branches.iter_mut(),
            result: None,
            otherwise: self.otherwise,
        }
    }
}

impl<'e> Iterator for ChildIter<'e> {
    type Item = &'e Expr;

    fn next(&mut self) -> Option<&'e Expr> {
        if let Some(fixed) = self.fixed.by_ref().flatten().next() {
            return Some(fixed);
        }
        if let Some(listed) = self.list.next() {
            return Some(listed);
        }
        if let Some(result) = self.result.take() {
            return Some(result);
        }
        if let Some((condition, result)) = self.branches.next() {
            self.result = Some(result);
            return Some(condition);
        }
        self.otherwise.take()
    }
}

impl<'e> Iterator for ChildIterMut<'e> {
    type Item = &'e mut Expr;

    fn next(&mut self) -> Option<&'e mut Expr> {
        if let Some(fixed) = self.fixed.by_ref().flatten().next() {
            return Some(fixed);
        }
        if let Some(listed) = self.list.next() {
            return Some(listed);
        }
        if let Some(result) = self.result.take() {
            return Some(result);
        }
        if let Some((condition, result)) = self.branches.next() {
            self.result = Some(result);
            return Some(condition);
        }
        self.otherwise.take()
    }
}

/// The operands of `expr`, in order (see [`Children`]).
pub(crate) fn children(expr: &Expr) -> Children<'_> {
    match &expr.kind {
        ExprKind::Literal(_)
        | ExprKind::Column { .. }
        | ExprKind::Slot { .. }
        | ExprKind::SessionValue { .. }
        | ExprKind::CurrentSetting { .. }
        | ExprKind::OuterColumn { .. }
        | ExprKind::Constant { .. } => Children::fixed([None, None, None]),
        ExprKind::Subquery(subquery) => Children::fixed([subquery.test.operand(), None, None]),
        ExprKind::Unary(_, operand)
        | ExprKind::IsNull { expr: operand, .. }
        | ExprKind::IsTruth { expr: operand, .. }
        | ExprKind::Cast { expr: operand, .. } => Children::fixed([Some(operand), None, None]),
        ExprKind::Binary(_, left, right)
        | ExprKind::IsDistinctFrom { left, right, .. }
        | ExprKind::Like {
            expr: left,
            pattern: right,
            ..
        } => Children::fixed([Some(left), Some(right), None]),
        ExprKind::Between {
            expr, low, high, ..
        } => Children::fixed([Some(expr), Some(low), Some(high)]),
        ExprKind::InList { expr, list, .. } => Children::list(Some(expr), list),
        ExprKind::And(operands) | ExprKind::Or(operands) => Children::list(None, operands),
        ExprKind::Case {
            operand,
            branches,
            otherwise,
        } => Children {
            branches,
            otherwise: otherwise.as_deref(),
            ..Children::fixed([operand.as_deref(), None, None])
        },
        ExprKind::Function { arguments, .. }
        | ExprKind::SqliteFunction { arguments, .. }
        | ExprKind::OracleFunction { arguments, .. }
        | ExprKind::SqlServerFunction { arguments, .. } => Children::list(None, arguments),
        ExprKind::Aggregate { argument, .. } => Children::fixed([argument.as_deref(), None, None]),
    }
}

/// The operands of `expr`, in order, borrowed mutably (see [`Children`]).
pub(crate) fn children_mut(expr: &mut Expr) -> ChildrenMut<'_> {
    match &mut expr.kind {
        ExprKind::Literal(_)
        | ExprKind::Column { .. }
        | ExprKind::Slot { .. }
        | ExprKind::SessionValue { .. }
        | ExprKind::CurrentSetting { .. }
        | ExprKind::OuterColumn { .. }
        | ExprKind::Constant { .. } => ChildrenMut::fixed([None, None, None]),
        ExprKind::Subquery(subquery) => {
            ChildrenMut::fixed([subquery.test.operand_mut(), None, None])
        }
        ExprKind::Unary(_, operand)
        | ExprKind::IsNull { expr: operand, .. }
        | ExprKind::IsTruth { expr: operand, .. }
        | ExprKind::Cast { expr: operand, .. } => ChildrenMut::fixed([Some(operand), None, None]),
        ExprKind::Binary(_, left, right)
        | ExprKind::IsDistinctFrom { left, right, .. }
        | ExprKind::Like {
            expr: left,
            pattern: right,
            ..
        } => ChildrenMut::fixed([Some(left), Some(right), None]),
        ExprKind::Between {
            expr, low, high, ..
        } => ChildrenMut::fixed([Some(expr), Some(low), Some(high)]),
        ExprKind::InList { expr, list, .. } => ChildrenMut::list(Some(expr), list),
        ExprKind::And(operands) | ExprKind::Or(operands) => ChildrenMut::list(None, operands),
        ExprKind::Case {
            operand,
            branches,
            otherwise,
        } => ChildrenMut {
            branches,
            otherwise: otherwise.as_deref_mut(),
            ..ChildrenMut::fixed([operand.as_deref_mut(), None, None])
        },
        ExprKind::Function { arguments, .. }
        | ExprKind::SqliteFunction { arguments, .. }
        | ExprKind::OracleFunction { arguments, .. }
        | ExprKind::SqlServerFunction { arguments, .. } => ChildrenMut::list(None, arguments),
        ExprKind::Aggregate { argument, .. } => {
            ChildrenMut::fixed([argument.as_deref_mut(), None, None])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{children, children_mut};
    use crate::ast::{Expr, ExprKind, Literal};

    fn number(value: i64) -> Expr {
        Expr {
            kind: ExprKind::Literal(Literal::Integer(value)),
            position: 0,
        }
    }

    fn numbers_of(expr: &Expr) -> Vec<i64> {
        children(expr)
            .into_iter()
            .map(|child| match &child.kind {
                ExprKind::Literal(Literal::Integer(value)) => *value,
                other => panic!("not a number: {other:?}"),
            })
            .collect()
    }

    fn numbers_of_mut(expr: &mut Expr) -> Vec<i64> {
        children_mut(expr)
            .into_iter()
            .map(|child| match &child.kind {
                ExprKind::Literal(Literal::Integer(value)) => *value,
                other => panic!("not a number: {other:?}"),
            })
            .collect()
    }

    fn assert_children(mut expr: Expr, expected: &[i64]) {
        assert_eq!(numbers_of(&expr), expected);
        assert_eq!(numbers_of_mut(&mut expr), expected);
    }

    fn of(kind: ExprKind) -> Expr {
        Expr { kind, position: 0 }
    }

    #[test]
    fn a_case_gives_its_operand_then_each_condition_and_result_then_its_else() {
        assert_children(
            of(ExprKind::Case {
                operand: Some(Box::new(number(1))),
                branches: vec![(number(2), number(3)), (number(4), number(5))],
                otherwise: Some(Box::new(number(6))),
            }),
            &[1, 2, 3, 4, 5, 6],
        );
        assert_children(
            of(ExprKind::Case {
                operand: None,
                branches: vec![(number(2), number(3))],
                otherwise: None,
            }),
            &[2, 3],
        );
    }

    #[test]
    fn a_list_comes_after_the_tested_value() {
        assert_children(
            of(ExprKind::InList {
                expr: Box::new(number(1)),
                list: vec![number(2), number(3)],
                negated: false,
            }),
            &[1, 2, 3],
        );
        assert_children(
            of(ExprKind::InList {
                expr: Box::new(number(1)),
                list: Vec::new(),
                negated: true,
            }),
            &[1],
        );
        assert_children(of(ExprKind::And(vec![number(1), number(2)])), &[1, 2]);
        assert_children(
            of(ExprKind::Function {
                name: "f".into(),
                arguments: vec![number(1), number(2), number(3), number(4)],
            }),
            &[1, 2, 3, 4],
        );
    }

    #[test]
    fn fixed_operands_keep_their_written_order() {
        assert_children(
            of(ExprKind::Between {
                expr: Box::new(number(1)),
                low: Box::new(number(2)),
                high: Box::new(number(3)),
                negated: false,
            }),
            &[1, 2, 3],
        );
        assert_children(
            of(ExprKind::Binary(
                crate::ast::BinaryOperator::Add,
                Box::new(number(1)),
                Box::new(number(2)),
            )),
            &[1, 2],
        );
        assert_children(number(1), &[]);
    }

    #[test]
    fn an_aggregate_gives_its_argument_when_it_has_one() {
        assert_children(
            of(ExprKind::Aggregate {
                function: crate::ast::AggregateFunction::Sum,
                argument: Some(Box::new(number(1))),
                distinct: false,
            }),
            &[1],
        );
        assert_children(
            of(ExprKind::Aggregate {
                function: crate::ast::AggregateFunction::Count,
                argument: None,
                distinct: false,
            }),
            &[],
        );
    }
}
