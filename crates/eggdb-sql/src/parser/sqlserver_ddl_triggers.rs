use super::Parser;
use crate::ast::{SqlServerStatement, Statement};
use crate::error::SqlResult;
use crate::lexer::TokenKind;
use crate::sqlserver_ddl_triggers::{DdlTriggerCommand, DdlTriggerScope};

const MAX_TRIGGER_HEAD_TOKENS: usize = 64;

impl Parser<'_> {
    /// Whether the statement is a DDL trigger statement: `trigger` is followed, before `AS`
    /// or the statement's end, by `ON DATABASE` or `ON ALL SERVER`.
    pub(super) fn at_ddl_trigger(&self) -> bool {
        let names_trigger = self.is_word_at(1, "trigger")
            || (self.is_word("create") && self.is_word_at(1, "or") && self.is_word_at(2, "alter") && self.is_word_at(3, "trigger"));
        if !names_trigger {
            return false;
        }
        for ahead in 2..MAX_TRIGGER_HEAD_TOKENS {
            match self.peek_kind_at(ahead) {
                TokenKind::End | TokenKind::Semicolon => return false,
                TokenKind::Word(word) if word == "as" => return false,
                TokenKind::Word(word) if word == "on" => {
                    return self.is_word_at(ahead + 1, "database")
                        || (self.is_word_at(ahead + 1, "all") && self.is_word_at(ahead + 2, "server"));
                }
                _ => {}
            }
        }
        false
    }

    fn ddl_trigger_scope(&mut self) -> SqlResult<DdlTriggerScope> {
        self.expect_word("on")?;
        if self.take_word("database") {
            return Ok(DdlTriggerScope::Database);
        }
        self.expect_word("all")?;
        self.expect_word("server")?;
        Ok(DdlTriggerScope::Server)
    }

    pub(super) fn sqlserver_ddl_trigger(&mut self) -> SqlResult<Statement> {
        let TokenKind::Word(verb) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let verb = verb.to_string();
        self.at += 1;
        let command = match verb.as_str() {
            "drop" => {
                self.expect_word("trigger")?;
                let if_exists = self.take_word("if") && {
                    self.expect_word("exists")?;
                    true
                };
                let mut names = vec![self.name()?];
                while self.take(&TokenKind::Comma) {
                    names.push(self.name()?);
                }
                let scope = self.ddl_trigger_scope()?;
                DdlTriggerCommand::Drop {
                    names,
                    scope,
                    if_exists,
                }
            }
            "enable" | "disable" => {
                self.expect_word("trigger")?;
                let names = if self.take_word("all") {
                    None
                } else {
                    let mut names = vec![self.name()?];
                    while self.take(&TokenKind::Comma) {
                        names.push(self.name()?);
                    }
                    Some(names)
                };
                let scope = self.ddl_trigger_scope()?;
                DdlTriggerCommand::Enable {
                    names,
                    scope,
                    enable: verb == "enable",
                }
            }
            _ => {
                let alter = verb == "alter";
                let replace = !alter && self.take_word("or") && {
                    self.expect_word("alter")?;
                    true
                };
                self.expect_word("trigger")?;
                let name = self.name()?;
                let scope = self.ddl_trigger_scope()?;
                if self.take_word("with") {
                    while !(self.is_word("for") || self.is_word("after"))
                        && self.peek_kind() != &TokenKind::End
                    {
                        self.at += 1;
                    }
                }
                if !(self.take_word("for") || self.take_word("after")) {
                    return Err(self.unexpected());
                }
                let mut events = vec![self.name()?];
                while self.take(&TokenKind::Comma) {
                    events.push(self.name()?);
                }
                self.expect_word("as")?;
                let body_start = self.peek().start;
                if self.at_end() {
                    return Err(self.unexpected());
                }
                self.compile_batch(true)?;
                DdlTriggerCommand::Create {
                    name,
                    scope,
                    events,
                    body: self.slice(body_start, self.previous_end()),
                    replace,
                    alter,
                }
            }
        };
        Ok(Statement::SqlServer(SqlServerStatement::DdlTrigger(Box::new(command))))
    }
}
