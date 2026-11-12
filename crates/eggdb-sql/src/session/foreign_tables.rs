use super::{QueryResult, Session};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::foreign_tables::{CREATE_REGISTRY, ForeignDataCommand, SERVERS_TABLE, TABLES_TABLE, server_source, table_source, view_sql};
use crate::sqlserver_security::{literal, qualified};
use crate::types::Value;

impl Session<'_> {
    fn foreign_sql(&mut self, text: &str) -> SqlResult<Vec<QueryResult>> {
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(text);
        self.dialect = dialect;
        outcome
    }

    fn foreign_texts(&mut self, text: &str) -> SqlResult<Vec<String>> {
        Ok(self
            .foreign_sql(text)?
            .iter()
            .filter(|result| result.columns.is_some())
            .flat_map(|result| result.rows.iter())
            .filter_map(|row| row.first().and_then(Value::to_text))
            .collect())
    }

    fn server_of(&mut self, name: &str) -> SqlResult<Option<String>> {
        Ok(self
            .foreign_texts(&format!("SELECT source FROM {SERVERS_TABLE} WHERE name = {}", literal(name)))?
            .into_iter()
            .next())
    }

    pub(super) fn run_foreign_data(&mut self, command: &ForeignDataCommand) -> SqlResult<QueryResult> {
        self.foreign_sql(CREATE_REGISTRY)?;
        match command {
            ForeignDataCommand::CreateWrapper | ForeignDataCommand::DropWrapper => {}
            ForeignDataCommand::CreateServer {
                name,
                wrapper,
                options,
                if_not_exists,
            } => {
                let source = server_source(wrapper, options)?;
                if self.server_of(&name.text)?.is_some() {
                    if *if_not_exists {
                        return Ok(QueryResult::command(command.tag()));
                    }
                    return Err(SqlError::new(SqlState::DUPLICATE_OBJECT, format!("server \"{}\" already exists", name.text)));
                }
                self.foreign_sql(&format!(
                    "INSERT INTO {SERVERS_TABLE} VALUES ({}, {}, {})",
                    literal(&name.text),
                    literal(wrapper),
                    literal(&source)
                ))?;
            }
            ForeignDataCommand::DropServer { names, if_exists, cascade } => {
                for name in names {
                    if self.server_of(&name.text)?.is_none() {
                        if *if_exists {
                            continue;
                        }
                        return Err(SqlError::new(SqlState::UNDEFINED_OBJECT, format!("server \"{}\" does not exist", name.text)));
                    }
                    let tables = self.foreign_texts(&format!("SELECT name FROM {TABLES_TABLE} WHERE server = {}", literal(&name.text)))?;
                    if !tables.is_empty() && !*cascade {
                        return Err(SqlError::new(
                            SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                            format!("cannot drop server {} because other objects depend on it", name.text),
                        )
                        .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
                    }
                    for table in &tables {
                        self.foreign_sql(&format!("DROP VIEW IF EXISTS {}; DELETE FROM {TABLES_TABLE} WHERE name = {}", qualified(table), literal(table)))?;
                    }
                    self.foreign_sql(&format!("DELETE FROM {SERVERS_TABLE} WHERE name = {}", literal(&name.text)))?;
                }
            }
            ForeignDataCommand::CreateTable {
                name,
                columns,
                server,
                options,
                if_not_exists,
            } => {
                let Some(source) = self.server_of(server)? else {
                    return Err(SqlError::new(SqlState::UNDEFINED_OBJECT, format!("server \"{server}\" does not exist")));
                };
                let known = !self
                    .foreign_texts(&format!("SELECT name FROM {TABLES_TABLE} WHERE name = {}", literal(&name.text)))?
                    .is_empty();
                if known && *if_not_exists {
                    return Ok(QueryResult::command(command.tag()));
                }
                let (source, query) = table_source(&source, crate::schemas::base_name(&name.text), options)?;
                self.foreign_sql(&view_sql(&name.text, columns, &source, &query))?;
                self.foreign_sql(&format!("INSERT INTO {TABLES_TABLE} VALUES ({}, {})", literal(&name.text), literal(server)))?;
            }
            ForeignDataCommand::DropTable { names, if_exists } => {
                for name in names {
                    let known = !self
                        .foreign_texts(&format!("SELECT name FROM {TABLES_TABLE} WHERE name = {}", literal(&name.text)))?
                        .is_empty();
                    if !known {
                        if *if_exists {
                            continue;
                        }
                        return Err(SqlError::new(SqlState::UNDEFINED_TABLE, format!("foreign table \"{}\" does not exist", name.text)));
                    }
                    self.foreign_sql(&format!(
                        "DROP VIEW {}; DELETE FROM {TABLES_TABLE} WHERE name = {}",
                        qualified(&name.text),
                        literal(&name.text)
                    ))?;
                }
            }
        }
        Ok(QueryResult::command(command.tag()))
    }
}
