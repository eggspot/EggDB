use super::Parser;
use crate::ast::{ExtendedStatistics, Statement};
use crate::error::SqlResult;
use crate::extended_statistics::{KIND_DEPENDENCIES, KIND_NDISTINCT};
use crate::lexer::TokenKind;

impl Parser<'_> {
    pub(super) fn create_statistics(&mut self) -> SqlResult<Statement> {
        self.expect_word("statistics")?;
        let if_not_exists = if self.is_word("if") && self.is_word_at(1, "not") {
            self.at += 2;
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let name = self.table_name()?;
        let mut kinds = 0u8;
        if self.take(&TokenKind::LeftParen) {
            loop {
                let kind = self.name()?;
                kinds |= match kind.text.as_str() {
                    "ndistinct" => KIND_NDISTINCT,
                    "dependencies" => KIND_DEPENDENCIES,
                    "mcv" => 0,
                    _ => {
                        return Err(crate::error::SqlError::new(
                            crate::error::SqlState::INVALID_PARAMETER_VALUE,
                            format!("unrecognized statistics kind \"{}\"", kind.text),
                        )
                        .at(kind.position));
                    }
                };
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        self.expect_word("on")?;
        let mut columns = vec![self.name()?];
        while self.take(&TokenKind::Comma) {
            columns.push(self.name()?);
        }
        self.expect_word("from")?;
        let table = self.table_name()?;
        Ok(Statement::ExtendedStatistics(ExtendedStatistics::Create {
            name,
            if_not_exists,
            kinds,
            columns,
            table,
        }))
    }

    pub(super) fn drop_statistics(&mut self) -> SqlResult<Statement> {
        self.expect_word("statistics")?;
        let if_exists = if self.is_word("if") && self.is_word_at(1, "exists") {
            self.at += 2;
            true
        } else {
            false
        };
        let mut names = vec![self.table_name()?];
        while self.take(&TokenKind::Comma) {
            names.push(self.table_name()?);
        }
        let _ = self.take_word("cascade") || self.take_word("restrict");
        Ok(Statement::ExtendedStatistics(ExtendedStatistics::Drop { names, if_exists }))
    }
}
