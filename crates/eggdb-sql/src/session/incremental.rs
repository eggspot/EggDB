use super::{QueryResult, Session};
use crate::dialect::Dialect;
use crate::dump::sql_string;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::incremental_views::{plan, trigger_body};

const REGISTRY: &str = "eggdb_internal.incremental_views";
const CREATE_REGISTRY: &str = "CREATE SCHEMA IF NOT EXISTS eggdb_internal; CREATE TABLE IF NOT EXISTS eggdb_internal.incremental_views (name text PRIMARY KEY, base_table text NOT NULL, trigger_name text NOT NULL, function_name text NOT NULL)";
const CREATE_PREFIX: &str = "create incremental materialized view ";

fn object_suffix(name: &str) -> String {
    name.chars().map(|character| if character.is_ascii_alphanumeric() { character.to_ascii_lowercase() } else { '_' }).collect()
}

impl Session<'_> {
    fn incremental_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(text);
        self.dialect = dialect;
        outcome
    }

    fn primary_key_columns(&mut self, table: &str) -> SqlResult<Vec<String>> {
        let internal = match table.split_once('.') {
            Some((schema, name)) => crate::schemas::qualified(&schema.to_ascii_lowercase(), &name.to_ascii_lowercase()),
            None => table.to_ascii_lowercase(),
        };
        self.read(|reader, _| {
            let Some(stored) = reader.get(&crate::encoding::table_key(&internal))? else {
                return Err(SqlError::new(SqlState::UNDEFINED_TABLE, format!("relation \"{table}\" does not exist")));
            };
            let definition = crate::encoding::TableDefinition::decode(&internal, &stored)?;
            Ok(definition.primary_key.iter().map(|&index| definition.columns[index].name.to_ascii_lowercase()).collect())
        })
    }

    fn incremental_rows(&mut self, text: &str) -> Vec<Vec<String>> {
        self.incremental_sql(text)
            .map(|results| {
                results
                    .iter()
                    .filter(|result| result.columns.is_some())
                    .flat_map(|result| result.rows.iter())
                    .map(|row| row.iter().map(|value| value.to_text().unwrap_or_default()).collect())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Runs `CREATE INCREMENTAL MATERIALIZED VIEW`, and before `DROP MATERIALIZED VIEW` removes
    /// the triggers that kept the view up to date (D1033).
    pub(super) fn prepare_incremental_views(&mut self, text: &str, on_result: &mut dyn FnMut(QueryResult) -> bool) -> SqlResult<bool> {
        let trimmed = text.trim().trim_end_matches(';').trim_end();
        let lowered = trimmed.to_ascii_lowercase();
        if lowered.starts_with("drop materialized view ") {
            self.drop_incremental_triggers(&lowered["drop materialized view ".len()..])?;
            return Ok(false);
        }
        if !lowered.starts_with(CREATE_PREFIX) {
            return Ok(false);
        }
        let rest = &trimmed[CREATE_PREFIX.len()..];
        let as_at = rest
            .to_ascii_lowercase()
            .find(" as ")
            .ok_or_else(|| SqlError::new(SqlState::SYNTAX_ERROR, "CREATE INCREMENTAL MATERIALIZED VIEW name AS query"))?;
        let name = rest[..as_at].trim().to_owned();
        let query = rest[as_at + 4..].trim().to_owned();
        let incremental = plan(&query, &mut |table| self.primary_key_columns(table))?;
        let suffix = object_suffix(&name);
        let function = format!("eggdb_internal.ivm_{suffix}");
        let trigger = format!("eggdb_ivm_{suffix}");
        let body = trigger_body(&name, &incremental);
        let base = incremental.base_table.clone();
        self.atomically(|session| {
            session.incremental_sql(&format!("CREATE MATERIALIZED VIEW {name} AS {query}"))?;
            session.incremental_sql(CREATE_REGISTRY)?;
            session.incremental_sql(&format!("CREATE OR REPLACE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $eggdb_ivm$ {body} $eggdb_ivm$"))?;
            session.incremental_sql(&format!("CREATE TRIGGER {trigger} AFTER INSERT OR UPDATE OR DELETE ON {base} FOR EACH ROW EXECUTE FUNCTION {function}()"))?;
            session.incremental_sql(&format!(
                "INSERT INTO {REGISTRY} VALUES ({}, {}, {}, {})",
                sql_string(&name.to_ascii_lowercase()),
                sql_string(&base),
                sql_string(&trigger),
                sql_string(&function)
            ))?;
            Ok(())
        })?;
        on_result(QueryResult::command("CREATE MATERIALIZED VIEW"));
        Ok(true)
    }

    fn drop_incremental_triggers(&mut self, names: &str) -> SqlResult<()> {
        let names = names.trim().strip_prefix("if exists ").unwrap_or(names).trim();
        let names = names.trim_end_matches(" cascade").trim_end_matches(" restrict");
        for name in names.split(',').map(str::trim).filter(|name| !name.is_empty()) {
            let rows = self.incremental_rows(&format!(
                "SELECT base_table, trigger_name, function_name FROM {REGISTRY} WHERE name = {}",
                sql_string(name)
            ));
            let Some([base, trigger, function]) = rows.first().map(Vec::as_slice) else {
                continue;
            };
            let (base, trigger, function) = (base.clone(), trigger.clone(), function.clone());
            self.incremental_sql(&format!("DROP TRIGGER IF EXISTS {trigger} ON {base}"))?;
            self.incremental_sql(&format!("DROP FUNCTION IF EXISTS {function}()"))?;
            self.incremental_sql(&format!("DELETE FROM {REGISTRY} WHERE name = {}", sql_string(name)))?;
        }
        Ok(())
    }
}
