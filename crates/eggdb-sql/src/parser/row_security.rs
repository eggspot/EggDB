use super::Parser;
use crate::ast::Statement;
use crate::error::SqlResult;
use crate::lexer::TokenKind;
use crate::row_security::{Policy, PolicyCommand, RowSecurityStatement};

const MAX_LOOKAHEAD: usize = 12;

impl Parser<'_> {
    pub(super) fn alters_row_security(&self) -> bool {
        (2..MAX_LOOKAHEAD).any(|ahead| {
            self.is_word_at(ahead, "row")
                && self.is_word_at(ahead + 1, "level")
                && self.is_word_at(ahead + 2, "security")
        })
    }

    pub(super) fn alter_row_security(&mut self) -> SqlResult<Statement> {
        self.at += 2;
        let table = self.table_name()?;
        let enabled = if self.take_word("enable") {
            true
        } else if self.take_word("disable") {
            false
        } else if self.take_word("force") || (self.take_word("no") && self.take_word("force")) {
            return Err(self.not_supported("FORCE ROW LEVEL SECURITY (EggDB has no table owners yet)"));
        } else {
            return Err(self.unexpected());
        };
        self.expect_word("row")?;
        self.expect_word("level")?;
        self.expect_word("security")?;
        Ok(Statement::RowSecurity(RowSecurityStatement::Switch { table, enabled }))
    }

    fn condition_text(&mut self) -> SqlResult<String> {
        self.expect(&TokenKind::LeftParen)?;
        let start = self.peek().start;
        self.expression()?;
        let end = self.tokens[self.at.saturating_sub(1)].end;
        self.expect(&TokenKind::RightParen)?;
        Ok(self.text.get(start..end).unwrap_or_default().to_owned())
    }

    pub(super) fn create_policy(&mut self) -> SqlResult<Statement> {
        self.expect_word("policy")?;
        let name = self.name()?;
        self.expect_word("on")?;
        let table = self.table_name()?;
        let mut permissive = true;
        if self.take_word("as") {
            if self.take_word("restrictive") {
                permissive = false;
            } else {
                self.expect_word("permissive")?;
            }
        }
        let mut command = PolicyCommand::All;
        if self.take_word("for") {
            command = if self.take_word("all") {
                PolicyCommand::All
            } else if self.take_word("select") {
                PolicyCommand::Select
            } else if self.take_word("insert") {
                PolicyCommand::Insert
            } else if self.take_word("update") {
                PolicyCommand::Update
            } else if self.take_word("delete") {
                PolicyCommand::Delete
            } else {
                return Err(self.unexpected());
            };
        }
        let mut roles = Vec::new();
        if self.take_word("to") {
            loop {
                if self.take_word("public") {
                    roles.push(crate::roles::PUBLIC.to_owned());
                } else {
                    roles.push(self.name()?.text);
                }
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        } else {
            roles.push(crate::roles::PUBLIC.to_owned());
        }
        let using = if self.take_word("using") {
            Some(self.condition_text()?)
        } else {
            None
        };
        let check = if self.take_word("with") {
            self.expect_word("check")?;
            Some(self.condition_text()?)
        } else {
            None
        };
        Ok(Statement::RowSecurity(RowSecurityStatement::CreatePolicy {
            table,
            policy: Policy {
                name: name.text,
                command,
                permissive,
                roles,
                using,
                check,
            },
        }))
    }

    pub(super) fn drop_policy(&mut self) -> SqlResult<Statement> {
        self.expect_word("policy")?;
        let if_exists = self.if_exists()?;
        let name = self.name()?;
        self.expect_word("on")?;
        let table = self.table_name()?;
        let _ = self.take_word("cascade") || self.take_word("restrict");
        Ok(Statement::RowSecurity(RowSecurityStatement::DropPolicy {
            table,
            name,
            if_exists,
        }))
    }
}
