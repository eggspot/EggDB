//! An OData v4 service over the schema's tables (D877), for Excel, Power BI, SAP and the
//! other OData consumers: the service document, `$metadata` in CSDL XML, entity sets with
//! `$filter`, `$select`, `$orderby`, `$top`, `$skip` and `$count`, server-driven paging with
//! `@odata.nextLink`, entities by key, `$count` segments, navigation properties from foreign
//! keys with `$expand`, and inserts, updates and deletes.
//! Every request runs as SQL in the signed-in user's session. OData is a standard of OASIS.

use crate::request::{identifier, literal, percent_decode};
use crate::{Endpoint, json_literal, json_string, value_json};
use eggdb_http::{Request, Response};
use eggdb_sql::json::{Json, parse_document};
use eggdb_sql::{KeyValueStore, QueryResult, Value};

const PAGE_ROWS: u64 = 10_000;
const MAX_FILTER_DEPTH: usize = 64;
const MAX_FILTER_BYTES: usize = 64 << 10;

#[derive(Debug, Clone)]
struct Property {
    name: String,
    data_type: String,
    nullable: bool,
}

#[derive(Debug, Clone)]
struct Navigation {
    name: String,
    target: String,
    many: bool,
    from: Vec<String>,
    to: Vec<String>,
}

#[derive(Debug, Clone)]
struct EntitySet {
    name: String,
    properties: Vec<Property>,
    key: Vec<String>,
    navigations: Vec<Navigation>,
}

impl EntitySet {
    fn has_member(&self, name: &str) -> bool {
        self.properties.iter().any(|property| property.name == name)
            || self.navigations.iter().any(|navigation| navigation.name == name)
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.properties.iter().position(|property| property.name == name)
    }

    fn key_columns(&self) -> Vec<&Property> {
        if self.key.is_empty() {
            return self.properties.iter().collect();
        }
        self.key
            .iter()
            .filter_map(|name| self.properties.iter().find(|property| property.name == *name))
            .collect()
    }
}

/// OData's primitive type for a column's SQL type.
fn edm_type(data_type: &str) -> &'static str {
    match data_type {
        "smallint" => "Edm.Int16",
        "integer" => "Edm.Int32",
        "bigint" => "Edm.Int64",
        "numeric" | "money" => "Edm.Decimal",
        "double precision" => "Edm.Double",
        "real" => "Edm.Single",
        "boolean" => "Edm.Boolean",
        "date" => "Edm.Date",
        "timestamp with time zone" | "timestamp without time zone" => "Edm.DateTimeOffset",
        "time without time zone" | "time with time zone" => "Edm.TimeOfDay",
        "uuid" => "Edm.Guid",
        "bytea" => "Edm.Binary",
        _ => "Edm.String",
    }
}

fn xml_escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn odata_error(status: u16, code: &str, message: &str) -> Response {
    let mut response = crate::json(
        status,
        format!(
            "{{\"error\":{{\"code\":{},\"message\":{}}}}}",
            json_string(code),
            json_string(message)
        ),
    );
    response.headers.push(("OData-Version".to_owned(), "4.0".to_owned()));
    response
}

fn sql_error(error: &eggdb_sql::SqlError) -> Response {
    let status = match error.state().code() {
        "42501" => 403,
        "42P01" | "42703" => 404,
        "23505" => 409,
        _ => 400,
    };
    odata_error(status, error.state().code(), error.message())
}

fn no_content() -> Response {
    Response {
        status: 204,
        content_type: "application/json".to_owned(),
        body: Vec::new(),
        headers: vec![("OData-Version".to_owned(), "4.0".to_owned())],
    }
}

fn odata_json(status: u16, body: String) -> Response {
    let mut response = crate::json(status, body);
    response.content_type = "application/json;odata.metadata=minimal;odata.streaming=true".to_owned();
    response.headers.push(("OData-Version".to_owned(), "4.0".to_owned()));
    response
}

/// A timestamp's text as OData's `DateTimeOffset`: `2026-11-12T10:00:00Z`.
fn date_time_offset(text: &str) -> String {
    let mut iso = text.replacen(' ', "T", 1);
    let zone_at = iso
        .char_indices()
        .skip(11)
        .find(|(_, character)| matches!(character, '+' | '-'))
        .map(|(at, _)| at);
    match zone_at {
        None => iso.push('Z'),
        Some(at) => {
            let zone = iso[at..].to_owned();
            iso.truncate(at);
            match zone.as_str() {
                "+00" | "+00:00" | "-00" => iso.push('Z'),
                _ if zone.len() == 3 => iso.push_str(&format!("{zone}:00")),
                _ => iso.push_str(&zone),
            }
        }
    }
    iso
}

