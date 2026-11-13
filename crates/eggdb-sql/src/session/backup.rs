use super::{QueryResult, Session};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{TokenKind, tokenize_in};

struct BackupStatement {
    path: String,
    clone: bool,
}

fn word(kind: &TokenKind<'_>, expected: &str) -> bool {
    matches!(kind, TokenKind::Word(found) if found.eq_ignore_ascii_case(expected))
}

fn backup_statement(text: &str, dialect: Dialect) -> SqlResult<Option<BackupStatement>> {
    let trimmed = text.trim_start();
    if trimmed.len() < 6 || !trimmed[..6].eq_ignore_ascii_case("backup") {
        return Ok(None);
    }
    let tokens = tokenize_in(text, dialect)?;
    let kinds: Vec<&TokenKind<'_>> = tokens
        .iter()
        .map(|token| &token.kind)
        .filter(|kind| !matches!(kind, TokenKind::Semicolon | TokenKind::End))
        .collect();
    let invalid = || {
        SqlError::new(
            SqlState::SYNTAX_ERROR,
            "BACKUP takes the form BACKUP TO 'path' [CLONE], or BACKUP DATABASE name TO DISK = 'path' [WITH ...] in Transact-SQL",
        )
    };
    let path_at = match kinds.as_slice() {
        [first, second, ..] if word(first, "backup") && word(second, "to") => 2,
        [first, second, _, fourth, fifth, ..] if word(first, "backup") && word(second, "database") && word(fourth, "to") && word(fifth, "disk") => {
            if !matches!(kinds.get(5), Some(TokenKind::Operator("="))) {
                return Err(invalid());
            }
            6
        }
        _ => return Ok(None),
    };
    let path_at = if kinds.get(path_at).is_some_and(|kind| word(kind, "n")) { path_at + 1 } else { path_at };
    let Some(TokenKind::String(path)) = kinds.get(path_at) else {
        return Err(invalid());
    };
    let rest = &kinds[path_at + 1..];
    let clone = match rest {
        [] => false,
        [only] if word(only, "clone") => true,
        [first, ..] if word(first, "with") => rest.iter().any(|kind| word(kind, "clone")),
        _ => return Err(invalid()),
    };
    Ok(Some(BackupStatement { path: path.to_string(), clone }))
}

impl Session<'_> {
    /// Runs `BACKUP TO 'path' [CLONE]`, or Transact-SQL's `BACKUP DATABASE name TO DISK =
    /// 'path'`, through the server that holds the file (D1023): a superuser writes a copy of
    /// the last commit to a new file on the server's disk.
    pub(super) fn prepare_backup(&mut self, text: &str, on_result: &mut dyn FnMut(QueryResult) -> bool) -> SqlResult<bool> {
        let Some(statement) = backup_statement(text, self.dialect)? else {
            return Ok(false);
        };
        if self.settings.checked_role.is_some() || !self.settings.is_superuser() {
            return Err(SqlError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied: BACKUP writes a file on the server, so it needs a superuser"));
        }
        if statement.path.is_empty() {
            return Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "BACKUP needs the path of a new file on the server"));
        }
        let Some(done) = self.store.erased_backup_to(std::path::Path::new(&statement.path), statement.clone)? else {
            return Err(SqlError::new(SqlState::FEATURE_NOT_SUPPORTED, "this server keeps no file to back up"));
        };
        let how = if done.instant {
            "a copy-on-write clone".to_owned()
        } else if statement.clone {
            format!("a copy of {} entries (the file system makes no clones)", done.entries)
        } else {
            format!("a copy of {} entries", done.entries)
        };
        on_result(QueryResult::command("BACKUP").with_notice(format!(
            "backed up the commit stamped {} to {}: {how}",
            done.stamp, statement.path
        )));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_statements_read_in_both_forms() {
        let plain = backup_statement("BACKUP TO '/backups/shop.eggdb' CLONE;", Dialect::PostgreSql).unwrap().unwrap();
        assert_eq!(plain.path, "/backups/shop.eggdb");
        assert!(plain.clone);
        let tsql = backup_statement("BACKUP DATABASE shop TO DISK = N'C:\\backups\\shop.eggdb' WITH COPY_ONLY, INIT", Dialect::SqlServer)
            .unwrap()
            .unwrap();
        assert_eq!(tsql.path, "C:\\backups\\shop.eggdb");
        assert!(!tsql.clone);
        assert!(backup_statement("SELECT 1", Dialect::PostgreSql).unwrap().is_none());
        assert!(backup_statement("BACKUP TO 42", Dialect::PostgreSql).is_err());
    }
}
