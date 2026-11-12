use super::Session;
use crate::ast::OracleContextCommand;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::QueryResult;

pub(crate) const CONTEXTS_TABLE: &str = "eggdb_internal.oracle_contexts";
pub(crate) const CREATE_CONTEXTS_SCHEMA: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal";
pub(crate) const CREATE_CONTEXTS_TABLE: &str = "CREATE TABLE IF NOT EXISTS eggdb_internal.oracle_contexts (namespace text PRIMARY KEY, package text NOT NULL)";

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

impl Session<'_> {
    fn context_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(text);
        self.dialect = dialect;
        outcome
    }

    fn context_exists(&mut self, namespace: &str) -> SqlResult<bool> {
        let results = self.context_sql(&format!(
            "SELECT 1 FROM {CONTEXTS_TABLE} WHERE namespace = {}",
            literal(namespace)
        ))?;
        Ok(results.last().is_some_and(|result| !result.rows.is_empty()))
    }

    pub(super) fn run_oracle_context(&mut self, command: &OracleContextCommand) -> SqlResult<QueryResult> {
        self.context_sql(CREATE_CONTEXTS_SCHEMA)?;
        self.context_sql(CREATE_CONTEXTS_TABLE)?;
        match command {
            OracleContextCommand::Create {
                namespace,
                package,
                replace,
            } => {
                if !replace && self.context_exists(namespace)? {
                    return Err(SqlError::new(
                        SqlState::DUPLICATE_OBJECT,
                        "ORA-00955: name is already used by an existing object",
                    )
                    .with_oracle_code(955));
                }
                self.context_sql(&format!(
                    "INSERT INTO {CONTEXTS_TABLE} VALUES ({}, {}) ON CONFLICT (namespace) DO UPDATE SET package = EXCLUDED.package",
                    literal(namespace),
                    literal(package)
                ))?;
                Ok(QueryResult::command("CREATE CONTEXT"))
            }
            OracleContextCommand::Drop { namespace } => {
                if !self.context_exists(namespace)? {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_OBJECT,
                        "ORA-04043: object does not exist",
                    )
                    .with_oracle_code(4043));
                }
                self.context_sql(&format!(
                    "DELETE FROM {CONTEXTS_TABLE} WHERE namespace = {}",
                    literal(namespace)
                ))?;
                Ok(QueryResult::command("DROP CONTEXT"))
            }
        }
    }
}
