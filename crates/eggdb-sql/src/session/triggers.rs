use super::{Block, QueryResult, Session};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::store::{KeyValueStore, KeyValueWrite};
use crate::triggers::{StatementTriggerCall, TriggerAction};
use std::sync::Arc;

fn transaction_ended_in_trigger() -> SqlError {
    SqlError::new(
        SqlState::TRANSACTION_ROLLBACK,
        "The transaction ended in the trigger. The batch has been aborted.",
    )
    .with_sqlserver_code(3609)
}

fn statement_level_statements() -> SqlError {
    SqlError::not_supported("statement-level triggers with a statement body")
}

impl<'s, S: KeyValueStore + 's> Session<'s, S> {
    pub(super) fn write_firing_triggers(
        &mut self,
        command: &str,
        body: impl FnOnce(&mut dyn KeyValueWrite, &mut TableCache) -> SqlResult<QueryResult>,
    ) -> SqlResult<QueryResult> {
        let scope = crate::triggers::enter_statement(Arc::clone(&self.settings));
        let written = self.write(command, body);
        let pending = scope.take_pending();
        drop(scope);
        let opened = std::mem::take(&mut self.trigger_transaction);
        let fired = !pending.is_empty();
        let result = match written {
            Ok(result) if fired => self.fire_statement_triggers(pending).map(|()| result),
            other => other,
        };
        if opened {
            let finished = self.finish_trigger_transaction(result.is_ok());
            return finished.and(result);
        }
        if fired && result.is_err() && self.in_block() {
            if self.sqlserver() {
                self.abandon_transaction()?;
            } else {
                self.rollback()?;
            }
        }
        result
    }

    fn fire_statement_triggers(&mut self, calls: Vec<StatementTriggerCall>) -> SqlResult<()> {
        let was_in_block = self.in_block();
        for call in calls {
            match &call.trigger.action {
                TriggerAction::Batch(_) => self.run_tsql_trigger(call)?,
                TriggerAction::Function { .. } | TriggerAction::PlSql { .. } => {
                    self.run_function_trigger(&call)?;
                }
                TriggerAction::Statements(_) => return Err(statement_level_statements()),
            }
            if was_in_block && !self.in_block() {
                return Err(transaction_ended_in_trigger());
            }
        }
        Ok(())
    }

    fn run_function_trigger(&mut self, call: &StatementTriggerCall) -> SqlResult<()> {
        let settings = Arc::clone(&self.settings);
        let tag = call.event.keyword();
        self.write_firing_triggers(tag, |writer, tables| {
            crate::triggers::run_statement_function(writer, tables, &settings, call)?;
            Ok(QueryResult::command(tag))
        })?;
        Ok(())
    }

    fn finish_trigger_transaction(&mut self, commit: bool) -> SqlResult<()> {
        let Block::Writing(writer) = std::mem::replace(&mut self.block, Block::None) else {
            return Ok(());
        };
        if !commit {
            self.temporary.rollback();
            return writer.rollback();
        }
        let pending = crate::deferred_checks::take_statement_pending();
        if !pending.is_empty() {
            let checked = crate::constraints::check_pending(
                &crate::temporary::TemporaryRead {
                    inner: &writer,
                    objects: &self.temporary,
                },
                &mut self.tables,
                &pending,
            );
            if let Err(error) = checked {
                let _ = writer.rollback();
                self.temporary.rollback();
                return Err(error);
            }
        }
        if let Err(error) = writer.commit() {
            self.temporary.rollback();
            return Err(error);
        }
        self.temporary.commit();
        crate::sequences::mark_changes_committed();
        Ok(())
    }

    pub(super) fn in_trigger_dialect<T>(
        &mut self,
        dialect: Dialect,
        body: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let outer = self.dialect;
        self.dialect = dialect;
        let outcome = body(self);
        self.dialect = outer;
        outcome
    }
}
