use super::mysql_events::quoted;
use super::{QueryResult, Session};
use crate::ast::{AlterAction, SystemVersioning};
use crate::dialect::Dialect;
use crate::error::{SqlResult, SqlState};
use crate::schemas::{DEFAULT_SCHEMA, base_name, schema_of};

const PERIOD_MISSING_VENDOR_CODE: u16 = 13510;
const SQL_SERVER_OPEN_END: &str = "9999-12-31 23:59:59.999999";
const MARIADB_OPEN_END: &str = "2038-01-19 03:14:07.999999";

const DECLARED_VERSIONING_SQL: &str = r##"
CREATE EXTENSION IF NOT EXISTS eggdb_temporal;
CREATE TABLE IF NOT EXISTS eggdb_temporal.declared_periods (schema_name text NOT NULL, table_name text NOT NULL, system_start text NOT NULL, system_end text NOT NULL, PRIMARY KEY (schema_name, table_name));
CREATE TABLE IF NOT EXISTS eggdb_temporal.history_retention (schema_name text NOT NULL, table_name text NOT NULL, history_table text NOT NULL, end_column text NOT NULL, keep interval NOT NULL, PRIMARY KEY (schema_name, table_name));
CREATE OR REPLACE FUNCTION eggdb_temporal.trim_history() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    rule record;
BEGIN
    FOR rule IN SELECT r.end_column, r.keep FROM eggdb_temporal.history_retention r WHERE r.schema_name = TG_TABLE_SCHEMA AND r.history_table = TG_TABLE_NAME LOOP
        EXECUTE format('DELETE FROM %I.%I WHERE %I < %L', TG_TABLE_SCHEMA, TG_TABLE_NAME, rule.end_column, now() - rule.keep);
    END LOOP;
    RETURN NULL;
END
$$;
CREATE OR REPLACE FUNCTION eggdb_temporal.set_history_retention(schema_name text, table_name text, keep interval) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    history_name text;
    end_name text;
BEGIN
    SELECT r.history_table INTO history_name FROM eggdb_temporal.history_retention r WHERE r.schema_name = set_history_retention.schema_name AND r.table_name = set_history_retention.table_name;
    IF history_name IS NOT NULL THEN
        EXECUTE format('DROP TRIGGER IF EXISTS eggdb_temporal_retention ON %I.%I', set_history_retention.schema_name, history_name);
        DELETE FROM eggdb_temporal.history_retention r WHERE r.schema_name = set_history_retention.schema_name AND r.table_name = set_history_retention.table_name;
    END IF;
    IF set_history_retention.keep IS NULL THEN
        RETURN;
    END IF;
    SELECT t.history_table, t.system_end INTO history_name, end_name FROM eggdb_temporal.tables t WHERE t.schema_name = set_history_retention.schema_name AND t.table_name = set_history_retention.table_name;
    IF history_name IS NULL THEN
        RAISE EXCEPTION 'table %.% is not system-versioned', set_history_retention.schema_name, set_history_retention.table_name USING ERRCODE = '42809';
    END IF;
    INSERT INTO eggdb_temporal.history_retention (schema_name, table_name, history_table, end_column, keep) VALUES (set_history_retention.schema_name, set_history_retention.table_name, history_name, end_name, set_history_retention.keep);
    EXECUTE format('CREATE TRIGGER eggdb_temporal_retention AFTER INSERT ON %I.%I FOR EACH STATEMENT EXECUTE FUNCTION eggdb_temporal.trim_history()', set_history_retention.schema_name, history_name);
    EXECUTE format('DELETE FROM %I.%I WHERE %I < %L', set_history_retention.schema_name, history_name, end_name, now() - set_history_retention.keep);
END
$$;
CREATE OR REPLACE FUNCTION eggdb_temporal.declare_system_period(schema_name text, table_name text, start_column text, end_column text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM eggdb_temporal.declared_periods p WHERE p.schema_name = declare_system_period.schema_name AND p.table_name = declare_system_period.table_name;
    INSERT INTO eggdb_temporal.declared_periods (schema_name, table_name, system_start, system_end) VALUES (declare_system_period.schema_name, declare_system_period.table_name, declare_system_period.start_column, declare_system_period.end_column);
END
$$;
CREATE OR REPLACE FUNCTION eggdb_temporal.enable_declared_versioning(schema_name text, table_name text, history_table text, open_end text, keep interval) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    start_name text;
    end_name text;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM eggdb_temporal.tables t WHERE t.schema_name = enable_declared_versioning.schema_name AND t.table_name = enable_declared_versioning.table_name) THEN
        SELECT p.system_start, p.system_end INTO start_name, end_name FROM eggdb_temporal.declared_periods p WHERE p.schema_name = enable_declared_versioning.schema_name AND p.table_name = enable_declared_versioning.table_name;
        IF start_name IS NULL THEN
            RAISE EXCEPTION 'Cannot set SYSTEM_VERSIONING to ON when SYSTEM_TIME period is not defined.' USING ERRCODE = '42P16';
        END IF;
        PERFORM eggdb_temporal.enable_system_versioning(enable_declared_versioning.schema_name, enable_declared_versioning.table_name, start_name, end_name, enable_declared_versioning.history_table, enable_declared_versioning.open_end);
    END IF;
    PERFORM eggdb_temporal.set_history_retention(enable_declared_versioning.schema_name, enable_declared_versioning.table_name, enable_declared_versioning.keep);
