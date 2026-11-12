use super::{QueryResult, Session};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::foreign_tables::{
    CREATE_REGISTRY, ForeignDataCommand, SERVERS_TABLE, TABLES_TABLE, columns_query, local_type, server_source, table_source,
    view_sql,
};
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

    fn foreign_rows(&mut self, text: &str) -> SqlResult<Vec<Vec<String>>> {
        Ok(self
            .foreign_sql(text)?
            .iter()
            .filter(|result| result.columns.is_some())
            .flat_map(|result| result.rows.iter())
            .map(|row| row.iter().map(|value| value.to_text().unwrap_or_default()).collect())
            .collect())
    }

    fn create_foreign_table(
        &mut self,
        name: &str,
        columns: &[(String, String)],
        server: &str,
        options: &[(String, String)],
        if_not_exists: bool,
    ) -> SqlResult<()> {
        let Some(source) = self.server_of(server)? else {
            return Err(SqlError::new(SqlState::UNDEFINED_OBJECT, format!("server \"{server}\" does not exist")));
        };
        let known = !self
            .foreign_texts(&format!("SELECT name FROM {TABLES_TABLE} WHERE name = {}", literal(name)))?
            .is_empty();
        if known && if_not_exists {
            return Ok(());
        }
        let (source, query) = table_source(&source, crate::schemas::base_name(name), options)?;
        self.foreign_sql(&view_sql(name, columns, &source, &query))?;
        self.foreign_sql(&format!("INSERT INTO {TABLES_TABLE} VALUES ({}, {})", literal(name), literal(server)))?;
        Ok(())
    }

    fn import_foreign_schema(&mut self, remote: &str, limit: Option<&(bool, Vec<String>)>, server: &str, local: &str) -> SqlResult<()> {
        let Some(source) = self.server_of(server)? else {
            return Err(SqlError::new(SqlState::UNDEFINED_OBJECT, format!("server \"{server}\" does not exist")));
        };
        if source == "file:" {
            return Err(SqlError::new(SqlState::FEATURE_NOT_SUPPORTED, "a file_fdw server has no schema to import"));
        }
        let rows = self.foreign_rows(&format!(
            "SELECT r.o ->> 'table_name', r.o ->> 'column_name', r.o ->> 'data_type' FROM eggdb_foreign({}, {}) AS r(o)",
            literal(&source),
            literal(&columns_query(&source, remote))
        ))?;
        let mut tables: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for row in rows {
            let [table, column, data_type] = row.as_slice() else {
                continue;
            };
            let wanted = match limit {
                Some((false, names)) => names.iter().any(|name| name == table),
                Some((true, names)) => !names.iter().any(|name| name == table),
                None => true,
            };
            if !wanted {
                continue;
            }
            let column = (column.clone(), local_type(data_type).to_owned());
            match tables.last_mut() {
                Some((known, columns)) if known == table => columns.push(column),
                _ => tables.push((table.clone(), vec![column])),
            }
        }
        for (table, columns) in tables {
            let options = vec![("schema_name".to_owned(), remote.to_owned()), ("table_name".to_owned(), table.clone())];
            let name = crate::schemas::written_qualified(local, &table);
            self.create_foreign_table(&name, &columns, server, &options, false)?;
        }
        Ok(())
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
            } => self.create_foreign_table(&name.text, columns, server, options, *if_not_exists)?,
            ForeignDataCommand::ImportSchema {
                remote,
                limit,
                server,
                local,
                ..
            } => self.import_foreign_schema(remote, limit.as_ref(), server, local)?,
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
