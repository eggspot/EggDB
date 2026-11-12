use super::Parser;
use crate::ast::{Name, SqlServerStatement, Statement};
use crate::error::SqlResult;
use crate::lexer::TokenKind;
use crate::sqlserver_partitions::PartitionSchemeCommand;

impl Parser<'_> {
    pub(super) fn at_partition_scheme_command(&self) -> bool {
        self.is_word_at(1, "partition") && (self.is_word_at(2, "function") || self.is_word_at(2, "scheme"))
    }

    fn source_since(&self, start: usize) -> String {
        self.text
            .get(start..self.previous_end())
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    pub(super) fn sqlserver_partition_command(&mut self) -> SqlResult<Statement> {
        let creating = self.is_word("create");
        let function = self.is_word_at(2, "function");
        self.at += 3;
        let name = self.name()?;
        let command = match (creating, function) {
            (false, true) => PartitionSchemeCommand::DropFunction(name),
            (false, false) => PartitionSchemeCommand::DropScheme(name),
            (true, true) => {
                self.expect(&TokenKind::LeftParen)?;
                let start = self.position();
                self.sql_type()?;
                let input_type = self.source_since(start);
                self.expect(&TokenKind::RightParen)?;
                self.expect_word("as")?;
                self.expect_word("range")?;
                let right = self.take_word("right");
                if !right {
                    self.take_word("left");
                }
                self.expect_word("for")?;
                self.expect_word("values")?;
                self.expect(&TokenKind::LeftParen)?;
                let mut boundaries = Vec::new();
                if !self.take(&TokenKind::RightParen) {
                    loop {
                        let start = self.position();
                        self.expression()?;
                        boundaries.push(self.source_since(start));
                        if !self.take(&TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(&TokenKind::RightParen)?;
                }
                PartitionSchemeCommand::CreateFunction {
                    name,
                    input_type,
                    right,
                    boundaries,
                }
            }
            (true, false) => {
                self.expect_word("as")?;
                self.expect_word("partition")?;
                let function = self.name()?;
                self.take_word("all");
                self.expect_word("to")?;
                self.expect(&TokenKind::LeftParen)?;
                loop {
                    self.label()?;
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RightParen)?;
                PartitionSchemeCommand::CreateScheme { name, function }
            }
        };
        Ok(Statement::SqlServer(SqlServerStatement::PartitionScheme(Box::new(command))))
    }

    pub(super) fn storage_clause(&mut self) -> SqlResult<Option<(Name, Name)>> {
        if !self.take_word("on") {
            return Ok(None);
        }
        let position = self.position();
        let target = self.label()?;
        if !self.take(&TokenKind::LeftParen) {
            return Ok(None);
        }
        let column_position = self.position();
        let column = self.label()?;
        self.expect(&TokenKind::RightParen)?;
        Ok(Some((
            Name {
                text: target,
                position,
            },
            Name {
                text: column,
                position: column_position,
            },
        )))
    }
}
