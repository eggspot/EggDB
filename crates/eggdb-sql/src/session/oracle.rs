use super::sqlite::Scratch;
use super::{OutputColumn, QueryResult, Session};
use crate::ast::{
    ColumnDefinition, CreateTable, Expr, ExprKind, Insert, InsertSource, InsertValue, Literal,
    Name, OnDuplicate, Select, TableReference,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{self, TableCache};
use crate::lexer::swap_uniform_case;
use crate::schema::{ColumnSchema, TableSchema, read_tables};
use crate::store::KeyValueStore;
use crate::types::SqlType;

const TABLE_COLUMNS: [(&str, SqlType); 3] = [
    ("owner", SqlType::Text),
    ("table_name", SqlType::Text),
    ("tablespace_name", SqlType::Text),
];

const COLUMN_COLUMNS: [(&str, SqlType); 9] = [
    ("owner", SqlType::Text),
    ("table_name", SqlType::Text),
    ("column_name", SqlType::Text),
    ("data_type", SqlType::Text),
    ("data_length", SqlType::BigInt),
    ("data_precision", SqlType::BigInt),
    ("data_scale", SqlType::BigInt),
    ("nullable", SqlType::Text),
    ("column_id", SqlType::BigInt),
];

const INDEX_COLUMNS: [(&str, SqlType); 6] = [
    ("owner", SqlType::Text),
    ("index_name", SqlType::Text),
    ("index_type", SqlType::Text),
    ("table_owner", SqlType::Text),
    ("table_name", SqlType::Text),
    ("uniqueness", SqlType::Text),
];

const VIEW_COLUMNS: [(&str, SqlType); 4] = [
    ("owner", SqlType::Text),
    ("view_name", SqlType::Text),
    ("text_length", SqlType::BigInt),
    ("text", SqlType::Text),
];

const TABLE_COMMENT_COLUMNS: [(&str, SqlType); 4] = [
    ("owner", SqlType::Text),
    ("table_name", SqlType::Text),
    ("table_type", SqlType::Text),
    ("comments", SqlType::Text),
];

const COLUMN_COMMENT_COLUMNS: [(&str, SqlType); 4] = [
    ("owner", SqlType::Text),
    ("table_name", SqlType::Text),
    ("column_name", SqlType::Text),
    ("comments", SqlType::Text),
];

const INDEX_COLUMN_COLUMNS: [(&str, SqlType); 6] = [
    ("index_owner", SqlType::Text),
    ("index_name", SqlType::Text),
    ("table_name", SqlType::Text),
    ("column_name", SqlType::Text),
    ("column_position", SqlType::BigInt),
    ("descend", SqlType::Text),
];

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

fn oracle_type_name(sql_type: SqlType) -> &'static str {
    match sql_type {
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt => "NUMBER",
        SqlType::Real => "BINARY_FLOAT",
        SqlType::Double => "BINARY_DOUBLE",
        SqlType::Boolean => "BOOLEAN",
        SqlType::Text | SqlType::Varchar(None) => "CLOB",
        SqlType::Varchar(Some(_)) => "VARCHAR2",
        SqlType::Numeric(_) => "NUMBER",
        SqlType::Date => "DATE",
        SqlType::Timestamp(Some(0)) => "DATE",
        SqlType::Timestamp(_) => "TIMESTAMP",
        SqlType::TimestampTz(_) => "TIMESTAMP WITH TIME ZONE",
        SqlType::Json | SqlType::Jsonb => "JSON",
        SqlType::Bpchar(_) => "CHAR",
        SqlType::Bytea => "BLOB",
        SqlType::Time(_) | SqlType::TimeTz(_) | SqlType::Interval { .. } | SqlType::Uuid => {
            "VARCHAR2"
        }
    }
}

fn precision_of(sql_type: SqlType) -> Option<i64> {
    match sql_type {
        SqlType::SmallInt => Some(5),
        SqlType::Integer => Some(10),
        SqlType::BigInt => Some(19),
        _ => None,
    }
}

fn length_of(sql_type: SqlType) -> i64 {
    match sql_type {
        SqlType::Varchar(Some(length)) => i64::from(length),
        SqlType::Real => 4,
        SqlType::Double => 8,
        SqlType::Text | SqlType::Varchar(None) => 4000,
        SqlType::Boolean => 1,
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt | SqlType::Numeric(_) => 22,
        SqlType::Date | SqlType::Timestamp(Some(0)) => 7,
        SqlType::Timestamp(_) => 11,
        SqlType::TimestampTz(_) => 13,
        SqlType::Json | SqlType::Jsonb => 8200,
        SqlType::Bpchar(length) => length.map_or(2000, i64::from),
        SqlType::Bytea => 4000,
        SqlType::Time(_) | SqlType::TimeTz(_) | SqlType::Interval { .. } | SqlType::Uuid => 64,
    }
}

