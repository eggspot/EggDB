//! Runs GraphQL requests over the schema's tables in Hasura's style (D875): a list field, a
//! `<table>_by_pk` field and a `<table>_aggregate` field per table on the query root, and
//! `insert_<table>`, `insert_<table>_one`, `update_<table>`, `update_<table>_by_pk`,
//! `delete_<table>` and `delete_<table>_by_pk` on the mutation root, with `where`,
//! `order_by`, `limit`, `offset` and `distinct_on`, and relationships named after the tables
//! a foreign key joins, and nested inserts through those relationships (`{data: ...}`). Each
//! root field becomes one SQL statement that builds its JSON with `json_build_object` and
//! `json_agg`, run as the signed-in user; a nested insert becomes a few, in the same
//! transaction. Hasura is a trademark of
//! its owners; EggDB is compatible with its query conventions.

use crate::graphql::{self, Directive, Field, Fragment, GqlValue, OperationKind, Selection};
use crate::live::Polled;
use crate::request::{identifier, literal};
use crate::{Endpoint, json, json_string, value_json};
use eggdb_http::{Reply, Request, Response};
use eggdb_sql::json::{Json, Node, parse_document};
use eggdb_sql::{KeyValueStore, QueryResult, Value};
use std::collections::HashMap;

const MAX_FRAGMENT_DEPTH: usize = 32;
const INSERTED_ROWS: &str = "eggdb_graphql_inserted";
const NEW_ROW: &str = "eggdb_graphql_new";

pub(crate) const AGGREGATE_FUNCTIONS: [&str; 10] = [
    "sum",
    "avg",
    "max",
    "min",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "var_pop",
    "var_samp",
    "variance",
];

#[derive(Debug, Clone)]
pub(crate) struct ForeignKey {
    pub(crate) child: String,
    pub(crate) child_columns: Vec<String>,
    pub(crate) parent: String,
    pub(crate) parent_columns: Vec<String>,
}

enum Relationship {
    Object(ForeignKey),
    Array(ForeignKey),
}

fn errors_json(message: &str, code: &str) -> String {
    format!(
        "{{\"errors\":[{{\"message\":{},\"extensions\":{{\"code\":{},\"path\":\"$\"}}}}]}}",
        json_string(message),
        json_string(code)
    )
}

fn graphql_errors(message: &str, code: &str) -> Response {
    json(200, errors_json(message, code))
}

/// A request read and checked, with what its operations run against.
struct Prepared {
    document: graphql::Document,
    operation: usize,
    variables: HashMap<String, GqlValue>,
    schema: String,
    tables: Vec<String>,
    foreign_keys: Vec<ForeignKey>,
}

impl Prepared {
    fn context(&self) -> Context<'_> {
        Context {
            variables: self.variables.clone(),
            fragments: &self.document.fragments,
            schema: self.schema.clone(),
            tables: self.tables.clone(),
            foreign_keys: self.foreign_keys.clone(),
            aliases: 0,
        }
    }
}

fn request_error(message: &str) -> Response {
    json(
        400,
        format!(
            "{{\"errors\":[{{\"message\":{},\"extensions\":{{\"code\":\"invalid-json\",\"path\":\"$\"}}}}]}}",
            json_string(message)
        ),
    )
}

fn from_json(node: &Node) -> GqlValue {
    match &node.value {
        Json::Null => GqlValue::Null,
        Json::Boolean(flag) => GqlValue::Boolean(*flag),
        Json::Number(text) if text.contains(['.', 'e', 'E']) => GqlValue::Float(text.clone()),
        Json::Number(text) => GqlValue::Int(text.clone()),
        Json::String(text) => GqlValue::String(text.clone()),
        Json::Array(items) => GqlValue::List(items.iter().map(from_json).collect()),
        Json::Object(fields) => GqlValue::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), from_json(value)))
                .collect(),
        ),
    }
}

fn json_text(value: &GqlValue) -> String {
    match value {
        GqlValue::Null | GqlValue::Variable(_) => "null".to_owned(),
        GqlValue::Boolean(flag) => flag.to_string(),
        GqlValue::Int(text) | GqlValue::Float(text) => text.clone(),
        GqlValue::String(text) | GqlValue::Enum(text) => json_string(text),
        GqlValue::List(items) => {
            let items: Vec<String> = items.iter().map(json_text).collect();
            format!("[{}]", items.join(","))
        }
        GqlValue::Object(fields) => {
            let fields: Vec<String> = fields
                .iter()
                .map(|(key, value)| format!("{}:{}", json_string(key), json_text(value)))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
    }
}

fn is_number(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E'))
}

fn sql_literal(value: &GqlValue) -> Result<String, String> {
    Ok(match value {
        GqlValue::Null | GqlValue::Variable(_) => "NULL".to_owned(),
        GqlValue::Boolean(flag) => if *flag { "TRUE" } else { "FALSE" }.to_owned(),
        GqlValue::Int(text) | GqlValue::Float(text) if is_number(text) => text.clone(),
        GqlValue::Int(text) | GqlValue::Float(text) => return Err(format!("{text} is not a number")),
        GqlValue::String(text) | GqlValue::Enum(text) => literal(text),
        GqlValue::List(_) | GqlValue::Object(_) => format!("{}::jsonb", literal(&json_text(value))),
    })
}

fn column(alias: &str, name: &str) -> Result<String, String> {
    identifier(name)
        .map(|quoted| format!("{alias}.{quoted}"))
        .ok_or_else(|| format!("field '{name}' is not a column name"))
}

fn whole_number(value: Option<&GqlValue>, argument: &str) -> Result<Option<u64>, String> {
    match value {
        None | Some(GqlValue::Null) => Ok(None),
        Some(GqlValue::Int(text)) => text
            .parse::<u64>()
            .map(Some)
            .map_err(|_| format!("expected a non-negative Int for '{argument}'")),
        Some(_) => Err(format!("expected an Int for '{argument}'")),
    }
}

fn first_text(results: &[QueryResult]) -> Option<String> {
    let value = results
        .iter()
        .rev()
        .find(|result| result.columns.is_some())?
        .rows
        .first()?
        .first()?;
    Some(match value {
        Value::Null => "null".to_owned(),
        Value::Json(text) | Value::Jsonb(text) | Value::Text(text) => text.clone(),
        other => value_json(other),
    })
}

fn row_texts(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .filter_map(|row| row.first())
        .map(|value| match value {
            Value::Json(text) | Value::Jsonb(text) | Value::Text(text) => text.clone(),
            other => value_json(other),
        })
        .collect()
}

pub(crate) struct Context<'d> {
    variables: HashMap<String, GqlValue>,
    fragments: &'d [Fragment],
    schema: String,
    pub(crate) tables: Vec<String>,
    pub(crate) foreign_keys: Vec<ForeignKey>,
    aliases: usize,
}

