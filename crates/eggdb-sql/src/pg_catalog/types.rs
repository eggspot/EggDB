use crate::arrays::ArrayElement;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{Conversion, SqlType, Value};

const MAX_NAME_BYTES: usize = 63;

/// A type of PostgreSQL's system catalog: a type of expressions and of the catalog's rows,
/// never the type of a column of a stored table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogType {
    /// `oid`: an object identifier, an unsigned 32-bit number.
    Oid,
    /// `name`: an identifier; a cast to it keeps at most 63 bytes.
    Name,
    /// `"char"`: the one-character codes of the catalog's kind columns.
    InternalChar,
    /// `pg_node_tree`: a stored expression, shown as its text.
    NodeTree,
    /// `xid`: a transaction identifier, an unsigned 32-bit number.
    Xid,
    /// `aclitem`: an access privilege; always NULL in EggDB's catalog.
    AclItem,
    /// `anyarray`: an array of any type; always NULL in EggDB's catalog.
    AnyArray,
    /// A `reg*` type: an object identifier shown as the name of the object it identifies.
    Reg(RegKind),
}

/// The kind of object a `reg*` value identifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegKind {
    /// `regclass`: a table, view, sequence or index.
    Class,
    /// `regtype`: a data type.
    Type,
    /// `regnamespace`: a schema.
    Namespace,
    /// `regrole`: a role.
    Role,
    /// `regproc`: a function.
    Proc,
}

/// A `reg*` value: an object identifier with the name its type shows for it.
#[derive(Debug, Clone, PartialEq)]
pub struct RegValue {
    /// The kind of object identified.
    pub kind: RegKind,
    /// The object identifier.
    pub oid: u32,
    /// The name shown: qualified with its schema only when the object is not visible on the
    /// search path, or the number itself when no object has the identifier.
    pub name: String,
}

impl RegKind {
    pub(crate) fn type_name(self) -> &'static str {
        match self {
            Self::Class => "regclass",
            Self::Type => "regtype",
            Self::Namespace => "regnamespace",
            Self::Role => "regrole",
            Self::Proc => "regproc",
        }
    }
}

impl CatalogType {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Oid => "oid",
            Self::Name => "name",
            Self::InternalChar => "\"char\"",
            Self::NodeTree => "pg_node_tree",
            Self::Xid => "xid",
            Self::AclItem => "aclitem",
            Self::AnyArray => "anyarray",
            Self::Reg(kind) => kind.type_name(),
        }
    }

    pub(crate) fn internal_name(self) -> &'static str {
        match self {
            Self::InternalChar => "char",
            other => other.name(),
        }
    }

    pub(crate) fn is_text_like(self) -> bool {
        matches!(self, Self::Name | Self::InternalChar | Self::NodeTree)
    }

    pub(crate) fn is_identifier(self) -> bool {
        matches!(self, Self::Oid | Self::Xid | Self::Reg(_))
    }
}

pub(crate) fn cast_type_named(word: &str) -> Option<SqlType> {
    Some(match word {
        "oid" => SqlType::Catalog(CatalogType::Oid),
        "name" => SqlType::Catalog(CatalogType::Name),
        "pg_node_tree" => SqlType::Catalog(CatalogType::NodeTree),
        "xid" => SqlType::Catalog(CatalogType::Xid),
        "aclitem" => SqlType::Catalog(CatalogType::AclItem),
        "regclass" => SqlType::Catalog(CatalogType::Reg(RegKind::Class)),
        "regtype" => SqlType::Catalog(CatalogType::Reg(RegKind::Type)),
        "regnamespace" => SqlType::Catalog(CatalogType::Reg(RegKind::Namespace)),
        "regrole" => SqlType::Catalog(CatalogType::Reg(RegKind::Role)),
        "regproc" => SqlType::Catalog(CatalogType::Reg(RegKind::Proc)),
        "int2vector" => SqlType::Array(ArrayElement::Int2Vector),
        "oidvector" => SqlType::Array(ArrayElement::OidVector),
        _ => return None,
    })
}

pub(crate) fn read_only(name: &str) -> SqlError {
    SqlError::new(
        SqlState::INSUFFICIENT_PRIVILEGE,
        format!("permission denied for table {name}"),
    )
}

pub(crate) fn system_catalog(name: &str) -> SqlError {
    SqlError::new(
        SqlState::INSUFFICIENT_PRIVILEGE,
        format!("permission denied: \"{name}\" is a system catalog"),
    )
}

