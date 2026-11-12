//! SQL Server's security policies (D822): `CREATE`, `ALTER` and `DROP SECURITY POLICY` with
//! filter predicates, kept on top of PostgreSQL-style row-level security (D186).

use crate::ast::Name;

/// What a security predicate does: filter the rows a query sees, or block writes whose new
/// rows fail it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PredicateKind {
    Filter,
    BlockAfterInsert,
    BlockAfterUpdate,
}

impl PredicateKind {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::BlockAfterInsert => "block_insert",
            Self::BlockAfterUpdate => "block_update",
        }
    }

    pub(crate) fn from_code(code: &str) -> Option<Self> {
        [Self::Filter, Self::BlockAfterInsert, Self::BlockAfterUpdate]
            .into_iter()
            .find(|kind| kind.code() == code)
    }
}

/// One `ADD FILTER PREDICATE fn(columns) ON table` or `ADD BLOCK PREDICATE ... AFTER INSERT |
/// AFTER UPDATE`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FilterPredicate {
    pub(crate) kind: PredicateKind,
    pub(crate) function: Name,
    pub(crate) arguments: Vec<Name>,
    pub(crate) table: Name,
}

/// A security policy statement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SecurityPolicyCommand {
    Create {
        name: Name,
        predicates: Vec<FilterPredicate>,
        state_on: bool,
    },
    Alter {
        name: Name,
        added: Vec<FilterPredicate>,
        dropped: Vec<(PredicateKind, Name)>,
        state_on: Option<bool>,
    },
    Drop {
        name: Name,
        if_exists: bool,
    },
}

pub(crate) const POLICIES_TABLE: &str = "eggdb_internal.sqlserver_security_predicates";

pub(crate) const CREATE_POLICIES_TABLE: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; CREATE TABLE IF NOT EXISTS eggdb_internal.sqlserver_security_predicates (policy_name text NOT NULL, table_name text NOT NULL, kind text NOT NULL, function_name text NOT NULL, arguments text NOT NULL, enabled boolean NOT NULL, PRIMARY KEY (policy_name, table_name, kind))";

pub(crate) fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(crate) fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `schema.name` with each part quoted, the default schema when `name` names none.
pub(crate) fn qualified(name: &str) -> String {
    let schema = crate::schemas::schema_of(name);
    let schema = if schema.is_empty() { crate::schemas::DEFAULT_SCHEMA } else { schema };
    format!("{}.{}", identifier(schema), identifier(crate::schemas::base_name(name)))
}

/// The row-security policy a predicate becomes on its table.
pub(crate) fn row_policy_name(policy: &str, kind: PredicateKind) -> String {
    format!("sqlserver_policy_{}_{}", crate::schemas::base_name(policy), kind.code())
}

/// The `USING` condition: the predicate function returns a row for the rows that may be seen.
pub(crate) fn using_condition(function: &str, arguments: &[String]) -> String {
    let arguments: Vec<String> = arguments.iter().map(|argument| identifier(argument)).collect();
    format!("EXISTS (SELECT 1 FROM {}({}))", qualified(function), arguments.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicates_become_exists_over_the_function() {
        assert_eq!(
            using_condition("fn_rep", &["SalesRep".to_owned()]),
            "EXISTS (SELECT 1 FROM \"public\".\"fn_rep\"(\"SalesRep\"))"
        );
        assert_eq!(row_policy_name("SalesFilter", PredicateKind::Filter), "sqlserver_policy_SalesFilter_filter");
        assert_eq!(PredicateKind::from_code("block_update"), Some(PredicateKind::BlockAfterUpdate));
    }

    fn rows(session: &mut crate::Session<'_>, sql: &str) -> String {
        let mut found = String::new();
        session
            .execute(sql, &mut |result| found = crate::tests::render(&result))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        found
    }

    #[test]
    fn a_filter_predicate_limits_what_a_checked_role_sees() {
        let store = crate::memory_store::MemoryStore::default();
        let mut admin = crate::Session::with_dialect(&store, crate::Dialect::SqlServer);
        for sql in [
            "CREATE TABLE Sales (OrderID int PRIMARY KEY, SalesRep sysname)",
            "INSERT INTO Sales VALUES (1, 'alice'), (2, 'bob')",
            "CREATE FUNCTION dbo.fn_rep(@rep sysname) RETURNS TABLE WITH SCHEMABINDING AS RETURN SELECT 1 AS ok WHERE @rep = USER_NAME()",
            "CREATE SECURITY POLICY SalesFilter ADD FILTER PREDICATE dbo.fn_rep(SalesRep) ON dbo.Sales WITH (STATE = ON)",
        ] {
            rows(&mut admin, sql);
        }
        let counted = rows(&mut admin, "SELECT count(*) AS n FROM Sales");
        assert!(counted.contains("n\n2"), "{counted}");
        let policies = rows(&mut admin, "SELECT count(*) AS n FROM eggdb_internal.sqlserver_security_predicates");
        assert!(policies.contains("n\n1"), "{policies}");
        rows(&mut admin, "ALTER SECURITY POLICY SalesFilter WITH (STATE = OFF)");
        rows(&mut admin, "ALTER SECURITY POLICY SalesFilter WITH (STATE = ON)");
        rows(&mut admin, "DROP SECURITY POLICY SalesFilter");
        rows(&mut admin, "DROP SECURITY POLICY IF EXISTS SalesFilter");
        rows(&mut admin, "CREATE SECURITY POLICY Blocker ADD BLOCK PREDICATE dbo.fn_rep(SalesRep) ON dbo.Sales AFTER INSERT");
        let refused = admin.execute(
            "CREATE SECURITY POLICY Before ADD BLOCK PREDICATE dbo.fn_rep(SalesRep) ON dbo.Sales BEFORE DELETE",
            &mut |_| {},
        );
        assert!(refused.is_err());
    }
}