impl Context<'_> {
    fn alias(&mut self) -> String {
        self.aliases += 1;
        format!("\"_{}\"", self.aliases)
    }

    fn target(&self, table: &str) -> Result<String, String> {
        identifier(table)
            .map(|quoted| format!("{}.{quoted}", self.schema))
            .ok_or_else(|| format!("'{table}' is not a table name"))
    }

    fn resolve(&self, value: &GqlValue) -> GqlValue {
        match value {
            GqlValue::Variable(name) => self.variables.get(name).cloned().unwrap_or(GqlValue::Null),
            GqlValue::List(items) => GqlValue::List(items.iter().map(|item| self.resolve(item)).collect()),
            GqlValue::Object(fields) => GqlValue::Object(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), self.resolve(value)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    pub(crate) fn argument(&self, field: &Field, name: &str) -> Option<GqlValue> {
        field.argument(name).map(|value| self.resolve(value))
    }

    fn included(&self, directives: &[Directive]) -> Result<bool, String> {
        for directive in directives {
            let wanted = match directive.name.as_str() {
                "include" => true,
                "skip" => false,
                _ => continue,
            };
            let condition = directive
                .arguments
                .iter()
                .find(|(name, _)| name == "if")
                .map(|(_, value)| self.resolve(value));
            match condition {
                Some(GqlValue::Boolean(flag)) if flag != wanted => return Ok(false),
                Some(GqlValue::Boolean(_)) => {}
                _ => return Err(format!("@{} needs a Boolean 'if' argument", directive.name)),
            }
        }
        Ok(true)
    }

    pub(crate) fn fields(&self, selections: &[Selection], depth: usize) -> Result<Vec<Field>, String> {
        if depth > MAX_FRAGMENT_DEPTH {
            return Err("fragments nest too deeply or spread themselves".to_owned());
        }
        let mut fields = Vec::new();
        for selection in selections {
            match selection {
                Selection::Field(field) => {
                    if self.included(&field.directives)? {
                        fields.push(field.clone());
                    }
                }
                Selection::FragmentSpread { name, directives } => {
                    if !self.included(directives)? {
                        continue;
                    }
                    let fragment = self
                        .fragments
                        .iter()
                        .find(|fragment| fragment.name == *name)
                        .ok_or_else(|| format!("fragment '{name}' is not defined"))?;
                    fields.extend(self.fields(&fragment.selections, depth + 1)?);
                }
                Selection::InlineFragment {
                    directives,
                    selections,
                    ..
                } => {
                    if self.included(directives)? {
                        fields.extend(self.fields(selections, depth + 1)?);
                    }
                }
            }
        }
        Ok(fields)
    }

    fn relationship(&self, table: &str, name: &str) -> Option<Relationship> {
        let object = self.foreign_keys.iter().find(|key| {
            key.child == table
                && (key.parent == name
                    || matches!(key.child_columns.as_slice(), [only] if only.strip_suffix("_id") == Some(name)))
        });
        if let Some(key) = object {
            return Some(Relationship::Object(key.clone()));
        }
        self.foreign_keys
            .iter()
            .find(|key| key.parent == table && key.child == name)
            .map(|key| Relationship::Array(key.clone()))
    }

    fn join(left: &str, left_columns: &[String], right: &str, right_columns: &[String]) -> Result<String, String> {
        let mut parts = Vec::new();
        for (left_column, right_column) in left_columns.iter().zip(right_columns) {
            parts.push(format!("{} = {}", column(left, left_column)?, column(right, right_column)?));
        }
        Ok(if parts.is_empty() { "true".to_owned() } else { parts.join(" AND ") })
    }

    fn object_sql(&mut self, table: &str, alias: &str, fields: &[Field]) -> Result<String, String> {
        let mut parts = Vec::new();
        for field in fields {
            let value = if field.name == "__typename" {
                literal(table)
            } else if field.selections.is_empty() {
                column(alias, &field.name)?
            } else {
                self.related_sql(table, alias, field)?
            };
            parts.push(format!("{}, {value}", literal(field.key())));
        }
        Ok(format!("json_build_object({})", parts.join(", ")))
    }

    fn related_sql(&mut self, table: &str, alias: &str, field: &Field) -> Result<String, String> {
        let fields = self.fields(&field.selections, 0)?;
        let key = match self.relationship(table, &field.name) {
            Some(Relationship::Object(key)) => {
                let inner = self.alias();
                let object = self.object_sql(&key.parent, &inner, &fields)?;
                let condition = Self::join(&inner, &key.parent_columns, alias, &key.child_columns)?;
                return Ok(format!(
                    "(SELECT {object} FROM {} AS {inner} WHERE {condition})",
                    self.target(&key.parent)?
                ));
            }
            Some(Relationship::Array(key)) => key,
            None => {
                return Err(format!("field '{}' not found in type: '{table}'", field.name));
            }
        };
        let inner = self.alias();
        let outer = self.alias();
        let mut condition = Self::join(&inner, &key.child_columns, alias, &key.parent_columns)?;
        if let Some(filter) = self.argument(field, "where") {
            condition = format!("{condition} AND ({})", self.where_sql(&key.child, &inner, &filter)?);
        }
        let order = self.argument(field, "order_by");
        let inner_order = self.order_sql(&inner, order.as_ref())?;
        let outer_order = self.order_sql(&outer, order.as_ref())?;
        let paging = self.paging_sql(field)?;
        let object = self.object_sql(&key.child, &outer, &fields)?;
        Ok(format!(
            "(SELECT coalesce(json_agg({object}{outer_order}), '[]'::json) FROM (SELECT * FROM {} AS {inner} WHERE {condition}{inner_order}{paging}) AS {outer})",
            self.target(&key.child)?
        ))
    }

    fn where_sql(&mut self, table: &str, alias: &str, filter: &GqlValue) -> Result<String, String> {
        let fields = match filter {
            GqlValue::Null => return Ok("true".to_owned()),
            GqlValue::Object(fields) => fields,
            _ => return Err("expected an object for 'where'".to_owned()),
        };
        let mut parts = Vec::new();
        for (key, value) in fields {
            let part = match key.as_str() {
                "_and" | "_or" => {
                    let items = match value {
                        GqlValue::List(items) => items.clone(),
                        GqlValue::Null => Vec::new(),
                        other => vec![other.clone()],
                    };
                    let mut joined = Vec::new();
                    for item in &items {
                        joined.push(format!("({})", self.where_sql(table, alias, item)?));
                    }
                    match (joined.is_empty(), key.as_str()) {
                        (true, "_and") => "true".to_owned(),
                        (true, _) => "false".to_owned(),
                        (false, "_and") => format!("({})", joined.join(" AND ")),
                        (false, _) => format!("({})", joined.join(" OR ")),
                    }
                }
                "_not" => format!("NOT ({})", self.where_sql(table, alias, value)?),
                name => match self.relationship(table, name) {
                    Some(relationship) => {
                        let (related, condition_columns) = match relationship {
                            Relationship::Object(key) => (key.parent, (key.parent_columns, key.child_columns)),
                            Relationship::Array(key) => (key.child, (key.child_columns, key.parent_columns)),
                        };
                        let inner = self.alias();
                        let joined = Self::join(&inner, &condition_columns.0, alias, &condition_columns.1)?;
                        let nested = self.where_sql(&related, &inner, value)?;
                        format!(
                            "EXISTS (SELECT 1 FROM {} AS {inner} WHERE {joined} AND ({nested}))",
                            self.target(&related)?
                        )
                    }
                    None => Self::comparisons(&column(alias, name)?, value)?,
                },
            };
            parts.push(part);
        }
        Ok(if parts.is_empty() { "true".to_owned() } else { parts.join(" AND ") })
    }

    fn comparisons(column: &str, operators: &GqlValue) -> Result<String, String> {
        let GqlValue::Object(operators) = operators else {
            return Err(format!("expected comparison operators for {column}"));
        };
        let mut parts = Vec::new();
        for (operator, value) in operators {
            let binary = |symbol: &str| sql_literal(value).map(|right| format!("{column} {symbol} {right}"));
            let part = match operator.as_str() {
                "_eq" => binary("=")?,
                "_neq" => binary("<>")?,
                "_gt" => binary(">")?,
                "_gte" => binary(">=")?,
                "_lt" => binary("<")?,
                "_lte" => binary("<=")?,
                "_like" => binary("LIKE")?,
                "_nlike" => binary("NOT LIKE")?,
                "_ilike" => binary("ILIKE")?,
                "_nilike" => binary("NOT ILIKE")?,
                "_similar" => binary("SIMILAR TO")?,
                "_nsimilar" => binary("NOT SIMILAR TO")?,
                "_regex" => binary("~")?,
                "_nregex" => binary("!~")?,
                "_iregex" => binary("~*")?,
                "_niregex" => binary("!~*")?,
                "_contains" => binary("@>")?,
                "_contained_in" => binary("<@")?,
                "_in" | "_nin" => {
                    let items = match value {
                        GqlValue::List(items) => items.clone(),
                        GqlValue::Null => Vec::new(),
                        other => vec![other.clone()],
                    };
                    let negated = operator == "_nin";
                    if items.is_empty() {
                        if negated { "true" } else { "false" }.to_owned()
                    } else {
                        let listed: Result<Vec<String>, String> = items.iter().map(sql_literal).collect();
                        format!(
                            "{column} {}IN ({})",
                            if negated { "NOT " } else { "" },
                            listed?.join(", ")
                        )
                    }
                }
                "_is_null" => match value {
                    GqlValue::Boolean(true) => format!("{column} IS NULL"),
                    GqlValue::Boolean(false) => format!("{column} IS NOT NULL"),
                    _ => return Err("expected a Boolean for '_is_null'".to_owned()),
                },
                other => return Err(format!("field '{other}' not found in a comparison expression")),
            };
            parts.push(part);
        }
        Ok(if parts.is_empty() { "true".to_owned() } else { parts.join(" AND ") })
    }

    fn order_sql(&self, alias: &str, order: Option<&GqlValue>) -> Result<String, String> {
        let items = match order {
            None | Some(GqlValue::Null) => return Ok(String::new()),
            Some(GqlValue::List(items)) => items.clone(),
            Some(other) => vec![other.clone()],
        };
        let mut keys = Vec::new();
        for item in &items {
            let GqlValue::Object(fields) = item else {
                return Err("expected an object for 'order_by'".to_owned());
            };
            for (name, direction) in fields {
                let GqlValue::Enum(direction) = direction else {
                    return Err(format!("expected an order_by direction for '{name}'"));
                };
                let shown = match direction.as_str() {
                    "asc" => "ASC",
                    "asc_nulls_first" => "ASC NULLS FIRST",
                    "asc_nulls_last" => "ASC NULLS LAST",
                    "desc" => "DESC",
                    "desc_nulls_first" => "DESC NULLS FIRST",
                    "desc_nulls_last" => "DESC NULLS LAST",
                    other => return Err(format!("unexpected order_by direction '{other}'")),
                };
                keys.push(format!("{} {shown}", column(alias, name)?));
            }
        }
        Ok(if keys.is_empty() {
            String::new()
        } else {
            format!(" ORDER BY {}", keys.join(", "))
        })
    }

    fn paging_sql(&self, field: &Field) -> Result<String, String> {
        let mut paging = String::new();
        if let Some(limit) = whole_number(self.argument(field, "limit").as_ref(), "limit")? {
            paging.push_str(&format!(" LIMIT {limit}"));
        }
        if let Some(offset) = whole_number(self.argument(field, "offset").as_ref(), "offset")? {
            paging.push_str(&format!(" OFFSET {offset}"));
        }
        Ok(paging)
    }

    fn distinct_sql(&self, alias: &str, field: &Field) -> Result<String, String> {
        let names = match self.argument(field, "distinct_on") {
            None | Some(GqlValue::Null) => return Ok(String::new()),
            Some(GqlValue::List(items)) => items,
            Some(other) => vec![other],
        };
        let mut columns = Vec::new();
        for name in &names {
            let GqlValue::Enum(name) = name else {
                return Err("expected column names for 'distinct_on'".to_owned());
            };
            columns.push(column(alias, name)?);
        }
        Ok(format!("DISTINCT ON ({}) ", columns.join(", ")))
    }

    fn rows_from(&mut self, table: &str, alias: &str, field: &Field, columns: &str) -> Result<String, String> {
        let distinct = self.distinct_sql(alias, field)?;
        let filter = match self.argument(field, "where") {
            Some(filter) => self.where_sql(table, alias, &filter)?,
            None => "true".to_owned(),
        };
        let order = self.order_sql(alias, self.argument(field, "order_by").as_ref())?;
        let paging = self.paging_sql(field)?;
        Ok(format!(
            "SELECT {distinct}{columns} FROM {} AS {alias} WHERE {filter}{order}{paging}",
            self.target(table)?
        ))
    }

    fn list_sql(&mut self, table: &str, field: &Field) -> Result<String, String> {
        let alias = self.alias();
        let fields = self.fields(&field.selections, 0)?;
        let object = self.object_sql(table, &alias, &fields)?;
        self.rows_from(table, &alias, field, &object)
    }

    fn by_key_sql(&mut self, table: &str, field: &Field, key: &GqlValue) -> Result<String, String> {
        let alias = self.alias();
        let fields = self.fields(&field.selections, 0)?;
        let object = self.object_sql(table, &alias, &fields)?;
        let condition = Self::key_condition(&alias, key)?;
        Ok(format!(
            "SELECT {object} FROM {} AS {alias} WHERE {condition}",
            self.target(table)?
        ))
    }

    fn key_condition(alias: &str, key: &GqlValue) -> Result<String, String> {
        let GqlValue::Object(columns) = key else {
            return Err("expected the primary key's columns".to_owned());
        };
        if columns.is_empty() {
            return Err("expected the primary key's columns".to_owned());
        }
        let mut parts = Vec::new();
        for (name, value) in columns {
            parts.push(format!("{} = {}", column(alias, name)?, sql_literal(value)?));
        }
        Ok(parts.join(" AND "))
    }

    fn aggregate_sql(&mut self, table: &str, field: &Field) -> Result<String, String> {
        let base_alias = self.alias();
        let base = self.rows_from(table, &base_alias, field, "*")?;
        let mut parts = Vec::new();
        for part in self.fields(&field.selections, 0)? {
            let value = match part.name.as_str() {
                "__typename" => literal(&format!("{table}_aggregate")),
                "aggregate" => {
                    let rows = self.alias();
                    let object = self.aggregate_object(table, &rows, &part)?;
                    format!("(SELECT {object} FROM ({base}) AS {rows})")
                }
                "nodes" => {
                    let rows = self.alias();
                    let nodes = self.fields(&part.selections, 0)?;
                    let object = self.object_sql(table, &rows, &nodes)?;
                    let order = self.order_sql(&rows, self.argument(field, "order_by").as_ref())?;
                    format!("(SELECT coalesce(json_agg({object}{order}), '[]'::json) FROM ({base}) AS {rows})")
                }
                other => return Err(format!("field '{other}' not found in type: '{table}_aggregate'")),
            };
            parts.push(format!("{}, {value}", literal(part.key())));
        }
        Ok(format!("SELECT json_build_object({})", parts.join(", ")))
    }

    fn aggregate_object(&mut self, table: &str, rows: &str, part: &Field) -> Result<String, String> {
        let mut values = Vec::new();
        for item in self.fields(&part.selections, 0)? {
            let value = match item.name.as_str() {
                "__typename" => literal(&format!("{table}_aggregate_fields")),
                "count" => {
                    let distinct = matches!(self.argument(&item, "distinct"), Some(GqlValue::Boolean(true)));
                    let columns = match self.argument(&item, "columns") {
                        None | Some(GqlValue::Null) => Vec::new(),
                        Some(GqlValue::List(items)) => items,
                        Some(other) => vec![other],
                    };
                    let mut counted = Vec::new();
                    for name in &columns {
                        let GqlValue::Enum(name) = name else {
                            return Err("expected column names for 'columns'".to_owned());
                        };
                        counted.push(column(rows, name)?);
                    }
                    match (counted.as_slice(), distinct) {
                        ([], _) => "count(*)".to_owned(),
                        ([only], true) => format!("count(DISTINCT {only})"),
                        ([only], false) => format!("count({only})"),
                        (many, true) => format!("count(DISTINCT ({}))", many.join(", ")),
                        (many, false) => format!("count(CASE WHEN {} THEN 1 END)", many.iter().map(|one| format!("{one} IS NOT NULL")).collect::<Vec<_>>().join(" AND ")),
                    }
                }
                function if AGGREGATE_FUNCTIONS.contains(&function) => {
                    let mut columns = Vec::new();
                    for target in self.fields(&item.selections, 0)? {
                        let computed = if target.name == "__typename" {
                            literal(&format!("{table}_{function}_fields"))
                        } else {
                            format!("{function}({})", column(rows, &target.name)?)
                        };
                        columns.push(format!("{}, {computed}", literal(target.key())));
                    }
                    format!("json_build_object({})", columns.join(", "))
                }
                other => return Err(format!("field '{other}' not found in type: '{table}_aggregate_fields'")),
            };
            values.push(format!("{}, {value}", literal(item.key())));
        }
        Ok(format!("json_build_object({})", values.join(", ")))
    }

    fn query_sql(&mut self, field: &Field) -> Result<(String, bool), String> {
        let name = field.name.as_str();
        if self.tables.iter().any(|table| table == name) {
            return Ok((self.list_sql(name, field)?, true));
        }
        if let Some(table) = name.strip_suffix("_by_pk").filter(|table| self.tables.iter().any(|known| known == table)) {
            let key = GqlValue::Object(
                field
                    .arguments
                    .iter()
                    .map(|(name, value)| (name.clone(), self.resolve(value)))
                    .collect(),
            );
            return Ok((self.by_key_sql(table, field, &key)?, false));
        }
        if let Some(table) = name.strip_suffix("_aggregate").filter(|table| self.tables.iter().any(|known| known == table)) {
            return Ok((self.aggregate_sql(table, field)?, false));
        }
        Err(format!("field '{name}' not found in type: 'query_root'"))
    }

    fn returning_object(&mut self, table: &str, alias: &str, field: &Field, single: bool) -> Result<String, String> {
        if single {
            let fields = self.fields(&field.selections, 0)?;
            return self.object_sql(table, alias, &fields);
        }
        let mut returning = Vec::new();
        for part in self.fields(&field.selections, 0)? {
            if part.name == "returning" {
                returning.extend(self.fields(&part.selections, 0)?);
            }
        }
        self.object_sql(table, alias, &returning)
    }

    fn assignments(alias: &str, set: Option<&GqlValue>, increments: Option<&GqlValue>) -> Result<Vec<String>, String> {
        let mut assignments = Vec::new();
        if let Some(GqlValue::Object(columns)) = set {
            for (name, value) in columns {
                let quoted = identifier(name).ok_or_else(|| format!("field '{name}' is not a column name"))?;
                assignments.push(format!("{quoted} = {}", sql_literal(value)?));
            }
        }
        if let Some(GqlValue::Object(columns)) = increments {
            for (name, value) in columns {
                let quoted = identifier(name).ok_or_else(|| format!("field '{name}' is not a column name"))?;
                assignments.push(format!("{quoted} = {alias}.{quoted} + {}", sql_literal(value)?));
            }
        }
        if assignments.is_empty() {
            return Err("at least any one of _set, _inc is expected".to_owned());
        }
        Ok(assignments)
    }

    fn nests(&self, table: &str, object: &GqlValue) -> bool {
        let GqlValue::Object(fields) = object else {
            return false;
        };
        fields
            .iter()
            .any(|(name, value)| value.field("data").is_some() && self.relationship(table, name).is_some())
    }

    fn values_insert(&self, table: &str, object: &GqlValue, links: &[(String, String)]) -> Result<String, String> {
        let GqlValue::Object(fields) = object else {
            return Err("expected objects to insert".to_owned());
        };
        if self.nests(table, object) {
            return Err("nested inserts go one relationship deep".to_owned());
        }
        let mut columns = Vec::new();
        let mut values = Vec::new();
        for (name, value) in fields {
            columns.push(identifier(name).ok_or_else(|| format!("field '{name}' is not a column name"))?);
            values.push(sql_literal(value)?);
        }
        for (column, value) in links {
            columns.push(column.clone());
            values.push(value.clone());
        }
        let head = if columns.is_empty() {
            "DEFAULT VALUES".to_owned()
        } else {
            format!("({}) VALUES ({})", columns.join(", "), values.join(", "))
        };
        Ok(format!("INSERT INTO {} {head} RETURNING *", self.target(table)?))
    }

    fn links(key: &ForeignKey, from: &[String], to: &[String], source: &str) -> Result<Vec<(String, String)>, String> {
        from.iter()
            .zip(to)
            .map(|(target, origin)| {
                let target = identifier(target).ok_or_else(|| format!("'{target}' is not a column name"))?;
                let origin = identifier(origin).ok_or_else(|| format!("'{origin}' is not a column name"))?;
                Ok((target, format!("(SELECT {origin} FROM {source})")))
            })
            .collect::<Result<Vec<_>, String>>()
            .and_then(|links| if links.is_empty() { Err(format!("the relationship to '{}' has no columns", key.parent)) } else { Ok(links) })
    }

    fn nested_object_sql(&mut self, table: &str, object: &GqlValue) -> Result<String, String> {
        let GqlValue::Object(fields) = object else {
            return Err("expected objects to insert".to_owned());
        };
        let mut steps: Vec<String> = Vec::new();
        let mut own = Vec::new();
        let mut links = Vec::new();
        let mut children = Vec::new();
        for (name, value) in fields {
            match (value.field("data"), self.relationship(table, name)) {
                (Some(data), Some(Relationship::Object(key))) => {
                    let step = format!("eggdb_graphql_parent_{}", steps.len());
                    steps.push(format!("{step} AS ({})", self.values_insert(&key.parent, data, &[])?));
                    links.extend(Self::links(&key, &key.child_columns, &key.parent_columns, &step)?);
                }
                (Some(data), Some(Relationship::Array(key))) => children.push((key, data.clone())),
                _ => own.push((name.clone(), value.clone())),
            }
        }
        steps.push(format!("{NEW_ROW} AS ({})", self.values_insert(table, &GqlValue::Object(own), &links)?));
        for (key, data) in children {
            let items = match data {
                GqlValue::List(items) => items,
                single => vec![single],
            };
            let child_links = Self::links(&key, &key.child_columns, &key.parent_columns, NEW_ROW)?;
            for item in &items {
                let step = format!("eggdb_graphql_child_{}", steps.len());
                steps.push(format!("{step} AS ({})", self.values_insert(&key.child, item, &child_links)?));
            }
        }
        Ok(format!("WITH {} INSERT INTO {INSERTED_ROWS} SELECT * FROM {NEW_ROW}", steps.join(", ")))
    }

    fn nested_insert_sql(&mut self, table: &str, field: &Field, single: bool, objects: &[GqlValue]) -> Result<String, String> {
        if self.argument(field, "on_conflict").is_some() {
            return Err("on_conflict does not go with nested inserts yet".to_owned());
        }
        let alias = identifier(table).ok_or_else(|| format!("'{table}' is not a table name"))?;
        let mut statements = vec![format!("CREATE TEMPORARY TABLE {INSERTED_ROWS} (LIKE {})", self.target(table)?)];
        for object in objects {
            statements.push(self.nested_object_sql(table, object)?);
        }
        let returning = self.returning_object(table, &alias, field, single)?;
        statements.push(format!("SELECT {returning} FROM {INSERTED_ROWS} AS {alias}"));
        statements.push(format!("DROP TABLE {INSERTED_ROWS}"));
        Ok(statements.join("; "))
    }

    fn insert_sql(&mut self, table: &str, field: &Field, single: bool) -> Result<String, String> {
        let objects = match self.argument(field, if single { "object" } else { "objects" }) {
            Some(GqlValue::List(items)) => items,
            Some(object @ GqlValue::Object(_)) => vec![object],
            _ => return Err(format!("missing required field '{}'", if single { "object" } else { "objects" })),
        };
        if objects.iter().any(|object| self.nests(table, object)) {
            return self.nested_insert_sql(table, field, single, &objects);
        }
        let alias = identifier(table).ok_or_else(|| format!("'{table}' is not a table name"))?;
        let mut columns: Vec<String> = Vec::new();
        for object in &objects {
            let GqlValue::Object(fields) = object else {
                return Err("expected objects to insert".to_owned());
            };
            for (name, _) in fields {
                if !columns.contains(name) {
                    columns.push(name.clone());
                }
            }
        }
        let quoted: Result<Vec<String>, String> = columns
            .iter()
            .map(|name| identifier(name).ok_or_else(|| format!("field '{name}' is not a column name")))
            .collect();
        let mut rows = Vec::new();
        for object in &objects {
            let mut values = Vec::new();
            for name in &columns {
                values.push(match object.field(name) {
                    Some(value) => sql_literal(value)?,
                    None => "DEFAULT".to_owned(),
                });
            }
            rows.push(format!("({})", values.join(", ")));
        }
        let head = if columns.is_empty() {
            "DEFAULT VALUES".to_owned()
        } else {
            format!("({}) VALUES {}", quoted?.join(", "), rows.join(", "))
        };
        let conflict = match self.argument(field, "on_conflict") {
            Some(GqlValue::Object(_)) => self.conflict_sql(table, &alias, field)?,
            _ => String::new(),
        };
        let returning = self.returning_object(table, &alias, field, single)?;
        Ok(format!(
            "INSERT INTO {} {head}{conflict} RETURNING {returning}",
            self.target(table)?
        ))
    }

    fn conflict_sql(&mut self, table: &str, alias: &str, field: &Field) -> Result<String, String> {
        let conflict = self.argument(field, "on_conflict").unwrap_or(GqlValue::Null);
        let Some(GqlValue::Enum(constraint)) = conflict.field("constraint").cloned() else {
            return Err("on_conflict needs a constraint".to_owned());
        };
        let constraint = identifier(&constraint).ok_or_else(|| format!("'{constraint}' is not a constraint name"))?;
        let updated = match conflict.field("update_columns") {
            Some(GqlValue::List(items)) => items.clone(),
            Some(GqlValue::Enum(name)) => vec![GqlValue::Enum(name.clone())],
            _ => Vec::new(),
        };
        if updated.is_empty() {
            return Ok(format!(" ON CONFLICT ON CONSTRAINT {constraint} DO NOTHING"));
        }
        let mut assignments = Vec::new();
        for name in &updated {
            let GqlValue::Enum(name) = name else {
                return Err("expected column names for 'update_columns'".to_owned());
            };
            let quoted = identifier(name).ok_or_else(|| format!("field '{name}' is not a column name"))?;
            assignments.push(format!("{quoted} = EXCLUDED.{quoted}"));
        }
        let condition = match conflict.field("where") {
            Some(filter) if *filter != GqlValue::Null => {
                let filter = filter.clone();
                format!(" WHERE {}", self.where_sql(table, alias, &filter)?)
            }
            _ => String::new(),
        };
        Ok(format!(
            " ON CONFLICT ON CONSTRAINT {constraint} DO UPDATE SET {}{condition}",
            assignments.join(", ")
        ))
    }

    fn update_sql(&mut self, table: &str, field: &Field, by_key: bool) -> Result<String, String> {
        let alias = identifier(table).ok_or_else(|| format!("'{table}' is not a table name"))?;
        let condition = if by_key {
            let key = self.argument(field, "pk_columns").unwrap_or(GqlValue::Null);
            Self::key_condition(&alias, &key)?
        } else {
            let filter = self
                .argument(field, "where")
                .ok_or_else(|| "missing required field 'where'".to_owned())?;
            self.where_sql(table, &alias, &filter)?
        };
        let set = self.argument(field, "_set");
        let increments = self.argument(field, "_inc");
        let assignments = Self::assignments(&alias, set.as_ref(), increments.as_ref())?;
        let returning = self.returning_object(table, &alias, field, by_key)?;
        Ok(format!(
            "UPDATE {} SET {} WHERE {condition} RETURNING {returning}",
            self.target(table)?,
            assignments.join(", ")
        ))
    }

    fn delete_sql(&mut self, table: &str, field: &Field, by_key: bool) -> Result<String, String> {
        let alias = identifier(table).ok_or_else(|| format!("'{table}' is not a table name"))?;
        let condition = if by_key {
            let key = GqlValue::Object(
                field
                    .arguments
                    .iter()
                    .map(|(name, value)| (name.clone(), self.resolve(value)))
                    .collect(),
            );
            Self::key_condition(&alias, &key)?
        } else {
            let filter = self
                .argument(field, "where")
                .ok_or_else(|| "missing required field 'where'".to_owned())?;
            self.where_sql(table, &alias, &filter)?
        };
        let returning = self.returning_object(table, &alias, field, by_key)?;
        Ok(format!(
            "DELETE FROM {} WHERE {condition} RETURNING {returning}",
            self.target(table)?
        ))
    }

    fn known_table<'n>(&self, name: &'n str, prefix: &str, suffix: &str) -> Option<&'n str> {
        let table = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
        self.tables.iter().any(|known| known == table).then_some(table)
    }

    fn mutation_sql(&mut self, field: &Field) -> Result<(String, bool), String> {
        let name = field.name.as_str();
        if let Some(table) = self.known_table(name, "insert_", "_one") {
            return Ok((self.insert_sql(table, field, true)?, true));
        }
        if let Some(table) = self.known_table(name, "insert_", "") {
            return Ok((self.insert_sql(table, field, false)?, false));
        }
        if let Some(table) = self.known_table(name, "update_", "_by_pk") {
            return Ok((self.update_sql(table, field, true)?, true));
        }
        if let Some(table) = self.known_table(name, "update_", "") {
            return Ok((self.update_sql(table, field, false)?, false));
        }
        if let Some(table) = self.known_table(name, "delete_", "_by_pk") {
            return Ok((self.delete_sql(table, field, true)?, true));
        }
        if let Some(table) = self.known_table(name, "delete_", "") {
            return Ok((self.delete_sql(table, field, false)?, false));
        }
        Err(format!("field '{name}' not found in type: 'mutation_root'"))
    }

    fn mutation_response(&self, field: &Field, rows: &[String]) -> Result<String, String> {
        let mut parts = Vec::new();
        for part in self.fields(&field.selections, 0)? {
            let value = match part.name.as_str() {
                "affected_rows" => rows.len().to_string(),
                "returning" => format!("[{}]", rows.join(",")),
                "__typename" => json_string(&format!("{}_mutation_response", field.name)),
                other => return Err(format!("field '{other}' not found in type: 'mutation_response'")),
            };
            parts.push(format!("{}:{value}", json_string(part.key())));
        }
        Ok(format!("{{{}}}", parts.join(",")))
    }
}

