use super::{Level, Parser};
use crate::ast::SystemTime;
use crate::error::SqlResult;
use crate::lexer::TokenKind;

impl Parser<'_> {
    pub(super) fn system_time_clause(&mut self) -> SqlResult<Option<Box<SystemTime>>> {
        if !(self.is_word("for") && self.is_word_at(1, "system_time")) {
            return Ok(None);
        }
        self.at += 2;
        let clause = if self.take_word("all") {
            SystemTime::All
        } else if self.take_word("as") {
            self.expect_word("of")?;
            if self.is_word("transaction") {
                return Err(self.not_supported("FOR SYSTEM_TIME AS OF TRANSACTION"));
            }
            SystemTime::AsOf(self.expression_above(Level::Other)?)
        } else if self.take_word("from") {
            let start = self.expression_above(Level::Other)?;
            self.expect_word("to")?;
            SystemTime::FromTo(start, self.expression_above(Level::Other)?)
        } else if self.take_word("between") {
            let start = self.expression_above(Level::Other)?;
            self.expect_word("and")?;
            SystemTime::Between(start, self.expression_above(Level::Other)?)
        } else if self.take_word("contained") {
            self.expect_word("in")?;
            self.expect(&TokenKind::LeftParen)?;
            let start = self.expression()?;
            self.expect(&TokenKind::Comma)?;
            let end = self.expression()?;
            self.expect(&TokenKind::RightParen)?;
            SystemTime::ContainedIn(start, end)
        } else {
            return Err(self.unexpected());
        };
        Ok(Some(Box::new(clause)))
    }
}
