use super::{OutputColumn, QueryResult, Session};
use crate::lexer::swap_uniform_case;
use crate::oracle_dictionary::type_text;
use crate::schema::{ColumnSchema, TableSchema};
use crate::store::KeyValueStore;

pub(super) fn shown_columns(columns: Vec<OutputColumn>) -> Vec<OutputColumn> {
    columns
        .into_iter()
        .map(|column| OutputColumn {
            name: swap_uniform_case(&column.name),
            sql_type: column.sql_type,
        })
        .collect()
}

pub(super) fn after_statement(result: &mut QueryResult) {
    result.notices.clear();
    if let Some(columns) = result.columns.take() {
        result.columns = Some(shown_columns(columns));
    }
}

fn identifier(name: &str) -> String {
    let shown = swap_uniform_case(name);
    let plain = shown
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
        && shown
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '$' | '#'))
        && !crate::parser::is_oracle_reserved(&name.to_ascii_lowercase());
    if plain {
        shown
    } else {
        format!("\"{}\"", shown.replace('"', "\"\""))
    }
}

fn column_text(table: &TableSchema, column: &ColumnSchema) -> String {
    let mut text = format!(
        "{} {}",
        identifier(&column.name),
        type_text(column.sql_type)
    );
    let single_key = table.primary_key.len() == 1 && table.primary_key[0] == column.name;
    if let Some(default) = &column.default {
        text.push_str(&format!(" DEFAULT {default}"));
    }
    if single_key {
        text.push_str(" PRIMARY KEY");
    } else if column.not_null {
        text.push_str(" NOT NULL");
    }
    text
}

/// The `CREATE TABLE` statement that makes `table` in Oracle's SQL.
pub(crate) fn create_text(table: &TableSchema) -> String {
    let mut parts: Vec<String> = table
        .columns
        .iter()
        .map(|column| column_text(table, column))
        .collect();
    if table.primary_key.len() > 1 {
        let key: Vec<String> = table
            .primary_key
            .iter()
            .map(|name| identifier(name))
            .collect();
        parts.push(format!("PRIMARY KEY ({})", key.join(", ")));
    }
    format!(
        "CREATE TABLE {} ({})",
        identifier(&table.name),
        parts.join(", ")
    )
}

impl<'s, S: KeyValueStore + 's> Session<'s, S> {
    pub(super) fn oracle_user(&self) -> String {
        self.settings
            .get("session_authorization")
            .unwrap_or_default()
            .to_uppercase()
    }
}
