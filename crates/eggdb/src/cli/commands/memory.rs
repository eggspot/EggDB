use super::sql::{exit_for, print_result, statement_object};
use super::{Access, Session};
use crate::cli::args::{CommandName, Format, MemoryVerb};
use crate::cli::failure::{Exit, Failure};
use eggdb::sql::Store as DatabaseStore;
use eggdb_sql::{Dialect, QueryResult, SqlError};

const DEFAULT_RECALLED: u64 = 5;

fn sql_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn memory_failure(error: &SqlError) -> Failure {
    let message = if error.message().contains("eggdb_ai") {
        format!("{}; run CREATE EXTENSION eggdb_ai in the database first", error.message())
    } else {
        error.message().to_owned()
    };
    Failure::Database {
        message,
        try_command: None,
        exit: exit_for(error),
        code: "sql_error",
    }
}

impl Session<'_> {
    pub(super) fn memory_command(&mut self) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Memory));
        let Some((verb, agent, text)) = self.request.memory.clone() else {
            return Err(usage("memory needs add, get or delete".to_owned()));
        };
        let sql = match verb {
            MemoryVerb::Add => format!("SELECT eggdb_ai.remember({}, {}) AS id", sql_text(&agent), sql_text(&text)),
            MemoryVerb::Get => format!(
                "SELECT id, content, score, created_at FROM eggdb_ai.recall({}, {}, {})",
                sql_text(&agent),
                sql_text(&text),
                self.request.scan.limit.unwrap_or(DEFAULT_RECALLED).clamp(1, 100)
            ),
            MemoryVerb::Delete => {
                let id: i64 = text
                    .trim()
                    .parse()
                    .map_err(|_| usage(format!("{text} is not a memory id: give the whole number memory add or get printed")))?;
                format!("SELECT eggdb_ai.forget({}, {id}) AS forgotten", sql_text(&agent))
            }
        };
        let access = match verb {
            MemoryVerb::Get => Access::ReadOnly,
            MemoryVerb::Add | MemoryVerb::Delete => Access::Write { create: false },
        };
        let db = self.open(access, None)?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::PostgreSql);
        let mut results: Vec<QueryResult> = Vec::new();
        let ran = session.execute(&sql, &mut |result| results.push(result));
        let _ = session.close();
        ran.map_err(|error| memory_failure(&error))?;
        drop(db);
        let Some(result) = results.into_iter().last() else {
            return Ok(Exit::Done);
        };
        match self.format {
            Format::Json => self.out.text(&statement_object(&result, Dialect::PostgreSql).line()),
            _ => print_result(&mut self.out, &result, Dialect::PostgreSql),
        }
        Ok(Exit::Done)
    }
}