fn odata_value(value: &Value, data_type: &str) -> String {
    if matches!(value, Value::Null) {
        return "null".to_owned();
    }
    match edm_type(data_type) {
        "Edm.DateTimeOffset" => json_string(&date_time_offset(&value.to_text().unwrap_or_default())),
        "Edm.Double" | "Edm.Single" => match value {
            Value::Double(number) if !number.is_finite() => json_string(&value.to_text().unwrap_or_default()),
            Value::Real(number) if !number.is_finite() => json_string(&value.to_text().unwrap_or_default()),
            _ => value_json(value),
        },
        "Edm.String" | "Edm.Guid" | "Edm.Date" | "Edm.TimeOfDay" | "Edm.Binary" => {
            json_string(&value.to_text().unwrap_or_default())
        }
        _ => value_json(value),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum FilterToken {
    Word(String),
    Number(String),
    Text(String),
    Temporal(String),
    Open,
    Close,
    Comma,
    Minus,
    End,
}

fn filter_tokens(text: &str) -> Result<Vec<FilterToken>, String> {
    if text.len() > MAX_FILTER_BYTES {
        return Err("the filter is too long".to_owned());
    }
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        match byte {
            b' ' | b'\t' => at += 1,
            b'(' => {
                tokens.push(FilterToken::Open);
                at += 1;
            }
            b')' => {
                tokens.push(FilterToken::Close);
                at += 1;
            }
            b',' => {
                tokens.push(FilterToken::Comma);
                at += 1;
            }
            b'-' => {
                tokens.push(FilterToken::Minus);
                at += 1;
            }
            b'\'' => {
                let mut value = String::new();
                at += 1;
                loop {
                    let Some(rest) = text.get(at..) else {
                        return Err("unterminated string in $filter".to_owned());
                    };
                    let Some(character) = rest.chars().next() else {
                        return Err("unterminated string in $filter".to_owned());
                    };
                    at += character.len_utf8();
                    if character == '\'' {
                        if bytes.get(at) == Some(&b'\'') {
                            value.push('\'');
                            at += 1;
                            continue;
                        }
                        break;
                    }
                    value.push(character);
                }
                tokens.push(FilterToken::Text(value));
            }
            b'0'..=b'9' => {
                let start = at;
                while bytes.get(at).is_some_and(|next| next.is_ascii_digit() || matches!(next, b'.' | b'e' | b'E')) {
                    at += 1;
                }
                let temporal = (at - start == 4 && bytes.get(at) == Some(&b'-'))
                    || (at - start == 2 && bytes.get(at) == Some(&b':'));
                if temporal {
                    while bytes
                        .get(at)
                        .is_some_and(|next| next.is_ascii_alphanumeric() || matches!(next, b'-' | b':' | b'.' | b'+'))
                    {
                        at += 1;
                    }
                    tokens.push(FilterToken::Temporal(text[start..at].to_owned()));
                } else {
                    tokens.push(FilterToken::Number(text[start..at].to_owned()));
                }
            }
            _ if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80 => {
                let start = at;
                while bytes.get(at).is_some_and(|next| next.is_ascii_alphanumeric() || *next == b'_' || *next >= 0x80) {
                    at += 1;
                }
                tokens.push(FilterToken::Word(text[start..at].to_owned()));
            }
            _ => {
                return Err(format!("unexpected character at offset {at} in $filter"));
            }
        }
    }
    tokens.push(FilterToken::End);
    Ok(tokens)
}

struct FilterParser<'m> {
    tokens: Vec<FilterToken>,
    at: usize,
    depth: usize,
    set: &'m EntitySet,
}

#[derive(Debug, Clone, PartialEq)]
enum Operand {
    Null,
    Sql(String),
}

impl Operand {
    fn sql(self) -> String {
        match self {
            Self::Null => "NULL".to_owned(),
            Self::Sql(sql) => sql,
        }
    }
}

