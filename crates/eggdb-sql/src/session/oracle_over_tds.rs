use super::Session;
use super::oracle::identifier;
use crate::dialect::Dialect;
use crate::error::SqlResult;
use crate::lexer::{TokenKind, tokenize_in};
use crate::routines::{RoutineKind, RoutineLanguage};
use crate::types::SqlType;
use std::sync::Arc;

const CATALOG_PROCEDURE_PREFIX: &[u8] = b"sp_";

/// A PL/SQL stored procedure as a server calls it by name for a client, such as a TDS RPC
/// request naming it in a session that reads Oracle SQL ([`Session::plsql_procedure`]).
#[derive(Debug, Clone, PartialEq)]
pub struct PlsqlProcedure {
    /// The name as the client wrote it, checked to hold only identifiers joined by `.`, so
    /// that it reads back as this procedure after Oracle's `CALL`.
    pub call_name: String,
    /// Its parameters in the order declared.
    pub parameters: Vec<PlsqlParameter>,
}

/// One parameter of a [`PlsqlProcedure`].
#[derive(Debug, Clone, PartialEq)]
pub struct PlsqlParameter {
    /// The name as stored, without an `@`.
    pub name: String,
    /// The name as Oracle SQL writes it before `=>` in a call, quoted when it needs to be.
    pub call_name: String,
    /// The engine type its declared type is kept as.
    pub sql_type: SqlType,
    /// Whether the procedure reads a value for it (`IN` or `IN OUT`).
    pub input: bool,
    /// Whether the procedure gives a value back through it (`OUT` or `IN OUT`).
    pub output: bool,
}

impl PlsqlParameter {
    /// Whether `name`, as a TDS client sends it (`@p_name`, in any case), names this
    /// parameter.
    pub fn is_named(&self, name: &str) -> bool {
        name.strip_prefix('@')
            .unwrap_or(name)
            .eq_ignore_ascii_case(&self.name)
    }
}

fn is_plain_name(text: &str) -> bool {
    let Ok(tokens) = tokenize_in(text, Dialect::Oracle) else {
        return false;
    };
    let mut expects_part = true;
    for token in &tokens {
        match &token.kind {
            TokenKind::End => return !expects_part,
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) if expects_part => {
                expects_part = false;
            }
            TokenKind::Dot if !expects_part => expects_part = true,
            _ => return false,
        }
    }
    false
}

fn mentions_catalog_procedure_prefix(text: &str) -> bool {
    text.as_bytes()
        .windows(CATALOG_PROCEDURE_PREFIX.len())
        .any(|window| window.eq_ignore_ascii_case(CATALOG_PROCEDURE_PREFIX))
}

