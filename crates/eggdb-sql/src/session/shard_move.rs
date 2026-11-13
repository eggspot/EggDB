use super::{QueryResult, Session};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{TokenKind, tokenize_in};
use std::sync::OnceLock;

type ShardMoveHook = fn(u32, &str) -> Result<String, String>;

static HOOK: OnceLock<ShardMoveHook> = OnceLock::new();

/// Lets `MOVE SHARD <n> FROM '<host:port>'` move a shard of the cluster's shard map to this
/// server (D1040); the server that replicates installs it once. The hook returns the message
/// the client sees.
pub fn set_shard_move_hook(hook: ShardMoveHook) {
    let _ = HOOK.set(hook);
}

const FORM: &str = "MOVE SHARD takes the form MOVE SHARD <number> FROM 'host:port'";

fn request(text: &str, dialect: crate::dialect::Dialect) -> SqlResult<Option<(u32, String)>> {
    let trimmed = text.trim_start();
    if trimmed.len() < 4 || !trimmed[..4].eq_ignore_ascii_case("move") {
        return Ok(None);
    }
    let tokens = tokenize_in(text, dialect)?;
    let kinds: Vec<&TokenKind<'_>> = tokens
        .iter()
        .map(|token| &token.kind)
        .filter(|kind| !matches!(kind, TokenKind::Semicolon | TokenKind::End))
        .collect();
    match kinds.as_slice() {
        [
            TokenKind::Word(verb),
            TokenKind::Word(shard),
            TokenKind::Integer(number),
            TokenKind::Word(from),
            TokenKind::String(address),
        ] if verb.eq_ignore_ascii_case("move")
            && shard.eq_ignore_ascii_case("shard")
            && from.eq_ignore_ascii_case("from") =>
        {
            let number = number
                .parse::<u32>()
                .map_err(|_| SqlError::new(SqlState::SYNTAX_ERROR, FORM))?;
            Ok(Some((number, address.to_string())))
        }
        [TokenKind::Word(verb), TokenKind::Word(shard), ..]
            if verb.eq_ignore_ascii_case("move") && shard.eq_ignore_ascii_case("shard") =>
        {
            Err(SqlError::new(SqlState::SYNTAX_ERROR, FORM))
        }
        _ => Ok(None),
    }
}

impl Session<'_> {
    /// Runs `MOVE SHARD <n> FROM 'host:port'`: a superuser moves the shard to this server,
    /// copying it from its owner over the replication protocol (D1040).
    pub(super) fn prepare_shard_move(
        &mut self,
        text: &str,
        on_result: &mut dyn FnMut(QueryResult) -> bool,
    ) -> SqlResult<bool> {
        let Some((shard, address)) = request(text, self.dialect)? else {
            return Ok(false);
        };
        if self.settings.checked_role.is_some() || !self.settings.is_superuser() {
            return Err(SqlError::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "permission denied: MOVE SHARD needs a superuser",
            ));
        }
        let Some(hook) = HOOK.get() else {
            return Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "this server does not take part in a sharded cluster",
            ));
        };
        let message = hook(shard, &address)
            .map_err(|message| SqlError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, message))?;
        on_result(QueryResult::command("MOVE SHARD").with_notice(message));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;

    #[test]
    fn the_move_statement_names_the_shard_and_its_owner() {
        assert_eq!(
            request("MOVE SHARD 7 FROM 'db2:5444';", Dialect::PostgreSql).unwrap(),
            Some((7, "db2:5444".to_owned()))
        );
        assert!(request("move shard x from 'a:1'", Dialect::PostgreSql).is_err());
        assert!(request("MOVE SHARD 99999999999 FROM 'a:1'", Dialect::PostgreSql).is_err());
        assert_eq!(request("MOVE NEXT FROM c", Dialect::PostgreSql).unwrap(), None);
        assert_eq!(request("SELECT 1", Dialect::PostgreSql).unwrap(), None);
    }
}
