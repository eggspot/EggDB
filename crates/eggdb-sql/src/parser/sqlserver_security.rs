use super::Parser;
use crate::ast::{SqlServerStatement, Statement};
use crate::error::SqlResult;
use crate::lexer::TokenKind;
use crate::sqlserver_security::{FilterPredicate, PredicateKind, SecurityPolicyCommand};

impl Parser<'_> {
    pub(super) fn at_security_policy(&self) -> bool {
        self.is_word_at(1, "security") && self.is_word_at(2, "policy")
    }

    pub(super) fn sqlserver_security_policy(&mut self) -> SqlResult<Statement> {
        let TokenKind::Word(verb) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let verb = verb.to_string();
        self.at += 3;
        let command = match verb.as_str() {
            "drop" => {
                let if_exists = self.take_word("if") && {
                    self.expect_word("exists")?;
                    true
                };
                SecurityPolicyCommand::Drop {
                    name: self.table_name()?,
                    if_exists,
                }
            }
            "create" => {
                let name = self.table_name()?;
                let mut predicates = Vec::new();
                loop {
                    self.expect_word("add")?;
                    predicates.push(self.filter_predicate()?);
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                let state = self.policy_options()?;
                self.skip_not_for_replication();
                SecurityPolicyCommand::Create {
                    name,
                    predicates,
                    state_on: state.unwrap_or(true),
                }
            }
            _ => {
                let name = self.table_name()?;
                let mut added = Vec::new();
                let mut dropped = Vec::new();
                while self.is_word("add") || self.is_word("drop") || self.is_word("alter") {
                    if self.take_word("add") {
                        added.push(self.filter_predicate()?);
                    } else if self.take_word("drop") {
                        let block = self.predicate_kind()?;
                        self.expect_word("predicate")?;
                        self.expect_word("on")?;
                        let table = self.table_name()?;
                        let kind = if block { self.block_operation()? } else { PredicateKind::Filter };
                        dropped.push((kind, table));
                    } else {
                        return Err(self.not_supported("ALTER ... PREDICATE in a security policy"));
                    }
                    if !self.take(&TokenKind::Comma) {
                        break;
                    }
                }
                let state_on = self.policy_options()?;
                self.skip_not_for_replication();
                SecurityPolicyCommand::Alter {
                    name,
                    added,
                    dropped,
                    state_on,
                }
            }
        };
        Ok(Statement::SqlServer(SqlServerStatement::SecurityPolicy(Box::new(command))))
    }

    fn predicate_kind(&mut self) -> SqlResult<bool> {
        if self.take_word("filter") {
            return Ok(false);
        }
        if self.take_word("block") {
            return Ok(true);
        }
        Err(self.unexpected())
    }

    fn block_operation(&mut self) -> SqlResult<PredicateKind> {
        if self.take_word("after") {
            if self.take_word("insert") {
                return Ok(PredicateKind::BlockAfterInsert);
            }
            if self.take_word("update") {
                return Ok(PredicateKind::BlockAfterUpdate);
            }
            return Err(self.unexpected());
        }
        if self.take_word("before") {
            if self.take_word("update") {
                return Ok(PredicateKind::BlockBeforeUpdate);
            }
            if self.take_word("delete") {
                return Ok(PredicateKind::BlockBeforeDelete);
            }
            return Err(self.unexpected());
        }
        Ok(PredicateKind::BlockAll)
    }

    fn filter_predicate(&mut self) -> SqlResult<FilterPredicate> {
        let block = self.predicate_kind()?;
        self.expect_word("predicate")?;
        let function = self.table_name()?;
        self.expect(&TokenKind::LeftParen)?;
        let mut arguments = Vec::new();
        if !self.take(&TokenKind::RightParen) {
            loop {
                arguments.push(self.name()?);
                if self.take(&TokenKind::RightParen) {
                    break;
                }
                self.expect(&TokenKind::Comma)?;
            }
        }
        self.expect_word("on")?;
        let table = self.table_name()?;
        let kind = if block { self.block_operation()? } else { PredicateKind::Filter };
        Ok(FilterPredicate {
            kind,
            function,
            arguments,
            table,
        })
    }

    fn policy_options(&mut self) -> SqlResult<Option<bool>> {
        if !self.take_word("with") {
            return Ok(None);
        }
        self.expect(&TokenKind::LeftParen)?;
        let mut state = None;
        loop {
            let TokenKind::Word(option) = self.peek_kind() else {
                return Err(self.unexpected());
            };
            let option = option.to_string();
            self.at += 1;
            self.expect(&TokenKind::Operator("="))?;
            let on = if self.take_word("on") {
                true
            } else if self.take_word("off") {
                false
            } else {
                return Err(self.unexpected());
            };
            if option == "state" {
                state = Some(on);
            }
            if self.take(&TokenKind::RightParen) {
                break;
            }
            self.expect(&TokenKind::Comma)?;
        }
        Ok(state)
    }

    fn skip_not_for_replication(&mut self) {
        if self.is_word("not") && self.is_word_at(1, "for") && self.is_word_at(2, "replication") {
            self.at += 3;
        }
    }
}
