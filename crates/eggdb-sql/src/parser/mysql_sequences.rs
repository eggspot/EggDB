use super::Parser;
use crate::ast::{Expr, ExprKind, Literal, Name};
use crate::error::SqlResult;
use crate::lexer::TokenKind;
use crate::types::SqlType;

const NEXT_VALUE: &str = "oracle_nextval";
const CURRENT_VALUE: &str = "oracle_currval";
const SET_VALUE: &str = "setval";

fn sequence_argument(sequence: Name) -> Expr {
    Expr {
        kind: ExprKind::Literal(Literal::String(sequence.text)),
        position: sequence.position,
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

impl Parser<'_> {
    pub(super) fn mysql_sequence_function(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        let internal = match name.text.as_str() {
            "nextval" => NEXT_VALUE,
            "lastval" => CURRENT_VALUE,
            "setval" => SET_VALUE,
            _ => return Ok(None),
        };
        if self.peek_kind() != &TokenKind::LeftParen {
            return Ok(None);
        }
        self.expect(&TokenKind::LeftParen)?;
        let sequence = self.table_name()?;
        let mut arguments = vec![sequence_argument(sequence)];
        if internal == SET_VALUE {
            self.expect(&TokenKind::Comma)?;
            arguments.push(self.expression()?);
            if self.take(&TokenKind::Comma) {
                let used = self.expression()?;
                let position = used.position;
                arguments.push(Expr {
                    kind: ExprKind::Cast {
                        expr: Box::new(used),
                        sql_type: SqlType::Boolean,
                    },
                    position,
                });
                if self.take(&TokenKind::Comma) {
                    let round_position = self.position();
                    let zero = matches!(self.peek_kind(), TokenKind::Integer(digits) if *digits == "0");
                    if !zero {
                        return Err(self
                            .not_supported("SETVAL with a round other than 0")
                            .at(round_position));
                    }
                    self.at += 1;
                }
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(Some(call(internal, arguments, name.position)))
    }

    pub(super) fn mysql_sequence_value(
        &mut self,
        word: &str,
        position: usize,
    ) -> SqlResult<Option<Expr>> {
        let internal = match word {
            "next" => NEXT_VALUE,
            "previous" => CURRENT_VALUE,
            _ => return Ok(None),
        };
        if !(self.is_word_at(1, "value") && self.is_word_at(2, "for")) {
            return Ok(None);
        }
        self.at += 3;
        let sequence = self.table_name()?;
        Ok(Some(call(internal, vec![sequence_argument(sequence)], position)))
    }
}
