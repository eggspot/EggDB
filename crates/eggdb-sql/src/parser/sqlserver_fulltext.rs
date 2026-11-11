use super::Parser;
use crate::ast::{Expr, ExprKind, Name};
use crate::error::SqlResult;
use crate::lexer::TokenKind;

impl Parser<'_> {
    pub(super) fn sqlserver_full_text(&mut self, name: &Name, function: &str) -> SqlResult<Expr> {
        let position = name.position;
        self.expect(&TokenKind::LeftParen)?;
        let mut columns = Vec::new();
        if self.is_operator("*") {
            return Err(self.not_supported("CONTAINS and FREETEXT over every column (*); name the columns"));
        }
        if self.take(&TokenKind::LeftParen) {
            loop {
                columns.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        } else {
            columns.push(self.expression()?);
        }
        self.expect(&TokenKind::Comma)?;
        let query = self.expression()?;
        if self.take(&TokenKind::Comma) {
            self.expect_word("language")?;
            self.expression()?;
        }
        self.expect(&TokenKind::RightParen)?;
        let mut arguments = vec![query];
        arguments.extend(columns);
        Ok(Expr {
            kind: ExprKind::Function {
                name: function.to_owned(),
                arguments,
            },
            position,
        })
    }
}