fn type_text(sql_type: SqlType) -> String {
    match (sql_type, precision_of(sql_type)) {
        (_, Some(precision)) => format!("NUMBER({precision})"),
        (SqlType::Varchar(Some(length)), _) => format!("VARCHAR2({length} CHAR)"),
        (SqlType::Bpchar(Some(length)), _) => format!("CHAR({length} CHAR)"),
        (SqlType::Numeric(Some(modifier)), _) => {
            format!("NUMBER({},{})", modifier.precision, modifier.scale)
        }
        (SqlType::Timestamp(Some(precision)) | SqlType::TimestampTz(Some(precision)), _)
            if precision != 0 =>
        {
            format!("{}({precision})", oracle_type_name(sql_type))
        }
        _ => oracle_type_name(sql_type).to_owned(),
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

fn value_literal(literal: Literal) -> InsertValue {
    InsertValue::Expr(Expr {
        kind: ExprKind::Literal(literal),
        position: 0,
    })
}

fn text_literal(text: String) -> InsertValue {
    value_literal(Literal::String(text))
}

fn number_literal(number: Option<i64>) -> InsertValue {
    value_literal(number.map_or(Literal::Null, Literal::Integer))
}

fn name(text: &str) -> Name {
    Name {
        text: text.to_owned(),
        position: 0,
    }
}

type ViewComments = (Option<String>, Vec<(String, String)>);

fn optional_text(text: Option<&String>) -> InsertValue {
    match text {
        Some(text) => text_literal(text.clone()),
        None => InsertValue::Expr(Expr {
            kind: ExprKind::Literal(Literal::Null),
            position: 0,
        }),
    }
}

fn view_rows(views: &[crate::encoding::ViewDefinition], owner: &str) -> Vec<Vec<InsertValue>> {
    views
        .iter()
        .map(|view| {
            vec![
                text_literal(owner.to_owned()),
                text_literal(swap_uniform_case(&view.name)),
                number_literal(Some(view.query.len() as i64)),
                text_literal(view.query.clone()),
            ]
        })
        .collect()
}

fn table_comment_rows(
    tables: &[TableSchema],
    views: &[crate::encoding::ViewDefinition],
    comments: &[ViewComments],
    owner: &str,
) -> Vec<Vec<InsertValue>> {
    let mut rows: Vec<Vec<InsertValue>> = tables
        .iter()
        .map(|table| {
            vec![
                text_literal(owner.to_owned()),
                text_literal(swap_uniform_case(&table.name)),
                text_literal("TABLE".to_owned()),
                optional_text(table.comment.as_ref()),
            ]
        })
        .collect();
    for (view, (comment, _)) in views.iter().zip(comments) {
        rows.push(vec![
            text_literal(owner.to_owned()),
            text_literal(swap_uniform_case(&view.name)),
            text_literal("VIEW".to_owned()),
            optional_text(comment.as_ref()),
        ]);
    }
    rows
}

fn column_comment_rows(
    tables: &[TableSchema],
    views: &[crate::encoding::ViewDefinition],
    comments: &[ViewComments],
    owner: &str,
) -> Vec<Vec<InsertValue>> {
    let mut rows = Vec::new();
    for table in tables {
        for column in &table.columns {
            rows.push(vec![
                text_literal(owner.to_owned()),
                text_literal(swap_uniform_case(&table.name)),
                text_literal(swap_uniform_case(&column.name)),
                optional_text(column.comment.as_ref()),
            ]);
        }
    }
    for (view, (_, columns)) in views.iter().zip(comments) {
        for column in &view.columns {
            let comment = columns
                .iter()
                .find(|(name, _)| *name == column.name)
                .map(|(_, text)| text);
            rows.push(vec![
                text_literal(owner.to_owned()),
                text_literal(swap_uniform_case(&view.name)),
                text_literal(swap_uniform_case(&column.name)),
                optional_text(comment),
            ]);
        }
    }
    rows
}

fn table_rows(tables: &[TableSchema], owner: &str) -> Vec<Vec<InsertValue>> {
    tables
        .iter()
        .map(|table| {
            vec![
                text_literal(owner.to_owned()),
                text_literal(swap_uniform_case(&table.name)),
                text_literal("USERS".to_owned()),
            ]
        })
        .collect()
}

fn column_rows(tables: &[TableSchema], owner: &str) -> Vec<Vec<InsertValue>> {
    let mut rows = Vec::new();
    for table in tables {
        for (index, column) in table.columns.iter().enumerate() {
            let nullable = !column.not_null && !table.primary_key.contains(&column.name);
            rows.push(vec![
                text_literal(owner.to_owned()),
                text_literal(swap_uniform_case(&table.name)),
                text_literal(swap_uniform_case(&column.name)),
                text_literal(oracle_type_name(column.sql_type).to_owned()),
                number_literal(Some(length_of(column.sql_type))),
                number_literal(precision_of(column.sql_type)),
                number_literal(precision_of(column.sql_type).map(|_| 0)),
                text_literal(if nullable { "Y" } else { "N" }.to_owned()),
                number_literal(Some(index as i64 + 1)),
            ]);
        }
    }
    rows
}

fn index_rows(tables: &[TableSchema], owner: &str) -> Vec<Vec<InsertValue>> {
    let mut rows = Vec::new();
    for table in tables {
        for index in &table.indexes {
            rows.push(vec![
                text_literal(owner.to_owned()),
                text_literal(swap_uniform_case(&index.name)),
                text_literal("NORMAL".to_owned()),
                text_literal(owner.to_owned()),
                text_literal(swap_uniform_case(&table.name)),
                text_literal(if index.unique { "UNIQUE" } else { "NONUNIQUE" }.to_owned()),
            ]);
        }
    }
    rows
}

fn index_column_rows(tables: &[TableSchema], owner: &str) -> Vec<Vec<InsertValue>> {
    let mut rows = Vec::new();
    for table in tables {
        for index in &table.indexes {
            for (position, column) in index.columns.iter().enumerate() {
                rows.push(vec![
                    text_literal(owner.to_owned()),
                    text_literal(swap_uniform_case(&index.name)),
                    text_literal(swap_uniform_case(&table.name)),
                    text_literal(swap_uniform_case(column)),
                    number_literal(Some(position as i64 + 1)),
                    text_literal("ASC".to_owned()),
                ]);
            }
        }
    }
    rows
}

impl<'s, S: KeyValueStore + 's> Session<'s, S> {
    pub(super) fn oracle_user(&self) -> String {
        self.settings
            .get("session_authorization")
            .unwrap_or_default()
            .to_uppercase()
    }

    pub(super) fn select_dictionary(
        &mut self,
        select: &Select,
        columns_only: bool,
    ) -> SqlResult<QueryResult> {
        let Some(from) = &select.from else {
            return Err(SqlError::new(
                SqlState::INTERNAL_ERROR,
                "a dictionary query without FROM",
            ));
        };
        if !from.joins.is_empty() {
            return Err(SqlError::not_supported(
                "joining a data dictionary view is not supported yet",
            ));
        }
        let view = from.first.name.text.clone();
        let (tables, views, view_comments) = self.read(|reader, _| {
            let tables: Vec<TableSchema> = read_tables(reader)?
                .into_iter()
                .filter(|table| table.schema == crate::schemas::DEFAULT_SCHEMA)
                .collect();
            let views: Vec<crate::encoding::ViewDefinition> = crate::views::every_view(reader)?
                .into_iter()
                .filter(|view| !crate::schemas::is_qualified(&view.name))
                .collect();
            let mut comments = Vec::with_capacity(views.len());
            for view in &views {
                comments.push((
                    crate::comments::relation_comment(reader, &view.name)?,
                    crate::comments::column_comments(reader, &view.name)?,
                ));
            }
            Ok((tables, views, comments))
        })?;
        let owner = self.oracle_user();
        let (columns, mut rows): (&[(&str, SqlType)], _) = if view.ends_with("_views") {
            (&VIEW_COLUMNS, view_rows(&views, &owner))
        } else if view.ends_with("_tab_comments") {
            (
                &TABLE_COMMENT_COLUMNS,
                table_comment_rows(&tables, &views, &view_comments, &owner),
            )
        } else if view.ends_with("_col_comments") {
            (
                &COLUMN_COMMENT_COLUMNS,
                column_comment_rows(&tables, &views, &view_comments, &owner),
            )
        } else if view.ends_with("_ind_columns") {
            (&INDEX_COLUMN_COLUMNS, index_column_rows(&tables, &owner))
        } else if view.ends_with("_indexes") {
            (&INDEX_COLUMNS, index_rows(&tables, &owner))
        } else if view.contains("_col") {
            (&COLUMN_COLUMNS, column_rows(&tables, &owner))
        } else {
            (&TABLE_COLUMNS, table_rows(&tables, &owner))
        };
        let columns = if view.starts_with("user_") {
            for row in &mut rows {
                row.remove(0);
            }
            &columns[1..]
        } else {
            columns
        };
        let mut scratch = Scratch::default();
        let mut cache = TableCache::default();
        let create = CreateTable {
            comment: None,
            name: name(&view),
            if_not_exists: false,
            columns: columns
                .iter()
                .map(|(column, sql_type)| ColumnDefinition {
                    name: name(column),
                    sql_type: *sql_type,
                    not_null: false,
                    primary_key: false,
                    default: None,
                    auto_increment: false,
                    unique: None,
                    constraints: Vec::new(),
                    comment: None,
                })
                .collect(),
            primary_key: None,
            primary_key_name: None,
            indexes: Vec::new(),
            constraints: Vec::new(),
        };
        execute::create_table(
            &mut scratch,
            &mut cache,
            &create,
            crate::Dialect::PostgreSql,
        )?;
        if !rows.is_empty() {
            let insert = Insert {
                table: TableReference::named(name(&view), None),
                columns: None,
                source: InsertSource::Values(rows),
                returning: None,
                on_duplicate: OnDuplicate::Fail,
                rollback_on_conflict: false,
            };
            execute::insert(&mut scratch, &mut cache, &insert)?;
        }
        execute::select(&scratch, &mut cache, select, columns_only)
    }
}
