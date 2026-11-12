use super::Session;
use crate::dialect::Dialect;
use crate::error::SqlResult;
use crate::schemas::{DEFAULT_SCHEMA, base_name, schema_of};

pub(crate) const ROWVERSION_SEQUENCE: &str = "eggdb_internal.sqlserver_rowversion";
const FIRST_ROWVERSION: i64 = 2001;

fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Session<'_> {
    /// Keeps SQL Server's `rowversion` column `column` of `table`: every insert and update
    /// stamps it with the next value of one database-wide counter, as 8 big-endian bytes (D798).
    pub(super) fn enable_rowversion(&mut self, table: &str, column: &str) -> SqlResult<()> {
        let schema = schema_of(table);
        let schema = if schema.is_empty() { DEFAULT_SCHEMA } else { schema };
        let name = base_name(table);
        let function = identifier(&format!("rowversion_{schema}_{name}"));
        let text = format!(
            "CREATE SCHEMA IF NOT EXISTS eggdb_internal; \
             CREATE SEQUENCE IF NOT EXISTS {ROWVERSION_SEQUENCE} START {FIRST_ROWVERSION}; \
             CREATE FUNCTION eggdb_internal.{function}() RETURNS trigger LANGUAGE plpgsql AS $rowversion$ \
             BEGIN \
                 NEW.{column} := decode(lpad(to_hex(nextval('{ROWVERSION_SEQUENCE}')), 16, '0'), 'hex'); \
                 RETURN NEW; \
             END \
             $rowversion$; \
             CREATE TRIGGER eggdb_rowversion BEFORE INSERT OR UPDATE ON {}.{} FOR EACH ROW \
             EXECUTE FUNCTION eggdb_internal.{function}()",
            identifier(schema),
            identifier(name),
            column = identifier(column),
        );
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(&text);
        self.dialect = dialect;
        outcome.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::{Dialect, Session};

    fn rows(session: &mut Session<'_>, sql: &str) -> Vec<crate::Value> {
        let mut found = Vec::new();
        session
            .execute(sql, &mut |result| {
                found.extend(result.rows.into_iter().filter_map(|row| row.into_iter().next()));
            })
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        found
    }

    #[test]
    fn a_rowversion_column_changes_on_every_write() {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, Dialect::SqlServer);
        rows(&mut session, "CREATE TABLE stock (id int PRIMARY KEY, amount int, version rowversion)");
        rows(&mut session, "INSERT INTO stock (id, amount) VALUES (1, 5)");
        let first = rows(&mut session, "SELECT version FROM stock WHERE id = 1");
        rows(&mut session, "UPDATE stock SET amount = 6 WHERE id = 1");
        let second = rows(&mut session, "SELECT version FROM stock WHERE id = 1");
        let (Some(crate::Value::Bytea(first)), Some(crate::Value::Bytea(second))) =
            (first.first(), second.first())
        else {
            panic!("rowversions are binary: {first:?} {second:?}");
        };
        assert_eq!(first.len(), 8);
        assert!(second > first);
    }
}
