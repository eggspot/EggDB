use super::Parser;
use crate::ast::{BinaryOperator, Expr, ExprKind, Literal, Name};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::mysql_date_functions as functions;

fn unit_expr(unit: &str, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Literal(Literal::String(unit.to_owned())),
        position,
    }
}

fn call(name: &str, arguments: Vec<Expr>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Function {
            name: name.to_owned(),
            arguments,
        },
        position,
    }
}

fn wrong_count(name: &Name) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "Incorrect parameter count in the call to native function '{}'",
            name.text
        ),
    )
    .with_detail("wrong parameter count")
    .at(name.position)
}

fn interval_parts(expr: Expr) -> Result<(Expr, Expr), Expr> {
    match expr.kind {
        ExprKind::Function { name, arguments }
            if name == functions::INTERVAL_OPERAND && arguments.len() == 2 =>
        {
            let mut arguments = arguments.into_iter();
            match (arguments.next(), arguments.next()) {
                (Some(amount), Some(unit)) => Ok((amount, unit)),
                _ => Err(Expr {
                    kind: ExprKind::Literal(Literal::Null),
                    position: expr.position,
                }),
            }
        }
        kind => Err(Expr {
            kind,
            position: expr.position,
        }),
    }
}

pub(super) fn with_interval(left: Expr, operator: BinaryOperator, right: Expr) -> Expr {
    let position = left.position;
    let right = match interval_parts(right) {
        Ok((amount, unit)) => {
            let internal = if operator == BinaryOperator::Subtract {
                functions::DATE_SUB
            } else {
                functions::DATE_ADD
            };
            return call(internal, vec![left, amount, unit], position);
        }
        Err(right) => right,
    };
    if operator == BinaryOperator::Add {
        match interval_parts(left) {
            Ok((amount, unit)) => {
                return call(functions::DATE_ADD, vec![right, amount, unit], position);
            }
            Err(left) => return Parser::binary(left, operator, right),
        }
    }
    Parser::binary(left, operator, right)
}

impl Parser<'_> {
    pub(super) fn mysql_interval_operand(&mut self, position: usize) -> SqlResult<Expr> {
        self.at += 1;
        let amount = self.expression()?;
        let unit_position = self.position();
        let unit = unit_expr(self.mysql_unit(false)?, unit_position);
        Ok(call(functions::INTERVAL_OPERAND, vec![amount, unit], position))
    }

    pub(super) fn mysql_date_function(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        let text = name.text.as_str();
        let special = functions::SPECIAL_SYNTAX.contains(&text);
        if !special && !functions::is_plain_function(text)
            || self.peek_kind() != &TokenKind::LeftParen
        {
            return Ok(None);
        }
        let position = name.position;
        self.expect(&TokenKind::LeftParen)?;
        let expr = if special {
            self.mysql_special_date_call(name)?
        } else {
            let mut arguments = Vec::new();
            if self.peek_kind() != &TokenKind::RightParen {
                loop {
                    arguments.push(self.expression()?);
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            let internal =
                functions::internal_name(text, arguments.len()).ok_or_else(|| wrong_count(name))?;
            call(internal, arguments, position)
        };
        self.expect(&TokenKind::RightParen)?;
        Ok(Some(expr))
    }

    fn mysql_unit(&mut self, simple_only: bool) -> SqlResult<&'static str> {
        let TokenKind::Word(word) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let word = word.to_ascii_lowercase();
        let unit = if simple_only {
            functions::timestamp_unit(&word)
        } else {
            functions::interval_unit(&word)
        };
        let unit = unit.ok_or_else(|| self.unexpected())?;
        self.at += 1;
        Ok(unit)
    }

    fn mysql_special_date_call(&mut self, name: &Name) -> SqlResult<Expr> {
        let position = name.position;
        Ok(match name.text.as_str() {
            "date_add" | "date_sub" | "adddate" | "subdate" => {
                let date = self.expression()?;
                self.expect(&TokenKind::Comma)?;
                let (amount, unit) = if self.take_word("interval") {
                    let amount = self.expression()?;
                    let unit_position = self.position();
                    (amount, unit_expr(self.mysql_unit(false)?, unit_position))
                } else if matches!(name.text.as_str(), "adddate" | "subdate") {
                    (self.expression()?, unit_expr("day", position))
                } else {
                    return Err(self.unexpected());
                };
                let internal = if matches!(name.text.as_str(), "date_add" | "adddate") {
                    functions::DATE_ADD
                } else {
                    functions::DATE_SUB
                };
                call(internal, vec![date, amount, unit], position)
            }
            "timestampadd" => {
                let unit_position = self.position();
                let unit = unit_expr(self.mysql_unit(true)?, unit_position);
                self.expect(&TokenKind::Comma)?;
                let amount = self.expression()?;
                self.expect(&TokenKind::Comma)?;
                let date = self.expression()?;
                call(functions::DATE_ADD, vec![date, amount, unit], position)
            }
            "timestampdiff" => {
                let unit_position = self.position();
                let unit = unit_expr(self.mysql_unit(true)?, unit_position);
                self.expect(&TokenKind::Comma)?;
                let from = self.expression()?;
                self.expect(&TokenKind::Comma)?;
                let to = self.expression()?;
                call(functions::TIMESTAMPDIFF, vec![unit, from, to], position)
            }
            _ => {
                let text = self.expression()?;
                if !self.take(&TokenKind::Comma) {
                    return Err(wrong_count(name));
                }
                let format = self.expression()?;
                let internal = match &format.kind {
                    ExprKind::Literal(Literal::String(format)) => {
                        functions::str_to_date_function(format)
                    }
                    _ => functions::STR_TO_DATETIME,
                };
                call(internal, vec![text, format], position)
            }
        })
    }
}
