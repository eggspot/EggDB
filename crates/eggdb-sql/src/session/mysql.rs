use super::{Block, OutputColumn, QueryResult, Session};
use crate::ast::{AssignedValue, MySqlStatement, Name, VariableTarget};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, like};
use crate::mysql_variables::SetRefused;
use crate::schema::{ColumnSchema, TableSchema, read_tables};
use crate::store::KeyValueStore;
use crate::types::{SqlType, Value};
use std::sync::Arc;

fn text_column(name: &str) -> OutputColumn {
    OutputColumn {
        name: name.to_owned(),
        sql_type: Some(SqlType::Text),
    }
}

fn number_column(name: &str) -> OutputColumn {
    OutputColumn {
        name: name.to_owned(),
        sql_type: Some(SqlType::BigInt),
    }
}

fn matches_pattern(text: &str, pattern: Option<&str>) -> SqlResult<bool> {
    match pattern {
        None => Ok(true),
        Some(pattern) => like(&text.to_lowercase(), &pattern.to_lowercase()),
    }
}

fn rows_result(columns: Vec<OutputColumn>, rows: Vec<Vec<Value>>) -> QueryResult {
    QueryResult::rows(format!("SELECT {}", rows.len()), columns, rows)
}

pub(crate) fn mysql_type_name(sql_type: SqlType) -> String {
    match sql_type {
        SqlType::SmallInt => "smallint".into(),
        SqlType::Integer => "int".into(),
        SqlType::BigInt => "bigint".into(),
        SqlType::Real => "float".into(),
        SqlType::Double => "double".into(),
        SqlType::Boolean => "tinyint(1)".into(),
        SqlType::Text | SqlType::Varchar(None) => "text".into(),
        SqlType::Varchar(Some(length)) => format!("varchar({length})"),
    }
}

