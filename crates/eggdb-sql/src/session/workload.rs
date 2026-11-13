use super::{OutputColumn, QueryResult, Session};
use crate::dialect::Dialect;
use crate::dump::sql_string;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind, tokenize_in};
use crate::types::{SqlType, Value};
use crate::workload_queues::{QueueSpec, Slot, admit, define, is_defined, remove, statuses};

const QUEUE_SETTING: &str = "eggdb.workload_queue";
const CREATE_REGISTRY: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; CREATE TABLE IF NOT EXISTS eggdb_internal.workload_queues (name text PRIMARY KEY, concurrency integer NOT NULL, timeout_ms bigint NOT NULL)";
const DEFAULT_TIMEOUT_MS: u64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkloadStatement {
    Create { name: String, spec: QueueSpec },
    Drop { name: String },
    Show,
}

fn word_is(token: &Token<'_>, word: &str) -> bool {
    matches!(&token.kind, TokenKind::Word(found) if found.eq_ignore_ascii_case(word))
}

fn statement(text: &str) -> SqlResult<Option<WorkloadStatement>> {
    let lowered = text.trim_start().to_ascii_lowercase();
    if !["create workload queue", "drop workload queue", "show workload queues"].iter().any(|start| lowered.starts_with(start)) {
        return Ok(None);
    }
    let tokens = tokenize_in(text, Dialect::PostgreSql)?;
    let tokens: Vec<Token<'_>> = tokens.into_iter().filter(|token| !matches!(token.kind, TokenKind::Semicolon | TokenKind::End)).collect();
    let malformed = || {
        SqlError::new(
            SqlState::SYNTAX_ERROR,
            "the workload queue statements are CREATE WORKLOAD QUEUE <name> (CONCURRENCY <n> [, TIMEOUT <milliseconds>]), DROP WORKLOAD QUEUE <name> and SHOW WORKLOAD QUEUES",
        )
    };
    let name_of = |token: &Token<'_>| match &token.kind {
        TokenKind::Word(word) => Some(word.to_ascii_lowercase()),
        TokenKind::QuotedIdentifier(name) => Some(name.clone()),
        _ => None,
    };
    match tokens.as_slice() {
        [show, _, _] if word_is(show, "show") => Ok(Some(WorkloadStatement::Show)),
        [drop, _, _, name] if word_is(drop, "drop") => Ok(Some(WorkloadStatement::Drop { name: name_of(name).ok_or_else(malformed)? })),
        [create, _, _, name, open, options @ .., close]
            if word_is(create, "create") && matches!(open.kind, TokenKind::LeftParen) && matches!(close.kind, TokenKind::RightParen) =>
        {
            let name = name_of(name).ok_or_else(malformed)?;
            let mut spec = QueueSpec { concurrency: 0, timeout_ms: DEFAULT_TIMEOUT_MS };
            for option in options.split(|token| matches!(token.kind, TokenKind::Comma)) {
                match option {
                    [key, Token { kind: TokenKind::Integer(value), .. }] if word_is(key, "concurrency") => {
                        spec.concurrency = value.parse().ok().filter(|value| *value > 0).ok_or_else(malformed)?;
                    }
                    [key, Token { kind: TokenKind::Integer(value), .. }] if word_is(key, "timeout") => {
                        spec.timeout_ms = value.parse().map_err(|_| malformed())?;
                    }
                    _ => return Err(malformed()),
                }
            }
            if spec.concurrency == 0 {
                return Err(malformed());
            }
            Ok(Some(WorkloadStatement::Create { name, spec }))
        }
        _ => Err(malformed()),
    }
}

