//! SQL Server's security policies (D822): `CREATE`, `ALTER` and `DROP SECURITY POLICY` with
//! filter predicates, kept on top of PostgreSQL-style row-level security (D186).

use crate::ast::Name;

/// What a security predicate does: filter the rows a query sees, block writes whose new rows
/// fail it, or block updates and deletes of existing rows that fail it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PredicateKind {
    Filter,
    BlockAfterInsert,
    BlockAfterUpdate,
    BlockBeforeUpdate,
    BlockBeforeDelete,
    BlockAll,
}

const ALL_KINDS: [PredicateKind; 6] = [
    PredicateKind::Filter,
    PredicateKind::BlockAfterInsert,
    PredicateKind::BlockAfterUpdate,
    PredicateKind::BlockBeforeUpdate,
    PredicateKind::BlockBeforeDelete,
    PredicateKind::BlockAll,
];

impl PredicateKind {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::BlockAfterInsert => "block_insert",
            Self::BlockAfterUpdate => "block_update",
            Self::BlockBeforeUpdate => "block_before_update",
            Self::BlockBeforeDelete => "block_delete",
            Self::BlockAll => "block_all",
        }
    }

    pub(crate) fn from_code(code: &str) -> Option<Self> {
        ALL_KINDS.into_iter().find(|kind| kind.code() == code)
    }

    /// The operations a predicate without one stands for: SQL Server applies a block predicate
    /// with no operation to all four.
    fn operations(self) -> Vec<Self> {
        match self {
            Self::BlockAll => vec![
                Self::BlockAfterInsert,
                Self::BlockAfterUpdate,
                Self::BlockBeforeUpdate,
                Self::BlockBeforeDelete,
            ],
            kind => vec![kind],
        }
    }

    /// Each restrictive row-security policy the predicate becomes, as the policy's name and
    /// the text after `AS RESTRICTIVE`. `condition` is the predicate as SQL, `table` the
    /// table it guards.
    pub(crate) fn row_policies(self, policy: &str, condition: &str, table: &str) -> Vec<(String, String)> {
        self.operations()
            .into_iter()
            .map(|operation| {
                let clause = match operation {
                    Self::Filter => format!("USING ({condition}) WITH CHECK (true)"),
                    Self::BlockAfterInsert => format!("FOR INSERT WITH CHECK ({condition})"),
                    Self::BlockAfterUpdate => format!("FOR UPDATE USING (true) WITH CHECK ({condition})"),
                    Self::BlockBeforeUpdate => {
                        format!("FOR UPDATE USING ({}) WITH CHECK (true)", raising_condition(condition, table))
                    }
                    _ => format!("FOR DELETE USING ({})", raising_condition(condition, table)),
                };
                (row_policy_name(policy, operation), clause)
            })
            .collect()
    }

    pub(crate) fn row_policy_names(self, policy: &str) -> Vec<String> {
        self.operations()
            .into_iter()
            .map(|operation| row_policy_name(policy, operation))
            .collect()
    }
}

/// The function a `BEFORE UPDATE` or `BEFORE DELETE` predicate calls on an existing row it
/// rejects: SQL Server fails the statement there, where row security would skip the row.
pub(crate) const BLOCK_VIOLATION_FUNCTION: &str = "eggdb_sqlserver_block_predicate_violation";

fn raising_condition(condition: &str, table: &str) -> String {
    format!(
        "CASE WHEN {condition} THEN true ELSE {BLOCK_VIOLATION_FUNCTION}({}) END",
        literal(table)
    )
}

/// The value of [`BLOCK_VIOLATION_FUNCTION`]: always error 33504.
pub(crate) fn block_violation(values: &[crate::types::Value]) -> crate::error::SqlResult<crate::types::Value> {
    let table = match values.first() {
        Some(crate::types::Value::Text(table)) => crate::schemas::display(table),
        _ => String::new(),
    };
    Err(crate::error::SqlError::new(
        crate::error::SqlState::INSUFFICIENT_PRIVILEGE,
        format!(
            "The attempted operation failed because the target object '{table}' has a block predicate that conflicts with this operation. If the operation is performed on a view, the block predicate might be enforced on the underlying table. Modify the operation to target only the rows that are allowed by the block predicate."
        ),
    )
    .with_sqlserver_code(33504))
}

/// One `ADD FILTER PREDICATE fn(columns) ON table` or `ADD BLOCK PREDICATE ... [AFTER INSERT |
/// AFTER UPDATE | BEFORE UPDATE | BEFORE DELETE]`.
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

/// The row-security policy one operation of a predicate becomes on its table.
fn row_policy_name(policy: &str, kind: PredicateKind) -> String {
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
        assert_eq!(PredicateKind::from_code("block_delete"), Some(PredicateKind::BlockBeforeDelete));
    }

    #[test]
    fn a_block_predicate_without_an_operation_guards_all_four() {
        let policies = PredicateKind::BlockAll.row_policies("Guard", "ok", "Sales");
        let names: Vec<&str> = policies.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "sqlserver_policy_Guard_block_insert",
                "sqlserver_policy_Guard_block_update",
                "sqlserver_policy_Guard_block_before_update",
                "sqlserver_policy_Guard_block_delete",
            ]
        );
        assert!(policies[3].1.starts_with("FOR DELETE USING (CASE WHEN ok THEN true ELSE eggdb_sqlserver_block_predicate_violation('Sales') END)"));
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
        rows(&mut admin, "CREATE SECURITY POLICY Before ADD BLOCK PREDICATE dbo.fn_rep(SalesRep) ON dbo.Sales BEFORE DELETE");
        rows(&mut admin, "ALTER SECURITY POLICY Before ADD BLOCK PREDICATE dbo.fn_rep(SalesRep) ON dbo.Sales BEFORE UPDATE");
        rows(&mut admin, "ALTER SECURITY POLICY Before DROP BLOCK PREDICATE ON dbo.Sales BEFORE DELETE");
        rows(&mut admin, "CREATE SECURITY POLICY Everything ADD BLOCK PREDICATE dbo.fn_rep(SalesRep) ON dbo.Sales");
        let refused = admin.execute(
            "CREATE SECURITY POLICY Later ADD BLOCK PREDICATE dbo.fn_rep(SalesRep) ON dbo.Sales AFTER DELETE",
            &mut |_| {},
        );
        assert!(refused.is_err());
    }

    #[test]
    fn the_violation_function_answers_error_33504() {
        let error = block_violation(&[crate::types::Value::Text("Sales".to_owned())]).unwrap_err();
        assert!(error.to_string().contains("has a block predicate that conflicts"), "{error}");
    }
}