fn backticked(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn create_statement(table: &TableSchema) -> String {
    let mut lines: Vec<String> = table
        .columns
        .iter()
        .map(|column| column_definition(table, column))
        .collect();
    if !table.primary_key.is_empty() {
        let key: Vec<String> = table
            .primary_key
            .iter()
            .map(|name| backticked(name))
            .collect();
        lines.push(format!("  PRIMARY KEY ({})", key.join(",")));
    }
    for index in &table.indexes {
        let columns: Vec<String> = index.columns.iter().map(|name| backticked(name)).collect();
        lines.push(format!(
            "  {}KEY {} ({})",
            if index.unique { "UNIQUE " } else { "" },
            backticked(&index.name),
            columns.join(",")
        ));
    }
    format!(
        "CREATE TABLE {} (\n{}\n) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci",
        backticked(&table.name),
        lines.join(",\n")
    )
}

fn column_definition(table: &TableSchema, column: &ColumnSchema) -> String {
    let mut line = format!(
        "  {} {}",
        backticked(&column.name),
        mysql_type_name(column.sql_type)
    );
    if column.not_null {
        line.push_str(" NOT NULL");
    }
    if column.auto_increment {
        line.push_str(" AUTO_INCREMENT");
    } else if let Some(default) = &column.default {
        line.push_str(&format!(" DEFAULT {default}"));
    } else if !column.not_null && !table.primary_key.contains(&column.name) {
        line.push_str(" DEFAULT NULL");
    }
    line
}

impl<'s, S: KeyValueStore + 's> Session<'s, S> {
    pub(super) fn describe_mysql(
        &mut self,
        statement: &MySqlStatement,
    ) -> SqlResult<Option<Vec<OutputColumn>>> {
        Ok(match statement {
            MySqlStatement::ShowTables { full, .. } => Some(self.tables_columns(*full)),
            MySqlStatement::ShowDatabases { .. } => Some(vec![text_column("Database")]),
            MySqlStatement::ShowColumns { .. } => Some(columns_columns()),
            MySqlStatement::ShowCreateTable(_) => {
                Some(vec![text_column("Table"), text_column("Create Table")])
            }
            MySqlStatement::ShowIndex(_) => Some(index_columns()),
            MySqlStatement::ShowVariables { .. } | MySqlStatement::ShowStatus => {
                Some(vec![text_column("Variable_name"), text_column("Value")])
            }
            MySqlStatement::ShowWarnings { .. } => Some(warnings_columns()),
            MySqlStatement::ShowCount { errors_only } => {
                Some(vec![number_column(count_name(*errors_only))])
            }
            MySqlStatement::Use(_) | MySqlStatement::Set(_) | MySqlStatement::Accepted(_) => None,
        })
    }

    fn tables_columns(&self, full: bool) -> Vec<OutputColumn> {
        let mut columns = vec![text_column(&format!(
            "Tables_in_{}",
            self.settings.database
        ))];
        if full {
            columns.push(text_column("Table_type"));
        }
        columns
    }

    pub(super) fn run_mysql(&mut self, statement: &MySqlStatement) -> SqlResult<QueryResult> {
        match statement {
            MySqlStatement::ShowTables { full, like } => {
                let tables = self.read(|reader, _| read_tables(reader))?;
                let mut rows = Vec::new();
                for table in tables {
                    if matches_pattern(&table.name, like.as_deref())? {
                        let mut row = vec![Value::Text(table.name)];
                        if *full {
                            row.push(Value::Text("BASE TABLE".into()));
                        }
                        rows.push(row);
                    }
                }
                Ok(rows_result(self.tables_columns(*full), rows))
            }
            MySqlStatement::ShowDatabases { like } => {
                let database = self.settings.database.clone();
                let rows = if matches_pattern(&database, like.as_deref())? {
                    vec![vec![Value::Text(database)]]
                } else {
                    Vec::new()
                };
                Ok(rows_result(vec![text_column("Database")], rows))
            }
            MySqlStatement::ShowColumns { table, like, .. } => {
                let schema = self.table_schema(table)?;
                let mut rows = Vec::new();
                for column in &schema.columns {
                    if !matches_pattern(&column.name, like.as_deref())? {
                        continue;
                    }
                    let key = column_key(&schema, &column.name);
                    rows.push(vec![
                        Value::Text(column.name.clone()),
                        Value::Text(mysql_type_name(column.sql_type)),
                        Value::Text(if column.not_null { "NO" } else { "YES" }.into()),
                        Value::Text(key.into()),
                        column
                            .default
                            .as_ref()
                            .map_or(Value::Null, |default| Value::Text(unquoted(default))),
                        Value::Text(
                            if column.auto_increment {
                                "auto_increment"
                            } else {
                                ""
                            }
                            .into(),
                        ),
                    ]);
                }
                Ok(rows_result(columns_columns(), rows))
            }
            MySqlStatement::ShowIndex(table) => {
                let schema = self.table_schema(table)?;
                Ok(rows_result(index_columns(), index_rows(&schema)))
            }
            MySqlStatement::ShowCreateTable(table) => {
                let schema = self.table_schema(table)?;
                let rows = vec![vec![
                    Value::Text(schema.name.clone()),
                    Value::Text(create_statement(&schema)),
                ]];
                Ok(rows_result(
                    vec![text_column("Table"), text_column("Create Table")],
                    rows,
                ))
            }
            MySqlStatement::ShowVariables { like } => {
                let mut rows = Vec::new();
                for (name, value) in self.settings.mysql.all() {
                    if matches_pattern(&name, like.as_deref())? {
                        let shown = Dialect::MySql.value_text(&value).unwrap_or_default();
                        rows.push(vec![Value::Text(name), Value::Text(shown)]);
                    }
                }
                Ok(rows_result(
                    vec![text_column("Variable_name"), text_column("Value")],
                    rows,
                ))
            }
            MySqlStatement::ShowStatus => Ok(rows_result(
                vec![text_column("Variable_name"), text_column("Value")],
                Vec::new(),
            )),
            MySqlStatement::ShowWarnings { errors_only } => {
                let rows = self
                    .warnings
                    .iter()
                    .filter(|warning| !errors_only || warning.level == "Error")
                    .map(|warning| {
                        vec![
                            Value::Text(warning.level.to_owned()),
                            Value::BigInt(i64::from(warning.code)),
                            Value::Text(warning.message.clone()),
                        ]
                    })
                    .collect();
                Ok(rows_result(warnings_columns(), rows))
            }
            MySqlStatement::ShowCount { errors_only } => {
                let count = self
                    .warnings
                    .iter()
                    .filter(|warning| !errors_only || warning.level == "Error")
                    .count();
                Ok(rows_result(
                    vec![number_column(count_name(*errors_only))],
                    vec![vec![Value::BigInt(count as i64)]],
                ))
            }
            MySqlStatement::Use(name) => {
                let database = &self.settings.database;
                if name.text != *database && name.text != database.to_lowercase() {
                    return Err(SqlError::new(
                        SqlState::INVALID_CATALOG_NAME,
                        format!("database \"{}\" does not exist", name.text),
                    ));
                }
                Ok(QueryResult::command("USE"))
            }
            MySqlStatement::Set(assignments) => {
                for assignment in assignments {
                    self.assign(&assignment.target, &assignment.value)?;
                }
                Ok(QueryResult::command("SET"))
            }
            MySqlStatement::Accepted(tag) => Ok(QueryResult::command(*tag)),
        }
    }

    fn table_schema(&mut self, table: &Name) -> SqlResult<TableSchema> {
        let tables = self.read(|reader, _| read_tables(reader))?;
        tables
            .into_iter()
            .find(|schema| schema.name == table.text)
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::UNDEFINED_TABLE,
                    format!("relation \"{}\" does not exist", table.text),
                )
                .at(table.position)
            })
    }

    fn assign(&mut self, target: &VariableTarget, value: &AssignedValue) -> SqlResult<()> {
        let value = match value {
            AssignedValue::Default => None,
            AssignedValue::Word(word) => Some(Value::Text(word.clone())),
            AssignedValue::Expr(expr) => Some(
                Evaluator {
                    scope: Scope::empty(),
                    row: &[],
                }
                .eval(expr, None)?,
            ),
        };
        match target {
            VariableTarget::User(name) => {
                Arc::make_mut(&mut self.settings)
                    .mysql
                    .set_user(name, value.unwrap_or(Value::Null));
                Ok(())
            }
            VariableTarget::System(name) => {
                let shown = value
                    .as_ref()
                    .and_then(|value| Dialect::MySql.value_text(value))
                    .unwrap_or_else(|| "NULL".to_owned());
                let was_autocommit = self.autocommit();
                Arc::make_mut(&mut self.settings)
                    .mysql
                    .set_system(name, value)
                    .map_err(|refused| match refused {
                        SetRefused::ReadOnly => SqlError::new(
                            SqlState::MYSQL_GENERAL,
                            format!("Variable '{name}' is a read only variable"),
                        )
                        .with_vendor_code(1238),
                        SetRefused::WrongValue => SqlError::new(
                            SqlState::MYSQL_SYNTAX_OR_ACCESS,
                            format!("Variable '{name}' can't be set to the value of '{shown}'"),
                        )
                        .with_vendor_code(1231),
                    })?;
                if !was_autocommit
                    && self.autocommit()
                    && let Block::Writing(_) = self.block
                {
                    self.commit()?;
                }
                Ok(())
            }
        }
    }
}