pub(crate) fn oid_number(value: &Value) -> Option<i64> {
    match value {
        Value::Oid(oid) => Some(i64::from(*oid)),
        Value::Reg(reg) => Some(i64::from(reg.oid)),
        other => other.as_integer(),
    }
}

fn invalid_input(text: &str, sql_type: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("invalid input syntax for type {sql_type}: \"{text}\""),
    )
}

pub(crate) fn parse_identifier(text: &str, sql_type: &str) -> SqlResult<u32> {
    let trimmed = text.trim_matches(|c: char| c.is_ascii_whitespace());
    let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_input(text, sql_type));
    }
    let out_of_range = || {
        SqlError::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("value \"{text}\" is out of range for type {sql_type}"),
        )
    };
    let number: i64 = trimmed.parse().map_err(|_| out_of_range())?;
    identifier_of(number).ok_or_else(out_of_range)
}

fn identifier_of(number: i64) -> Option<u32> {
    if (i64::from(i32::MIN)..=i64::from(u32::MAX)).contains(&number) {
        Some(number as u32)
    } else {
        None
    }
}

pub(crate) fn truncated_name(mut text: String) -> String {
    if text.len() > MAX_NAME_BYTES {
        let mut end = MAX_NAME_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn first_character(text: &str) -> String {
    text.chars().next().map(String::from).unwrap_or_default()
}

fn always_null(kind: CatalogType) -> SqlError {
    SqlError::not_supported(format!(
        "values of type {} are not supported yet",
        kind.name()
    ))
}

pub(crate) fn parse_input(text: &str, kind: CatalogType) -> SqlResult<Value> {
    match kind {
        CatalogType::Oid => parse_identifier(text, "oid").map(Value::Oid),
        CatalogType::Xid => parse_identifier(text, "xid").map(Value::Oid),
        CatalogType::Name => Ok(Value::Text(truncated_name(text.to_owned()))),
        CatalogType::InternalChar => Ok(Value::Text(first_character(text))),
        CatalogType::NodeTree => Err(node_tree_input()),
        CatalogType::AclItem | CatalogType::AnyArray => Err(always_null(kind)),
        CatalogType::Reg(reg) => super::snapshot::reg_from_text(reg, text).map(boxed),
    }
}

fn node_tree_input() -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "cannot accept a value of type pg_node_tree",
    )
}

pub(crate) fn check_expression_tree_argument(
    function: &str,
    arguments: &[crate::ast::Expr],
    scope: crate::eval::Scope<'_>,
) -> SqlResult<()> {
    let Some(first) = arguments.first() else {
        return Ok(());
    };
    if matches!(
        first.kind,
        crate::ast::ExprKind::Literal(crate::ast::Literal::String(_))
    ) {
        return Err(node_tree_input().at(first.position));
    }
    match crate::typing::static_type(first, scope) {
        None | Some(SqlType::Catalog(CatalogType::NodeTree)) => Ok(()),
        Some(_) => {
            let types: Vec<String> = arguments
                .iter()
                .map(|argument| {
                    crate::typing::static_type(argument, scope)
                        .map_or_else(|| "unknown".to_owned(), |sql_type| sql_type.name())
                })
                .collect();
            Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function {function}({}) does not exist", types.join(", ")),
            )
            .with_hint(
                "No function matches the given name and argument types. You might need to add explicit type casts.",
            ))
        }
    }
}

fn boxed(reg: RegValue) -> Value {
    Value::Reg(Box::new(reg))
}

fn cannot_cast(value: &Value, target: &str) -> SqlError {
    SqlError::new(
        SqlState::CANNOT_COERCE,
        format!("cannot cast type {} to {target}", value.type_name()),
    )
}

pub(crate) fn reg_value(kind: RegKind, value: Value) -> SqlResult<Value> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Text(text) | Value::Bpchar(text) => parse_input(&text, CatalogType::Reg(kind)),
        Value::Reg(reg) if reg.kind == kind => Ok(Value::Reg(reg)),
        other => match oid_number(&other).and_then(identifier_of) {
            Some(oid) => Ok(boxed(super::snapshot::reg_from_oid(kind, oid))),
            None if other.as_integer().is_some() => Err(SqlError::new(
                SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                "OID out of range",
            )),
            None => Err(cannot_cast(&other, kind.type_name())),
        },
    }
}

