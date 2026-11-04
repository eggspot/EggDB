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
        Self::set_generation(
            column,
            GenerationSpec {
                expr,
                text,
                stored,
                type_given,
                position,
            },
        )
    }

    fn set_generation(column: &mut ColumnDefinition, spec: GenerationSpec) -> SqlResult<()> {
        if column.generated.is_some() {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                format!(
                    "multiple generation clauses specified for column \"{}\"",
                    column.name.text
                ),
            )
            .at(spec.position));
        }
        if column.default.is_some() || column.auto_increment {
            return Err(both_default_and_generation(&column.name.text).at(spec.position));
        }
        column.generated = Some(spec);
        Ok(())
    }

    pub(super) fn sqlserver_computed_column(
        &mut self,
        column: &mut ColumnDefinition,
    ) -> SqlResult<()> {
        let position = self.position();
        self.expect_word("as")?;
        let first = self.at;
        let expr = self.expression()?;
        let last = self.at.saturating_sub(1);
        let (start, end) = match self.enclosing_parentheses(first, last) {
            true => (self.tokens[first].end, self.tokens[last].start),
            false => (self.tokens[first].start, self.tokens[last].end),
        };
        let text = self.text.get(start..end).unwrap_or("").trim().to_owned();
        let stored = self.take_word("persisted");
        Self::set_generation(
            column,
            GenerationSpec {
                expr,
                text,
                stored,
                type_given: false,
                position,
            },
        )
    }

    fn enclosing_parentheses(&self, first: usize, last: usize) -> bool {
        if last <= first || self.tokens[first].kind != TokenKind::LeftParen {
            return false;
        }
        let mut depth = 0usize;
        for (at, token) in self.tokens.iter().enumerate().take(last + 1).skip(first) {
            match token.kind {
                TokenKind::LeftParen => depth += 1,
                TokenKind::RightParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return at == last;
                    }
                }
                _ => {}
            }
        }
        false
    }
}

pub(super) fn both_default_and_generation(column: &str) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("both default and generation expression specified for column \"{column}\""),
    )
}