fn unquoted(default: &str) -> String {
    default
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .map_or_else(|| default.to_owned(), |inner| inner.replace("''", "'"))
}

fn column_key(table: &TableSchema, column: &str) -> &'static str {
    if table.primary_key.iter().any(|name| name == column) {
        return "PRI";
    }
    let leading = |unique: bool| {
        table.indexes.iter().any(|index| {
            index.columns.first().is_some_and(|first| first == column)
                && (!unique || index.unique && index.columns.len() == 1)
        })
    };
    if leading(true) {
        "UNI"
    } else if leading(false) {
        "MUL"
    } else {
        ""
    }
}

fn index_columns() -> Vec<OutputColumn> {
    [
        ("Table", false),
        ("Non_unique", true),
        ("Key_name", false),
        ("Seq_in_index", true),
        ("Column_name", false),
        ("Collation", false),
        ("Cardinality", true),
        ("Sub_part", true),
        ("Packed", false),
        ("Null", false),
        ("Index_type", false),
        ("Comment", false),
        ("Index_comment", false),
        ("Visible", false),
        ("Expression", false),
    ]
    .into_iter()
    .map(|(name, number)| {
        if number {
            number_column(name)
        } else {
            text_column(name)
        }
    })
    .collect()
}

fn index_rows(table: &TableSchema) -> Vec<Vec<Value>> {
    let primary = (!table.primary_key.is_empty()).then(|| ("PRIMARY", true, &table.primary_key));
    let keys = primary.into_iter().chain(
        table
            .indexes
            .iter()
            .map(|index| (index.name.as_str(), index.unique, &index.columns)),
    );
    let mut rows = Vec::new();
    for (name, unique, columns) in keys {
        for (position, column) in columns.iter().enumerate() {
            let nullable = table
                .columns
                .iter()
                .find(|known| &known.name == column)
                .is_some_and(|known| !known.not_null);
            let text = |text: &str| Value::Text(text.to_owned());
            rows.push(vec![
                text(&table.name),
                Value::BigInt(i64::from(!unique)),
                text(name),
                Value::BigInt(position as i64 + 1),
                text(column),
                text("A"),
                Value::Null,
                Value::Null,
                Value::Null,
                text(if nullable { "YES" } else { "" }),
                text("BTREE"),
                text(""),
                text(""),
                text("YES"),
                Value::Null,
            ]);
        }
    }
    rows
}

fn columns_columns() -> Vec<OutputColumn> {
    ["Field", "Type", "Null", "Key", "Default", "Extra"]
        .into_iter()
        .map(text_column)
        .collect()
}

fn warnings_columns() -> Vec<OutputColumn> {
    vec![
        text_column("Level"),
        number_column("Code"),
        text_column("Message"),
    ]
}

fn count_name(errors_only: bool) -> &'static str {
    if errors_only {
        "@@session.error_count"
    } else {
        "@@session.warning_count"
    }
}