pub(crate) fn convert(value: Value, kind: CatalogType, conversion: Conversion) -> SqlResult<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    match kind {
        CatalogType::Name | CatalogType::InternalChar | CatalogType::NodeTree => {
            let text = match value {
                Value::Text(text) => text,
                Value::Bpchar(text) => crate::types::without_trailing_spaces(&text).to_owned(),
                other if conversion == Conversion::Explicit && kind != CatalogType::NodeTree => {
                    other.to_text().unwrap_or_default()
                }
                other => return Err(cannot_cast(&other, kind.name())),
            };
            match kind {
                CatalogType::Name => Ok(Value::Text(truncated_name(text))),
                CatalogType::InternalChar => Ok(Value::Text(first_character(&text))),
                _ if conversion == Conversion::Assignment => Ok(Value::Text(text)),
                _ => Err(SqlError::new(
                    SqlState::CANNOT_COERCE,
                    "cannot cast type text to pg_node_tree",
                )),
            }
        }
        CatalogType::Oid | CatalogType::Xid => match value {
            Value::Text(text) | Value::Bpchar(text) if conversion == Conversion::Explicit => {
                parse_input(&text, kind)
            }
            other => match oid_number(&other) {
                Some(number) => identifier_of(number).map(Value::Oid).ok_or_else(|| {
                    SqlError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "OID out of range")
                }),
                None => Err(cannot_cast(&other, kind.name())),
            },
        },
        CatalogType::Reg(reg) => reg_value(reg, value),
        CatalogType::AclItem | CatalogType::AnyArray => Err(always_null(kind)),
    }
}

pub(crate) fn convert_from(
    value: &Value,
    target: SqlType,
    _conversion: Conversion,
) -> Option<SqlResult<Value>> {
    let number = match value {
        Value::Oid(oid) => *oid,
        Value::Reg(reg) => reg.oid,
        _ => return None,
    };
    Some(match target {
        SqlType::BigInt => Ok(Value::BigInt(i64::from(number))),
        SqlType::Integer => Ok(Value::Integer(number as i32)),
        SqlType::SmallInt => i16::try_from(number)
            .map(Value::SmallInt)
            .map_err(|_| crate::types::out_of_range(target)),
        SqlType::Numeric(_) => Ok(Value::Numeric(crate::numeric::Numeric::from_i64(
            i64::from(number),
        ))),
        _ => Err(cannot_cast(value, &target.name())),
    })
}

const MOST_NAME_PARTS: usize = 16;

fn invalid_name() -> SqlError {
    SqlError::new(SqlState::INVALID_NAME, "invalid name syntax")
}

pub(crate) fn name_parts(text: &str) -> SqlResult<Vec<String>> {
    let mut parts: Vec<String> = Vec::new();
    let mut chars = text.trim().chars().peekable();
    loop {
        let mut part = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        part.push('"');
                    }
                    Some('"') => break,
                    Some(c) => part.push(c),
                    None => return Err(invalid_name()),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == '.' || c == '"' || c.is_whitespace() {
                    break;
                }
                part.push(c.to_ascii_lowercase());
                chars.next();
            }
        }
        if part.is_empty() || parts.len() >= MOST_NAME_PARTS {
            return Err(invalid_name());
        }
        parts.push(truncated_name(part));
        match chars.next() {
            None => break,
            Some('.') => {}
            Some(_) => return Err(invalid_name()),
        }
    }
    Ok(parts)
}

pub(crate) fn split_qualified(text: &str) -> SqlResult<(Option<String>, String)> {
    let mut parts = name_parts(text)?.into_iter();
    match (parts.next(), parts.next(), parts.next()) {
        (Some(name), None, None) => Ok((None, name)),
        (Some(schema), Some(name), None) => Ok((Some(schema), name)),
        _ => Err(SqlError::not_supported(
            "cross-database references are not implemented",
        )),
    }
}

