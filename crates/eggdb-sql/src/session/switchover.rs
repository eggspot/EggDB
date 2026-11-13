use super::{QueryResult, Session};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{TokenKind, tokenize_in};
use std::sync::OnceLock;

type SwitchoverHook = fn(&str) -> Result<String, String>;

static HOOK: OnceLock<SwitchoverHook> = OnceLock::new();

/// Lets `SWITCHOVER TO '<host:port>'` hand the primary's role to that replica (D1026); the
/// server that replicates installs it once. The hook returns the message the client sees.
pub fn set_switchover_hook(hook: SwitchoverHook) {
    let _ = HOOK.set(hook);
}

fn target(text: &str, session_dialect: crate::dialect::Dialect) -> SqlResult<Option<String>> {
    let trimmed = text.trim_start();
    if !trimmed.get(..10).is_some_and(|start| start.eq_ignore_ascii_case("switchover")) {
        return Ok(None);
    }
    let tokens = tokenize_in(text, session_dialect)?;
    let kinds: Vec<&TokenKind<'_>> = tokens.iter().map(|token| &token.kind).filter(|kind| !matches!(kind, TokenKind::Semicolon | TokenKind::End)).collect();
    match kinds.as_slice() {
        [TokenKind::Word(first), TokenKind::Word(to), TokenKind::String(address)] if first.eq_ignore_ascii_case("switchover") && to.eq_ignore_ascii_case("to") => {
            Ok(Some(address.to_string()))
        }
        [TokenKind::Word(first), ..] if first.eq_ignore_ascii_case("switchover") => {
            Err(SqlError::new(SqlState::SYNTAX_ERROR, "SWITCHOVER takes the form SWITCHOVER TO 'host:port'"))
        }
        _ => Ok(None),
    }
}

impl Session<'_> {
    /// Runs `SWITCHOVER TO 'host:port'`: a superuser hands the primary's role to the replica
    /// listening for replication there (D1026).
    pub(super) fn prepare_switchover(&mut self, text: &str, on_result: &mut dyn FnMut(QueryResult) -> bool) -> SqlResult<bool> {
        let Some(address) = target(text, self.dialect)? else {
            return Ok(false);
        };
        if self.settings.checked_role.is_some() || !self.settings.is_superuser() {
            return Err(SqlError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied: SWITCHOVER needs a superuser"));
        }
        let Some(hook) = HOOK.get() else {
            return Err(SqlError::new(SqlState::FEATURE_NOT_SUPPORTED, "this server is not a replication primary"));
        };
        let message = hook(&address).map_err(|message| SqlError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, message))?;
        on_result(QueryResult::command("SWITCHOVER").with_notice(message));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switchover_statement_names_its_target() {
        assert_eq!(target("SWITCHOVER TO 'db2:5444';", crate::dialect::Dialect::PostgreSql).unwrap(), Some("db2:5444".into()));
        assert!(target("SWITCHOVER db2", crate::dialect::Dialect::PostgreSql).is_err());
        assert_eq!(target("SELECT 1", crate::dialect::Dialect::PostgreSql).unwrap(), None);
        assert_eq!(target("switchovñr", crate::dialect::Dialect::PostgreSql).unwrap(), None);
    }
}
