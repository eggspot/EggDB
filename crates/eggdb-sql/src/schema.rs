use crate::encoding::{TableDefinition, prefix_end, table_key};
use crate::error::SqlResult;
use crate::store::{KeyValueRead, ScanDirection};
use crate::types::SqlType;

/// A table as the catalog defines it.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSchema {
    /// The table's name, prefixed by its schema and a dot when that is not `public`.
    pub name: String,
    /// The schema the table is in: `public` unless it was created in another one.
    pub schema: String,
    /// The table's comment, from `COMMENT ON TABLE` or MySQL's `COMMENT` table option.
    pub comment: Option<String>,
    /// Its columns, in order.
    pub columns: Vec<ColumnSchema>,
    /// The names of the primary key's columns, in key order; empty without a primary key.
    pub primary_key: Vec<String>,
    /// Its secondary indexes, in the order they were created.
    pub indexes: Vec<IndexSchema>,
}

/// A secondary index of a [`TableSchema`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSchema {
    /// The index's name.
    pub name: String,
    /// Whether it refuses two rows with the same non-NULL values.
    pub unique: bool,
    /// The names of its columns, in key order.
    pub columns: Vec<String>,
}

impl IndexSchema {
    /// The `CREATE INDEX` statement that makes this index on `table` in `dialect`, ended by `;`.
    pub fn create_statement_in(&self, table: &str, dialect: crate::Dialect) -> String {
        let name = |text: &str| match dialect {
            crate::Dialect::MySql => format!("`{}`", text.replace('`', "``")),
            crate::Dialect::Oracle => crate::lexer::swap_uniform_case(text),
            _ => quoted(text),
        };
        let columns: Vec<String> = self.columns.iter().map(|column| name(column)).collect();
        let on = match dialect {
            crate::Dialect::Oracle => crate::lexer::swap_uniform_case(table),
            crate::Dialect::MySql => name(table),
            _ => quoted(table),
        };
        format!(
            "CREATE {}INDEX {} ON {on} ({});",
            if self.unique { "UNIQUE " } else { "" },
            name(&self.name),
            columns.join(", ")
        )
    }
}

/// A view as the catalog defines it.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewSchema {
    /// The view's name, prefixed by its schema and a dot when that is not `public`.
    pub name: String,
    /// The schema the view is in: `public` unless it was created in another one.
    pub schema: String,
    /// The view's comment, from `COMMENT ON VIEW`.
    pub comment: Option<String>,
    /// Its columns, in order.
    pub columns: Vec<ViewColumnSchema>,
}

/// One column of a [`ViewSchema`].
#[derive(Debug, Clone, PartialEq)]
pub struct ViewColumnSchema {
    /// The column's name.
    pub name: String,
    /// Its type, when the view's query fixes it.
    pub sql_type: Option<SqlType>,
}

pub(crate) fn read_views(reader: &dyn KeyValueRead) -> SqlResult<Vec<ViewSchema>> {
    let mut views = Vec::new();
    for view in crate::views::every_view(reader)? {
        views.push(ViewSchema {
            name: crate::schemas::display(&view.name),
            schema: crate::schemas::schema_of(&view.name).to_owned(),
            comment: crate::comments::relation_comment(reader, &view.name)?,
            columns: view
                .columns
                .into_iter()
                .map(|column| ViewColumnSchema {
                    name: column.name,
                    sql_type: column.sql_type,
                })
                .collect(),
        });
    }
    Ok(views)
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
    /// The column's comment, from `COMMENT ON COLUMN` or MySQL's `COMMENT` column option.
    pub comment: Option<String>,
}

impl TableSchema {
    /// The `CREATE TABLE` statement that makes this table in `dialect`, ended by `;`:
    /// SQLite's type names and `INTEGER PRIMARY KEY` in SQLite's, Oracle's types and upper-case
    /// names in Oracle's, otherwise PostgreSQL's.
    pub fn create_statement_in(&self, dialect: crate::Dialect) -> String {
        match dialect {
            crate::Dialect::Sqlite => format!("{};", crate::session::sqlite_create_text(self)),
            crate::Dialect::Oracle => format!("{};", crate::session::oracle_create_text(self)),
            _ => self.create_statement(),
        }
    }