fn names_catalog_procedure(kinds: &[&TokenKind<'_>]) -> bool {
    let mut at = 0;
    if matches!(kinds.first(), Some(TokenKind::Variable(_)))
        && matches!(kinds.get(1), Some(TokenKind::Operator("=")))
    {
        at = 2;
    }
    let mut last: Option<String> = None;
    loop {
        match kinds.get(at) {
            Some(TokenKind::Word(word)) => last = Some(word.to_ascii_lowercase()),
            Some(TokenKind::QuotedIdentifier(name)) => last = Some(name.to_ascii_lowercase()),
            _ => break,
        }
        if !matches!(kinds.get(at + 1), Some(TokenKind::Dot)) {
            break;
        }
        at += 2;
    }
    last.is_some_and(|name| crate::sqlserver_catalog::is_catalog_procedure(&name))
}

pub(super) fn executes_sqlserver_catalog_procedure(text: &str) -> bool {
    if !mentions_catalog_procedure_prefix(text) {
        return false;
    }
    let Ok(tokens) = tokenize_in(text, Dialect::SqlServer) else {
        return false;
    };
    let kinds: Vec<&TokenKind<'_>> = tokens.iter().map(|token| &token.kind).collect();
    kinds.iter().enumerate().any(|(at, kind)| {
        is_execute_keyword(kind) && names_catalog_procedure(kinds.get(at + 1..).unwrap_or_default())
    })
}

fn is_execute_keyword(kind: &TokenKind<'_>) -> bool {
    matches!(
        kind,
        TokenKind::Word(word) if word.eq_ignore_ascii_case("exec") || word.eq_ignore_ascii_case("execute")
    )
}

impl Session<'_> {
    pub(super) fn runs_sqlserver_catalog_procedure(&self, text: &str) -> bool {
        self.connection_dialect == Dialect::SqlServer
            && self.dialect != Dialect::SqlServer
            && executes_sqlserver_catalog_procedure(text)
    }

    /// Finds the PL/SQL stored procedure that `name` names, written as an Oracle client
    /// writes it (`log_it`, `hr.log_it`, `"HR"."LOG_IT"`, `pkg.proc`), so a server can run it
    /// with Oracle's `CALL`. `None` when no PL/SQL procedure has that name, including when
    /// `name` is not only a name or names a function or a procedure of another language. Of
    /// overloaded procedures, the first one declared is described.
    pub fn plsql_procedure(&mut self, name: &str) -> SqlResult<Option<PlsqlProcedure>> {
        if !is_plain_name(name) {
            return Ok(None);
        }
        let settings = Arc::clone(&self.settings);
        let Ok(parsed) = crate::parser::oracle_table_name(name, &settings) else {
            return Ok(None);
        };
        let _path = self.enter_search_path(&settings);
        let found =
            self.read(|reader, tables| crate::routines::resolve(reader, tables, &parsed.text))?;
        let Some(definition) = found.and_then(|named| {
            named
                .routines
                .iter()
                .find(|definition| {
                    definition.language == RoutineLanguage::PlSql
                        && definition.kind == RoutineKind::Procedure
                })
                .cloned()
        }) else {
            return Ok(None);
        };
        let parameters = definition
            .parameters
            .iter()
            .filter_map(|parameter| {
                let name = parameter.name.clone()?;
                Some(PlsqlParameter {
                    call_name: identifier(&name),
                    name,
                    sql_type: parameter.sql_type,
                    input: parameter.mode.is_input(),
                    output: parameter.mode.is_output(),
                })
            })
            .collect();
        Ok(Some(PlsqlProcedure {
            call_name: name.to_owned(),
            parameters,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{executes_sqlserver_catalog_procedure, is_plain_name};
    use crate::memory_store::MemoryStore;
    use crate::{Dialect, QueryResult, Session};

    #[test]
    fn only_dotted_identifiers_are_procedure_names() {
        assert!(is_plain_name("log_it"));
        assert!(is_plain_name("hr.log_it"));
        assert!(is_plain_name("\"HR\".\"LOG IT\""));
        assert!(!is_plain_name(""));
        assert!(!is_plain_name("hr."));
        assert!(!is_plain_name(".log_it"));
        assert!(!is_plain_name("log_it; DROP TABLE t"));
        assert!(!is_plain_name("log_it(1)"));
        assert!(!is_plain_name("log_it;"));
        assert!(!is_plain_name("[dbo].[p]"));
    }

    #[test]
    fn batches_that_execute_a_catalog_procedure_are_found() {
        assert!(executes_sqlserver_catalog_procedure(
            "EXEC sp_tables @eggdb_rpc_argument_0, @eggdb_rpc_argument_1"
        ));
        assert!(executes_sqlserver_catalog_procedure(
            "exec [sys].[sp_pkeys] N'items'"
        ));
        assert!(executes_sqlserver_catalog_procedure(
            "DECLARE @r TABLE(a int);INSERT INTO @r EXEC sp_columns_100 N'items',N'dbo',N'shop',N'%',2,3;SELECT * FROM @r"
        ));
        assert!(executes_sqlserver_catalog_procedure(
            "EXECUTE @status = master.dbo.sp_sproc_columns_100 N'log_it'"
        ));
        assert!(!executes_sqlserver_catalog_procedure(
            "SELECT 'EXEC sp_tables' FROM dual"
        ));
        assert!(!executes_sqlserver_catalog_procedure(
            "SELECT sp_tables FROM t"
        ));
        assert!(!executes_sqlserver_catalog_procedure("EXEC sp_unknown_one 1"));
        assert!(!executes_sqlserver_catalog_procedure("SELECT 1 FROM dual"));
        assert!(!executes_sqlserver_catalog_procedure("EXEC 'unterminated"));
    }

    fn run(session: &mut Session<'_>, text: &str) {
        session
            .execute(text, &mut |_: QueryResult| {})
            .unwrap_or_else(|error| panic!("{text}: {error}"));
    }

    #[test]
    fn plsql_procedures_are_described_with_their_parameter_modes() {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, Dialect::Oracle);
        run(
            &mut session,
            "CREATE OR REPLACE PROCEDURE split_it (\n  total IN NUMBER,\n  half OUT NUMBER,\n  note IN OUT VARCHAR2\n) AS\nBEGIN\n  half := total / 2;\n  note := note || '!';\nEND;",
        );
        run(
            &mut session,
            "CREATE OR REPLACE FUNCTION twice (n IN NUMBER) RETURN NUMBER IS\nBEGIN\n  RETURN n * 2;\nEND;",
        );
        let found = session.plsql_procedure("SPLIT_IT").unwrap().unwrap();
        assert_eq!(found.call_name, "SPLIT_IT");
        let modes: Vec<(&str, bool, bool)> = found
            .parameters
            .iter()
            .map(|parameter| (parameter.name.as_str(), parameter.input, parameter.output))
            .collect();
        assert_eq!(
            modes,
            [
                ("total", true, false),
                ("half", false, true),
                ("note", true, true)
            ]
        );
        assert!(found.parameters[0].is_named("@TOTAL"));
        assert!(found.parameters[0].is_named("total"));
        assert!(!found.parameters[0].is_named("@half"));
        assert_eq!(session.plsql_procedure("twice").unwrap(), None);
        assert_eq!(session.plsql_procedure("no_such_procedure").unwrap(), None);
        assert_eq!(session.plsql_procedure("split_it; COMMIT").unwrap(), None);
        assert_eq!(session.plsql_procedure("[dbo].[split_it]").unwrap(), None);
    }
}