pub(crate) fn split_object_name(text: &str, kind: RegKind) -> SqlResult<(Option<String>, String)> {
    let parts = name_parts(text)?;
    let joined = || parts.join(".");
    match (kind, parts.len()) {
        (RegKind::Namespace | RegKind::Role, count) if count > 1 => Err(invalid_name()),
        (_, 1 | 2) => split_qualified(text),
        (RegKind::Class, 3) => Err(SqlError::not_supported(format!(
            "cross-database references are not implemented: \"{}\"",
            joined()
        ))),
        (_, 3) => Err(SqlError::not_supported(format!(
            "cross-database references are not implemented: {}",
            joined()
        ))),
        (RegKind::Class, _) => Err(SqlError::new(
            SqlState::SYNTAX_ERROR,
            format!(
                "improper relation name (too many dotted names): {}",
                joined()
            ),
        )),
        _ => Err(SqlError::new(
            SqlState::SYNTAX_ERROR,
            format!(
                "improper qualified name (too many dotted names): {}",
                joined()
            ),
        )),
    }
}

const MODIFIABLE_TYPES: [u32; 8] = [1042, 1043, 1083, 1114, 1184, 1186, 1266, 1700];

fn missing_type(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("type \"{name}\" does not exist"),
    )
}

fn syntax_error_near(token: &str) -> SqlError {
    if token.is_empty() {
        SqlError::new(SqlState::SYNTAX_ERROR, "syntax error at end of input")
    } else {
        SqlError::new(
            SqlState::SYNTAX_ERROR,
            format!("syntax error at or near \"{token}\""),
        )
    }
}

fn plain_type_oid(name: &str) -> SqlResult<Option<u32>> {
    if let Some(found) = super::oids::builtin_type_named(name) {
        return Ok(Some(found.oid));
    }
    if let Some(fields) = name.strip_prefix("interval ")
        && fields.split_whitespace().all(|word| {
            matches!(
                word,
                "year" | "month" | "day" | "hour" | "minute" | "second" | "to"
            )
        })
    {
        return Ok(Some(1186));
    }
    let (schema, base) = split_object_name(name, RegKind::Type)?;
    if schema
        .as_deref()
        .is_some_and(|schema| schema != crate::schemas::CATALOG_SCHEMA)
    {
        return Ok(None);
    }
    Ok(super::oids::builtin_type_named(&base).map(|found| found.oid))
}

fn modifiers_are_numbers(inside: &str) -> SqlResult<()> {
    for argument in inside.split(',') {
        let argument = argument.trim();
        if argument.is_empty() || !argument.bytes().all(|byte| byte.is_ascii_digit()) {
            let token = argument
                .split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or_default();
            return Err(syntax_error_near(if token.is_empty() {
                ")"
            } else {
                token
            }));
        }
    }
    Ok(())
}

pub(crate) fn regtype_oid(text: &str) -> SqlResult<u32> {
    let written = text.trim();
    let lowered;
    let mut name = if written.contains('"') {
        written
    } else {
        lowered = written
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        lowered.as_str()
    };
    if let Some(found) = super::oids::builtin_type_named(name) {
        return Ok(found.oid);
    }
    let mut array = false;
    while let Some(element) = name.strip_suffix("[]") {
        name = element.trim_end();
        array = true;
    }
    let (base, modified) = match name.split_once('(') {
        Some((base, after)) => {
            let Some((inside, rest)) = after.split_once(')') else {
                return Err(syntax_error_near(""));
            };
            modifiers_are_numbers(inside)?;
            let base = base.trim_end();
            let rest = rest.trim();
            if base == "float" {
                let precision: u32 = inside.trim().parse().unwrap_or(u32::MAX);
                let oid = if precision <= 24 { 700 } else { 701 };
                return element_or_array(oid, array, base);
            }
            let full = if rest.is_empty() {
                base.to_owned()
            } else {
                format!("{base} {rest}")
            };
            (full, Some(base))
        }
        None => (name.to_owned(), None),
    };
    let shown = modified.unwrap_or(&base);
    let oid = plain_type_oid(&base)?.ok_or_else(|| missing_type(shown))?;
    if modified.is_some() && !MODIFIABLE_TYPES.contains(&oid) {
        return Err(SqlError::new(
            SqlState::SYNTAX_ERROR,
            format!("type modifier is not allowed for type \"{shown}\""),
        ));
    }
    element_or_array(oid, array, shown)
}

fn element_or_array(oid: u32, array: bool, shown: &str) -> SqlResult<u32> {
    if !array {
        return Ok(oid);
    }
    super::oids::builtin_type(oid)
        .map(|found| found.array)
        .filter(|array| *array != 0)
        .ok_or_else(|| missing_type(&format!("{shown}[]")))
}
