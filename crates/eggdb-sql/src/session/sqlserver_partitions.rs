use super::{QueryResult, Session};
use crate::ast::Name;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::schemas::{DEFAULT_SCHEMA, base_name, schema_of};
use crate::sqlserver_partitions::{
    CREATE_TABLES, FUNCTIONS_TABLE, PartitionSchemeCommand, SCHEMES_TABLE, joined, ranges, split,
};
use crate::sqlserver_security::literal;
use crate::types::Value;

fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn text_of(value: &Value) -> String {
    value.character_text().map(str::to_owned).unwrap_or_default()
}

fn already_exists(name: &Name) -> SqlError {
    SqlError::new(
        SqlState::DUPLICATE_OBJECT,
        format!("There is already an object named '{}' in the database.", name.text),
    )
    .with_sqlserver_code(2714)
    .at(name.position)
}

fn invalid_scheme(name: &Name) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("Invalid partition scheme '{}' specified.", name.text),
    )
    .with_sqlserver_code(1921)
    .at(name.position)
}

impl Session<'_> {
    fn partition_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        self.in_trigger_dialect(Dialect::PostgreSql, |session| session.run_quietly(text))
    }

    fn partition_rows(&mut self, text: &str) -> Vec<Vec<Value>> {
        self.partition_sql(text)
            .map(|results| results.into_iter().flat_map(|result| result.rows).collect())
            .unwrap_or_default()
    }

    fn has_partition_object(&mut self, table: &str, name: &Name) -> bool {
        !self
            .partition_rows(&format!(
                "SELECT 1 FROM {table} WHERE upper(name) = upper({})",
                literal(&name.text)
            ))
            .is_empty()
    }

    pub(super) fn run_partition_scheme_command(&mut self, command: &PartitionSchemeCommand) -> SqlResult<QueryResult> {
        self.partition_sql(CREATE_TABLES)?;
        match command {
            PartitionSchemeCommand::CreateFunction {
                name,
                input_type,
                right,
                boundaries,
            } => {
                if self.has_partition_object(FUNCTIONS_TABLE, name) {
                    return Err(already_exists(name));
                }
                if !right && ranges(input_type, false, boundaries).is_none() {
                    return Err(SqlError::new(SqlState::FEATURE_NOT_SUPPORTED, format!(
                        "RANGE LEFT on {input_type}: EggDB keeps ranges by their exclusive upper bound, and {input_type} has no next value; use RANGE RIGHT"
                    ))
                    .with_sqlserver_code(7705)
                    .at(name.position));
                }
                self.partition_sql(&format!(
                    "INSERT INTO {FUNCTIONS_TABLE} VALUES ({}, {}, {right}, {})",
                    literal(&name.text),
                    literal(input_type),
                    literal(&joined(boundaries))
                ))?;
            }
            PartitionSchemeCommand::CreateScheme { name, function } => {
                if self.has_partition_object(SCHEMES_TABLE, name) {
                    return Err(already_exists(name));
                }
                if !self.has_partition_object(FUNCTIONS_TABLE, function) {
                    return Err(SqlError::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("Invalid partition function name '{}'.", function.text),
                    )
                    .with_sqlserver_code(7702)
                    .at(function.position));
                }
                self.partition_sql(&format!(
                    "INSERT INTO {SCHEMES_TABLE} VALUES ({}, {})",
                    literal(&name.text),
                    literal(&function.text)
                ))?;
            }
            PartitionSchemeCommand::DropFunction(name) => {
                let used = !self
                    .partition_rows(&format!(
                        "SELECT 1 FROM {SCHEMES_TABLE} WHERE upper(function) = upper({})",
                        literal(&name.text)
                    ))
                    .is_empty();
                if used {
                    return Err(SqlError::new(
                        SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                        format!("Partition function '{}' is being used by one or more partition schemes.", name.text),
                    )
                    .with_sqlserver_code(7706)
                    .at(name.position));
                }
                self.drop_partition_object(FUNCTIONS_TABLE, name, "function")?;
            }
            PartitionSchemeCommand::DropScheme(name) => {
                self.drop_partition_object(SCHEMES_TABLE, name, "scheme")?;
            }
        }
        Ok(QueryResult::command(command.tag()))
    }

    fn drop_partition_object(&mut self, table: &str, name: &Name, kind: &str) -> SqlResult<()> {
        if !self.has_partition_object(table, name) {
            return Err(SqlError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("Cannot drop the partition {kind} '{}', because it does not exist or you do not have permission.", name.text),
            )
            .with_sqlserver_code(7716)
            .at(name.position));
        }
        self.partition_sql(&format!(
            "DELETE FROM {table} WHERE upper(name) = upper({})",
            literal(&name.text)
        ))
        .map(drop)
    }

    pub(super) fn partition_scheme_ranges(&mut self, scheme: &Name) -> SqlResult<Vec<(String, String)>> {
        let rows = self.partition_rows(&format!(
            "SELECT f.input_type, f.boundary_on_right, f.boundaries FROM {SCHEMES_TABLE} s \
             JOIN {FUNCTIONS_TABLE} f ON upper(f.name) = upper(s.function) WHERE upper(s.name) = upper({})",
            literal(&scheme.text)
        ));
        let Some(row) = rows.first().filter(|row| row.len() == 3) else {
            return Err(invalid_scheme(scheme));
        };
        let right = row[1] == Value::Boolean(true);
        ranges(&text_of(&row[0]), right, &split(&text_of(&row[2]))).ok_or_else(|| invalid_scheme(scheme))
    }

    /// Creates one partition of `table` for each range of its scheme's function, named
    /// `<table>_p<n>` from 1 as SQL Server numbers partitions.
    pub(super) fn create_scheme_partitions(&mut self, table: &str, ranges: &[(String, String)]) -> SqlResult<()> {
        let schema = schema_of(table);
        let schema = if schema.is_empty() { DEFAULT_SCHEMA } else { schema };
        let name = base_name(table);
        let script: String = ranges
            .iter()
            .enumerate()
            .map(|(index, (low, high))| {
                format!(
                    "CREATE TABLE {schema}.{partition} PARTITION OF {schema}.{parent} FOR VALUES FROM ({low}) TO ({high}); ",
                    schema = identifier(schema),
                    partition = identifier(&format!("{name}_p{}", index + 1)),
                    parent = identifier(name),
                )
            })
            .collect();
        self.partition_sql(&script).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::{Dialect, Session};

    fn run(session: &mut Session<'_>, sql: &str) -> String {
        let mut found = String::new();
        session
            .execute(sql, &mut |result| found = crate::tests::render(&result))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        found
    }

    #[test]
    fn a_table_on_a_partition_scheme_spreads_rows_over_the_function_ranges() {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, Dialect::SqlServer);
        run(&mut session, "CREATE PARTITION FUNCTION by_hundreds (int) AS RANGE LEFT FOR VALUES (100, 200)");
        run(&mut session, "CREATE PARTITION SCHEME hundreds AS PARTITION by_hundreds ALL TO ([PRIMARY])");
        run(&mut session, "CREATE TABLE readings (id int NOT NULL, reading int) ON hundreds(id)");
        run(&mut session, "INSERT INTO readings VALUES (100, 1), (101, 2), (250, 3)");
        assert!(run(&mut session, "SELECT count(*) AS n FROM readings_p1").contains("n\n1"));
        assert!(run(&mut session, "SELECT count(*) AS n FROM readings_p2").contains("n\n1"));
        assert!(run(&mut session, "SELECT count(*) AS n FROM readings_p3").contains("n\n1"));
        assert!(session.execute("DROP PARTITION FUNCTION by_hundreds", &mut |_| {}).is_err());
        assert!(session.execute("CREATE TABLE elsewhere (id int) ON nowhere(id)", &mut |_| {}).is_err());
        assert!(session.execute("CREATE PARTITION FUNCTION by_price (decimal(10,2)) AS RANGE LEFT FOR VALUES (1.5)", &mut |_| {}).is_err());
    }
}
