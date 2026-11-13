use super::Session;
use crate::dialect::Dialect;
use crate::error::SqlResult;
use crate::invisible_columns::{CREATE, TABLE, declared, set_frame, wanted};
use crate::sqlserver_security::literal;
use crate::types::Value;
use std::collections::HashMap;
use std::sync::Mutex;

static STORES_WITHOUT: Mutex<Option<HashMap<usize, bool>>> = Mutex::new(None);

fn known_empty(store: usize) -> bool {
    STORES_WITHOUT.lock().ok().and_then(|known| known.as_ref().and_then(|known| known.get(&store).copied())).unwrap_or(false)
}

fn remember(store: usize, empty: bool) {
    if let Ok(mut known) = STORES_WITHOUT.lock() {
        known.get_or_insert_with(HashMap::new).insert(store, empty);
    }
}

impl Session<'_> {
    fn internal_sql(&mut self, text: &str) -> SqlResult<Vec<super::QueryResult>> {
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(text);
        self.dialect = dialect;
        outcome
    }

    /// Loads the invisible columns (D1032) for a statement that could expand `*` or an implicit
    /// column list; clears them for any other.
    pub(super) fn prepare_invisible_columns(&mut self, text: &str) {
        let store = self.store.erased_store_id();
        if !wanted(text) || known_empty(store) {
            set_frame(Vec::new());
            return;
        }
        let rows: Vec<(String, String)> = self
            .internal_sql(&format!("SELECT table_name, column_name FROM {TABLE}"))
            .map(|results| {
                results
                    .iter()
                    .filter(|result| result.columns.is_some())
                    .flat_map(|result| result.rows.iter())
                    .filter_map(|row| Some((row.first().and_then(Value::to_text)?, row.get(1).and_then(Value::to_text)?)))
                    .collect()
            })
            .unwrap_or_default();
        remember(store, rows.is_empty());
        set_frame(rows);
    }

    /// The invisible columns `text` declares, when it is a `CREATE TABLE` that declares some.
    pub(super) fn invisible_columns_declared(&self, text: &str) -> Option<(String, Vec<String>)> {
        declared(text, self.dialect)
    }

    /// Records the invisible columns a `CREATE TABLE` that ran declared.
    pub(super) fn record_invisible_columns(&mut self, table: &str, columns: &[String]) -> SqlResult<()> {
        set_frame(Vec::new());
        remember(self.store.erased_store_id(), false);
        self.internal_sql(CREATE)?;
        for column in columns {
            self.internal_sql(&format!(
                "INSERT INTO {TABLE} VALUES ({}, {}) ON CONFLICT DO NOTHING",
                literal(table),
                literal(column)
            ))?;
        }
        Ok(())
    }
}