    /// The table's name as SQL spells it, schema-qualified when the schema is not `public`.
    pub fn qualified_name(&self) -> String {
        let base = self
            .name
            .strip_prefix(&format!("{}.", self.schema))
            .filter(|_| self.schema != crate::schemas::DEFAULT_SCHEMA)
            .unwrap_or(&self.name);
        if self.schema == crate::schemas::DEFAULT_SCHEMA {
            quoted(base)
        } else {
            format!("{}.{}", quoted(&self.schema), quoted(base))
        }
    }

    /// The `CREATE INDEX` statements that make this table's secondary indexes in `dialect`, each
    /// ended by `;` and a line break; empty without indexes.
    pub fn index_statements_in(&self, dialect: crate::Dialect) -> String {
        let mut out = String::new();
        for index in &self.indexes {
            let statement = index.create_statement_in(&self.name, dialect);
            let statement = if self.schema != crate::schemas::DEFAULT_SCHEMA
                && dialect == crate::Dialect::PostgreSql
            {
                statement.replacen(
                    &format!(" ON {} (", quoted(&self.name)),
                    &format!(" ON {} (", self.qualified_name()),
                    1,
                )
            } else {
                statement
            };
            out.push_str(&statement);
            out.push('\n');
        }
        out
    }

    /// The `COMMENT ON` statements that give this table and its columns their comments, each
    /// ended by `;` and a line break; empty without comments.
    pub fn comment_statements(&self) -> String {
        let literal = |text: &str| format!("'{}'", text.replace('\'', "''"));
        let mut out = String::new();
        if let Some(comment) = &self.comment {
            out.push_str(&format!(
                "COMMENT ON TABLE {} IS {};\n",
                self.qualified_name(),
                literal(comment)
            ));
        }
        for column in &self.columns {
            if let Some(comment) = &column.comment {
                out.push_str(&format!(
                    "COMMENT ON COLUMN {}.{} IS {};\n",
                    self.qualified_name(),
                    quoted(&column.name),
                    literal(comment)
                ));
            }
        }
        out
    }

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
            self.qualified_name(),
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
        if crate::encoding::is_view_record(value) {
            return Ok(true);
        }
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
    if let Some(error) = failure {
        return Err(error);
    }
    for table in &mut tables {
        let internal = if table.schema == crate::schemas::DEFAULT_SCHEMA {
            table.name.clone()
        } else {
            crate::schemas::qualified(&table.schema, &table.name)
        };
        table.comment = crate::comments::relation_comment(reader, &internal)?;
        for (column, comment) in crate::comments::column_comments(reader, &internal)? {
            if let Some(found) = table.columns.iter_mut().find(|known| known.name == column) {
                found.comment = Some(comment);
            }
        }
        table.name = crate::schemas::display(&internal);
    }
    Ok(tables)
}

fn schema_of(definition: &TableDefinition) -> TableSchema {
    TableSchema {
        name: crate::schemas::base_name(&definition.name).to_owned(),
        schema: crate::schemas::schema_of(&definition.name).to_owned(),
        comment: None,
        columns: definition
            .columns
            .iter()
            .map(|column| ColumnSchema {
                name: column.name.clone(),
                sql_type: column.sql_type,
                not_null: column.not_null,
                default: column.default.clone(),
                auto_increment: column.auto_increment,
                comment: None,
            })
            .collect(),
        primary_key: definition
            .primary_key
            .iter()
            .filter_map(|&index| definition.columns.get(index))
            .map(|column| column.name.clone())
            .collect(),
        indexes: definition
            .indexes
            .iter()
            .map(|index| IndexSchema {
                name: index.name.clone(),
                unique: index.unique,
                columns: index
                    .columns
                    .iter()
                    .filter_map(|&column| definition.columns.get(column))
                    .map(|column| column.name.clone())
                    .collect(),
            })
            .collect(),
    }
}
