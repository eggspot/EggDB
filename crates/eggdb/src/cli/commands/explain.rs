use super::sql::{exit_for, report_error, rows_text, statement_json};
use super::{Access, Session};
use crate::cli::args::{CommandName, Format, SqlSource};
use crate::cli::failure::{Exit, Failure};
use eggdb::sql::Store as DatabaseStore;
use eggdb_sql::{Dialect, OneStatementError, OneStatementRefusal, QueryResult};

const EXPLAIN_WORD: &str = "explain";

impl Session<'_> {
    pub(super) fn explain(&mut self) -> Result<Exit, Failure> {
        let statement = match &self.request.sql {
            Some(SqlSource::Command(text)) => text.clone(),
            _ => {
                return Err(Failure::usage(
                    "eggdb explain needs a statement: give it with -c",
                    Some(CommandName::Explain),
                ));
            }
        };
        let dialect = self.request.dialect;
        refuse_unexplainable(dialect, &statement)?;
        let prefix = explain_prefix(dialect);
        let explained = format!("{prefix}{statement}");
        let db = self.open(Access::ReadOnly, None)?;
        let store = DatabaseStore(&db);
        let mut session = eggdb_sql::Session::with_dialect(&store, dialect);
        let mut result: Option<QueryResult> = None;
        let ran = session.run_one_statement(&explained, &mut |returned| {
            result = Some(returned);
            true
        });
        let _ = session.close();
        drop(db);
        let json = self.format == Format::Json;
        match ran {
            Ok(()) => {
                if let Some(result) = result {
                    if json {
                        self.out.text(&statement_json(&result, dialect));
                    } else {
                        self.out.text(&rows_text(&result, dialect));
                    }
                }
                Ok(Exit::Done)
            }
            Err(OneStatementError::Refused(refusal)) => Err(refused(refusal, prefix.len())),
            Err(OneStatementError::Failed(error)) => {
                report_error(&mut self.messages, &error, &explained, json);
                Ok(exit_for(&error))
            }
        }
    }
}

fn explain_prefix(dialect: Dialect) -> &'static str {
    if dialect == Dialect::Sqlite {
        "EXPLAIN QUERY PLAN "
    } else {
        "EXPLAIN "
    }
}

fn refuse_unexplainable(dialect: Dialect, statement: &str) -> Result<(), Failure> {
    let not_yet = match dialect {
        Dialect::Oracle => Some(("Oracle's", "Oracle's EXPLAIN PLAN")),
        Dialect::SqlServer => Some(("SQL Server's", "SQL Server's showplan")),
        _ => None,
    };
    if let Some((dialect_name, feature)) = not_yet {
        return Err(Failure::coded(
            "not_supported",
            format!(
                "cannot explain in {dialect_name} dialect: {feature} is not in EggDB yet. Explain the query with --dialect postgresql; every dialect sees the same tables."
            ),
            Exit::DatabaseError,
        ));
    }
    let starts_with_explain = statement
        .trim_start()
        .get(..EXPLAIN_WORD.len())
        .is_some_and(|word| word.eq_ignore_ascii_case(EXPLAIN_WORD));
    if starts_with_explain {
        return Err(Failure::usage(
            "cannot explain the statement: it starts with EXPLAIN, which eggdb explain adds itself. Give the statement without it.",
            Some(CommandName::Explain),
        ));
    }
    Ok(())
}

fn refused(refusal: OneStatementRefusal, prefix_bytes: usize) -> Failure {
    match refusal {
        OneStatementRefusal::MoreThanOne { position } => Failure::coded_usage(
            "one_statement_per_call",
            format!(
                "cannot explain more than one statement: another one starts at byte {}. Give eggdb explain one statement.",
                position.saturating_sub(prefix_bytes)
            ),
            Some(CommandName::Explain),
        ),
        OneStatementRefusal::Empty
        | OneStatementRefusal::TransactionControl
        | OneStatementRefusal::Copy
        | OneStatementRefusal::Unconfirmed => Failure::usage(
            "eggdb explain needs a statement: give it with -c",
            Some(CommandName::Explain),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{explain_prefix, refuse_unexplainable, refused};
    use crate::cli::failure::{Exit, Failure};
    use eggdb_sql::{Dialect, OneStatementRefusal};

    #[test]
    fn oracle_and_sql_server_are_refused_and_explain_is_added_once() {
        let oracle = refuse_unexplainable(Dialect::Oracle, "SELECT 1");
        assert!(matches!(
            oracle,
            Err(Failure::Database { code: "not_supported", exit: Exit::DatabaseError, .. })
        ));
        assert!(refuse_unexplainable(Dialect::SqlServer, "SELECT 1").is_err());
        let doubled = refuse_unexplainable(Dialect::PostgreSql, "  explain SELECT 1");
        assert_eq!(doubled.map_err(|failure| failure.code()), Err("invalid_arguments"));
        assert!(refuse_unexplainable(Dialect::MySql, "SELECT 1").is_ok());
        assert_eq!(explain_prefix(Dialect::Sqlite), "EXPLAIN QUERY PLAN ");
        assert_eq!(explain_prefix(Dialect::PostgreSql), "EXPLAIN ");
    }

    #[test]
    fn a_second_statement_is_counted_from_the_user_s_text() {
        let failure = refused(OneStatementRefusal::MoreThanOne { position: 18 }, 8);
        assert_eq!(failure.code(), "one_statement_per_call");
        assert_eq!(failure.exit(), Exit::Usage);
        let Failure::Usage { message, .. } = failure else {
            panic!("a usage failure");
        };
        assert_eq!(
            message,
            "cannot explain more than one statement: another one starts at byte 10. Give eggdb explain one statement."
        );
    }
}
