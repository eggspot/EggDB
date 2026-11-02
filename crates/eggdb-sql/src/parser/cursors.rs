use super::Parser;
use crate::ast::{CursorCommand, Statement};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::portals::PortalDirection;

impl Parser<'_> {
    pub(super) fn declare_cursor(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        let name = self.name()?.text;
        self.take_word("binary");
        if !self.take_word("insensitive") {
            self.take_word("asensitive");
        }
        let scroll = if self.take_word("no") {
            self.expect_word("scroll")?;
            Some(false)
        } else if self.take_word("scroll") {
            Some(true)
        } else {
            None
        };
        self.expect_word("cursor")?;
        let hold = if self.take_word("with") {
            self.expect_word("hold")?;
            true
        } else {
            if self.take_word("without") {
                self.expect_word("hold")?;
            }
            false
        };
        self.expect_word("for")?;
        let query = self.query_statement()?;
        if !matches!(query, Statement::Select(_) | Statement::Query(_)) {
            return Err(SqlError::new(
                SqlState::INVALID_CURSOR_DEFINITION,
                "DECLARE CURSOR must not contain data-modifying statements in WITH",
            ));
        }
        Ok(Statement::Cursor(CursorCommand::Declare {
            name,
            scroll,
            hold,
            query: Box::new(query),
        }))
    }

    pub(super) fn fetch_cursor(&mut self) -> SqlResult<Statement> {
        let only_move = self.is_word("move");
        self.at += 1;
        let start = self.at;
        let mut direction = self.portal_direction()?;
        let from = self.take_word("from") || self.take_word("in");
        if !from && matches!(self.peek_kind(), TokenKind::Semicolon | TokenKind::End) {
            self.at = start;
            direction = PortalDirection::Next;
        }
        let name = self.name()?.text;
        Ok(Statement::Cursor(CursorCommand::Fetch {
            name,
            direction,
            only_move,
        }))
    }

    pub(super) fn close_cursor(&mut self) -> SqlResult<Statement> {
        self.at += 1;
        if self.take_word("all") {
            return Ok(Statement::Cursor(CursorCommand::Close(None)));
        }
        Ok(Statement::Cursor(CursorCommand::Close(Some(self.name()?.text))))
    }

    fn portal_direction(&mut self) -> SqlResult<PortalDirection> {
        if self.take_word("next") {
            return Ok(PortalDirection::Next);
        }
        if self.take_word("prior") {
            return Ok(PortalDirection::Prior);
        }
        if self.take_word("first") {
            return Ok(PortalDirection::First);
        }
        if self.take_word("last") {
            return Ok(PortalDirection::Last);
        }
        if self.take_word("absolute") {
            return Ok(PortalDirection::Absolute(self.fetch_count()?));
        }
        if self.take_word("relative") {
            return Ok(PortalDirection::Relative(self.fetch_count()?));
        }
        if self.take_word("all") {
            return Ok(PortalDirection::ForwardAll);
        }
        if self.take_word("forward") {
            if self.take_word("all") {
                return Ok(PortalDirection::ForwardAll);
            }
            if self.starts_count() {
                return Ok(PortalDirection::Forward(self.fetch_count()?));
            }
            return Ok(PortalDirection::Next);
        }
        if self.take_word("backward") {
            if self.take_word("all") {
                return Ok(PortalDirection::BackwardAll);
            }
            if self.starts_count() {
                return Ok(PortalDirection::Backward(self.fetch_count()?));
            }
            return Ok(PortalDirection::Prior);
        }
        if self.starts_count() {
            return Ok(PortalDirection::Forward(self.fetch_count()?));
        }
        Ok(PortalDirection::Next)
    }

    fn starts_count(&self) -> bool {
        match self.peek_kind() {
            TokenKind::Integer(_) => true,
            TokenKind::Operator(sign) if *sign == "-" || *sign == "+" => {
                matches!(self.peek_kind_at(1), TokenKind::Integer(_))
            }
            _ => false,
        }
    }

    fn fetch_count(&mut self) -> SqlResult<i64> {
        let position = self.position();
        let negative = self.take_operator("-");
        if !negative {
            self.take_operator("+");
        }
        let TokenKind::Integer(digits) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        self.at += 1;
        let count = digits.parse::<i64>().map_err(|_| {
            SqlError::new(
                SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                format!("value \"{digits}\" is out of range for type bigint"),
            )
            .at(position)
        })?;
        Ok(if negative { -count } else { count })
    }
}
