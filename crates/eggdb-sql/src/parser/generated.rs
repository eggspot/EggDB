use super::Parser;
use crate::ast::{ColumnDefinition, GenerationSpec};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;

impl Parser<'_> {
    pub(super) fn starts_generation(&self) -> bool {
        let opens_at = |ahead: usize| self.peek_kind_at(ahead) == &TokenKind::LeftParen;
        (self.is_word("as") && opens_at(1))
            || (self.is_word("generated")
                && self.is_word_at(1, "always")
                && self.is_word_at(2, "as")
                && opens_at(3))
    }

    pub(super) fn generation_clause(
        &mut self,
        column: &mut ColumnDefinition,
        type_given: bool,
    ) -> SqlResult<()> {
        let position = self.position();
        if self.take_word("generated") {
            self.expect_word("always")?;
        }
        self.expect_word("as")?;
        self.generation_body(column, type_given, position)
    }

    pub(super) fn generation_body(
        &mut self,
        column: &mut ColumnDefinition,
        type_given: bool,
        position: usize,
    ) -> SqlResult<()> {
        self.expect(&TokenKind::LeftParen)?;
        let start = self.position();
        let expr = self.expression()?;
        let end = self.tokens[self.at.saturating_sub(1)].end;
        let text = self.text.get(start..end).unwrap_or("").trim().to_owned();
        self.expect(&TokenKind::RightParen)?;
        let stored = match self.dialect {
            Dialect::SqlServer => self.take_word("persisted"),
            Dialect::Oracle => {
                self.take_word("virtual");
                false
            }
            _ => {
                let stored = self.take_word("stored");
                if !stored {
                    self.take_word("virtual");
                }
                stored
            }
        };
        if column.generated.is_some() {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                format!(
                    "multiple generation clauses specified for column \"{}\"",
                    column.name.text
                ),
            )
            .at(position));
        }
        if column.default.is_some() || column.auto_increment {
            return Err(both_default_and_generation(&column.name.text).at(position));
        }
        column.generated = Some(GenerationSpec {
            expr,
            text,
            stored,
            type_given,
            position,
        });
        Ok(())
    }

    pub(super) fn sqlserver_computed_column(
        &mut self,
        column: &mut ColumnDefinition,
    ) -> SqlResult<()> {
        let position = self.position();
        self.expect_word("as")?;
        if self.peek_kind() != &TokenKind::LeftParen {
            let start = self.position();
            let expr = self.expression()?;
            let end = self.tokens[self.at.saturating_sub(1)].end;
            let text = self.text.get(start..end).unwrap_or("").trim().to_owned();
            column.generated = Some(GenerationSpec {
                expr,
                text,
                stored: self.take_word("persisted"),
                type_given: false,
                position,
            });
            return Ok(());
        }
        self.generation_body(column, false, position)
    }
}

pub(super) fn both_default_and_generation(column: &str) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("both default and generation expression specified for column \"{column}\""),
    )
}