END
$$;
"##;

fn history_argument(history: Option<&crate::ast::Name>) -> String {
    history.map_or_else(|| "NULL".to_owned(), |given| quoted(base_name(&given.text)))
}

fn retention_argument(retention: Option<&str>) -> String {
    retention.map_or_else(|| "NULL".to_owned(), |keep| format!("{}::interval", quoted(keep)))
}

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

    fn open_end(&self) -> &'static str {
        match self.dialect {
            Dialect::SqlServer => SQL_SERVER_OPEN_END,
            Dialect::MySql => MARIADB_OPEN_END,
            _ => "infinity",
        }
    }

    /// Makes `table` system-versioned on `eggdb_temporal`'s machinery with the period columns
    /// and history table the statement named (D795), keeps its period for a later `SET
    /// (SYSTEM_VERSIONING = ON)` and its history retention (D900); a period declared without
    /// `SYSTEM_VERSIONING = ON` is only kept.
    pub(super) fn enable_system_versioning(
        &mut self,
        table: &str,
        versioning: &SystemVersioning,
    ) -> SqlResult<()> {
        self.declare_system_period(table, &versioning.start, &versioning.end)?;
        if !versioning.on {
            return Ok(());
        }
        let (schema, name) = schema_and_table(table);
        let history = history_argument(versioning.history.as_ref());
        let open_end = self.open_end();
        self.versioning_sql(&format!(
            "SELECT eggdb_temporal.enable_system_versioning({}, {}, {}, {}, {history}, {}); \
             SELECT eggdb_temporal.set_history_retention({}, {}, {})",
            quoted(&schema),
            quoted(&name),
            quoted(&versioning.start),
            quoted(&versioning.end),
            quoted(open_end),
            quoted(&schema),
            quoted(&name),
            retention_argument(versioning.retention.as_deref()),
        ))?;
        Ok(())
    }

    fn declare_system_period(&mut self, table: &str, start: &str, end: &str) -> SqlResult<()> {
        let (schema, name) = schema_and_table(table);
        self.versioning_sql(DECLARED_VERSIONING_SQL)?;
        self.versioning_sql(&format!(
            "SELECT eggdb_temporal.declare_system_period({}, {}, {}, {})",
            quoted(&schema),
            quoted(&name),
            quoted(start),
            quoted(end),
        ))?;
        Ok(())
    }

    fn enable_declared_versioning(
        &mut self,
        table: &str,
        history: Option<&crate::ast::Name>,
        retention: Option<&str>,
    ) -> SqlResult<()> {
        let (schema, name) = schema_and_table(table);
        self.versioning_sql(DECLARED_VERSIONING_SQL)?;
        let open_end = self.open_end();
        let outcome = self.versioning_sql(&format!(
            "SELECT eggdb_temporal.enable_declared_versioning({}, {}, {}, {}, {})",
            quoted(&schema),
            quoted(&name),
            history_argument(history),
            quoted(open_end),
            retention_argument(retention),
        ));
        match outcome {
            Err(error) if error.state() == SqlState::INVALID_TABLE_DEFINITION => {
                Err(error.with_vendor_code(PERIOD_MISSING_VENDOR_CODE))
            }
            other => other.map(drop),
        }
    }

    fn disable_system_versioning(&mut self, table: &str) -> SqlResult<()> {
        let (schema, name) = schema_and_table(table);
        self.versioning_sql(DECLARED_VERSIONING_SQL)?;
        self.versioning_sql(&format!(
            "SELECT eggdb_temporal.set_history_retention({}, {}, NULL); \
             SELECT eggdb_temporal.disable_system_versioning({}, {})",
            quoted(&schema),
            quoted(&name),
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
                AlterAction::SystemPeriod { start, end } => self.declare_system_period(table, start, end)?,
                AlterAction::SystemVersioningOn { history, retention } => {
                    self.enable_declared_versioning(table, history.as_ref(), retention.as_deref())?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}
