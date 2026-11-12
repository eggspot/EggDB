use super::Parser;
use crate::ast::{AlterAction, Name, SystemVersioning};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;

const IMPLICIT_START: &str = "row_start";
const IMPLICIT_END: &str = "row_end";

fn period_missing() -> SqlError {
    SqlError::new(
        SqlState::INVALID_TABLE_DEFINITION,
        "Cannot set SYSTEM_VERSIONING to ON when SYSTEM_TIME period is not defined.",
    )
    .with_vendor_code(13510)
}

impl Parser<'_> {
    pub(super) fn generated_row_period_column(&mut self) -> bool {
        if !(self.is_word("generated")
            && self.is_word_at(1, "always")
            && self.is_word_at(2, "as")
            && self.is_word_at(3, "row")
            && (self.is_word_at(4, "start") || self.is_word_at(4, "end")))
        {
            return false;
        }
        self.at += 5;
        self.take_word("hidden");
        true
    }

    pub(super) fn system_time_period(&mut self) -> SqlResult<bool> {
        if !(self.is_word("period") && self.is_word_at(1, "for") && self.is_word_at(2, "system_time"))
        {
            return Ok(false);
        }
        self.at += 3;
        self.expect(&TokenKind::LeftParen)?;
        let start = self.name()?.text;
        self.expect(&TokenKind::Comma)?;
        let end = self.name()?.text;
        self.expect(&TokenKind::RightParen)?;
        self.system_period = Some((start, end));
        Ok(true)
    }

    pub(super) fn mysql_with_system_versioning(&mut self) -> bool {
        if self.is_word("with") && self.is_word_at(1, "system") && self.is_word_at(2, "versioning") {
            self.at += 3;
            self.system_versioning_on = Some(None);
            return true;
        }
        false
    }

    pub(super) fn sqlserver_system_versioning_option(&mut self) -> SqlResult<bool> {
        if !self.is_word("system_versioning") {
            return Ok(false);
        }
        self.at += 1;
        if !self.take_operator("=") {
            return Err(self.unexpected());
        }
        if self.take_word("off") {
            self.system_versioning_on = None;
            return Ok(true);
        }
        self.expect_word("on")?;
        let mut history = None;
        if self.take(&TokenKind::LeftParen) {
            loop {
                if self.take_word("history_table") {
                    if !self.take_operator("=") {
                        return Err(self.unexpected());
                    }
                    history = Some(self.history_table_name()?);
                } else if self.take_word("data_consistency_check") {
                    if !self.take_operator("=") {
                        return Err(self.unexpected());
                    }
                    if !(self.take_word("on") || self.take_word("off")) {
                        return Err(self.unexpected());
                    }
                } else if self.take_word("history_retention_period") {
                    if !self.take_operator("=") {
                        return Err(self.unexpected());
                    }
                    self.system_versioning_retention = self.history_retention()?;
                } else {
                    return Err(self.unexpected());
                }
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        self.system_versioning_on = Some(history);
        Ok(true)
    }

    fn history_retention(&mut self) -> SqlResult<Option<String>> {
        if self.take_word("infinite") {
            return Ok(None);
        }
        let TokenKind::Integer(digits) = self.kind() else {
            return Err(self.unexpected());
        };
        let Some(count) = digits.parse::<u32>().ok().filter(|count| *count > 0) else {
            return Err(self.unexpected());
        };
        self.advance();
        let unit = match self.peek_kind() {
            TokenKind::Word(word) => match word.as_str() {
                "day" | "days" => "days",
                "week" | "weeks" => "weeks",
                "month" | "months" => "months",
                "year" | "years" => "years",
                _ => return Err(self.unexpected()),
            },
            _ => return Err(self.unexpected()),
        };
        self.advance();
        Ok(Some(format!("{count} {unit}")))
    }

    fn history_table_name(&mut self) -> SqlResult<Name> {
        let first = self.name()?;
        if self.take(&TokenKind::Dot) {
            let table = self.name()?;
            return Ok(Name {
                text: crate::schemas::written_qualified(&first.text, &table.text),
                position: first.position,
            });
        }
        Ok(first)
    }

    pub(super) fn take_system_versioning(&mut self) -> SqlResult<Option<SystemVersioning>> {
        let period = self.system_period.take();
        let retention = self.system_versioning_retention.take();
        let Some(history) = self.system_versioning_on.take() else {
            return Ok(period.map(|(start, end)| SystemVersioning {
                start,
                end,
                history: None,
                on: false,
                retention: None,
            }));
        };
        let (start, end) = match period {
            Some(pair) => pair,
            None if self.mysql() => (IMPLICIT_START.to_owned(), IMPLICIT_END.to_owned()),
            None => return Err(period_missing()),
        };
        Ok(Some(SystemVersioning {
            start,
            end,
            history,
            on: true,
            retention,
        }))
    }

    pub(super) fn alter_system_versioning(&mut self, actions: &mut Vec<AlterAction>) -> bool {
        let adding = self.is_word("add");
        if !((adding || self.is_word("drop"))
            && self.is_word_at(1, "system")
            && self.is_word_at(2, "versioning"))
        {
            return false;
        }
        self.at += 3;
        actions.push(AlterAction::SystemVersioning(adding.then(|| SystemVersioning {
            start: IMPLICIT_START.to_owned(),
            end: IMPLICIT_END.to_owned(),
            history: None,
            on: true,
            retention: None,
        })));
        true
    }

    pub(super) fn sqlserver_alter_system_versioning(
        &mut self,
        actions: &mut Vec<AlterAction>,
    ) -> SqlResult<bool> {
        if !(self.is_word("set")
            && self.peek_kind_at(1) == &TokenKind::LeftParen
            && self.is_word_at(2, "system_versioning"))
        {
            return Ok(false);
        }
        self.at += 2;
        self.sqlserver_system_versioning_option()?;
        self.expect(&TokenKind::RightParen)?;
        let retention = self.system_versioning_retention.take();
        match self.system_versioning_on.take() {
            None => actions.push(AlterAction::SystemVersioning(None)),
            Some(history) => actions.push(AlterAction::SystemVersioningOn { history, retention }),
        }
        Ok(true)
    }

    pub(super) fn sqlserver_add_system_period(&mut self, actions: &mut Vec<AlterAction>) -> SqlResult<bool> {
        if !self.system_time_period()? {
            return Ok(false);
        }
        if let Some((start, end)) = self.system_period.take() {
            actions.push(AlterAction::SystemPeriod { start, end });
        }
        Ok(true)
    }
}
