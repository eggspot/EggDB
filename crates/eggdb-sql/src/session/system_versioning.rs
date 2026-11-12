use super::mysql_events::quoted;
use super::{QueryResult, Session};
use crate::ast::{AlterAction, SystemVersioning};
use crate::dialect::Dialect;
use crate::error::SqlResult;
use crate::schemas::{DEFAULT_SCHEMA, base_name, schema_of};

const SQL_SERVER_OPEN_END: &str = "9999-12-31 23:59:59.999999";
const MARIADB_OPEN_END: &str = "2038-01-19 03:14:07.999999";

fn schema_and_table(name: &str) -> (String, String) {
    let schema = schema_of(name);
    let schema = if schema.is_empty() { DEFAULT_SCHEMA } else { schema };
    (schema.to_owned(), base_name(name).to_owned())
}

impl Session<'_> {
    fn versioning_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(text);
        self.dialect = dialect;
        outcome
    }

    /// Makes `table` system-versioned on `eggdb_temporal`'s machinery with the period columns
    /// and history table the statement named (D795).
    pub(super) fn enable_system_versioning(
        &mut self,
        table: &str,
        versioning: &SystemVersioning,
    ) -> SqlResult<()> {
        let (schema, name) = schema_and_table(table);
        let history = versioning
            .history
            .as_ref()
            .map_or_else(|| "NULL".to_owned(), |given| quoted(base_name(&given.text)));
        let open_end = match self.dialect {
            Dialect::SqlServer => SQL_SERVER_OPEN_END,
            Dialect::MySql => MARIADB_OPEN_END,
            _ => "infinity",
        };
        self.versioning_sql(&format!(
            "CREATE EXTENSION IF NOT EXISTS eggdb_temporal; \
             SELECT eggdb_temporal.enable_system_versioning({}, {}, {}, {}, {history}, {})",
            quoted(&schema),
            quoted(&name),
            quoted(&versioning.start),
            quoted(&versioning.end),
            quoted(open_end),
        ))?;
        Ok(())
    }

    fn disable_system_versioning(&mut self, table: &str) -> SqlResult<()> {
        let (schema, name) = schema_and_table(table);
        self.versioning_sql(&format!(
            "SELECT eggdb_temporal.disable_system_versioning({}, {})",
            quoted(&schema),
            quoted(&name),
        ))?;
        Ok(())
    }

    pub(super) fn apply_system_versioning_actions(
        &mut self,
        table: &str,
        actions: &[AlterAction],
    ) -> SqlResult<()> {
        for action in actions {
            match action {
                AlterAction::SystemVersioning(Some(versioning)) => {
                    self.enable_system_versioning(table, versioning)?;
                }
                AlterAction::SystemVersioning(None) => self.disable_system_versioning(table)?,
                _ => {}
            }
        }
        Ok(())
    }
}