impl<'s, S: KeyValueStore> Endpoint<'s, S> {
    fn schema_tables(&self) -> Result<Vec<String>, eggdb_sql::SqlError> {
        let results = self.run(&format!(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = {} ORDER BY table_name",
            literal(&self.schema)
        ))?;
        Ok(results
            .iter()
            .filter(|result| result.columns.is_some())
            .flat_map(|result| result.rows.iter())
            .filter_map(|row| row.first().and_then(Value::to_text))
            .collect())
    }

    fn schema_foreign_keys(&self) -> Result<Vec<ForeignKey>, eggdb_sql::SqlError> {
        let schema = literal(&self.schema);
        let results = self.run(&format!(
            "SELECT fk.constraint_name, fk.table_name, fk.column_name, pk.table_name, pk.column_name FROM information_schema.referential_constraints rc JOIN information_schema.key_column_usage fk ON fk.constraint_schema = rc.constraint_schema AND fk.constraint_name = rc.constraint_name JOIN information_schema.key_column_usage pk ON pk.constraint_schema = rc.unique_constraint_schema AND pk.constraint_name = rc.unique_constraint_name AND pk.ordinal_position = fk.position_in_unique_constraint WHERE fk.table_schema = {schema} AND pk.table_schema = {schema} ORDER BY fk.table_name, fk.constraint_name, fk.ordinal_position"
        ))?;
        let mut keys: Vec<(String, ForeignKey)> = Vec::new();
        for row in results
            .iter()
            .filter(|result| result.columns.is_some())
            .flat_map(|result| result.rows.iter())
        {
            let text = |at: usize| row.get(at).and_then(Value::to_text).unwrap_or_default();
            let (constraint, child) = (text(0), text(1));
            match keys.last_mut() {
                Some((name, key)) if *name == constraint && key.child == child => {
                    key.child_columns.push(text(2));
                    key.parent_columns.push(text(4));
                }
                _ => keys.push((
                    constraint,
                    ForeignKey {
                        child,
                        child_columns: vec![text(2)],
                        parent: text(3),
                        parent_columns: vec![text(4)],
                    },
                )),
            }
        }
        Ok(keys.into_iter().map(|(_, key)| key).collect())
    }