impl FilterParser<'_> {
    fn peek(&self) -> &FilterToken {
        self.tokens.get(self.at).unwrap_or(&FilterToken::End)
    }

    fn advance(&mut self) -> FilterToken {
        let token = self.peek().clone();
        self.at += 1;
        token
    }

    fn is_word(&self, word: &str) -> bool {
        matches!(self.peek(), FilterToken::Word(found) if found.eq_ignore_ascii_case(word))
    }

    fn take_word(&mut self, word: &str) -> bool {
        if self.is_word(word) {
            self.at += 1;
            return true;
        }
        false
    }

    fn expect(&mut self, token: &FilterToken) -> Result<(), String> {
        if self.peek() == token {
            self.at += 1;
            return Ok(());
        }
        Err(format!("expected {token:?} in $filter"))
    }

    fn deeper(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_FILTER_DEPTH {
            return Err("$filter nests too deeply".to_owned());
        }
        Ok(())
    }

    fn whole(&mut self) -> Result<String, String> {
        let expression = self.or_expression()?.sql();
        if *self.peek() != FilterToken::End {
            return Err("unexpected text after the end of $filter".to_owned());
        }
        Ok(expression)
    }

    fn or_expression(&mut self) -> Result<Operand, String> {
        self.deeper()?;
        let mut left = self.and_expression()?;
        while self.take_word("or") {
            let right = self.and_expression()?;
            left = Operand::Sql(format!("({} OR {})", left.sql(), right.sql()));
        }
        self.depth -= 1;
        Ok(left)
    }

    fn and_expression(&mut self) -> Result<Operand, String> {
        let mut left = self.not_expression()?;
        while self.take_word("and") {
            let right = self.not_expression()?;
            left = Operand::Sql(format!("({} AND {})", left.sql(), right.sql()));
        }
        Ok(left)
    }

    fn not_expression(&mut self) -> Result<Operand, String> {
        if self.take_word("not") {
            self.deeper()?;
            let inner = self.not_expression()?;
            self.depth -= 1;
            return Ok(Operand::Sql(format!("(NOT {})", inner.sql())));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Operand, String> {
        let left = self.additive()?;
        let operator = match self.peek() {
            FilterToken::Word(word) => word.to_ascii_lowercase(),
            _ => return Ok(left),
        };
        let symbol = match operator.as_str() {
            "eq" => "=",
            "ne" => "<>",
            "gt" => ">",
            "ge" => ">=",
            "lt" => "<",
            "le" => "<=",
            "in" => {
                self.at += 1;
                self.expect(&FilterToken::Open)?;
                let mut items = Vec::new();
                loop {
                    items.push(self.additive()?.sql());
                    if self.peek() == &FilterToken::Comma {
                        self.at += 1;
                        continue;
                    }
                    self.expect(&FilterToken::Close)?;
                    break;
                }
                return Ok(Operand::Sql(format!("({} IN ({}))", left.sql(), items.join(", "))));
            }
            _ => return Ok(left),
        };
        self.at += 1;
        let right = self.additive()?;
        Ok(Operand::Sql(match (left, right, symbol) {
            (Operand::Null, Operand::Null, "=") => "TRUE".to_owned(),
            (Operand::Null, Operand::Null, _) => "FALSE".to_owned(),
            (Operand::Sql(side), Operand::Null, "=") | (Operand::Null, Operand::Sql(side), "=") => {
                format!("({side} IS NULL)")
            }
            (Operand::Sql(side), Operand::Null, "<>") | (Operand::Null, Operand::Sql(side), "<>") => {
                format!("({side} IS NOT NULL)")
            }
            (left, right, symbol) => format!("({} {symbol} {})", left.sql(), right.sql()),
        }))
    }

    fn additive(&mut self) -> Result<Operand, String> {
        let mut left = self.multiplicative()?;
        loop {
            let symbol = if self.take_word("add") {
                "+"
            } else if self.take_word("sub") {
                "-"
            } else {
                return Ok(left);
            };
            let right = self.multiplicative()?;
            left = Operand::Sql(format!("({} {symbol} {})", left.sql(), right.sql()));
        }
    }

    fn multiplicative(&mut self) -> Result<Operand, String> {
        let mut left = self.unary()?;
        loop {
            let symbol = if self.take_word("mul") {
                "*"
            } else if self.take_word("div") || self.take_word("divby") {
                "/"
            } else if self.take_word("mod") {
                "%"
            } else {
                return Ok(left);
            };
            let right = self.unary()?;
            left = Operand::Sql(format!("({} {symbol} {})", left.sql(), right.sql()));
        }
    }

    fn unary(&mut self) -> Result<Operand, String> {
        if self.peek() == &FilterToken::Minus {
            self.at += 1;
            self.deeper()?;
            let inner = self.unary()?;
            self.depth -= 1;
            return Ok(Operand::Sql(format!("(-{})", inner.sql())));
        }
        self.primary()
    }

    fn arguments(&mut self) -> Result<Vec<String>, String> {
        self.expect(&FilterToken::Open)?;
        let mut arguments = Vec::new();
        if self.peek() == &FilterToken::Close {
            self.at += 1;
            return Ok(arguments);
        }
        loop {
            arguments.push(self.or_expression()?.sql());
            match self.advance() {
                FilterToken::Comma => continue,
                FilterToken::Close => return Ok(arguments),
                _ => return Err("expected ',' or ')' in a $filter function".to_owned()),
            }
        }
    }

    fn function(name: &str, arguments: &[String]) -> Result<String, String> {
        let wanted = |count: usize| {
            if arguments.len() == count {
                Ok(())
            } else {
                Err(format!("{name} takes {count} arguments"))
            }
        };
        let first = || arguments.first().cloned().unwrap_or_default();
        let second = || arguments.get(1).cloned().unwrap_or_default();
        Ok(match name {
            "contains" => {
                wanted(2)?;
                format!("(strpos({}, {}) > 0)", first(), second())
            }
            "startswith" => {
                wanted(2)?;
                format!("(left({}, length({})) = {})", first(), second(), second())
            }
            "endswith" => {
                wanted(2)?;
                format!("(right({}, length({})) = {})", first(), second(), second())
            }
            "length" => {
                wanted(1)?;
                format!("length({})", first())
            }
            "indexof" => {
                wanted(2)?;
                format!("(strpos({}, {}) - 1)", first(), second())
            }
            "substring" => match arguments {
                [text, start] => format!("substr({text}, ({start}) + 1)"),
                [text, start, length] => format!("substr({text}, ({start}) + 1, {length})"),
                _ => return Err("substring takes 2 or 3 arguments".to_owned()),
            },
            "tolower" => {
                wanted(1)?;
                format!("lower({})", first())
            }
            "toupper" => {
                wanted(1)?;
                format!("upper({})", first())
            }
            "trim" => {
                wanted(1)?;
                format!("trim({})", first())
            }
            "concat" => {
                wanted(2)?;
                format!("({} || {})", first(), second())
            }
            "year" | "month" | "day" | "hour" | "minute" => {
                wanted(1)?;
                format!("CAST(extract({name} FROM {}) AS integer)", first())
            }
            "second" => {
                wanted(1)?;
                format!("CAST(floor(extract(second FROM {})) AS integer)", first())
            }
            "date" => {
                wanted(1)?;
                format!("CAST({} AS date)", first())
            }
            "round" | "floor" => {
                wanted(1)?;
                format!("{name}({})", first())
            }
            "ceiling" => {
                wanted(1)?;
                format!("ceil({})", first())
            }
            "now" => {
                wanted(0)?;
                "now()".to_owned()
            }
            other => return Err(format!("the $filter function {other} is not supported")),
        })
    }

    fn primary(&mut self) -> Result<Operand, String> {
        match self.advance() {
            FilterToken::Open => {
                let inner = self.or_expression()?;
                self.expect(&FilterToken::Close)?;
                Ok(Operand::Sql(format!("({})", inner.sql())))
            }
            FilterToken::Number(number) => {
                let number = number.trim_end_matches(['m', 'M', 'd', 'D', 'f', 'F', 'L', 'l']).to_owned();
                if number.parse::<f64>().is_err() {
                    return Err(format!("{number} is not a number"));
                }
                Ok(Operand::Sql(number))
            }
            FilterToken::Text(text) => Ok(Operand::Sql(literal(&text))),
            FilterToken::Temporal(text) => Ok(Operand::Sql(literal(&text))),
            FilterToken::Word(word) => match word.to_ascii_lowercase().as_str() {
                "null" => Ok(Operand::Null),
                "true" => Ok(Operand::Sql("TRUE".to_owned())),
                "false" => Ok(Operand::Sql("FALSE".to_owned())),
                lowered if self.peek() == &FilterToken::Open => {
                    let lowered = lowered.to_owned();
                    self.deeper()?;
                    let arguments = self.arguments()?;
                    self.depth -= 1;
                    Self::function(&lowered, &arguments).map(Operand::Sql)
                }
                _ => {
                    if !self.set.properties.iter().any(|property| property.name == word) {
                        return Err(format!("Could not find a property named '{word}' on type '{}'.", self.set.name));
                    }
                    identifier(&word)
                        .map(Operand::Sql)
                        .ok_or_else(|| format!("'{word}' is not a property name"))
                }
            },
            other => Err(format!("unexpected {other:?} in $filter")),
        }
    }
}

fn filter_sql(set: &EntitySet, text: &str) -> Result<String, String> {
    let mut parser = FilterParser {
        tokens: filter_tokens(text)?,
        at: 0,
        depth: 0,
        set,
    };
    parser.whole()
}

fn order_sql(set: &EntitySet, text: &str) -> Result<String, String> {
    let mut keys = Vec::new();
    for item in text.split(',').map(str::trim).filter(|item| !item.is_empty()) {
        let (expression, direction) = match item.rsplit_once(' ') {
            Some((expression, direction)) if direction.eq_ignore_ascii_case("desc") => (expression.trim(), "DESC"),
            Some((expression, direction)) if direction.eq_ignore_ascii_case("asc") => (expression.trim(), "ASC"),
            _ => (item, "ASC"),
        };
        keys.push(format!("{} {direction}", filter_sql(set, expression)?));
    }
    Ok(if keys.is_empty() { String::new() } else { format!(" ORDER BY {}", keys.join(", ")) })
}

fn select_columns<'s>(set: &'s EntitySet, text: Option<&str>) -> Result<Vec<&'s Property>, String> {
    let Some(text) = text.filter(|text| !text.trim().is_empty() && text.trim() != "*") else {
        return Ok(set.properties.iter().collect());
    };
    let mut chosen = Vec::new();
    for name in text.split(',').map(str::trim) {
        let property = set
            .properties
            .iter()
            .find(|property| property.name == name)
            .ok_or_else(|| format!("Could not find a property named '{name}' on type '{}'.", set.name))?;
        chosen.push(property);
    }
    Ok(chosen)
}

