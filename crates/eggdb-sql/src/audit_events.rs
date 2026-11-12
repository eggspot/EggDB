//! Server events for the hash-chained audit log (D815): logins, schema changes and privilege
//! changes, handed to the sink the program running EggDB installed.

use std::sync::OnceLock;

/// What kind of server event an [`AuditEvent`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditKind {
    /// A login, accepted or refused.
    Login,
    /// A statement that changed the schema: tables, indexes, views, types, routines, triggers,
    /// sequences, schemas and databases.
    SchemaChange,
    /// A statement that changed who may do what: roles, grants, revokes and row-level security.
    PrivilegeChange,
    /// A statement that names one of the tables chosen with [`set_audited_tables`].
    AuditedRead,
}

impl AuditKind {
    /// The name the audit record carries: `login`, `schema_change` or `privilege_change`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::SchemaChange => "schema_change",
            Self::PrivilegeChange => "privilege_change",
            Self::AuditedRead => "audited_read",
        }
    }
}

/// One server event.
#[derive(Debug, Clone, Copy)]
pub struct AuditEvent<'a> {
    /// What happened.
    pub kind: AuditKind,
    /// The user it happened as.
    pub user: &'a str,
    /// The database.
    pub database: &'a str,
    /// The protocol, such as `postgresql`, or `embedded`.
    pub protocol: &'a str,
    /// The statement's text, or for a login the client's address and method.
    pub detail: &'a str,
    /// Whether it succeeded: a refused login, or a statement that failed.
    pub succeeded: bool,
}

/// Receives server events; it should not fail the event's work, so it reports nothing back.
pub type AuditSink = Box<dyn Fn(&AuditEvent<'_>) + Send + Sync>;

static SINK: OnceLock<AuditSink> = OnceLock::new();
static AUDITED_TABLES: OnceLock<Vec<String>> = OnceLock::new();

/// Sets the tables whose reads are audited, by name without schema, in any case. The first call
/// wins. A statement that is not already a schema or privilege change and names one of them,
/// as a word or a quoted identifier anywhere in its text, is recorded as an
/// [`AuditKind::AuditedRead`]: a column or alias of the same name is recorded too, so the audit
/// errs toward more records, and a read through a view of another name is not seen.
pub fn set_audited_tables(names: Vec<String>) {
    let _ = AUDITED_TABLES.set(names.into_iter().map(|name| name.to_lowercase()).filter(|name| !name.is_empty()).collect());
}

pub(crate) fn names_audited_table(tokens: &[crate::lexer::Token<'_>]) -> bool {
    use crate::lexer::TokenKind;
    let Some(tables) = AUDITED_TABLES.get().filter(|tables| !tables.is_empty()) else {
        return false;
    };
    tokens.iter().any(|token| match &token.kind {
        TokenKind::Word(word) => tables.iter().any(|table| table.eq_ignore_ascii_case(word)),
        TokenKind::QuotedIdentifier(name) => tables.iter().any(|table| table == &name.to_lowercase()),
        _ => false,
    })
}

/// Sets the process's audit sink. The first registration wins; until one is set, events go
/// nowhere and cost nothing.
pub fn set_audit_sink(sink: AuditSink) {
    let _ = SINK.set(sink);
}

pub(crate) fn wanted() -> bool {
    SINK.get().is_some()
}

/// Records an event when a sink is set.
pub fn record(event: &AuditEvent<'_>) {
    if let Some(sink) = SINK.get() {
        sink(event);
    }
}

/// Records a login by `user` to `database` over `protocol` from `client` with `method`.
pub fn record_login(user: &str, database: &str, protocol: &str, client: &str, method: &str, succeeded: bool) {
    if !wanted() {
        return;
    }
    let detail = format!("client={client} method={method}");
    record(&AuditEvent {
        kind: AuditKind::Login,
        user,
        database,
        protocol,
        detail: &detail,
        succeeded,
    });
}

pub(crate) fn statement_kind(statement: &crate::ast::Statement) -> Option<AuditKind> {
    use crate::ast::Statement;
    match statement {
        Statement::Role(_)
        | Statement::RowSecurity(_)
        | Statement::SqlServer(crate::ast::SqlServerStatement::SecurityPolicy(_)) => Some(AuditKind::PrivilegeChange),
        Statement::SqlServer(crate::ast::SqlServerStatement::DdlTrigger(_))
        | Statement::SqlServer(crate::ast::SqlServerStatement::PartitionScheme(_)) => Some(AuditKind::SchemaChange),
        Statement::CreateTable(_)
        | Statement::DropTable { .. }
        | Statement::Truncate(_)
        | Statement::Database(_)
        | Statement::CreateIndex(_)
        | Statement::DropIndex(_)
        | Statement::CreateSequence(_)
        | Statement::AlterSequence(_)
        | Statement::DropSequence { .. }
        | Statement::AlterTable(_)
        | Statement::CreateSchema { .. }
        | Statement::DropSchema { .. }
        | Statement::RenameSchema { .. }
        | Statement::CreateView(_)
        | Statement::DropView { .. }
        | Statement::AlterView(_)
        | Statement::InstallExtension { .. }
        | Statement::MaterializedView(_)
        | Statement::CreateTableAs(_)
        | Statement::CreateTableLike(_)
        | Statement::CreateType(_)
        | Statement::CreateDomain(_)
        | Statement::DropType { .. }
        | Statement::AlterType(_)
        | Statement::AlterDomain(_)
        | Statement::CreateTrigger(_)
        | Statement::DropTrigger(_)
        | Statement::EnableTrigger(_)
        | Statement::AlterTrigger(_)
        | Statement::CreateRoutine(_)
        | Statement::DropRoutine(_)
        | Statement::AlterRoutine(_)
        | Statement::Synonym(_)
        | Statement::OracleContext(_)
        | Statement::OracleSystemTrigger(_)
        | Statement::ForeignData(_)
        | Statement::CreatePackage(_)
        | Statement::DropPackage(_)
        | Statement::AlterPackage(_)
        | Statement::EventTrigger(_) => Some(AuditKind::SchemaChange),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_and_grants_are_classified_and_reads_are_not() {
        let kind = |sql: &str| {
            let tokens = crate::lexer::tokenize_in(sql, crate::dialect::Dialect::PostgreSql).expect("tokens");
            let mut parser = crate::parser::Parser::new(sql, &tokens).in_dialect(crate::dialect::Dialect::PostgreSql);
            statement_kind(&parser.statement().expect("parsed"))
        };
        assert_eq!(kind("CREATE TABLE t (a int)"), Some(AuditKind::SchemaChange));
        assert_eq!(kind("CREATE INDEX t_a ON t (a)"), Some(AuditKind::SchemaChange));
        assert_eq!(kind("CREATE ROLE reader"), Some(AuditKind::PrivilegeChange));
        assert_eq!(kind("SELECT 1"), None);
        assert_eq!(kind("INSERT INTO t VALUES (1)"), None);
        assert_eq!(AuditKind::PrivilegeChange.name(), "privilege_change");
    }

    #[test]
    fn chosen_tables_are_found_by_name_in_any_case() {
        set_audited_tables(vec!["Salaries".to_owned()]);
        let names = |sql: &str| {
            let tokens = crate::lexer::tokenize_in(sql, crate::dialect::Dialect::PostgreSql).expect("tokens");
            names_audited_table(&tokens)
        };
        assert!(names("SELECT * FROM salaries"));
        assert!(names("SELECT * FROM hr.\"Salaries\" s"));
        assert!(!names("SELECT * FROM salary_bands"));
    }
}
