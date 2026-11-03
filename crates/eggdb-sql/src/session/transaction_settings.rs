use super::{Block, Session, is_dialect_setting};
use crate::dialect::Dialect;
use crate::error::SqlResult;
use crate::session::QueryResult;
use crate::settings::{SettingChange, SettingScope};
use std::sync::Arc;

const SET_LOCAL_OUTSIDE_TRANSACTION: &str =
    "WARNING:  SET LOCAL can only be used in transaction blocks";

impl Session<'_> {
    pub(super) fn settings_follow_transaction(&self) -> bool {
        self.dialect == Dialect::PostgreSql
            && (!matches!(self.block, Block::None)
                || (self.implicit_transaction && !self.statement_stands_alone))
    }

    pub(super) fn change_setting(
        &mut self,
        name: &str,
        value: Option<&str>,
        scope: SettingScope,
    ) -> SqlResult<bool> {
        let follows = self.settings_follow_transaction();
        if scope == SettingScope::Transaction && !follows {
            return Ok(false);
        }
        if follows {
            self.transaction_settings
                .before_change(&self.settings, name, scope);
        }
        Arc::make_mut(&mut self.settings).set(name, value)?;
        Ok(true)
    }

    pub(super) fn set_statement(
        &mut self,
        name: &str,
        value: Option<&str>,
        scope: SettingScope,
    ) -> SqlResult<QueryResult> {
        let changed = self.change_setting(name, value, scope)?;
        let result = QueryResult::command("SET");
        Ok(if changed {
            result
        } else {
            result.with_notice(SET_LOCAL_OUTSIDE_TRANSACTION.into())
        })
    }

    pub(super) fn apply_routine_setting_changes(&mut self, changes: Vec<SettingChange>) {
        for change in changes {
            if is_dialect_setting(&change.name) {
                continue;
            }
            let _ = match change.value {
                Some(value) => self
                    .change_setting(&change.name, Some(value.as_str()), change.scope)
                    .map(drop),
                None if change.scope == SettingScope::Session => self.reset_setting(&change.name),
                None => self.change_setting(&change.name, None, change.scope).map(drop),
            };
        }
    }

    pub(super) fn end_transaction_settings(&mut self, committed: bool) {
        if self.transaction_settings.is_empty() {
            return;
        }
        let settings = Arc::make_mut(&mut self.settings);
        if committed {
            self.transaction_settings.commit(settings);
        } else {
            self.transaction_settings.rollback(settings);
        }
    }
}