    /// Answers `POST /graphql` (or `/v1/graphql`): a JSON body with `query`, and optionally
    /// `variables` and `operationName`, or the document itself as `application/graphql`. A
    /// subscription asked for with `Accept: text/event-stream` runs as a live query and sends
    /// graphql-sse's `next` events, then `complete`.
    pub(crate) fn graphql(&self, request: &Request) -> Reply<'s> {
        let prepared = match self.prepare_graphql(request) {
            Ok(prepared) => prepared,
            Err(response) => return response.into(),
        };
        let Some(operation) = prepared.document.operations.get(prepared.operation) else {
            return graphql_errors("the operation is missing", "validation-failed").into();
        };
        if operation.kind == OperationKind::Subscription {
            if !crate::live::wanted(request) {
                return graphql_errors(
                    "subscriptions are served as Server-Sent Events: send Accept: text/event-stream",
                    "not-supported",
                )
                .into();
            }
            let endpoint = self.copy();
            return crate::live::live_reply(
                crate::live::LiveOptions::of(request),
                "next",
                "next",
                Some("complete"),
                move || {
                    let Some(operation) = prepared.document.operations.get(prepared.operation) else {
                        return Polled::Failed(errors_json("the operation is missing", "validation-failed"));
                    };
                    let mut context = prepared.context();
                    match endpoint.run_query(&mut context, &operation.selections) {
                        Ok(data) => Polled::Result(format!("{{\"data\":{data}}}")),
                        Err((message, code)) => Polled::Failed(errors_json(&message, &code)),
                    }
                },
            );
        }
        let mut context = prepared.context();
        let outcome = match operation.kind {
            OperationKind::Mutation => self.run_mutation(&mut context, &operation.selections),
            _ => self.run_query(&mut context, &operation.selections),
        };
        match outcome {
            Ok(data) => json(200, format!("{{\"data\":{data}}}")).into(),
            Err((message, code)) => graphql_errors(&message, &code).into(),
        }
    }

    fn prepare_graphql(&self, request: &Request) -> Result<Prepared, Response> {
        if request.method != "POST" {
            return Err(request_error("use POST with a JSON body holding the query"));
        }
        let Ok(body) = std::str::from_utf8(&request.body) else {
            return Err(request_error("the body is not UTF-8"));
        };
        let content_type = request.header("content-type").unwrap_or_default().to_ascii_lowercase();
        let (query, variables, operation_name) = if content_type.starts_with("application/graphql") {
            (body.to_owned(), HashMap::new(), None)
        } else {
            let Ok(document) = parse_document(body) else {
                return Err(request_error("the body is not a JSON object"));
            };
            let Json::Object(members) = &document.value else {
                return Err(request_error("the body is not a JSON object"));
            };
            let member = |name: &str| members.iter().find(|(key, _)| key == name).map(|(_, value)| value);
            let Some(Node {
                value: Json::String(query),
                ..
            }) = member("query")
            else {
                return Err(request_error("the body has no 'query' string"));
            };
            let mut variables = HashMap::new();
            if let Some(Node {
                value: Json::Object(given),
                ..
            }) = member("variables")
            {
                for (name, value) in given {
                    variables.insert(name.clone(), from_json(value));
                }
            }
            let operation_name = match member("operationName") {
                Some(Node {
                    value: Json::String(name),
                    ..
                }) => Some(name.clone()),
                _ => None,
            };
            (query.clone(), variables, operation_name)
        };
        let document = graphql::parse(&query).map_err(|message| graphql_errors(&message, "validation-failed"))?;
        let operation = match &operation_name {
            Some(name) => document
                .operations
                .iter()
                .position(|operation| operation.name.as_deref() == Some(name.as_str())),
            None if document.operations.len() == 1 => Some(0),
            None => None,
        };
        let Some(operation) = operation else {
            return Err(graphql_errors(
                "exactly one operation has to be present in the document when operationName is not specified",
                "validation-failed",
            ));
        };
        let mut resolved = HashMap::new();
        for definition in document.operations.get(operation).map(|found| found.variables.as_slice()).unwrap_or_default() {
            let value = variables
                .get(&definition.name)
                .cloned()
                .or_else(|| definition.default.clone())
                .unwrap_or(GqlValue::Null);
            resolved.insert(definition.name.clone(), value);
        }
        let tables = self
            .schema_tables()
            .map_err(|error| graphql_errors(error.message(), error.state().code()))?;
        let foreign_keys = self
            .schema_foreign_keys()
            .map_err(|error| graphql_errors(error.message(), error.state().code()))?;
        Ok(Prepared {
            document,
            operation,
            variables: resolved,
            schema: identifier(&self.schema).unwrap_or_else(|| "\"public\"".to_owned()),
            tables,
            foreign_keys,
        })
    }

    fn run_query(&self, context: &mut Context<'_>, selections: &[Selection]) -> Result<String, (String, String)> {
        let validation = |message: String| (message, "validation-failed".to_owned());
        let fields = context.fields(selections, 0).map_err(validation)?;
        let mut parts = Vec::new();
        for field in &fields {
            let value = if field.name == "__typename" {
                json_string("query_root")
            } else if field.name == "__schema" || field.name == "__type" {
                let model = self.schema_model(context).map_err(|error| (error.message().to_owned(), error.state().code().to_owned()))?;
                crate::graphql_schema::introspect(context, &model, field).map_err(validation)?
            } else {
                let (sql, list) = context.query_sql(field).map_err(validation)?;
                let results = self
                    .run(&sql)
                    .map_err(|error| (error.message().to_owned(), error.state().code().to_owned()))?;
                if list {
                    let rows = results
                        .iter()
                        .rev()
                        .find(|result| result.columns.is_some())
                        .map(row_texts)
                        .unwrap_or_default();
                    format!("[{}]", rows.join(","))
                } else {
                    first_text(&results).unwrap_or_else(|| "null".to_owned())
                }
            };
            parts.push(format!("{}:{value}", json_string(field.key())));
        }
        Ok(format!("{{{}}}", parts.join(",")))
    }

    fn run_mutation(&self, context: &mut Context<'_>, selections: &[Selection]) -> Result<String, (String, String)> {
        let validation = |message: String| (message, "validation-failed".to_owned());
        let fields = context.fields(selections, 0).map_err(validation)?;
        let mut statements = Vec::new();
        for field in &fields {
            if field.name == "__typename" {
                continue;
            }
            statements.push(context.mutation_sql(field).map_err(validation)?);
        }
        let mut script = String::from("BEGIN;");
        for (sql, _) in &statements {
            script.push(' ');
            script.push_str(sql);
            script.push(';');
        }
        script.push_str(" COMMIT;");
        let results = self
            .run(&script)
            .map_err(|error| (error.message().to_owned(), error.state().code().to_owned()))?;
        let mut returned = results.iter().filter(|result| result.columns.is_some());
        let mut parts = Vec::new();
        let mut planned = statements.iter();
        for field in &fields {
            let value = if field.name == "__typename" {
                json_string("mutation_root")
            } else {
                let single = planned.next().is_some_and(|(_, single)| *single);
                let rows = returned.next().map(row_texts).unwrap_or_default();
                if single {
                    rows.first().cloned().unwrap_or_else(|| "null".to_owned())
                } else {
                    context.mutation_response(field, &rows).map_err(validation)?
                }
            };
            parts.push(format!("{}:{value}", json_string(field.key())));
        }
        Ok(format!("{{{}}}", parts.join(",")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(fragments: &[Fragment]) -> Context<'_> {
        Context {
            variables: HashMap::from([("min".to_owned(), GqlValue::Int("5".to_owned()))]),
            fragments,
            schema: "\"public\"".to_owned(),
            tables: vec!["authors".to_owned(), "books".to_owned()],
            foreign_keys: vec![ForeignKey {
                child: "books".to_owned(),
                child_columns: vec!["author_id".to_owned()],
                parent: "authors".to_owned(),
                parent_columns: vec!["id".to_owned()],
            }],
            aliases: 0,
        }
    }

    fn root_field(text: &str) -> (graphql::Document, Field) {
        let document = graphql::parse(text).unwrap();
        let Some(Selection::Field(field)) = document.operations[0].selections.first().cloned() else {
            panic!("a field");
        };
        (document, field)
    }

    #[test]
    fn a_list_field_becomes_one_select_building_json() {
        let (document, field) = root_field(
            "query($min: Int) { books(where: {pages: {_gte: $min}, _or: [{title: {_ilike: \"%rust%\"}}, {title: {_is_null: true}}]}, order_by: {title: asc}, limit: 2) { id title author { name } } }",
        );
        let mut context = context(&document.fragments);
        let (sql, list) = context.query_sql(&field).unwrap();
        assert!(list);
        assert_eq!(
            sql,
            "SELECT json_build_object('id', \"_1\".\"id\", 'title', \"_1\".\"title\", 'author', (SELECT json_build_object('name', \"_2\".\"name\") FROM \"public\".\"authors\" AS \"_2\" WHERE \"_2\".\"id\" = \"_1\".\"author_id\")) FROM \"public\".\"books\" AS \"_1\" WHERE \"_1\".\"pages\" >= 5 AND ((\"_1\".\"title\" ILIKE '%rust%') OR (\"_1\".\"title\" IS NULL)) ORDER BY \"_1\".\"title\" ASC LIMIT 2"
        );
    }

    #[test]
    fn array_relationships_aggregate_into_json_arrays() {
        let (document, field) = root_field("{ authors { name books(order_by: {id: desc}, limit: 1) { id } } }");
        let mut context = context(&document.fragments);
        let (sql, _) = context.query_sql(&field).unwrap();
        assert!(sql.contains("coalesce(json_agg(json_build_object('id', \"_3\".\"id\") ORDER BY \"_3\".\"id\" DESC), '[]'::json)"), "{sql}");
        assert!(sql.contains("WHERE \"_2\".\"author_id\" = \"_1\".\"id\" ORDER BY \"_2\".\"id\" DESC LIMIT 1"), "{sql}");
    }

    #[test]
    fn mutations_become_returning_statements() {
        let (document, field) = root_field(
            "mutation { insert_books(objects: [{title: \"A\", pages: 10}, {title: \"B\"}], on_conflict: {constraint: books_pkey, update_columns: [title]}) { affected_rows returning { id } } }",
        );
        let mut context = context(&document.fragments);
        let (sql, single) = context.mutation_sql(&field).unwrap();
        assert!(!single);
        assert_eq!(
            sql,
            "INSERT INTO \"public\".\"books\" (\"title\", \"pages\") VALUES ('A', 10), ('B', DEFAULT) ON CONFLICT ON CONSTRAINT \"books_pkey\" DO UPDATE SET \"title\" = EXCLUDED.\"title\" RETURNING json_build_object('id', \"books\".\"id\")"
        );
        let (document, field) = root_field("mutation { update_books_by_pk(pk_columns: {id: 3}, _inc: {pages: 1}) { id } }");
        let mut context = context(&document.fragments);
        let (sql, single) = context.mutation_sql(&field).unwrap();
        assert!(single);
        assert!(sql.starts_with("UPDATE \"public\".\"books\" SET \"pages\" = \"books\".\"pages\" + 1 WHERE \"books\".\"id\" = 3"), "{sql}");
    }

    #[test]
    fn mutation_responses_count_and_return_rows() {
        let (document, field) = root_field("mutation { delete_books(where: {}) { affected_rows returning { id } __typename } }");
        let context = context(&document.fragments);
        assert_eq!(
            context.mutation_response(&field, &["{\"id\":1}".to_owned(), "{\"id\":2}".to_owned()]).unwrap(),
            "{\"affected_rows\":2,\"returning\":[{\"id\":1},{\"id\":2}],\"__typename\":\"delete_books_mutation_response\"}"
        );
    }

    #[test]
    fn hostile_names_and_values_never_reach_the_sql_unquoted() {
        let (document, field) = root_field("{ books(where: {title: {_eq: \"x'; DROP TABLE books; --\"}}) { id } }");
        let mut context = context(&document.fragments);
        let (sql, _) = context.query_sql(&field).unwrap();
        assert!(sql.contains("'x''; DROP TABLE books; --'"), "{sql}");
        let (document, field) = root_field("{ unknown { id } }");
        let mut context = context(&document.fragments);
        assert!(context.query_sql(&field).is_err());
        let (document, field) = root_field("{ books(limit: -1) { id } }");
        let mut context = context(&document.fragments);
        assert!(context.query_sql(&field).is_err());
    }

    #[test]
    fn aggregates_count_and_summarize() {
        let (document, field) = root_field("{ books_aggregate(where: {pages: {_gt: 0}}) { aggregate { count sum { pages } } } }");
        let mut context = context(&document.fragments);
        let (sql, list) = context.query_sql(&field).unwrap();
        assert!(!list);
        assert!(sql.contains("json_build_object('count', count(*), 'sum', json_build_object('pages', sum(\"_2\".\"pages\")))"), "{sql}");
    }
}
