use super::Session;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::row_security::{DynamicPredicates, DYNAMIC_POLICIES_TABLE, dynamic_policy_generation};
use crate::types::Value;
use std::collections::HashMap;
use std::sync::Mutex;

static STORES_WITHOUT: Mutex<Option<HashMap<usize, u64>>> = Mutex::new(None);

fn known_without(store: usize, generation: u64) -> bool {
    STORES_WITHOUT
        .lock()
        .ok()
        .and_then(|known| known.as_ref().and_then(|known| known.get(&store).copied()))
        == Some(generation)
}

fn remember_without(store: usize, generation: u64) {
    if let Ok(mut known) = STORES_WITHOUT.lock() {
        known.get_or_insert_with(HashMap::new).insert(store, generation);
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::to_text)
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

impl Session<'_> {
    fn quietly_in(&mut self, dialect: Dialect, text: &str) -> SqlResult<Vec<super::QueryResult>> {
        let before = self.dialect;
        self.dialect = dialect;
        let outcome = self.run_quietly(text);
        self.dialect = before;
        outcome
    }

    /// Calls the policy function of every enabled dynamic Oracle VPD policy (D1321), as Oracle
    /// does once per statement, and keeps the predicates for the statement's row-security
    /// conditions until the returned guard drops. Nothing runs while a policy function itself
    /// is being called, nor for a database known to have no dynamic policy.
    pub(super) fn prepare_dynamic_policies(&mut self) -> SqlResult<Option<DynamicPredicates>> {
        if crate::row_security::evaluating_dynamic_policies() {
            return Ok(None);
        }
        let store = self.store.erased_store_id();
        let generation = dynamic_policy_generation();
        if known_without(store, generation) {
            return Ok(None);
        }
        let evaluating = crate::row_security::evaluate_dynamic_policies();
        let table_key = crate::encoding::table_key(&crate::schemas::qualified("eggdb_internal", "oracle_rls_dynamic"));
        let exists = self.read(|reader, _| Ok(reader.get(&table_key)?.is_some()))?;
        if !exists {
            remember_without(store, generation);
            return Ok(None);
        }
        let rows: Vec<Vec<Value>> = self
            .quietly_in(
                Dialect::PostgreSql,
                &format!(
                    "SELECT d.object_schema, d.object_name, d.policy_name, d.policy_function FROM {DYNAMIC_POLICIES_TABLE} d \
                     JOIN eggdb_internal.oracle_rls_policies p ON p.object_schema = d.object_schema \
                     AND p.object_name = d.object_name AND p.policy_name = d.policy_name WHERE p.enabled"
                ),
            )?
            .into_iter()
            .flat_map(|result| result.rows)
            .collect();
        if rows.is_empty() {
            remember_without(store, generation);
            return Ok(None);
        }
        let mut predicates = HashMap::new();
        for row in rows {
            let (Some(schema), Some(object), Some(policy), Some(function)) =
                (text(row.first()), text(row.get(1)), text(row.get(2)), text(row.get(3)))
            else {
                continue;
            };
            let answer = self
                .quietly_in(
                    Dialect::Oracle,
                    &format!("SELECT {function}({}, {}) FROM dual", quoted(&schema), quoted(&object)),
                )
                .map_err(|error| {
                    SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("ORA-28112: failed to execute policy function: {}", error.message()),
                    )
                    .with_oracle_code(28112)
                })?;
            let predicate = answer
                .into_iter()
                .flat_map(|result| result.rows)
                .next()
                .and_then(|row| text(row.first()))
                .filter(|predicate| !predicate.trim().is_empty());
            predicates.insert((schema, object, policy), predicate);
        }
        drop(evaluating);
        Ok(Some(DynamicPredicates::install(predicates)))
    }
}
