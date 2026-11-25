//! `WHERE` filters compiled into a flat program once per scan (D1342): columns resolved to
//! their positions in the row, constants made once, and the boolean structure as jumps over
//! a value stack, so a scan no longer walks the expression tree and looks columns up by name
//! for every row. The program decides only what it can decide exactly as the evaluator would;
//! for anything else it answers "undecided" and the scan asks the evaluator.

use crate::ast::{BinaryOperator, Expr, ExprKind, Literal, UnaryOperator};
use crate::eval::Scope;
use crate::types::{Value, compare_values};
use std::cmp::Ordering;

#[derive(Debug, Clone)]
enum Operand {
    Column(usize),
    Constant(Value),
}

#[derive(Debug, Clone)]
enum Node {
    Truth(Operand),
    Compare(BinaryOperator, Operand, Operand),
    IsNull(Operand, bool),
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
}

/// A compiled filter.
#[derive(Debug, Clone)]
pub(crate) struct Program {
    root: Node,
}

fn operand(scope: Scope<'_>, expr: &Expr) -> Option<Operand> {
    match &expr.kind {
        ExprKind::Column { qualifier, name } => scope.column(qualifier.as_deref(), name, expr.position).ok().map(Operand::Column),
        ExprKind::Slot { index, .. } => Some(Operand::Column(*index)),
        ExprKind::Literal(Literal::Integer(number)) => Some(Operand::Constant(Value::BigInt(*number))),
        ExprKind::Literal(Literal::Boolean(flag)) => Some(Operand::Constant(Value::Boolean(*flag))),
        ExprKind::Literal(Literal::Null) => Some(Operand::Constant(Value::Null)),
        _ => None,
    }
}

fn node(scope: Scope<'_>, expr: &Expr) -> Option<Node> {
    Some(match &expr.kind {
        ExprKind::And(operands) => Node::And(operands.iter().map(|operand| node(scope, operand)).collect::<Option<_>>()?),
        ExprKind::Or(operands) => Node::Or(operands.iter().map(|operand| node(scope, operand)).collect::<Option<_>>()?),
        ExprKind::Unary(UnaryOperator::Not, operand) => Node::Not(Box::new(node(scope, operand)?)),
        ExprKind::IsNull { expr, negated } => Node::IsNull(operand(scope, expr)?, *negated),
        ExprKind::Binary(
            operator @ (BinaryOperator::Equal
            | BinaryOperator::NotEqual
            | BinaryOperator::Less
            | BinaryOperator::LessOrEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterOrEqual),
            left,
            right,
        ) => Node::Compare(*operator, operand(scope, left)?, operand(scope, right)?),
        ExprKind::Column { .. } | ExprKind::Slot { .. } => Node::Truth(operand(scope, expr)?),
        _ => return None,
    })
}

/// The filter `expr` compiled for rows of `scope`, or `None` when it uses anything the
/// program does not run.
pub(crate) fn compile(scope: Scope<'_>, expr: &Expr) -> Option<Program> {
    node(scope, expr).map(|root| Program { root })
}

fn value<'r>(operand: &'r Operand, row: &'r [Value]) -> Option<&'r Value> {
    match operand {
        Operand::Column(index) => row.get(*index),
        Operand::Constant(value) => Some(value),
    }
}

fn exact_kind(value: &Value) -> Option<u8> {
    match value {
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => Some(0),
        Value::Boolean(_) => Some(1),
        _ => None,
    }
}

/// `Ok(Some(truth))`, with SQL's unknown as `None`, or `Err(())` when the evaluator must decide.
type Decided = Result<Option<bool>, ()>;

fn decide(node: &Node, row: &[Value]) -> Decided {
    match node {
        Node::Truth(operand) => match value(operand, row).ok_or(())? {
            Value::Null => Ok(None),
            Value::Boolean(flag) => Ok(Some(*flag)),
            _ => Err(()),
        },
        Node::IsNull(operand, negated) => Ok(Some(value(operand, row).ok_or(())?.is_null() != *negated)),
        Node::Compare(operator, left, right) => {
            let (left, right) = (value(left, row).ok_or(())?, value(right, row).ok_or(())?);
            if left.is_null() || right.is_null() {
                return Ok(None);
            }
            match (exact_kind(left), exact_kind(right)) {
                (Some(a), Some(b)) if a == b => {}
                _ => return Err(()),
            }
            let order = compare_values(left, right);
            Ok(Some(match operator {
                BinaryOperator::Equal => order == Ordering::Equal,
                BinaryOperator::NotEqual => order != Ordering::Equal,
                BinaryOperator::Less => order == Ordering::Less,
                BinaryOperator::LessOrEqual => order != Ordering::Greater,
                BinaryOperator::Greater => order == Ordering::Greater,
                BinaryOperator::GreaterOrEqual => order != Ordering::Less,
                _ => return Err(()),
            }))
        }
        Node::And(operands) => {
            let mut unknown = false;
            for operand in operands {
                match decide(operand, row)? {
                    Some(false) => return Ok(Some(false)),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            Ok(if unknown { None } else { Some(true) })
        }
        Node::Or(operands) => {
            let mut unknown = false;
            for operand in operands {
                match decide(operand, row)? {
                    Some(true) => return Ok(Some(true)),
                    None => unknown = true,
                    Some(false) => {}
                }
            }
            Ok(if unknown { None } else { Some(false) })
        }
        Node::Not(operand) => Ok(decide(operand, row)?.map(|truth| !truth)),
    }
}

impl Program {
    /// Whether `row` passes, or `None` when the evaluator must decide.
    pub(crate) fn passes(&self, row: &[Value]) -> Option<bool> {
        decide(&self.root, row).ok().map(|truth| truth == Some(true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(index: usize) -> Expr {
        Expr { kind: ExprKind::Slot { index, sql_type: None }, position: 0 }
    }

    fn integer(number: i64) -> Expr {
        Expr { kind: ExprKind::Literal(Literal::Integer(number)), position: 0 }
    }

    fn compare(operator: BinaryOperator, left: Expr, right: Expr) -> Expr {
        Expr { kind: ExprKind::Binary(operator, Box::new(left), Box::new(right)), position: 0 }
    }

    #[test]
    fn programs_answer_as_three_valued_logic_does_and_defer_the_rest() {
        let filter = Expr {
            kind: ExprKind::Or(vec![
                compare(BinaryOperator::Greater, slot(0), integer(10)),
                Expr {
                    kind: ExprKind::And(vec![
                        compare(BinaryOperator::Equal, slot(1), integer(3)),
                        Expr { kind: ExprKind::IsNull { expr: Box::new(slot(2)), negated: true }, position: 0 },
                    ]),
                    position: 0,
                },
            ]),
            position: 0,
        };
        let program = compile(Scope::empty(), &filter).unwrap();
        assert_eq!(program.passes(&[Value::Integer(11), Value::Null, Value::Null]), Some(true));
        assert_eq!(program.passes(&[Value::Integer(1), Value::BigInt(3), Value::Text("x".into())]), Some(true));
        assert_eq!(program.passes(&[Value::Integer(1), Value::BigInt(3), Value::Null]), Some(false));
        assert_eq!(program.passes(&[Value::Null, Value::Integer(4), Value::Null]), Some(false));
        assert_eq!(program.passes(&[Value::Text("a".into()), Value::Integer(4), Value::Null]), None);
        let unsupported = Expr { kind: ExprKind::Literal(Literal::String("x".into())), position: 0 };
        assert!(compile(Scope::empty(), &compare(BinaryOperator::Equal, slot(0), unsupported)).is_none());
    }
}
