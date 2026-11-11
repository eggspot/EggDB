use super::Parser;
use crate::ast::{AlterEvent, EventDefinition, EventSchedule, MySqlStatement, Name, Statement};
use crate::error::SqlResult;
use crate::lexer::TokenKind;

impl Parser<'_> {
    fn event_after_definer(&self) -> usize {
        let mut ahead = 1;
        if self.is_word_at(1, "definer") {
            while ahead < 8 && !self.is_word_at(ahead, "event") {
                ahead += 1;
            }
        }
        ahead
    }

    pub(super) fn creates_mysql_event(&self) -> bool {
        let ahead = self.event_after_definer();
        self.is_word_at(ahead, "event")
    }

    pub(super) fn alters_mysql_event(&self) -> bool {
        let ahead = self.event_after_definer();
        self.is_word_at(ahead, "event")
    }

    fn event_string(&mut self) -> SqlResult<String> {
        let TokenKind::String(text) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let text = text.to_string();
        self.at += 1;
        Ok(text)
    }

    fn event_unit(&mut self) -> SqlResult<String> {
        let TokenKind::Word(word) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let unit = crate::mysql_date_functions::interval_unit(&word.to_ascii_lowercase())
            .ok_or_else(|| self.unexpected())?;
        self.at += 1;
        Ok(unit.to_owned())
    }

    fn event_schedule(&mut self) -> SqlResult<EventSchedule> {
        self.expect_word("schedule")?;
        if self.take_word("at") {
            return Ok(EventSchedule::At(self.expression()?));
        }
        self.expect_word("every")?;
        let amount = self.expression()?;
        let unit = self.event_unit()?;
        let starts = if self.take_word("starts") {
            Some(self.expression()?)
        } else {
            None
        };
        let ends = if self.take_word("ends") {
            Some(self.expression()?)
        } else {
            None
        };
        Ok(EventSchedule::Every {
            amount,
            unit,
            starts,
            ends,
        })
    }

    fn event_completion(&mut self) -> SqlResult<Option<bool>> {
        if !(self.is_word("on") && self.is_word_at(1, "completion")) {
            return Ok(None);
        }
        self.at += 2;
        let preserve = !self.take_word("not");
        self.expect_word("preserve")?;
        Ok(Some(preserve))
    }

    fn event_status(&mut self) -> Option<bool> {
        if self.take_word("enable") {
            return Some(true);
        }
        if self.take_word("disable") {
            if self.is_word("on") && (self.is_word_at(1, "slave") || self.is_word_at(1, "replica"))
            {
                self.at += 2;
            }
            return Some(false);
        }
        None
    }

    fn event_comment(&mut self) -> SqlResult<Option<String>> {
        if self.take_word("comment") {
            return Ok(Some(self.event_string()?));
        }
        Ok(None)
    }

    fn event_body(&mut self) -> SqlResult<String> {
        self.expect_word("do")?;
        self.mysql_compound_body()
    }

    pub(super) fn mysql_create_event(&mut self) -> SqlResult<Statement> {
        let start = self.peek().start;
        self.at += 1;
        self.skip_definer()?;
        self.expect_word("event")?;
        let if_not_exists =
            self.is_word("if") && self.is_word_at(1, "not") && self.is_word_at(2, "exists");
        if if_not_exists {
            self.at += 3;
        }
        let name = self.table_name()?;
        self.expect_word("on")?;
        let schedule = self.event_schedule()?;
        let preserve = self.event_completion()?.unwrap_or(false);
        let enabled = self.event_status().unwrap_or(true);
        let comment = self.event_comment()?;
        let body = self.event_body()?;
        let text = self.slice(start, self.previous_end());
        Ok(Statement::MySql(MySqlStatement::CreateEvent(Box::new(
            EventDefinition {
                name,
                if_not_exists,
                schedule,
                preserve,
                enabled,
                comment,
                body,
                text,
            },
        ))))
    }

    pub(super) fn mysql_alter_event(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        self.skip_definer()?;
        self.expect_word("event")?;
        let name = self.table_name()?;
        let schedule = if self.is_word("on") && self.is_word_at(1, "schedule") {
            self.at += 1;
            Some(self.event_schedule()?)
        } else {
            None
        };
        let preserve = self.event_completion()?;
        let rename: Option<Name> = if self.is_word("rename") && self.is_word_at(1, "to") {
            self.at += 2;
            Some(self.table_name()?)
        } else {
            None
        };
        let enabled = self.event_status();
        let comment = self.event_comment()?;
        let body = if self.is_word("do") {
            Some(self.event_body()?)
        } else {
            None
        };
        if schedule.is_none()
            && preserve.is_none()
            && rename.is_none()
            && enabled.is_none()
            && comment.is_none()
            && body.is_none()
        {
            return Err(self.unexpected());
        }
        Ok(Statement::MySql(MySqlStatement::AlterEvent(Box::new(
            AlterEvent {
                name,
                schedule,
                preserve,
                enabled,
                rename,
                comment,
                body,
            },
        ))))
    }

    pub(super) fn mysql_drop_event(&mut self) -> SqlResult<Statement> {
        self.at += 2;
        let if_exists = self.is_word("if") && self.is_word_at(1, "exists");
        if if_exists {
            self.at += 2;
        }
        let name = self.table_name()?;
        Ok(Statement::MySql(MySqlStatement::DropEvent { name, if_exists }))
    }
}
