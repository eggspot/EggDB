use super::Parser;
use crate::ast::{Statement, TwoPhase};
use crate::error::SqlResult;
use crate::lexer::TokenKind;

impl Parser<'_> {
    pub(super) fn at_two_phase(&self) -> bool {
        (self.postgresql()
            && ((self.is_word("prepare") && self.is_word_at(1, "transaction"))
                || ((self.is_word("commit") || self.is_word("rollback")) && self.is_word_at(1, "prepared"))))
            || (self.mysql() && self.is_word("xa"))
    }

    fn transaction_identifier(&mut self) -> SqlResult<String> {
        let TokenKind::String(text) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let text = text.as_ref().to_owned();
        self.at += 1;
        Ok(text)
    }

    fn xa_identifier(&mut self) -> SqlResult<String> {
        let gid = self.transaction_identifier()?;
        let mut whole = gid;
        while self.take(&TokenKind::Comma) {
            match self.peek_kind() {
                TokenKind::String(part) => {
                    whole.push(',');
                    whole.push_str(part.as_ref());
                    self.at += 1;
                }
                TokenKind::Integer(number) => {
                    whole.push(',');
                    whole.push_str(number.as_ref());
                    self.at += 1;
                }
                _ => return Err(self.unexpected()),
            }
        }
        Ok(whole)
    }

    pub(super) fn two_phase(&mut self) -> SqlResult<Statement> {
        if self.take_word("prepare") {
            self.expect_word("transaction")?;
            return Ok(Statement::TwoPhase(TwoPhase::Prepare(self.transaction_identifier()?)));
        }
        if self.take_word("commit") {
            self.expect_word("prepared")?;
            return Ok(Statement::TwoPhase(TwoPhase::CommitPrepared(self.transaction_identifier()?)));
        }
        if self.take_word("rollback") {
            self.expect_word("prepared")?;
            return Ok(Statement::TwoPhase(TwoPhase::RollbackPrepared(self.transaction_identifier()?)));
        }
        self.expect_word("xa")?;
        let command = if self.take_word("start") || self.take_word("begin") {
            let gid = self.xa_identifier()?;
            let _ = self.take_word("join") || self.take_word("resume");
            TwoPhase::XaStart(gid)
        } else if self.take_word("end") {
            let gid = self.xa_identifier()?;
            if self.take_word("suspend") {
                let _ = self.take_word("for") && self.take_word("migrate");
            }
            TwoPhase::XaEnd(gid)
        } else if self.take_word("prepare") {
            TwoPhase::Prepare(self.xa_identifier()?)
        } else if self.take_word("commit") {
            let gid = self.xa_identifier()?;
            if self.take_word("one") {
                self.expect_word("phase")?;
                TwoPhase::XaCommitOnePhase(gid)
            } else {
                TwoPhase::CommitPrepared(gid)
            }
        } else if self.take_word("rollback") {
            TwoPhase::XaRollback(self.xa_identifier()?)
        } else if self.take_word("recover") {
            if self.take_word("convert") {
                self.expect_word("xid")?;
            }
            TwoPhase::Recover
        } else {
            return Err(self.unexpected());
        };
        Ok(Statement::TwoPhase(command))
    }
}
