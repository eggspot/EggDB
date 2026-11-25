use super::sql::{exit_for, print_result, statement_object};
use super::{Access, Session};
use crate::cli::args::{CommandName, Format};
use crate::cli::failure::{Exit, Failure};
use eggdb::sql::Store as DatabaseStore;
use eggdb_sql::{Dialect, QueryResult, SqlError};

fn sql_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn text_to_sql_statement(endpoint: &str, model: &str, question: &str, dialect: Dialect) -> String {
    format!(
        "SELECT eggdb_ai.text_to_sql({}, {}, {}, {}) AS answer",
        sql_text(endpoint),
        sql_text(model),
        sql_text(question),
        sql_text(dialect.name())
    )
}

fn question_failure(error: &SqlError) -> Failure {
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
    pub(super) fn question_command(&mut self) -> Result<Exit, Failure> {
        let Some((endpoint, model, question)) = self.request.question.clone() else {
            return Err(Failure::usage("question needs get", Some(CommandName::Question)));
        };
        super::model_endpoints::allow_from_environment();
        let sql = text_to_sql_statement(&endpoint, &model, &question, self.request.dialect);
        let db = self.open(Access::ReadOnly, None)?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::PostgreSql);
        let mut results: Vec<QueryResult> = Vec::new();
        let ran = session.execute(&sql, &mut |result| results.push(result));
        let _ = session.close();
        ran.map_err(|error| question_failure(&error))?;
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
