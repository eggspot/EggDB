use super::subplans::{Subplan, SubplanRole, subplan_reference};
use crate::ast::{
    BinaryOperator, Expr, ExprKind, Literal, Subquery, SubqueryTest, TruthTest, UnaryOperator,
};
use crate::types::Value;

const MOST_DEPTH: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Spelling {
    PostgreSql,
    MySql,
}

pub(crate) struct ExpressionText<'a> {
    pub(crate) spelling: Spelling,
    pub(crate) qualifier: Option<&'a str>,
    pub(crate) bare_columns: bool,
    pub(crate) subplans: &'a [Subplan],
    pub(crate) select_numbers: &'a [(usize, usize)],
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

impl ExpressionText<'_> {
    pub(crate) fn text(&self, expr: &Expr) -> String {
        self.at_depth(expr, 0)
    }

    fn word(&self, postgresql: &'static str, mysql: &'static str) -> &'static str {
        match self.spelling {
            Spelling::PostgreSql => postgresql,
            Spelling::MySql => mysql,
        }
    }

    fn literal(&self, literal: &Literal) -> String {
        match literal {
            Literal::Null => "NULL".to_owned(),
            Literal::Boolean(value) => value.to_string(),
            Literal::Integer(value) => value.to_string(),
            Literal::Decimal(text) => text.clone(),
            Literal::String(text) => match self.spelling {
                Spelling::PostgreSql => format!("{}::text", quoted(text)),
                Spelling::MySql => quoted(text),
            },
        }
    }

    fn constant(value: &Value) -> String {
        match value {
            Value::Null => "NULL".to_owned(),
            Value::Text(text) => quoted(text),
            other => other.to_text().unwrap_or_else(|| "?".to_owned()),
        }
    }

    fn joined(&self, exprs: &[Expr], separator: &str, depth: usize) -> String {
        exprs
            .iter()
            .map(|expr| self.at_depth(expr, depth))
            .collect::<Vec<_>>()
            .join(separator)
    }

    fn subquery(&self, subquery: &Subquery, depth: usize) -> String {
        let position = subquery.query.position;
        if self.spelling == Spelling::PostgreSql {
            let Some(subplan) = self
                .subplans
                .iter()
                .find(|subplan| subplan.position == position)
            else {
                return "(SubPlan)".to_owned();
            };
            if let (
                SubqueryTest::RowCompare { expr, operator },
                SubplanRole::Init { parameter, .. },
            ) = (&subquery.test, subplan.role)
                && let Some(fields) = crate::row_values::constructor_fields(expr)
            {
                return self.row_against_parameters(fields, *operator, parameter, depth);
            }
            return subplan_reference(subplan, &subquery.test);
        }
        let select = self
            .select_numbers
            .iter()
            .find(|(at, _)| *at == position)
            .map_or_else(
                || "select".to_owned(),
                |(_, number)| format!("select #{number}"),
            );
        match &subquery.test {
            SubqueryTest::Scalar => format!("({select})"),
            SubqueryTest::Exists => format!("exists({select})"),
            SubqueryTest::Array => format!("array({select})"),
            SubqueryTest::In { expr, negated } => format!(
                "({} {}in ({select}))",
                self.at_depth(expr, depth),
                if *negated { "not " } else { "" }
            ),
            SubqueryTest::Compare {
                expr,
                operator,
                all,
            } => format!(
                "({} {} {} ({select}))",
                self.at_depth(expr, depth),
                operator.symbol(),
                if *all { "all" } else { "any" }
            ),
            _ => format!("({select})"),
        }
    }

    fn row_against_parameters(
        &self,
        fields: &[Expr],
        operator: BinaryOperator,
        first: usize,
        depth: usize,
    ) -> String {
        let parameter = |at: usize| format!("${}", first.saturating_add(at));
        let symbol = operator.symbol();
        let pairs = || {
            fields.iter().enumerate().map(move |(at, field)| {
                format!(
                    "({} {symbol} {})",
                    self.at_depth(field, depth),
                    parameter(at)
                )
            })
        };
        match operator {
            BinaryOperator::Equal => format!("({})", pairs().collect::<Vec<_>>().join(" AND ")),
            BinaryOperator::NotEqual => format!("({})", pairs().collect::<Vec<_>>().join(" OR ")),
            _ => {
                let parameters: Vec<String> = (0..fields.len()).map(parameter).collect();
                format!(
                    "(ROW({}) {symbol} ROW({}))",
                    self.joined(fields, ", ", depth),
                    parameters.join(", ")
                )
            }
        }
    }

    fn row_text(&self, fields: &[Expr], depth: usize) -> String {
        match self.spelling {
            Spelling::PostgreSql => format!("ROW({})", self.joined(fields, ", ", depth)),
            Spelling::MySql => format!("({})", self.joined(fields, ",", depth)),
        }
    }

    fn row_call(&self, name: &str, arguments: &[Expr], depth: usize) -> Option<String> {
        if name == crate::records::ROW_FUNCTION {
            return Some(self.row_text(arguments, depth));
        }
        if name == crate::row_values::ROW_ORDERING {
            let shape = crate::row_values::ordering_shape(arguments)?;
            return Some(format!(
                "({} {} {})",
                self.row_text(shape.left, depth),
                shape.operator.symbol(),
                self.row_text(shape.right, depth)
            ));
        }
        if name != crate::row_values::ROW_MEMBERSHIP {
            return None;
        }
        let shape = crate::row_values::membership_shape(arguments)?;
        if self.spelling == Spelling::MySql {
            let items: Vec<String> = shape
                .items
                .iter()
                .map(|item| self.row_text(item, depth))
                .collect();
            return Some(format!(
                "({} in ({}))",
                self.row_text(shape.left, depth),
                items.join(",")
            ));
        }
        let alternatives: Vec<String> = shape
            .items
            .iter()
            .map(|item| {
                let equalities: Vec<String> = shape
                    .left
                    .iter()
                    .zip(item.iter())
                    .map(|(left, right)| {
                        format!(
                            "({} = {})",
                            self.at_depth(left, depth),
                            self.at_depth(right, depth)
                        )
                    })
                    .collect();
                match equalities.as_slice() {
                    [only] => only.clone(),
                    _ => format!("({})", equalities.join(" AND ")),
                }
            })
            .collect();
        Some(match alternatives.as_slice() {
            [only] => only.clone(),
            _ => format!("({})", alternatives.join(" OR ")),
        })
    }

    fn column(&self, qualifier: Option<&str>, name: &str) -> String {
        if self.bare_columns {
            return name.to_owned();
        }
        match qualifier.or(self.qualifier) {
            Some(qualifier) => format!("{qualifier}.{name}"),
            None => name.to_owned(),
        }
    }

    fn at_depth(&self, expr: &Expr, depth: usize) -> String {
        if depth > MOST_DEPTH {
            return "...".to_owned();
        }
        let depth = depth + 1;
        let text = |expr: &Expr| self.at_depth(expr, depth);
        match &expr.kind {
            ExprKind::Literal(literal) => self.literal(literal),
            ExprKind::Constant { value, .. } => Self::constant(value),
            ExprKind::Column { qualifier, name } => self.column(qualifier.as_deref(), name),
            ExprKind::Unary(UnaryOperator::Minus, operand) => format!("(- {})", text(operand)),
            ExprKind::Unary(UnaryOperator::Plus, operand) => text(operand),
            ExprKind::Unary(UnaryOperator::Not, operand) => match self.spelling {
                Spelling::PostgreSql => format!("(NOT {})", text(operand)),
                Spelling::MySql => format!("(not({}))", text(operand)),
            },
            ExprKind::And(operands) => {
                format!(
                    "({})",
                    self.joined(operands, self.word(" AND ", " and "), depth)
                )
            }
            ExprKind::Or(operands) => {
                format!(
                    "({})",
                    self.joined(operands, self.word(" OR ", " or "), depth)
                )
            }
            ExprKind::Binary(operator, left, right) => {
                format!("({} {} {})", text(left), operator.symbol(), text(right))
            }
            ExprKind::IsNull { expr, negated } => format!(
                "({} {})",
                text(expr),
                if *negated {
                    self.word("IS NOT NULL", "is not null")
                } else {
                    self.word("IS NULL", "is null")
                }
            ),
            ExprKind::IsTruth {
                expr,
                test,
                negated,
            } => {
                let test = match test {
                    TruthTest::True => "TRUE",
                    TruthTest::False => "FALSE",
                    TruthTest::Unknown => "UNKNOWN",
                };
                let not = if *negated { "NOT " } else { "" };
                let tested = format!("({} IS {not}{test})", text(expr));
                match self.spelling {
                    Spelling::PostgreSql => tested,
                    Spelling::MySql => tested.to_lowercase(),
                }
            }
            ExprKind::IsDistinctFrom {
                left,
                right,
                negated,
            } => {
                let not = if *negated { "NOT " } else { "" };
                format!("({} IS {not}DISTINCT FROM {})", text(left), text(right))
            }
            ExprKind::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let not = if *negated {
                    self.word("NOT ", "not ")
                } else {
                    ""
                };
                format!(
                    "({} {not}{} {} {} {})",
                    text(expr),
                    self.word("BETWEEN", "between"),
                    text(low),
                    self.word("AND", "and"),
                    text(high)
                )
            }
            ExprKind::InList {
                expr,
                list,
                negated,
            } => {
                let not = if *negated {
                    self.word("NOT ", "not ")
                } else {
                    ""
                };
                format!(
                    "({} {not}{} ({}))",
                    text(expr),
                    self.word("IN", "in"),
                    self.joined(list, ",", depth)
                )
            }
            ExprKind::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => {
                let operator = match (self.spelling, negated, case_insensitive) {
                    (Spelling::PostgreSql, false, false) => "~~",
                    (Spelling::PostgreSql, true, false) => "!~~",
                    (Spelling::PostgreSql, false, true) => "~~*",
                    (Spelling::PostgreSql, true, true) => "!~~*",
                    (Spelling::MySql, false, _) => "like",
                    (Spelling::MySql, true, _) => "not like",
                };
                format!("({} {operator} {})", text(expr), text(pattern))
            }
            ExprKind::Cast { expr, sql_type } => match self.spelling {
                Spelling::PostgreSql => format!("({})::{}", text(expr), sql_type.name()),
                Spelling::MySql => format!("cast({} as {})", text(expr), sql_type.name()),
            },
            ExprKind::Case {
                operand,
                branches,
                otherwise,
            } => {
                let mut parts = vec![self.word("CASE", "case").to_owned()];
                if let Some(operand) = operand {
                    parts.push(text(operand));
                }
                for (condition, result) in branches {
                    parts.push(format!(
                        "{} {} {} {}",
                        self.word("WHEN", "when"),
                        text(condition),
                        self.word("THEN", "then"),
                        text(result)
                    ));
                }
                if let Some(otherwise) = otherwise {
                    parts.push(format!("{} {}", self.word("ELSE", "else"), text(otherwise)));
                }
                parts.push(self.word("END", "end").to_owned());
                parts.join(" ")
            }
            ExprKind::Function { name, arguments } => self
                .row_call(name, arguments, depth)
                .unwrap_or_else(|| format!("{name}({})", self.joined(arguments, ", ", depth))),
            ExprKind::SqliteFunction { name, arguments }
            | ExprKind::OracleFunction { name, arguments } => {
                format!("{name}({})", self.joined(arguments, ", ", depth))
            }
            ExprKind::Aggregate {
                function,
                argument,
                distinct,
            } => {
                let distinct = if *distinct {
                    self.word("DISTINCT ", "distinct ")
                } else {
                    ""
                };
                let argument = argument.as_deref().map_or_else(|| "*".to_owned(), text);
                format!("{}({distinct}{argument})", function.name())
            }
            ExprKind::Subquery(subquery) => self.subquery(subquery, depth),
            _ => "?".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::BinaryOperator;

    fn expr(kind: ExprKind) -> Expr {
        Expr { kind, position: 0 }
    }

    fn column(name: &str) -> Expr {
        expr(ExprKind::Column {
            qualifier: None,
            name: name.to_owned(),
        })
    }

    fn integer(value: i64) -> Expr {
        expr(ExprKind::Literal(Literal::Integer(value)))
    }

    #[test]
    fn a_condition_reads_as_each_database_prints_it() {
        let condition = expr(ExprKind::And(vec![
            expr(ExprKind::Binary(
                BinaryOperator::Greater,
                Box::new(column("a")),
                Box::new(integer(1)),
            )),
            expr(ExprKind::IsNull {
                expr: Box::new(column("b")),
                negated: true,
            }),
        ]));
        let postgresql = ExpressionText {
            spelling: Spelling::PostgreSql,
            qualifier: None,
            bare_columns: true,
            subplans: &[],
            select_numbers: &[],
        };
        assert_eq!(postgresql.text(&condition), "((a > 1) AND (b IS NOT NULL))");
        let mysql = ExpressionText {
            spelling: Spelling::MySql,
            qualifier: Some("t"),
            bare_columns: false,
            subplans: &[],
            select_numbers: &[],
        };
        assert_eq!(mysql.text(&condition), "((t.a > 1) and (t.b is not null))");
    }
}