/// A key segment's literal: `1`, `'text'`, or a key property's `name=value` list.
fn key_condition(set: &EntitySet, text: &str) -> Result<String, String> {
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    let columns = set.key_columns();
    let mut conditions = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let (property, value) = match part.split_once('=') {
            Some((name, value)) if !name.trim().starts_with('\'') => {
                let name = name.trim();
                let property = columns
                    .iter()
                    .find(|property| property.name == name)
                    .ok_or_else(|| format!("'{name}' is not a key property of '{}'", set.name))?;
                (*property, value)
            }
            _ => {
                if parts.len() != 1 || columns.len() != 1 {
                    return Err(format!("the key of '{}' names its properties", set.name));
                }
                let property = columns.get(index).ok_or_else(|| "too many key values".to_owned())?;
                (*property, *part)
            }
        };
        let column = identifier(&property.name).ok_or_else(|| format!("'{}' is not a property name", property.name))?;
        let mut parser = FilterParser {
            tokens: filter_tokens(value)?,
            at: 0,
            depth: 0,
            set,
        };
        let value = parser.unary()?.sql();
        if *parser.peek() != FilterToken::End {
            return Err(format!("'{part}' is not a key value"));
        }
        conditions.push(format!("{column} = {value}"));
    }
    if conditions.len() != columns.len() {
        return Err(format!("the key of '{}' has {} properties", set.name, columns.len()));
    }
    Ok(conditions.join(" AND "))
}

fn query_options(query: Option<&str>) -> Vec<(String, String)> {
    query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(name), percent_decode(value))
        })
        .collect()
}

fn option<'o>(options: &'o [(String, String)], name: &str) -> Option<&'o str> {
    options
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn whole_number(options: &[(String, String)], name: &str) -> Result<Option<u64>, String> {
    option(options, name)
        .map(|text| text.trim().parse::<u64>().map_err(|_| format!("{name} must be a non-negative integer")))
        .transpose()
}

fn entity_json(context: &str, properties: &[&Property], row: &[Value]) -> String {
    entity_with(context, properties, row, &[])
}

fn entity_with(context: &str, properties: &[&Property], row: &[Value], expanded: &[String]) -> String {
    let mut members = Vec::new();
    if !context.is_empty() {
        members.push(format!("\"@odata.context\":{}", json_string(context)));
    }
    for (property, value) in properties.iter().zip(row) {
        members.push(format!("{}:{}", json_string(&property.name), odata_value(value, &property.data_type)));
    }
    members.extend(expanded.iter().cloned());
    format!("{{{}}}", members.join(","))
}

fn projected(set: &EntitySet, columns: &[&Property], row: &[Value]) -> Vec<Value> {
    columns
        .iter()
        .map(|property| set.position(&property.name).and_then(|at| row.get(at)).cloned().unwrap_or(Value::Null))
        .collect()
}

fn expand_names<'s>(set: &'s EntitySet, text: Option<&str>) -> Result<Vec<&'s Navigation>, String> {
    let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(Vec::new());
    };
    if text.contains('(') {
        return Err("options inside $expand are not supported yet".to_owned());
    }
    if text == "*" {
        return Ok(set.navigations.iter().collect());
    }
    text.split(',')
        .map(str::trim)
        .map(|name| {
            set.navigations
                .iter()
                .find(|navigation| navigation.name == name)
                .ok_or_else(|| format!("Could not find a navigation property named '{name}' on type '{}'.", set.name))
        })
        .collect()
}

fn add_navigations(sets: &mut [EntitySet], child: &str, child_columns: &[String], parent: &str, parent_columns: &[String]) {
    let single_name = match child_columns {
        [only] => only.strip_suffix("_id").filter(|name| !name.is_empty()).unwrap_or(parent).to_owned(),
        _ => parent.to_owned(),
    };
    if let Some(set) = sets.iter_mut().find(|set| set.name == child)
        && !set.has_member(&single_name)
    {
        set.navigations.push(Navigation {
            name: single_name,
            target: parent.to_owned(),
            many: false,
            from: child_columns.to_vec(),
            to: parent_columns.to_vec(),
        });
    }
    if let Some(set) = sets.iter_mut().find(|set| set.name == parent)
        && !set.has_member(child)
    {
        set.navigations.push(Navigation {
            name: child.to_owned(),
            target: child.to_owned(),
            many: true,
            from: parent_columns.to_vec(),
            to: child_columns.to_vec(),
        });
    }
}

fn rows_of(results: &[QueryResult]) -> Vec<Vec<Value>> {
    results
        .iter()
        .rev()
        .find(|result| result.columns.is_some())
        .map(|result| result.rows.clone())
        .unwrap_or_default()
}

