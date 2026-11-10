use super::Parser;
use crate::ast::{AggregateFunction, BinaryOperator, Expr, ExprKind, Name, OrderItem};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::types::{SqlType, Value};

fn constant(value: Value, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Constant {
            sql_type: value.sql_type(),
            value,
        },
        position,
    }
}

fn as_text(expr: Expr) -> Expr {
    let position = expr.position;
    Expr {
        kind: ExprKind::Cast {
            expr: Box::new(expr),
            sql_type: SqlType::Text,
        },
        position,
    }
}

fn joined(value: Expr, separator: Expr, order: Vec<OrderItem>, distinct: bool, position: usize) -> Expr {
    let arguments = Expr {
        kind: ExprKind::Function {
            name: "string_agg_arguments".to_owned(),
            arguments: vec![value, separator],
        },
        position,
    };
    let argument = if order.is_empty() {
        arguments
    } else {
        crate::aggregate::ordered_arguments(arguments, order)
    };
    Expr {
        kind: ExprKind::Aggregate {
            function: AggregateFunction::StringAgg,
            argument: Some(Box::new(argument)),
            distinct,
        },
        position,
    }
}

impl Parser<'_> {
    pub(super) fn group_concat(&mut self, name: &Name) -> SqlResult<Expr> {
        let position = name.position;
        self.expect(&TokenKind::LeftParen)?;
        let distinct = self.take_word("distinct");
        if !distinct {
            self.take_word("all");
        }
        let mut arguments = vec![self.expression()?];
        while self.take(&TokenKind::Comma) {
            arguments.push(self.expression()?);
        }
        let order = if self.take_word("order") {
            self.expect_word("by")?;
            self.order_items()?
        } else {
            Vec::new()
        };
        let mut separator = None;
        if self.mysql() && self.take_word("separator") {
            let TokenKind::String(text) = self.peek_kind() else {
                return Err(self.unexpected());
            };
            separator = Some(constant(Value::Text(text.as_ref().to_owned()), self.position()));
            self.at += 1;
        }
        self.expect(&TokenKind::RightParen)?;
        let value = if self.mysql() {
            let mut parts = arguments.into_iter().map(as_text);
            let first = parts
                .next()
                .ok_or_else(|| SqlError::new(SqlState::SYNTAX_ERROR, "GROUP_CONCAT needs an argument"))?;
            parts.fold(first, |left, right| Expr {
                kind: ExprKind::Binary(BinaryOperator::Concat, Box::new(left), Box::new(right)),
                position,
            })
        } else {
            if arguments.len() > 2 || (distinct && arguments.len() > 1) {
                return Err(SqlError::new(
                    SqlState::SYNTAX_ERROR,
                    if distinct {
                        "DISTINCT aggregates must have exactly one argument"
                    } else {
                        "wrong number of arguments to function group_concat()"
                    },
                )
                .at(position));
            }
            let mut arguments = arguments.into_iter();
            let value = arguments
                .next()
                .map(as_text)
                .ok_or_else(|| SqlError::new(SqlState::SYNTAX_ERROR, "group_concat needs an argument"))?;
            separator = arguments.next().map(as_text);
            value
        };
        let separator =
            separator.unwrap_or_else(|| constant(Value::Text(",".to_owned()), position));
        Ok(joined(value, separator, order, distinct, position))
    }

    pub(super) fn sqlite_total(&mut self, name: &Name) -> SqlResult<Expr> {
        let position = name.position;
        self.expect(&TokenKind::LeftParen)?;
        let distinct = self.take_word("distinct");
        if !distinct {
            self.take_word("all");
        }
        let value = self.expression()?;
        self.expect(&TokenKind::RightParen)?;
        let sum = Expr {
            kind: ExprKind::Aggregate {
                function: AggregateFunction::Sum,
                argument: Some(Box::new(Expr {
                    kind: ExprKind::Cast {
                        expr: Box::new(value),
                        sql_type: SqlType::Double,
                    },
                    position,
                })),
                distinct,
            },
            position,
        };
        Ok(Expr {
            kind: ExprKind::Function {
                name: "coalesce".to_owned(),
                arguments: vec![sum, constant(Value::Double(0.0), position)],
            },
            position,
        })
    }

    pub(super) fn sqlite_json_aggregate(&mut self, name: &Name) -> SqlResult<Expr> {
        let position = name.position;
        let object = name.text.eq_ignore_ascii_case("json_group_object");
        self.expect(&TokenKind::LeftParen)?;
        let mut arguments = vec![self.expression()?];
        while self.take(&TokenKind::Comma) {
            arguments.push(self.expression()?);
        }
        let order = if self.take_word("order") {
            self.expect_word("by")?;
            self.order_items()?
        } else {
            Vec::new()
        };
        self.expect(&TokenKind::RightParen)?;
        let wanted = if object { 2 } else { 1 };
        if arguments.len() != wanted {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("wrong number of arguments to function {}()", name.text),
            )
            .with_sqlite_code(1)
            .at(position));
        }
        let (function, argument) = if object {
            let pair = Expr {
                kind: ExprKind::Function {
                    name: "object_agg_arguments".to_owned(),
                    arguments,
                },
                position,
            };
            (AggregateFunction::JsonObjectAgg, pair)
        } else {
            let mut arguments = arguments.into_iter();
            let value = arguments
                .next()
                .ok_or_else(|| SqlError::new(SqlState::SYNTAX_ERROR, "json_group_array needs an argument"))?;
            (AggregateFunction::JsonAgg, value)
        };
        let argument = if order.is_empty() {
            argument
        } else {
            crate::aggregate::ordered_arguments(argument, order)
        };
        let aggregate = Expr {
            kind: ExprKind::Aggregate {
                function,
                argument: Some(Box::new(argument)),
                distinct: false,
            },
            position,
        };
        let empty = constant(
            Value::Json(if object { "{}" } else { "[]" }.to_owned()),
            position,
        );
        Ok(Expr {
            kind: ExprKind::SqliteFunction {
                name: "sqlite_json",
                arguments: vec![Expr {
                    kind: ExprKind::Function {
                        name: "coalesce".to_owned(),
                        arguments: vec![aggregate, empty],
                    },
                    position,
                }],
            },
            position,
        })
    }
}

