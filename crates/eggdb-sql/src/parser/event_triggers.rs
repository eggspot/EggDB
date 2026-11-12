use super::Parser;
use crate::ast::{EventTriggerChange, EventTriggerCommand, Statement};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;

impl Parser<'_> {
    pub(super) fn at_event_trigger(&self) -> bool {
        self.postgresql() && self.is_word_at(1, "event") && self.is_word_at(2, "trigger")
    }

    pub(super) fn create_event_trigger(&mut self) -> SqlResult<Statement> {
        self.at += 3;
        let name = self.name()?;
        self.expect_word("on")?;
        let event = self.name()?;
        let mut tags = Vec::new();
        if self.take_word("when") {
            loop {
                let variable = self.name()?;
                if variable.text != "tag" {
                    return Err(SqlError::new(
                        SqlState::SYNTAX_ERROR,
                        format!("unrecognized filter variable \"{}\"", variable.text),
                    )
                    .at(variable.position));
                }
                if !tags.is_empty() {
                    return Err(SqlError::new(
                        SqlState::SYNTAX_ERROR,
                        "filter variable \"tag\" specified more than once",
                    )
                    .at(variable.position));
                }
                self.expect_word("in")?;
                self.expect(&TokenKind::LeftParen)?;
                loop {
                    let TokenKind::String(text) = self.peek_kind() else {
                        return Err(self.unexpected());
                    };
                    tags.push(text.as_ref().to_ascii_uppercase());
                    self.at += 1;
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RightParen)?;
                if !self.take_word("and") {
                    break;
                }
            }
        }
        self.expect_word("execute")?;
        if !(self.take_word("function") || self.take_word("procedure")) {
            return Err(self.unexpected());
        }
        let (schema, function) = self.trigger_name()?;
        let function = match schema {
            Some(schema) => format!("{schema}.{}", function.text),
            None => function.text,
        };
        self.expect(&TokenKind::LeftParen)?;
        self.expect(&TokenKind::RightParen)?;
        Ok(Statement::EventTrigger(EventTriggerCommand::Create {
            name,
            event,
            tags,
            function,
        }))
    }

    pub(super) fn drop_event_trigger(&mut self) -> SqlResult<Statement> {
        self.at += 3;
        let if_exists = if self.take_word("if") {
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let name = self.name()?;
        if !self.take_word("restrict") {
            self.take_word("cascade");
        }
        Ok(Statement::EventTrigger(EventTriggerCommand::Drop {
            name,
            if_exists,
        }))
    }

    pub(super) fn alter_event_trigger(&mut self) -> SqlResult<Statement> {
        self.at += 3;
        let name = self.name()?;
        let change = if self.take_word("disable") {
            EventTriggerChange::Disable
        } else if self.take_word("enable") {
            if self.take_word("replica") {
                EventTriggerChange::Enable('R')
            } else if self.take_word("always") {
                EventTriggerChange::Enable('A')
            } else {
                EventTriggerChange::Enable('O')
            }
        } else if self.take_word("rename") {
            self.expect_word("to")?;
            EventTriggerChange::Rename(self.name()?)
        } else if self.take_word("owner") {
            self.expect_word("to")?;
            EventTriggerChange::Owner(self.name()?)
        } else {
            return Err(self.unexpected());
        };
        Ok(Statement::EventTrigger(EventTriggerCommand::Alter { name, change }))
    }
}