impl Session<'_> {
    fn workload_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(text);
        self.dialect = dialect;
        outcome
    }

    fn load_queue(&mut self, name: &str) -> SqlResult<()> {
        let internal = crate::schemas::qualified("eggdb_internal", "workload_queues");
        let registered = self.read(|reader, _| Ok(reader.get(&crate::encoding::table_key(&internal))?.is_some()))?;
        if !registered {
            return Ok(());
        }
        let results = self.workload_sql(&format!(
            "SELECT concurrency, timeout_ms FROM eggdb_internal.workload_queues WHERE name = {}",
            sql_string(name)
        ))?;
        if let Some(row) = results.iter().flat_map(|result| result.rows.iter()).next() {
            let number = |index: usize| row.get(index).and_then(Value::to_text).and_then(|text| text.parse::<u64>().ok());
            if let (Some(concurrency), Some(timeout_ms)) = (number(0), number(1)) {
                define(name, QueueSpec { concurrency: u32::try_from(concurrency).unwrap_or(u32::MAX), timeout_ms });
            }
        }
        Ok(())
    }

    /// Runs `CREATE WORKLOAD QUEUE`, `DROP WORKLOAD QUEUE` and `SHOW WORKLOAD QUEUES` (D1050).
    pub(super) fn prepare_workload(&mut self, text: &str, on_result: &mut dyn FnMut(QueryResult) -> bool) -> SqlResult<bool> {
        let Some(statement) = statement(text)? else {
            return Ok(false);
        };
        let superuser = self.settings.checked_role.is_none() && self.settings.is_superuser();
        match statement {
            WorkloadStatement::Create { name, spec } => {
                if !superuser {
                    return Err(SqlError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied: CREATE WORKLOAD QUEUE needs a superuser"));
                }
                self.atomically(|session| {
                    session.workload_sql(CREATE_REGISTRY)?;
                    session.workload_sql(&format!("DELETE FROM eggdb_internal.workload_queues WHERE name = {}", sql_string(&name)))?;
                    session.workload_sql(&format!(
                        "INSERT INTO eggdb_internal.workload_queues (name, concurrency, timeout_ms) VALUES ({}, {}, {})",
                        sql_string(&name),
                        spec.concurrency,
                        spec.timeout_ms
                    ))?;
                    Ok(())
                })?;
                define(&name, spec);
                on_result(QueryResult::command("CREATE WORKLOAD QUEUE"));
            }
            WorkloadStatement::Drop { name } => {
                if !superuser {
                    return Err(SqlError::new(SqlState::INSUFFICIENT_PRIVILEGE, "permission denied: DROP WORKLOAD QUEUE needs a superuser"));
                }
                self.workload_sql(CREATE_REGISTRY)?;
                self.workload_sql(&format!("DELETE FROM eggdb_internal.workload_queues WHERE name = {}", sql_string(&name)))?;
                remove(&name);
                on_result(QueryResult::command("DROP WORKLOAD QUEUE"));
            }
            WorkloadStatement::Show => {
                let column = |name: &str, sql_type: SqlType| OutputColumn { name: name.to_owned(), sql_type: Some(sql_type) };
                let columns = vec![
                    column("name", SqlType::Text),
                    column("concurrency", SqlType::Integer),
                    column("timeout_ms", SqlType::BigInt),
                    column("running", SqlType::Integer),
                    column("waiting", SqlType::Integer),
                    column("admitted", SqlType::BigInt),
                    column("timed_out", SqlType::BigInt),
                ];
                let rows = statuses()
                    .into_iter()
                    .map(|status| {
                        vec![
                            Value::Text(status.name),
                            Value::Integer(i32::try_from(status.spec.concurrency).unwrap_or(i32::MAX)),
                            Value::BigInt(i64::try_from(status.spec.timeout_ms).unwrap_or(i64::MAX)),
                            Value::Integer(i32::try_from(status.running).unwrap_or(i32::MAX)),
                            Value::Integer(i32::try_from(status.waiting).unwrap_or(i32::MAX)),
                            Value::BigInt(i64::try_from(status.admitted).unwrap_or(i64::MAX)),
                            Value::BigInt(i64::try_from(status.timed_out).unwrap_or(i64::MAX)),
                        ]
                    })
                    .collect();
                on_result(QueryResult::rows("SHOW".to_owned(), columns, rows));
            }
        }
        Ok(true)
    }

    /// The place in the session's workload queue (`eggdb.workload_queue`) the next statement
    /// runs in, waiting for it when the queue is full (D1050); `None` without a queue.
    pub(super) fn admit_workload(&mut self) -> SqlResult<Option<Slot>> {
        let Some(name) = self.settings.get(QUEUE_SETTING).ok().filter(|name| !name.is_empty()) else {
            return Ok(None);
        };
        let name = name.to_ascii_lowercase();
        if !is_defined(&name) {
            self.load_queue(&name)?;
        }
        admit(&name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workload_statements_read_their_options() {
        assert_eq!(
            statement("CREATE WORKLOAD QUEUE reporting (CONCURRENCY 4, TIMEOUT 30000)").unwrap(),
            Some(WorkloadStatement::Create { name: "reporting".to_owned(), spec: QueueSpec { concurrency: 4, timeout_ms: 30_000 } })
        );
        assert_eq!(
            statement("create workload queue etl (concurrency 1)").unwrap(),
            Some(WorkloadStatement::Create { name: "etl".to_owned(), spec: QueueSpec { concurrency: 1, timeout_ms: DEFAULT_TIMEOUT_MS } })
        );
        assert!(statement("CREATE WORKLOAD QUEUE x (CONCURRENCY 0)").is_err());
        assert!(statement("CREATE WORKLOAD QUEUE x (SPEED 3)").is_err());
        assert_eq!(statement("DROP WORKLOAD QUEUE etl").unwrap(), Some(WorkloadStatement::Drop { name: "etl".to_owned() }));
        assert_eq!(statement("SHOW WORKLOAD QUEUES").unwrap(), Some(WorkloadStatement::Show));
        assert_eq!(statement("SELECT 1").unwrap(), None);
    }
}
