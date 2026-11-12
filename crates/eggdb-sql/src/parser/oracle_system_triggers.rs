use super::Parser;
use crate::ast::{Name, Statement};
use crate::error::SqlResult;
use crate::oracle_system_triggers::{
    CreateSystemTrigger, DATABASE_EVENTS, DDL_EVENTS, SystemScope, SystemTiming, is_system_event,
};

impl Parser<'_> {
    pub(super) fn at_system_trigger_heading(&self) -> bool {
        let event_at = if self.is_word("before") || self.is_word("after") {
            1
        } else {
            return false;
        };
        matches!(self.peek_kind_at(event_at), crate::lexer::TokenKind::Word(word) if is_system_event(word))
    }

    fn system_event(&mut self) -> SqlResult<String> {
        let position = self.position();
        let crate::lexer::TokenKind::Word(word) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let event = word.to_ascii_lowercase();
        self.at += 1;
        if DDL_EVENTS.contains(&event.as_str()) || DATABASE_EVENTS.contains(&event.as_str()) {
            return Ok(event);
        }
        Err(self
            .not_supported(&format!("{} triggers", event.to_ascii_uppercase()))
            .at(position))
    }

    pub(super) fn plsql_system_trigger(&mut self, name: Name, replace: bool) -> SqlResult<Statement> {
        let timing = if self.take_word("before") {
            SystemTiming::Before
        } else {
            self.expect_word("after")?;
            SystemTiming::After
        };
        let mut events = vec![self.system_event()?];
        while self.take_word("or") {
            events.push(self.system_event()?);
        }
        if timing == SystemTiming::Before
            && events.iter().any(|event| matches!(event.as_str(), "logon" | "servererror"))
        {
            return Err(self.not_supported("BEFORE LOGON and BEFORE SERVERERROR"));
        }
        if timing == SystemTiming::After && events.iter().any(|event| event == "logoff") {
            return Err(self.not_supported("AFTER LOGOFF"));
        }
        self.expect_word("on")?;
        let scope = if self.take_word("database") {
            SystemScope::Database
        } else if self.take_word("pluggable") {
            self.expect_word("database")?;
            SystemScope::Database
        } else if self.take_word("schema") {
            SystemScope::Schema(None)
        } else {
            let owner = self.name()?;
            self.expect(&crate::lexer::TokenKind::Dot)?;
            self.expect_word("schema")?;
            SystemScope::Schema(Some(owner.text.to_uppercase()))
        };
        if self.is_word("follows") || self.is_word("precedes") {
            return Err(self.not_supported("FOLLOWS and PRECEDES on system triggers"));
        }
        let enabled = if self.take_word("disable") {
            false
        } else {
            self.take_word("enable");
            true
        };
        if self.is_word("when") {
            return Err(self.not_supported("WHEN on system triggers"));
        }
        let body = if self.is_word("call") {
            return Err(self.not_supported("CALL bodies on system triggers"));
        } else {
            self.plsql_anonymous_block()?.1
        };
        Ok(Statement::OracleSystemTrigger(Box::new(CreateSystemTrigger {
            name,
            replace,
            timing,
            events,
            scope,
            enabled,
            body,
        })))
    }
}
