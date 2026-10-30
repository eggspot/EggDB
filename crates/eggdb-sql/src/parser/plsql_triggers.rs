use super::Parser;
use super::triggers::inconsistent;
use crate::ast::{
    AlterTrigger, AlterTriggerAction, CreateTrigger, DropTrigger, EnableTrigger, Statement,
    TriggerName, TriggerPlacement,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::plsql::TriggerRecords;
use crate::triggers::{
    TriggerAction, TriggerDefinition, TriggerEvents, TriggerLevel, TriggerTiming,
};

const DML_EVENTS: [&str; 3] = ["insert", "update", "delete"];
const CROSSEDITION_WORDS: [&str; 3] = ["forward", "reverse", "crossedition"];

fn trigger_error(code: u16, message: &str, position: usize) -> SqlError {
    SqlError::new(SqlState::INVALID_OBJECT_DEFINITION, message)
        .with_oracle_code(code)
        .at(position)
}

fn table_level_when(position: usize) -> SqlError {
    trigger_error(
        4077,
        "WHEN clause cannot be used with table level triggers",
        position,
    )
}

fn table_level_references(position: usize) -> SqlError {
    trigger_error(
        4082,
        "NEW or OLD references not allowed in table level triggers",
        position,
    )
}

fn new_values_fixed(position: usize) -> SqlError {
    trigger_error(
        4084,
        "cannot change NEW values for this trigger type",
        position,
    )
}

fn old_values_fixed(position: usize) -> SqlError {
    trigger_error(
        4085,
        "cannot change the value of an OLD reference variable",
        position,
    )
}

struct TriggerHeading {
    timing: TriggerTiming,
    events: TriggerEvents,
    columns: Vec<String>,
    table: crate::ast::Name,
    records: TriggerRecords,
    level: TriggerLevel,
    placement: Option<TriggerPlacement>,
    enabled: bool,
    condition: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecordUse {
    Read,
    Assigned,
}

impl<'t> Parser<'t> {
    pub(super) fn plsql_create_trigger(
        &mut self,
        start: usize,
        replace: bool,
    ) -> SqlResult<Statement> {
        let position = self.position();
        self.expect_word("trigger")?;
        let (_, name) = self.trigger_name()?;
        let heading = self.plsql_trigger_heading()?;
        let body_tokens = self.at;
        let body = if self.take_word("call") {
            self.plsql_trigger_call(&heading.records)?
        } else {
            self.plsql_trigger_block(&heading.records)?
        };
        self.check_record_uses(body_tokens, &heading)?;
        let TriggerRecords { new, old } = heading.records;
        let definition = TriggerDefinition {
            name: name.text.clone(),
            dialect: Dialect::Oracle,
            timing: heading.timing,
            events: heading.events,
            columns: heading.columns,
            level: heading.level,
            condition: heading.condition,
            old_table: None,
            new_table: None,
            action: TriggerAction::PlSql { body, new, old },
            enabled: heading.enabled,
            text: self.slice(start, self.previous_end()),
        };
        if !definition.is_consistent() {
            return Err(inconsistent(position));
        }
        Ok(Statement::CreateTrigger(Box::new(CreateTrigger {
            name,
            table: heading.table,
            replace,
            if_not_exists: false,
            placement: heading.placement,
            definition,
        })))
    }

    fn plsql_trigger_heading(&mut self) -> SqlResult<TriggerHeading> {
        if self.is_word("for") {
            return Err(self.not_supported("compound triggers"));
        }
        let Some(timing) = self.trigger_timing()? else {
            return Err(self.unexpected());
        };
        if !DML_EVENTS.iter().any(|event| self.is_word(event)) {
            return Err(self.not_supported("DDL and database event triggers"));
        }
        let mut columns = Vec::new();
        let mut events = TriggerEvents::default().with(self.trigger_event(&mut columns)?);
        while self.take_word("or") {
            events = events.with(self.trigger_event(&mut columns)?);
        }
        self.expect_word("on")?;
        if self.is_word("nested") {
            return Err(self.not_supported("triggers on nested table columns"));
        }
        let table = self.table_name()?;
        let records = self.plsql_trigger_referencing()?;
        let level = if self.take_word("for") {
            self.expect_word("each")?;
            self.expect_word("row")?;
            TriggerLevel::Row
        } else if timing == TriggerTiming::InsteadOf {
            TriggerLevel::Row
        } else {
            TriggerLevel::Statement
        };
        if CROSSEDITION_WORDS.iter().any(|word| self.is_word(word)) {
            return Err(self.not_supported("crossedition triggers"));
        }
        let placement = self.plsql_trigger_ordering()?;
        let enabled = if self.take_word("disable") {
            false
        } else {
            self.take_word("enable");
            true
        };
        let condition = if self.is_word("when") {
            if level == TriggerLevel::Statement {
                return Err(table_level_when(self.position()));
            }
            self.at += 1;
            Some(self.plsql_trigger_condition(&records)?)
        } else {
            None
        };
        Ok(TriggerHeading {
            timing,
            events,
            columns,
            table,
            records,
            level,
            placement,
            enabled,
            condition,
        })
    }

    fn plsql_trigger_referencing(&mut self) -> SqlResult<TriggerRecords> {
        let mut records = TriggerRecords::default();
        if !self.take_word("referencing") {
            return Ok(records);
        }
        let mut named = false;
        loop {
            let is_new = if self.take_word("new") {
                Some(true)
            } else if self.take_word("old") {
                Some(false)
            } else if self.take_word("parent") {
                None
            } else {
                break;
            };
            self.take_word("as");
            let alias = self.name()?.text;
            named = true;
            match is_new {
                Some(true) => records.new = alias,
                Some(false) => records.old = alias,
                None => {}
            }
        }
        if !named {
            return Err(self.unexpected());
        }
        Ok(records)
    }

    fn plsql_trigger_ordering(&mut self) -> SqlResult<Option<TriggerPlacement>> {
        let follows = if self.take_word("follows") {
            true
        } else if self.take_word("precedes") {
            false
        } else {
            return Ok(None);
        };
        let (_, other) = self.trigger_name()?;
        while self.take(&TokenKind::Comma) {
            self.trigger_name()?;
        }
        Ok(Some(TriggerPlacement { follows, other }))
    }

    fn plsql_trigger_condition(&mut self, records: &TriggerRecords) -> SqlResult<String> {
        self.expect(&TokenKind::LeftParen)?;
        let first = self.at;
        let start = self.position();
        self.expression()?;
        let end = self.previous_end();
        let last = self.at;
        self.expect(&TokenKind::RightParen)?;
        let mut text = String::new();
        let mut copied = start;
        for at in first..last {
            let Some(token) = self.tokens.get(at) else {
                break;
            };
            let TokenKind::Word(word) = &token.kind else {
                continue;
            };
            if self.peek_kind_at_index(at + 1) != &TokenKind::Dot {
                continue;
            }
            let record = if word.eq_ignore_ascii_case(&records.new) {
                "new"
            } else if word.eq_ignore_ascii_case(&records.old) {
                "old"
            } else {
                continue;
            };
            text.push_str(&self.slice(copied, token.start));
            text.push_str(record);
            copied = token.end;
        }
        text.push_str(&self.slice(copied, end));
        Ok(text)
    }

    fn peek_kind_at_index(&self, index: usize) -> &'t TokenKind<'t> {
        let index = index.min(self.tokens.len().saturating_sub(1));
        &self.tokens[index].kind
    }

    fn plsql_trigger_call(&mut self, records: &TriggerRecords) -> SqlResult<String> {
        let start = self.position();
        let first = self.at;
        while !matches!(self.peek_kind(), TokenKind::Semicolon | TokenKind::End) {
            self.at += 1;
        }
        if self.at == first {
            return Err(self.unexpected());
        }
        let call = self.slice(start, self.previous_end());
        let body = format!("BEGIN\n  {call};\nEND;");
        crate::plsql::compile_trigger(&body, records)?;
        Ok(body)
    }

    fn assignment_follows(&self, index: usize) -> bool {
        match self.peek_kind_at_index(index) {
            TokenKind::Operator(symbol) => *symbol == ":=",
            TokenKind::Colon => matches!(
                self.peek_kind_at_index(index + 1),
                TokenKind::Operator(symbol) if *symbol == "="
            ),
            _ => false,
        }
    }

    fn record_use(&self, at: usize, records: &TriggerRecords) -> Option<(bool, RecordUse)> {
        let token = self.tokens.get(at)?;
        if !matches!(token.kind, TokenKind::NumberedParameter(_)) {
            return None;
        }
        let written = self.text.get(token.start + 1..token.end)?;
        let is_new = if written.eq_ignore_ascii_case(&records.new) {
            true
        } else if written.eq_ignore_ascii_case(&records.old) {
            false
        } else {
            return None;
        };
        let assigned = self.peek_kind_at_index(at + 1) == &TokenKind::Dot
            && matches!(
                self.peek_kind_at_index(at + 2),
                TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
            )
            && self.assignment_follows(at + 3);
        let used = if assigned {
            RecordUse::Assigned
        } else {
            RecordUse::Read
        };
        Some((is_new, used))
    }

    fn check_record_uses(&self, first: usize, heading: &TriggerHeading) -> SqlResult<()> {
        for at in first..self.at {
            let Some((is_new, used)) = self.record_use(at, &heading.records) else {
                continue;
            };
            let position = self.tokens.get(at).map_or(0, |token| token.start);
            if heading.level == TriggerLevel::Statement {
                return Err(table_level_references(position));
            }
            if used == RecordUse::Assigned && !is_new {
                return Err(old_values_fixed(position));
            }
            if used == RecordUse::Assigned && heading.timing != TriggerTiming::Before {
                return Err(new_values_fixed(position));
            }
        }
        Ok(())
    }

    pub(super) fn plsql_drop_trigger(&mut self) -> SqlResult<Statement> {
        self.at += 2;
        let if_exists = if self.is_word("if") && self.is_word_at(1, "exists") {
            self.at += 2;
            true
        } else {
            false
        };
        let (schema, name) = self.trigger_name()?;
        Ok(Statement::DropTrigger(DropTrigger {
            names: vec![TriggerName { schema, name }],
            table: None,
            if_exists,
        }))
    }

    pub(super) fn plsql_alter_trigger(&mut self) -> SqlResult<Statement> {
        self.at += 2;
        let (schema, name) = self.trigger_name()?;
        let action = if self.take_word("enable") {
            AlterTriggerAction::Enable
        } else if self.take_word("disable") {
            AlterTriggerAction::Disable
        } else if self.take_word("rename") {
            self.expect_word("to")?;
            AlterTriggerAction::RenameTo(self.name()?)
        } else if self.take_editioning() {
            AlterTriggerAction::Compile
        } else {
            self.expect_word("compile")?;
            self.take_word("debug");
            self.plsql_compiler_settings()?;
            AlterTriggerAction::Compile
        };
        Ok(Statement::AlterTrigger(AlterTrigger {
            name: TriggerName { schema, name },
            action,
        }))
    }

    pub(super) fn alters_all_triggers(&self) -> bool {
        if !self.is_word_at(1, "table") {
            return false;
        }
        let mut ahead = 2;
        loop {
            if !matches!(
                self.peek_kind_at(ahead),
                TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
            ) {
                return false;
            }
            ahead += 1;
            if self.peek_kind_at(ahead) != &TokenKind::Dot {
                break;
            }
            ahead += 1;
        }
        (self.is_word_at(ahead, "enable") || self.is_word_at(ahead, "disable"))
            && self.is_word_at(ahead + 1, "all")
            && self.is_word_at(ahead + 2, "triggers")
    }

    pub(super) fn plsql_enable_all_triggers(&mut self) -> SqlResult<Statement> {
        self.at += 2;
        let table = self.table_name()?;
        let enable = self.take_word("enable");
        if !enable {
            self.expect_word("disable")?;
        }
        self.expect_word("all")?;
        self.expect_word("triggers")?;
        Ok(Statement::EnableTrigger(EnableTrigger {
            names: None,
            table,
            enable,
        }))
    }
}
