use crate::encoding::{TableDefinition, prefix_end, table_key};
use crate::error::SqlResult;
use crate::store::{KeyValueRead, ScanDirection};
use crate::types::SqlType;

/// A table as the catalog defines it.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSchema {
    /// The table's name.
    pub name: String,
    /// Its columns, in order.
    pub columns: Vec<ColumnSchema>,
    /// The names of the primary key's columns, in key order; empty without a primary key.
    pub primary_key: Vec<String>,
}

/// One column of a [`TableSchema`].
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSchema {
    /// The column's name.
    pub name: String,
    /// Its type.
    pub sql_type: SqlType,
    /// Whether it refuses NULL.
    pub not_null: bool,
    /// The text of its `DEFAULT` expression, when it has one.
    pub default: Option<String>,
    /// Whether MySQL's `AUTO_INCREMENT` numbers its rows.
    pub auto_increment: bool,
}

impl TableSchema {
    /// The `CREATE TABLE` statement that makes this table, ended by `;`.
    pub fn create_statement(&self) -> String {
        let mut parts: Vec<String> = self
            .columns
            .iter()
            .map(|column| {
                let mut part = format!("{} {}", quoted(&column.name), column.sql_type.name());
                if let Some(default) = &column.default {
                    part.push_str(&format!(" DEFAULT {default}"));
                }
                if column.not_null && !self.primary_key.contains(&column.name) {
                    part.push_str(" NOT NULL");
                }
                part
            })
            .collect();
        if !self.primary_key.is_empty() {
            let key: Vec<String> = self.primary_key.iter().map(|name| quoted(name)).collect();
            parts.push(format!("PRIMARY KEY ({})", key.join(", ")));
        }
        format!(
            "CREATE TABLE {} ({});",
            quoted(&self.name),
            parts.join(", ")
        )
    }
}

/// `name` as SQL spells it: as it is when it is a lower-case identifier, otherwise in double
/// quotes with any `"` doubled.
pub fn quoted(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        name.to_owned()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

pub(crate) fn read_tables(reader: &dyn KeyValueRead) -> SqlResult<Vec<TableSchema>> {
    let prefix = table_key("");
    let end = prefix_end(&prefix);
    let mut tables = Vec::new();
    let mut failure = None;
    reader.scan(&prefix, &end, ScanDirection::Forward, &mut |key, value| {
        let name = String::from_utf8_lossy(&key[prefix.len()..]).into_owned();
        match TableDefinition::decode(&name, value) {
            Ok(definition) => tables.push(schema_of(&definition)),
            Err(error) => {
                failure = Some(error);
                return Ok(false);
            }
        }
        Ok(true)
    })?;
    match failure {
        Some(error) => Err(error),
        None => Ok(tables),
    }
}

fn schema_of(definition: &TableDefinition) -> TableSchema {
    TableSchema {
        name: definition.name.clone(),
        columns: definition
            .columns
            .iter()
            .map(|column| ColumnSchema {
                name: column.name.clone(),
                sql_type: column.sql_type,
                not_null: column.not_null,
                default: column.default.clone(),
                auto_increment: column.auto_increment,
            })
            .collect(),
        primary_key: definition
            .primary_key
            .iter()
            .filter_map(|&index| definition.columns.get(index))
            .map(|column| column.name.clone())
            .collect(),
    }
}
