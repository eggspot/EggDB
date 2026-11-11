use super::Parser;
use crate::ast::{Expr, ExprKind, Name};
use crate::error::SqlResult;
use crate::lexer::TokenKind;
use crate::types::{SqlType, Value};

pub(crate) const MATCH_FUNCTION: &str = "mysql_match_against";
pub(crate) const NATURAL_MODE: &str = "natural";
pub(crate) const BOOLEAN_MODE: &str = "boolean";

impl Parser<'_> {
    pub(super) fn mysql_match(&mut self, name: &Name) -> SqlResult<Expr> {
        let position = name.position;
        self.expect(&TokenKind::LeftParen)?;
        let mut columns = vec![self.expression()?];
        while self.take(&TokenKind::Comma) {
            columns.push(self.expression()?);
        }
        self.expect(&TokenKind::RightParen)?;
        self.expect_word("against")?;
        self.expect(&TokenKind::LeftParen)?;
        let query = self.expression()?;
        let mode = if self.take_word("in") {
            if self.take_word("boolean") {
                self.expect_word("mode")?;
                BOOLEAN_MODE
            } else {
                self.expect_word("natural")?;
                self.expect_word("language")?;
                self.expect_word("mode")?;
                if self.take_word("with") {
                    self.expect_word("query")?;
                    self.expect_word("expansion")?;
                }
                NATURAL_MODE
            }
        } else {
            if self.take_word("with") {
                self.expect_word("query")?;
                self.expect_word("expansion")?;
            }
            NATURAL_MODE
        };
        self.expect(&TokenKind::RightParen)?;
        let mut arguments = vec![
            Expr {
                kind: ExprKind::Constant {
                    sql_type: Some(SqlType::Text),
                    value: Value::Text(mode.to_owned()),
                },
                position,
            },
            query,
        ];
        arguments.extend(columns);
        Ok(Expr {
            kind: ExprKind::Function {
                name: MATCH_FUNCTION.to_owned(),
                arguments,
            },
            position,
        })
    }
}
