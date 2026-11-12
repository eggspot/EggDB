use super::Session;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::QueryResult;
use crate::sqlserver_security::{
    CREATE_POLICIES_TABLE, FilterPredicate, POLICIES_TABLE, PredicateKind, SecurityPolicyCommand, identifier,
    literal, qualified, using_condition,
};

struct StoredPredicate {
    table: String,
    kind: PredicateKind,
    function: String,
    arguments: Vec<String>,
}

impl Session<'_> {
    pub(super) fn run_security_policy(&mut self, command: &SecurityPolicyCommand) -> SqlResult<QueryResult> {
        self.policy_sql(CREATE_POLICIES_TABLE)?;
        match command {
            SecurityPolicyCommand::Create {
                name,
                predicates,
                state_on,
            } => {
                if !self.stored_predicates(&name.text)?.is_empty() {
                    return Err(SqlError::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("There is already an object named '{}' in the database.", name.text),
                    )
                    .with_sqlserver_code(2714));
                }
                for predicate in predicates {
                    self.store_predicate(&name.text, predicate, *state_on)?;
                }
                if *state_on {
                    self.switch_policy(&name.text, true)?;
                }
                Ok(QueryResult::command("CREATE SECURITY POLICY"))
            }
            SecurityPolicyCommand::Alter {
                name,
                added,
                dropped,
                state_on,
            } => {
                let stored = self.stored_predicates(&name.text)?;
                if stored.is_empty() {
                    return Err(missing_policy(&name.text));
                }
                let enabled = self.policy_enabled(&name.text)?;
                self.switch_policy(&name.text, false)?;
                for (kind, table) in dropped {
                    self.policy_sql(&format!(
                        "DELETE FROM {POLICIES_TABLE} WHERE policy_name = {} AND table_name = {} AND kind = {}",
                        literal(&name.text),
                        literal(&table.text),
                        literal(kind.code())
                    ))?;
                }
                for predicate in added {
                    self.store_predicate(&name.text, predicate, enabled)?;
                }
                let on = state_on.unwrap_or(enabled);
                self.policy_sql(&format!(
                    "UPDATE {POLICIES_TABLE} SET enabled = {on} WHERE policy_name = {}",
                    literal(&name.text)
                ))?;
                if on {
                    self.switch_policy(&name.text, true)?;
                }
                Ok(QueryResult::command("ALTER SECURITY POLICY"))
            }
            SecurityPolicyCommand::Drop { name, if_exists } => {
                if self.stored_predicates(&name.text)?.is_empty() {
                    if *if_exists {
                        return Ok(QueryResult::command("DROP SECURITY POLICY"));
                    }
                    return Err(missing_policy(&name.text));
                }
                self.switch_policy(&name.text, false)?;
                self.policy_sql(&format!(
                    "DELETE FROM {POLICIES_TABLE} WHERE policy_name = {}",
                    literal(&name.text)
                ))?;
                Ok(QueryResult::command("DROP SECURITY POLICY"))
            }
        }
    }

    fn policy_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(text);
        self.dialect = dialect;
        outcome
    }

    fn store_predicate(&mut self, policy: &str, predicate: &FilterPredicate, enabled: bool) -> SqlResult<()> {
        let arguments: Vec<&str> = predicate.arguments.iter().map(|argument| argument.text.as_str()).collect();
        self.policy_sql(&format!(
            "INSERT INTO {POLICIES_TABLE} VALUES ({}, {}, {}, {}, {}, {enabled})",
            literal(policy),
            literal(&predicate.table.text),
            literal(predicate.kind.code()),
            literal(&predicate.function.text),
            literal(&arguments.join("\n"))
        ))
        .map(drop)
    }

    fn stored_predicates(&mut self, policy: &str) -> SqlResult<Vec<StoredPredicate>> {
        let results = self.policy_sql(&format!(
            "SELECT table_name, kind, function_name, arguments FROM {POLICIES_TABLE} WHERE policy_name = {} ORDER BY table_name, kind",
            literal(policy)
        ))?;
        let text = |value: &crate::Value| match value {
            crate::Value::Text(text) => text.clone(),
            _ => String::new(),
        };
        Ok(results
            .last()
            .map(|result| {
                result
                    .rows
                    .iter()
                    .filter(|row| row.len() == 4)
                    .filter_map(|row| {
                        Some(StoredPredicate {
                            table: text(&row[0]),
                            kind: PredicateKind::from_code(&text(&row[1]))?,
                            function: text(&row[2]),
                            arguments: text(&row[3]).split('\n').filter(|part| !part.is_empty()).map(str::to_owned).collect(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn policy_enabled(&mut self, policy: &str) -> SqlResult<bool> {
        let results = self.policy_sql(&format!(
            "SELECT bool_or(enabled) FROM {POLICIES_TABLE} WHERE policy_name = {}",
            literal(policy)
        ))?;
        Ok(matches!(
            results.last().and_then(|result| result.rows.first()).and_then(|row| row.first()),
            Some(crate::Value::Boolean(true))
        ))
    }

    fn switch_policy(&mut self, policy: &str, on: bool) -> SqlResult<()> {
        for predicate in self.stored_predicates(policy)? {
            let table = qualified(&predicate.table);
            let text = if on {
                let condition = using_condition(&predicate.function, &predicate.arguments);
                let mut text = format!(
                    "ALTER TABLE {table} ENABLE ROW LEVEL SECURITY; \
                     DROP POLICY IF EXISTS eggdb_sqlserver_base ON {table}; \
                     CREATE POLICY eggdb_sqlserver_base ON {table} AS PERMISSIVE USING (true) WITH CHECK (true)"
                );
                for (name, clause) in predicate.kind.row_policies(policy, &condition, &predicate.table) {
                    let name = identifier(&name);
                    text.push_str(&format!(
                        "; DROP POLICY IF EXISTS {name} ON {table}; CREATE POLICY {name} ON {table} AS RESTRICTIVE {clause}"
                    ));
                }
                text
            } else {
                predicate
                    .kind
                    .row_policy_names(policy)
                    .iter()
                    .map(|name| format!("DROP POLICY IF EXISTS {} ON {table}", identifier(name)))
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            self.policy_sql(&text)?;
        }
        Ok(())
    }
}

fn missing_policy(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("Cannot find the object '{name}', because it does not exist or you do not have permission."),
    )
    .with_sqlserver_code(15151)
}