impl<S: KeyValueStore> Endpoint<'_, S> {
    fn odata_model(&self) -> Result<Vec<EntitySet>, eggdb_sql::SqlError> {
        let schema = literal(&self.schema);
        let mut sets: Vec<EntitySet> = Vec::new();
        let columns = self.run(&format!(
            "SELECT c.table_name, c.column_name, c.data_type, c.is_nullable FROM information_schema.columns c JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name WHERE c.table_schema = {schema} ORDER BY c.table_name, c.ordinal_position"
        ))?;
        for row in columns
            .iter()
            .filter(|result| result.columns.is_some())
            .flat_map(|result| result.rows.iter())
        {
            let text = |at: usize| row.get(at).and_then(Value::to_text).unwrap_or_default();
            let property = Property {
                name: text(1),
                data_type: text(2),
                nullable: text(3) != "NO",
            };
            match sets.last_mut() {
                Some(set) if set.name == text(0) => set.properties.push(property),
                _ => sets.push(EntitySet {
                    name: text(0),
                    properties: vec![property],
                    key: Vec::new(),
                    navigations: Vec::new(),
                }),
            }
        }
        let keys = self.run(&format!(
            "SELECT kcu.table_name, kcu.column_name FROM information_schema.table_constraints tc JOIN information_schema.key_column_usage kcu ON kcu.constraint_schema = tc.constraint_schema AND kcu.constraint_name = tc.constraint_name AND kcu.table_name = tc.table_name WHERE tc.table_schema = {schema} AND tc.constraint_type = 'PRIMARY KEY' ORDER BY kcu.table_name, kcu.ordinal_position"
        ))?;
        for row in keys
            .iter()
            .filter(|result| result.columns.is_some())
            .flat_map(|result| result.rows.iter())
        {
            let text = |at: usize| row.get(at).and_then(Value::to_text).unwrap_or_default();
            if let Some(set) = sets.iter_mut().find(|set| set.name == text(0)) {
                set.key.push(text(1));
            }
        }
        let foreign = self.run(&format!(
            "SELECT fk.constraint_name, fk.table_name, fk.column_name, pk.table_name, pk.column_name FROM information_schema.referential_constraints rc JOIN information_schema.key_column_usage fk ON fk.constraint_schema = rc.constraint_schema AND fk.constraint_name = rc.constraint_name JOIN information_schema.key_column_usage pk ON pk.constraint_schema = rc.unique_constraint_schema AND pk.constraint_name = rc.unique_constraint_name AND pk.ordinal_position = fk.position_in_unique_constraint WHERE fk.table_schema = {schema} AND pk.table_schema = {schema} ORDER BY fk.table_name, fk.constraint_name, fk.ordinal_position"
        ))?;
        let mut keys: Vec<(String, String, Vec<String>, String, Vec<String>)> = Vec::new();
        for row in foreign
            .iter()
            .filter(|result| result.columns.is_some())
            .flat_map(|result| result.rows.iter())
        {
            let text = |at: usize| row.get(at).and_then(Value::to_text).unwrap_or_default();
            match keys.last_mut() {
                Some((constraint, child, child_columns, _, parent_columns)) if *constraint == text(0) && *child == text(1) => {
                    child_columns.push(text(2));
                    parent_columns.push(text(4));
                }
                _ => keys.push((text(0), text(1), vec![text(2)], text(3), vec![text(4)])),
            }
        }
        for (_, child, child_columns, parent, parent_columns) in &keys {
            add_navigations(&mut sets, child, child_columns, parent, parent_columns);
        }
        Ok(sets)
    }

    fn expansions(&self, set: &EntitySet, sets: &[EntitySet], navigations: &[&Navigation], rows: &[Vec<Value>]) -> Result<Vec<Vec<String>>, eggdb_sql::SqlError> {
        let mut members: Vec<Vec<String>> = vec![Vec::new(); rows.len()];
        let schema = identifier(&self.schema).unwrap_or_else(|| "\"public\"".to_owned());
        for navigation in navigations {
            let Some(target) = sets.iter().find(|candidate| candidate.name == navigation.target) else {
                continue;
            };
            let positions: Vec<Option<usize>> = navigation.from.iter().map(|column| set.position(column)).collect();
            let key_of = |row: &[Value]| -> Option<Vec<String>> {
                positions.iter().map(|at| at.and_then(|at| row.get(at)).and_then(Value::to_text)).collect()
            };
            let mut wanted: Vec<Vec<String>> = rows.iter().filter_map(|row| key_of(row.as_slice())).collect();
            wanted.sort();
            wanted.dedup();
            let mut found: Vec<(Vec<String>, String)> = Vec::new();
            if !wanted.is_empty() {
                let to: Vec<String> = navigation.to.iter().filter_map(|column| identifier(column)).collect();
                let conditions: Vec<String> = wanted
                    .iter()
                    .map(|key| {
                        let parts: Vec<String> = to.iter().zip(key).map(|(column, value)| format!("{column} = {}", literal(value))).collect();
                        format!("({})", parts.join(" AND "))
                    })
                    .collect();
                let columns: Vec<String> = target.properties.iter().filter_map(|property| identifier(&property.name)).collect();
                let table = identifier(&target.name).unwrap_or_default();
                let results = self.run(&format!(
                    "SELECT {} FROM {schema}.{table} WHERE {}",
                    columns.join(", "),
                    conditions.join(" OR ")
                ))?;
                let target_positions: Vec<Option<usize>> = navigation.to.iter().map(|column| target.position(column)).collect();
                let all: Vec<&Property> = target.properties.iter().collect();
                for row in rows_of(&results) {
                    let key: Option<Vec<String>> = target_positions.iter().map(|at| at.and_then(|at| row.get(at)).and_then(Value::to_text)).collect();
                    if let Some(key) = key {
                        found.push((key, entity_json("", &all, &row)));
                    }
                }
            }
            for (row, member) in rows.iter().zip(members.iter_mut()) {
                let matches: Vec<&str> = match key_of(row.as_slice()) {
                    Some(key) => found.iter().filter(|(found_key, _)| *found_key == key).map(|(_, entity)| entity.as_str()).collect(),
                    None => Vec::new(),
                };
                let value = if navigation.many {
                    format!("[{}]", matches.join(","))
                } else {
                    matches.first().map_or_else(|| "null".to_owned(), |entity| (*entity).to_owned())
                };
                member.push(format!("{}:{value}", json_string(&navigation.name)));
            }
        }
        Ok(members)
    }

    fn odata_metadata(&self, sets: &[EntitySet]) -> Response {
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><edmx:Edmx Version=\"4.0\" xmlns:edmx=\"http://docs.oasis-open.org/odata/ns/edmx\"><edmx:DataServices><Schema Namespace=\"EggDB\" xmlns=\"http://docs.oasis-open.org/odata/ns/edm\">",
        );
        for set in sets {
            xml.push_str(&format!("<EntityType Name=\"{}\"><Key>", xml_escaped(&set.name)));
            for property in set.key_columns() {
                xml.push_str(&format!("<PropertyRef Name=\"{}\"/>", xml_escaped(&property.name)));
            }
            xml.push_str("</Key>");
            for property in &set.properties {
                let is_key = set.key_columns().iter().any(|key| key.name == property.name);
                xml.push_str(&format!(
                    "<Property Name=\"{}\" Type=\"{}\"{}/>",
                    xml_escaped(&property.name),
                    edm_type(&property.data_type),
                    if property.nullable && !is_key { "" } else { " Nullable=\"false\"" }
                ));
            }
            for navigation in &set.navigations {
                let target = xml_escaped(&navigation.target);
                if navigation.many {
                    xml.push_str(&format!(
                        "<NavigationProperty Name=\"{}\" Type=\"Collection(EggDB.{target})\"/>",
                        xml_escaped(&navigation.name)
                    ));
                } else {
                    xml.push_str(&format!("<NavigationProperty Name=\"{}\" Type=\"EggDB.{target}\">", xml_escaped(&navigation.name)));
                    for (from, to) in navigation.from.iter().zip(&navigation.to) {
                        xml.push_str(&format!(
                            "<ReferentialConstraint Property=\"{}\" ReferencedProperty=\"{}\"/>",
                            xml_escaped(from),
                            xml_escaped(to)
                        ));
                    }
                    xml.push_str("</NavigationProperty>");
                }
            }
            xml.push_str("</EntityType>");
        }
        xml.push_str("<EntityContainer Name=\"Container\">");
        for set in sets {
            let name = xml_escaped(&set.name);
            xml.push_str(&format!("<EntitySet Name=\"{name}\" EntityType=\"EggDB.{name}\">"));
            for navigation in &set.navigations {
                xml.push_str(&format!(
                    "<NavigationPropertyBinding Path=\"{}\" Target=\"{}\"/>",
                    xml_escaped(&navigation.name),
                    xml_escaped(&navigation.target)
                ));
            }
            xml.push_str("</EntitySet>");
        }
        xml.push_str("</EntityContainer></Schema></edmx:DataServices></edmx:Edmx>");
        Response {
            status: 200,
            content_type: "application/xml".to_owned(),
            body: xml.into_bytes(),
            headers: vec![("OData-Version".to_owned(), "4.0".to_owned())],
        }
    }

    /// Answers a request under `/odata`; `path` is what follows `/odata/`.
    pub(crate) fn odata(&self, path: &str, request: &Request) -> Response {
        let path = percent_decode(path);
        let path = path.as_str();
        let host = request.header("host").unwrap_or("localhost");
        let scheme = request.header("x-forwarded-proto").unwrap_or("http");
        let base = format!("{scheme}://{host}/odata/");
        let sets = match self.odata_model() {
            Ok(sets) => sets,
            Err(error) => return sql_error(&error),
        };
        if path.is_empty() {
            let listed: Vec<String> = sets
                .iter()
                .map(|set| {
                    format!(
                        "{{\"name\":{name},\"kind\":\"EntitySet\",\"url\":{name}}}",
                        name = json_string(&set.name)
                    )
                })
                .collect();
            return odata_json(
                200,
                format!("{{\"@odata.context\":{},\"value\":[{}]}}", json_string(&format!("{base}$metadata")), listed.join(",")),
            );
        }
        if path == "$metadata" {
            return self.odata_metadata(&sets);
        }
        let (head, tail) = path.split_once('/').unwrap_or((path, ""));
        let (name, key) = match head.split_once('(') {
            Some((name, rest)) => match rest.strip_suffix(')') {
                Some(key) => (name.to_owned(), Some(key.to_owned())),
                None => return odata_error(400, "BadRequest", "a key segment needs a closing parenthesis"),
            },
            None => (head.to_owned(), None),
        };
        let Some(set) = sets.iter().find(|set| set.name == name) else {
            return odata_error(404, "NotFound", &format!("Resource not found for the segment '{name}'."));
        };
        let Some(target) = identifier(&set.name).map(|table| {
            format!("{}.{table}", identifier(&self.schema).unwrap_or_else(|| "\"public\"".to_owned()))
        }) else {
            return odata_error(404, "NotFound", &format!("'{name}' cannot be served"));
        };
        let options = query_options(request.query.as_deref());
        match (key, tail, request.method.as_str()) {
            (None, "$count", "GET") => self.odata_count(set, &target, &options),
            (None, "", "GET") => self.odata_collection(set, &sets, &target, &options, &base, request),
            (None, "", "POST") => self.odata_insert(set, &target, &request.body, &base),
            (Some(key), "", "GET") => self.odata_entity(set, &sets, &target, &key, &options, &base),
            (Some(key), "", "PATCH" | "PUT" | "MERGE") => self.odata_update(set, &target, &key, &request.body),
            (Some(key), "", "DELETE") => self.odata_delete(set, &target, &key),
            (Some(key), property, "GET") if !property.is_empty() => {
                self.odata_property(set, &target, &key, property, &base)
            }
            _ => odata_error(405, "MethodNotAllowed", "this resource does not accept that method"),
        }
    }

    fn odata_filter(set: &EntitySet, options: &[(String, String)]) -> Result<String, Response> {
        if option(options, "$search").is_some() {
            return Err(odata_error(501, "NotImplemented", "$search is not supported yet"));
        }
        match option(options, "$filter") {
            Some(text) => filter_sql(set, text)
                .map(|sql| format!(" WHERE {sql}"))
                .map_err(|message| odata_error(400, "BadRequest", &message)),
            None => Ok(String::new()),
        }
    }

    fn odata_count(&self, set: &EntitySet, target: &str, options: &[(String, String)]) -> Response {
        let filter = match Self::odata_filter(set, options) {
            Ok(filter) => filter,
            Err(response) => return response,
        };
        match self.run(&format!("SELECT count(*) FROM {target}{filter}")) {
            Ok(results) => {
                let count = rows_of(&results)
                    .first()
                    .and_then(|row| row.first())
                    .and_then(Value::to_text)
                    .unwrap_or_else(|| "0".to_owned());
                let mut response = odata_json(200, count);
                response.content_type = "text/plain".to_owned();
                response
            }
            Err(error) => sql_error(&error),
        }
    }

    fn odata_collection(
        &self,
        set: &EntitySet,
        sets: &[EntitySet],
        target: &str,
        options: &[(String, String)],
        base: &str,
        request: &Request,
    ) -> Response {
        let filter = match Self::odata_filter(set, options) {
            Ok(filter) => filter,
            Err(response) => return response,
        };
        let outcome = (|| -> Result<(Vec<&Property>, Vec<&Navigation>, String, Option<u64>, u64, bool), String> {
            let columns = select_columns(set, option(options, "$select"))?;
            let expanded = expand_names(set, option(options, "$expand"))?;
            let order = match option(options, "$orderby") {
                Some(text) => order_sql(set, text)?,
                None => String::new(),
            };
            let top = whole_number(options, "$top")?;
            let skip = whole_number(options, "$skip")?.unwrap_or(0);
            let counted = option(options, "$count").is_some_and(|value| value.eq_ignore_ascii_case("true"));
            Ok((columns, expanded, order, top, skip, counted))
        })();
        let (columns, expanded, order, top, skip, counted) = match outcome {
            Ok(parts) => parts,
            Err(message) => return odata_error(400, "BadRequest", &message),
        };
        let quoted: Vec<String> = set.properties.iter().filter_map(|property| identifier(&property.name)).collect();
        let page = top.unwrap_or(PAGE_ROWS).min(PAGE_ROWS);
        let ordered = if order.is_empty() && !set.key_columns().is_empty() {
            let keys: Vec<String> = set.key_columns().iter().filter_map(|property| identifier(&property.name)).collect();
            format!(" ORDER BY {}", keys.join(", "))
        } else {
            order
        };
        let sql = format!(
            "SELECT {} FROM {target}{filter}{ordered} LIMIT {} OFFSET {skip}",
            quoted.join(", "),
            page + 1
        );
        let results = match self.run(&sql) {
            Ok(results) => results,
            Err(error) => return sql_error(&error),
        };
        let mut rows = rows_of(&results);
        let more = u64::try_from(rows.len()).unwrap_or(u64::MAX) > page && top.is_none_or(|top| top > page);
        rows.truncate(usize::try_from(page).unwrap_or(usize::MAX));
        let mut body = format!(
            "{{\"@odata.context\":{}",
            json_string(&format!("{base}$metadata#{}", set.name))
        );
        if counted {
            match self.run(&format!("SELECT count(*) FROM {target}{filter}")) {
                Ok(results) => {
                    let count = rows_of(&results)
                        .first()
                        .and_then(|row| row.first())
                        .and_then(Value::to_text)
                        .unwrap_or_else(|| "0".to_owned());
                    body.push_str(&format!(",\"@odata.count\":{count}"));
                }
                Err(error) => return sql_error(&error),
            }
        }
        let expansions = match self.expansions(set, sets, &expanded, &rows) {
            Ok(expansions) => expansions,
            Err(error) => return sql_error(&error),
        };
        let entities: Vec<String> = rows
            .iter()
            .zip(&expansions)
            .map(|(row, members)| entity_with("", &columns, &projected(set, &columns, row), members))
            .collect();
        body.push_str(&format!(",\"value\":[{}]", entities.join(",")));
        if more {
            let mut kept: Vec<String> = request
                .query
                .as_deref()
                .unwrap_or_default()
                .split('&')
                .filter(|pair| !pair.is_empty())
                .filter(|pair| {
                    let name = percent_decode(pair.split_once('=').map_or(*pair, |(name, _)| name));
                    !name.eq_ignore_ascii_case("$skip") && !name.eq_ignore_ascii_case("$top")
                })
                .map(str::to_owned)
                .collect();
            kept.push(format!("$skip={}", skip + page));
            if let Some(top) = top {
                kept.push(format!("$top={}", top - page));
            }
            body.push_str(&format!(
                ",\"@odata.nextLink\":{}",
                json_string(&format!("{base}{}?{}", set.name, kept.join("&")))
            ));
        }
        body.push('}');
        odata_json(200, body)
    }

    fn odata_entity(&self, set: &EntitySet, sets: &[EntitySet], target: &str, key: &str, options: &[(String, String)], base: &str) -> Response {
        let condition = match key_condition(set, key) {
            Ok(condition) => condition,
            Err(message) => return odata_error(400, "BadRequest", &message),
        };
        let columns = match select_columns(set, option(options, "$select")) {
            Ok(columns) => columns,
            Err(message) => return odata_error(400, "BadRequest", &message),
        };
        let expanded = match expand_names(set, option(options, "$expand")) {
            Ok(expanded) => expanded,
            Err(message) => return odata_error(400, "BadRequest", &message),
        };
        let quoted: Vec<String> = set.properties.iter().filter_map(|property| identifier(&property.name)).collect();
        let rows = match self.run(&format!("SELECT {} FROM {target} WHERE {condition}", quoted.join(", "))) {
            Ok(results) => rows_of(&results),
            Err(error) => return sql_error(&error),
        };
        let Some(row) = rows.first() else {
            return odata_error(404, "NotFound", "No entity has that key.");
        };
        let expansions = match self.expansions(set, sets, &expanded, std::slice::from_ref(row)) {
            Ok(expansions) => expansions,
            Err(error) => return sql_error(&error),
        };
        odata_json(
            200,
            entity_with(
                &format!("{base}$metadata#{}/$entity", set.name),
                &columns,
                &projected(set, &columns, row),
                expansions.first().map(Vec::as_slice).unwrap_or_default(),
            ),
        )
    }

    fn odata_property(&self, set: &EntitySet, target: &str, key: &str, path: &str, base: &str) -> Response {
        let (name, raw) = match path.strip_suffix("/$value") {
            Some(name) => (name, true),
            None => (path, false),
        };
        let Some(property) = set.properties.iter().find(|property| property.name == name) else {
            return odata_error(404, "NotFound", &format!("Could not find a property named '{name}' on type '{}'.", set.name));
        };
        let condition = match key_condition(set, key) {
            Ok(condition) => condition,
            Err(message) => return odata_error(400, "BadRequest", &message),
        };
        let Some(column) = identifier(&property.name) else {
            return odata_error(404, "NotFound", "that property cannot be served");
        };
        match self.run(&format!("SELECT {column} FROM {target} WHERE {condition}")) {
            Ok(results) => match rows_of(&results).first().and_then(|row| row.first()) {
                Some(Value::Null) if raw => no_content(),
                Some(value) if raw => {
                    let mut response = odata_json(200, value.to_text().unwrap_or_default());
                    response.content_type = "text/plain".to_owned();
                    response
                }
                Some(value) => odata_json(
                    200,
                    format!(
                        "{{\"@odata.context\":{},\"value\":{}}}",
                        json_string(&format!("{base}$metadata#{}({key})/{name}", set.name)),
                        odata_value(value, &property.data_type)
                    ),
                ),
                None => odata_error(404, "NotFound", "No entity has that key."),
            },
            Err(error) => sql_error(&error),
        }
    }

    fn odata_body(body: &[u8]) -> Result<Vec<(String, String)>, Response> {
        let text = std::str::from_utf8(body).map_err(|_| odata_error(400, "BadRequest", "the body is not UTF-8"))?;
        let document = parse_document(text).map_err(|_| odata_error(400, "BadRequest", "the body is not JSON"))?;
        let Json::Object(members) = &document.value else {
            return Err(odata_error(400, "BadRequest", "the body is not a JSON object"));
        };
        let mut assignments = Vec::new();
        for (name, node) in members {
            if name.starts_with('@') || name.contains("@odata.") {
                continue;
            }
            let column = identifier(name).ok_or_else(|| odata_error(400, "BadRequest", &format!("'{name}' is not a property name")))?;
            let value = json_literal(node, text).map_err(|message| odata_error(400, "BadRequest", &message))?;
            assignments.push((column, value));
        }
        Ok(assignments)
    }

    fn odata_insert(&self, set: &EntitySet, target: &str, body: &[u8], base: &str) -> Response {
        let assignments = match Self::odata_body(body) {
            Ok(assignments) => assignments,
            Err(response) => return response,
        };
        let columns: Vec<&Property> = set.properties.iter().collect();
        let returned: Vec<String> = columns.iter().filter_map(|property| identifier(&property.name)).collect();
        let sql = if assignments.is_empty() {
            format!("INSERT INTO {target} DEFAULT VALUES RETURNING {}", returned.join(", "))
        } else {
            let (names, values): (Vec<String>, Vec<String>) = assignments.into_iter().unzip();
            format!(
                "INSERT INTO {target} ({}) VALUES ({}) RETURNING {}",
                names.join(", "),
                values.join(", "),
                returned.join(", ")
            )
        };
        match self.run(&sql) {
            Ok(results) => match rows_of(&results).first() {
                Some(row) => {
                    let mut response = odata_json(
                        201,
                        entity_json(&format!("{base}$metadata#{}/$entity", set.name), &columns, row),
                    );
                    let keys: Vec<String> = set
                        .key_columns()
                        .iter()
                        .filter_map(|key| {
                            let at = columns.iter().position(|property| property.name == key.name)?;
                            let value = row.get(at)?;
                            let text = value.to_text().unwrap_or_default();
                            let shown = match edm_type(&key.data_type) {
                                "Edm.Int16" | "Edm.Int32" | "Edm.Int64" | "Edm.Decimal" | "Edm.Double" | "Edm.Single" => text,
                                _ => format!("'{}'", text.replace('\'', "''")),
                            };
                            Some(format!("{}={shown}", key.name))
                        })
                        .collect();
                    let segment = match keys.as_slice() {
                        [only] => only.split_once('=').map_or_else(String::new, |(_, value)| value.to_owned()),
                        _ => keys.join(","),
                    };
                    response.headers.push(("Location".to_owned(), format!("{base}{}({segment})", set.name)));
                    response
                }
                None => odata_error(500, "InternalError", "the insert returned no row"),
            },
            Err(error) => sql_error(&error),
        }
    }

    fn odata_update(&self, set: &EntitySet, target: &str, key: &str, body: &[u8]) -> Response {
        let condition = match key_condition(set, key) {
            Ok(condition) => condition,
            Err(message) => return odata_error(400, "BadRequest", &message),
        };
        let assignments = match Self::odata_body(body) {
            Ok(assignments) => assignments,
            Err(response) => return response,
        };
        if assignments.is_empty() {
            return odata_error(400, "BadRequest", "the body changes no property");
        }
        let set_list: Vec<String> = assignments.iter().map(|(name, value)| format!("{name} = {value}")).collect();
        self.odata_changed(&format!("UPDATE {target} SET {} WHERE {condition} RETURNING 1", set_list.join(", ")))
    }

    fn odata_delete(&self, set: &EntitySet, target: &str, key: &str) -> Response {
        let condition = match key_condition(set, key) {
            Ok(condition) => condition,
            Err(message) => return odata_error(400, "BadRequest", &message),
        };
        self.odata_changed(&format!("DELETE FROM {target} WHERE {condition} RETURNING 1"))
    }

    fn odata_changed(&self, sql: &str) -> Response {
        match self.run(sql) {
            Ok(results) if rows_of(&results).is_empty() => odata_error(404, "NotFound", "No entity has that key."),
            Ok(_) => no_content(),
            Err(error) => sql_error(&error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn books() -> EntitySet {
        let property = |name: &str, data_type: &str| Property {
            name: name.to_owned(),
            data_type: data_type.to_owned(),
            nullable: true,
        };
        EntitySet {
            name: "books".to_owned(),
            properties: vec![
                property("id", "integer"),
                property("title", "text"),
                property("pages", "integer"),
                property("published", "timestamp with time zone"),
            ],
            key: vec!["id".to_owned()],
            navigations: Vec::new(),
        }
    }

    #[test]
    fn filters_become_quoted_sql() {
        let set = books();
        assert_eq!(
            filter_sql(&set, "pages ge 100 and (contains(tolower(title),'o''b') or title eq null)").unwrap(),
            "((\"pages\" >= 100) AND (((strpos(lower(\"title\"), 'o''b') > 0) OR (\"title\" IS NULL))))"
        );
        assert_eq!(
            filter_sql(&set, "published lt 2026-01-01T00:00:00Z and not (id in (1, 2))").unwrap(),
            "((\"published\" < '2026-01-01T00:00:00Z') AND (NOT ((\"id\" IN (1, 2)))))"
        );
        assert_eq!(filter_sql(&set, "pages mul 2 sub -1 gt 10").unwrap(), "(((\"pages\" * 2) - (-1)) > 10)");
        for bad in ["", "nothing eq 1", "title eq 'open", "pages gt", "id in 1", "bogus(title)", "title; drop"] {
            assert!(filter_sql(&set, bad).is_err(), "{bad}");
        }
        let deep = format!("{}id eq 1{}", "(".repeat(200), ")".repeat(200));
        assert!(filter_sql(&set, &deep).is_err());
    }

    #[test]
    fn keys_order_and_values_follow_odata() {
        let set = books();
        assert_eq!(key_condition(&set, "7").unwrap(), "\"id\" = 7");
        assert_eq!(key_condition(&set, "id=7").unwrap(), "\"id\" = 7");
        assert!(key_condition(&set, "7,8").is_err());
        assert_eq!(order_sql(&set, "pages desc,title").unwrap(), " ORDER BY \"pages\" DESC, \"title\" ASC");
        assert_eq!(date_time_offset("2026-11-12 10:00:00+00"), "2026-11-12T10:00:00Z");
        assert_eq!(date_time_offset("2026-11-12 10:00:00.5+05:30"), "2026-11-12T10:00:00.5+05:30");
        assert_eq!(date_time_offset("2026-11-12 10:00:00-07"), "2026-11-12T10:00:00-07:00");
        assert_eq!(date_time_offset("2026-11-12 10:00:00"), "2026-11-12T10:00:00Z");
        assert_eq!(edm_type("bigint"), "Edm.Int64");
    }
}
