use crate::ast::{Expr, ExprKind, Literal};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Concatenated, Evaluator, Scope, and, compare_truth, or, truth_value};
use crate::pg_catalog::CatalogType;
use crate::types::{Conversion, SqlType, Value, compare_values, convert, equality_key};
use std::cmp::Ordering;

const MAX_ITEMS: usize = 1 << 27;
const ITEM_OVERHEAD: usize = std::mem::size_of::<Value>();

/// The element type of a one-dimensional array value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayElement {
    /// `smallint[]`.
    SmallInt,
    /// `integer[]`.
    Integer,
    /// `bigint[]`.
    BigInt,
    /// `real[]`.
    Real,
    /// `double precision[]`.
    Double,
    /// `numeric[]`.
    Numeric,
    /// `boolean[]`.
    Boolean,
    /// `text[]`.
    Text,
    /// `character varying[]`.
    Varchar,
    /// `character[]` (`bpchar[]`).
    Bpchar,
    /// `oid[]`.
    Oid,
    /// `name[]`.
    Name,
    /// `"char"[]`.
    InternalChar,
    /// `aclitem[]`: always NULL in EggDB's catalog.
    AclItem,
    /// `int2vector`: `smallint` elements numbered from 0 and written separated by spaces.
    Int2Vector,
    /// `oidvector`: `oid` elements numbered from 0 and written separated by spaces.
    OidVector,
    /// `date[]`.
    Date,
    /// `timestamp[]`.
    Timestamp,
    /// `timestamptz[]`.
    TimestampTz,
    /// `time[]`.
    Time,
    /// `timetz[]`.
    TimeTz,
    /// `interval[]`.
    Interval,
    /// `uuid[]`.
    Uuid,
    /// `json[]`.
    Json,
    /// `jsonb[]`.
    Jsonb,
    /// `bytea[]`.
    Bytea,
}

const ELEMENT_CODES: [(ArrayElement, u8); 26] = [
    (ArrayElement::SmallInt, 1),
    (ArrayElement::Integer, 2),
    (ArrayElement::BigInt, 3),
    (ArrayElement::Real, 4),
    (ArrayElement::Double, 5),
    (ArrayElement::Numeric, 6),
    (ArrayElement::Boolean, 7),
    (ArrayElement::Text, 8),
    (ArrayElement::Varchar, 9),
    (ArrayElement::Bpchar, 10),
    (ArrayElement::Oid, 11),
    (ArrayElement::Name, 12),
    (ArrayElement::InternalChar, 13),
    (ArrayElement::AclItem, 14),
    (ArrayElement::Int2Vector, 15),
    (ArrayElement::OidVector, 16),
    (ArrayElement::Date, 17),
    (ArrayElement::Timestamp, 18),
    (ArrayElement::TimestampTz, 19),
    (ArrayElement::Time, 20),
    (ArrayElement::TimeTz, 21),
    (ArrayElement::Interval, 22),
    (ArrayElement::Uuid, 23),
    (ArrayElement::Json, 24),
    (ArrayElement::Jsonb, 25),
    (ArrayElement::Bytea, 26),
];

impl ArrayElement {
    /// The SQL type of one element.
    pub fn sql_type(self) -> SqlType {
        match self {
            Self::SmallInt => SqlType::SmallInt,
            Self::Integer => SqlType::Integer,
            Self::BigInt => SqlType::BigInt,
            Self::Real => SqlType::Real,
            Self::Double => SqlType::Double,
            Self::Numeric => SqlType::Numeric(None),
            Self::Boolean => SqlType::Boolean,
            Self::Text => SqlType::Text,
            Self::Varchar => SqlType::Varchar(None),
            Self::Bpchar => SqlType::Bpchar(None),
            Self::Oid | Self::OidVector => SqlType::Catalog(CatalogType::Oid),
            Self::Name => SqlType::Catalog(CatalogType::Name),
            Self::InternalChar => SqlType::Catalog(CatalogType::InternalChar),
            Self::AclItem => SqlType::Catalog(CatalogType::AclItem),
            Self::Int2Vector => SqlType::SmallInt,
            Self::Date => SqlType::Date,
            Self::Timestamp => SqlType::Timestamp(None),
            Self::TimestampTz => SqlType::TimestampTz(None),
            Self::Time => SqlType::Time(None),
            Self::TimeTz => SqlType::TimeTz(None),
            Self::Interval => SqlType::INTERVAL,
            Self::Uuid => SqlType::Uuid,
            Self::Json => SqlType::Json,
            Self::Jsonb => SqlType::Jsonb,
            Self::Bytea => SqlType::Bytea,
        }
    }

    pub(crate) fn code(self) -> u8 {
        ELEMENT_CODES
            .iter()
            .find(|(element, _)| *element == self)
            .map_or(0, |(_, code)| *code)
    }

    pub(crate) fn of_code(code: u8) -> Option<Self> {
        ELEMENT_CODES
            .iter()
            .find(|(_, known)| *known == code)
            .map(|(element, _)| *element)
    }

    pub(crate) fn is_storable(self) -> bool {
        !matches!(
            self,
            Self::Oid
                | Self::Name
                | Self::InternalChar
                | Self::AclItem
                | Self::Int2Vector
                | Self::OidVector
        )
    }

    pub(crate) fn of(sql_type: SqlType) -> Option<Self> {
        Some(match sql_type {
            SqlType::SmallInt => Self::SmallInt,
            SqlType::Integer => Self::Integer,
            SqlType::BigInt => Self::BigInt,
            SqlType::Real => Self::Real,
            SqlType::Double => Self::Double,
            SqlType::Numeric(_) => Self::Numeric,
            SqlType::Boolean => Self::Boolean,
            SqlType::Text => Self::Text,
            SqlType::Varchar(_) => Self::Varchar,
            SqlType::Bpchar(_) => Self::Bpchar,
            SqlType::Catalog(CatalogType::Oid) => Self::Oid,
            SqlType::Catalog(CatalogType::Name) => Self::Name,
            SqlType::Catalog(CatalogType::InternalChar) => Self::InternalChar,
            SqlType::Catalog(CatalogType::AclItem) => Self::AclItem,
            SqlType::Date => Self::Date,
            SqlType::Timestamp(_) => Self::Timestamp,
            SqlType::TimestampTz(_) => Self::TimestampTz,
            SqlType::Time(_) => Self::Time,
            SqlType::TimeTz(_) => Self::TimeTz,
            SqlType::Interval { .. } => Self::Interval,
            SqlType::Uuid => Self::Uuid,
            SqlType::Json => Self::Json,
            SqlType::Jsonb => Self::Jsonb,
            SqlType::Bytea => Self::Bytea,
            _ => return None,
        })
    }

    pub(crate) fn required(sql_type: SqlType) -> SqlResult<Self> {
        Self::of(sql_type).ok_or_else(|| unsupported_element(sql_type))
    }

    pub(crate) fn type_name(self) -> String {
        match self {
            Self::Bpchar => "character[]".to_owned(),
            Self::Int2Vector => "int2vector".to_owned(),
            Self::OidVector => "oidvector".to_owned(),
            other => format!("{}[]", other.sql_type().name()),
        }
    }

    pub(crate) fn internal_name(self) -> &'static str {
        match self {
            Self::Int2Vector => "int2vector",
            Self::OidVector => "oidvector",
            other => other.sql_type().internal_name(),
        }
    }

    pub(crate) fn is_vector(self) -> bool {
        matches!(self, Self::Int2Vector | Self::OidVector)
    }

    fn is_string(self) -> bool {
        matches!(
            self,
            Self::Text | Self::Varchar | Self::Name | Self::InternalChar
        )
    }

    pub(crate) fn comparable(self, other: Self) -> bool {
        self == other || (self.is_string() && other.is_string())
    }
}

/// A one-dimensional array: its element type, the subscript of its first element (1 unless
/// written otherwise) and its elements, each NULL or a value of the element type.
#[derive(Debug, Clone, PartialEq)]
pub struct ArrayValue {
    /// The type of every element.
    pub element: ArrayElement,
    /// The subscript of the first element.
    pub lower_bound: i32,
    /// The elements in order.
    pub items: Vec<Value>,
}

impl ArrayValue {
    pub(crate) fn new(element: ArrayElement, items: Vec<Value>) -> Self {
        Self {
            element,
            lower_bound: 1,
            items,
        }
    }

    fn upper_bound(&self) -> Option<i64> {
        if self.items.is_empty() {
            return None;
        }
        Some(i64::from(self.lower_bound) + self.items.len() as i64 - 1)
    }
}

pub(crate) fn held_bytes(array: &ArrayValue) -> usize {
    array
        .items
        .iter()
        .map(|item| ITEM_OVERHEAD.saturating_add(crate::memory::held_bytes(item)))
        .fold(ITEM_OVERHEAD, usize::saturating_add)
}

pub(crate) fn unsupported_element(sql_type: SqlType) -> SqlError {
    SqlError::not_supported(format!(
        "arrays of type {} are not supported yet",
        sql_type.name()
    ))
}

fn too_many_items() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("an array may hold at most {MAX_ITEMS} elements"),
    )
}

fn multidimensional() -> SqlError {
    SqlError::not_supported("multidimensional arrays are not supported yet")
}

struct ArrayBuilder {
    element: ArrayElement,
    items: Vec<Value>,
    bytes: usize,
}

impl ArrayBuilder {
    fn new(element: ArrayElement) -> Self {
        Self {
            element,
            items: Vec::new(),
            bytes: ITEM_OVERHEAD,
        }
    }

    fn push(&mut self, value: Value) -> SqlResult<()> {
        if matches!(value, Value::Array(_)) {
            return Err(multidimensional());
        }
        if self.items.len() >= MAX_ITEMS {
            return Err(too_many_items());
        }
        let value = element_value(value, self.element, Conversion::Assignment)?;
        let bytes = self
            .bytes
            .saturating_add(ITEM_OVERHEAD)
            .saturating_add(crate::memory::held_bytes(&value));
        crate::memory::check_value_bytes(bytes)?;
        self.bytes = bytes;
        self.items.push(value);
        Ok(())
    }

    fn finish(self) -> ArrayValue {
        ArrayValue::new(self.element, self.items)
    }
}

fn element_value(value: Value, element: ArrayElement, conversion: Conversion) -> SqlResult<Value> {
    match (value, element) {
        (Value::Null, _) => Ok(Value::Null),
        (
            Value::Text(text),
            ArrayElement::Text
            | ArrayElement::Varchar
            | ArrayElement::Name
            | ArrayElement::InternalChar,
        ) => Ok(Value::Text(text)),
        (value, element) => convert(value, element.sql_type(), conversion),
    }
}

fn needs_quotes(text: &str) -> bool {
    text.is_empty()
        || text.eq_ignore_ascii_case("null")
        || text.bytes().any(|byte| {
            matches!(
                byte,
                b'"' | b'\\' | b'{' | b'}' | b',' | b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C
            )
        })
}

fn format_items(items: &[Value]) -> String {
    let mut out = String::from("{");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let Some(text) = item.to_text() else {
            out.push_str("NULL");
            continue;
        };
        if !needs_quotes(&text) {
            out.push_str(&text);
            continue;
        }
        out.push('"');
        for character in text.chars() {
            if character == '"' || character == '\\' {
                out.push('\\');
            }
            out.push(character);
        }
        out.push('"');
    }
    out.push('}');
    out
}

pub(crate) fn format_text(array: &ArrayValue) -> String {
    if array.element.is_vector() {
        return array
            .items
            .iter()
            .map(|item| item.to_text().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(" ");
    }
    let items = format_items(&array.items);
    match array.upper_bound() {
        Some(upper) if array.lower_bound != 1 => format!("[{}:{upper}]={items}", array.lower_bound),
        _ => items,
    }
}

fn is_array_space(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}')
}

fn malformed(text: &str, detail: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("malformed array literal: \"{text}\""),
    )
    .with_detail(detail)
}

pub(crate) fn parse_array_text(text: &str, element: ArrayElement) -> SqlResult<ArrayValue> {
    if !element.is_vector() {
        return parse_text(text, element);
    }
    let mut builder = ArrayBuilder::new(element);
    for item in text.split(is_array_space).filter(|item| !item.is_empty()) {
        builder.push(element_from_text(item, element)?)?;
    }
    let mut vector = builder.finish();
    vector.lower_bound = 0;
    Ok(vector)
}

fn element_from_text(text: &str, element: ArrayElement) -> SqlResult<Value> {
    match element {
        ArrayElement::Text
        | ArrayElement::Varchar
        | ArrayElement::Name
        | ArrayElement::InternalChar => Ok(Value::Text(text.to_owned())),
        ArrayElement::Bpchar => Ok(Value::Bpchar(text.to_owned())),
        other => crate::types::parse_input(text, other.sql_type()),
    }
}

pub(crate) fn parse_text(text: &str, element: ArrayElement) -> SqlResult<ArrayValue> {
    let characters: Vec<char> = text.chars().collect();
    let mut at = 0;
    let skip_spaces = |at: &mut usize| {
        while characters.get(*at).copied().is_some_and(is_array_space) {
            *at += 1;
        }
    };
    skip_spaces(&mut at);
    match characters.get(at) {
        Some('[') => {
            return Err(SqlError::not_supported(
                "array bounds such as [1:2]= in an array literal are not supported yet",
            ));
        }
        Some('{') => at += 1,
        _ => {
            return Err(malformed(
                text,
                "Array value must start with \"{\" or dimension information.",
            ));
        }
    }
    let mut builder = ArrayBuilder::new(element);
    skip_spaces(&mut at);
    if characters.get(at) == Some(&'}') {
        at += 1;
    } else {
        loop {
            skip_spaces(&mut at);
            let (item, quoted) = match characters.get(at) {
                None => return Err(malformed(text, "Unexpected end of input.")),
                Some('{') => return Err(multidimensional()),
                Some(',') => return Err(malformed(text, "Unexpected \",\" character.")),
                Some('}') => return Err(malformed(text, "Unexpected \"}\" character.")),
                Some('"') => {
                    at += 1;
                    let mut item = String::new();
                    loop {
                        match characters.get(at) {
                            None => return Err(malformed(text, "Unexpected end of input.")),
                            Some('\\') => {
                                at += 1;
                                let Some(&escaped) = characters.get(at) else {
                                    return Err(malformed(text, "Unexpected end of input."));
                                };
                                item.push(escaped);
                                at += 1;
                            }
                            Some('"') => {
                                at += 1;
                                break;
                            }
                            Some(&other) => {
                                item.push(other);
                                at += 1;
                            }
                        }
                    }
                    skip_spaces(&mut at);
                    (item, true)
                }
                Some(_) => {
                    let mut item = String::new();
                    let mut kept = 0;
                    let mut escaped_any = false;
                    loop {
                        match characters.get(at) {
                            None => return Err(malformed(text, "Unexpected end of input.")),
                            Some(',' | '}') => break,
                            Some('"') => return Err(malformed(text, "Unexpected array element.")),
                            Some('{') => {
                                return Err(malformed(text, "Unexpected \"{\" character."));
                            }
                            Some('\\') => {
                                at += 1;
                                let Some(&escaped) = characters.get(at) else {
                                    return Err(malformed(text, "Unexpected end of input."));
                                };
                                item.push(escaped);
                                kept = item.len();
                                escaped_any = true;
                                at += 1;
                            }
                            Some(&other) => {
                                item.push(other);
                                if !is_array_space(other) {
                                    kept = item.len();
                                }
                                at += 1;
                            }
                        }
                    }
                    item.truncate(kept);
                    (item, escaped_any)
                }
            };
            match characters.get(at) {
                Some(',') | Some('}') => {}
                None => return Err(malformed(text, "Unexpected end of input.")),
                Some(_) => return Err(malformed(text, "Unexpected array element.")),
            }
            let value = if !quoted && item.eq_ignore_ascii_case("null") {
                Value::Null
            } else {
                element_from_text(&item, element)?
            };
            builder.push(value)?;
            let closing = characters.get(at) == Some(&'}');
            at += 1;
            if closing {
                break;
            }
        }
    }
    skip_spaces(&mut at);
    if at < characters.len() {
        return Err(malformed(text, "Junk after closing right brace."));
    }
    Ok(builder.finish())
}

pub(crate) fn convert_to(
    value: Value,
    element: ArrayElement,
    conversion: Conversion,
) -> SqlResult<Value> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Array(array) => {
            let mut builder = ArrayBuilder::new(element);
            for item in array.items {
                let converted = element_value(item, element, conversion)?;
                builder.push(converted)?;
            }
            let mut converted = builder.finish();
            converted.lower_bound = array.lower_bound;
            Ok(Value::Array(Box::new(converted)))
        }
        Value::Text(text) if conversion == Conversion::Explicit => {
            parse_array_text(&text, element).map(|array| Value::Array(Box::new(array)))
        }
        Value::Bpchar(text) if conversion == Conversion::Explicit => {
            parse_array_text(crate::types::without_trailing_spaces(&text), element)
                .map(|array| Value::Array(Box::new(array)))
        }
        other => Err(SqlError::new(
            SqlState::CANNOT_COERCE,
            format!(
                "cannot cast type {} to {}",
                other.type_name(),
                element.type_name()
            ),
        )),
    }
}

pub(crate) fn compare(left: &ArrayValue, right: &ArrayValue) -> Ordering {
    for (a, b) in left.items.iter().zip(&right.items) {
        let ordering = match (a.is_null(), b.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => compare_values(a, b),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.items
        .len()
        .cmp(&right.items.len())
        .then(left.lower_bound.cmp(&right.lower_bound))
}

pub(crate) fn equality_key_of(out: &mut Vec<u8>, array: &ArrayValue) {
    out.push(15);
    out.extend_from_slice(&(array.items.len() as u64).to_le_bytes());
    out.extend_from_slice(&array.lower_bound.to_le_bytes());
    for item in &array.items {
        equality_key(out, item);
    }
}

pub(crate) fn encode_stored(array: &ArrayValue) -> Vec<u8> {
    let text = format_items(&array.items);
    let mut out = Vec::with_capacity(4 + text.len());
    out.extend_from_slice(&array.lower_bound.to_le_bytes());
    out.extend_from_slice(text.as_bytes());
    out
}

pub(crate) fn decode_stored(bytes: &[u8], element: ArrayElement) -> Option<ArrayValue> {
    let (bound, text) = bytes.split_first_chunk::<4>()?;
    let text = std::str::from_utf8(text).ok()?;
    let mut array = parse_text(text, element).ok()?;
    array.lower_bound = i32::from_le_bytes(*bound);
    Some(array)
}

fn array_of(value: &Value) -> Option<&ArrayValue> {
    match value {
        Value::Array(array) => Some(array),
        _ => None,
    }
}

fn dimension_is_one(value: &Value) -> Option<bool> {
    value.as_integer().map(|dimension| dimension == 1)
}

fn integer(value: i64) -> SqlResult<Value> {
    crate::types::integer_value(value, SqlType::Integer)
}

fn joined(array: &ArrayValue, separator: &str, null_text: Option<&str>) -> SqlResult<Value> {
    let mut out = String::new();
    let mut first = true;
    for item in &array.items {
        let text = match item.to_text() {
            Some(text) => text,
            None => match null_text {
                Some(shown) => shown.to_owned(),
                None => continue,
            },
        };
        let separator_bytes = if first { 0 } else { separator.len() };
        crate::memory::check_value_bytes(
            out.len()
                .saturating_add(separator_bytes)
                .saturating_add(text.len()),
        )?;
        if !first {
            out.push_str(separator);
        }
        out.push_str(&text);
        first = false;
    }
    Ok(Value::Text(out))
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let known = matches!(
        name,
        "array_to_string"
            | "array_length"
            | "array_upper"
            | "array_lower"
            | "array_ndims"
            | "cardinality"
            | "array_subscript"
    );
    if !known {
        return None;
    }
    if values.first().is_some_and(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    let array = array_of(values.first()?)?;
    Some(match (name, &values[1..]) {
        ("array_to_string", [Value::Null] | [Value::Null, _]) => Ok(Value::Null),
        ("array_to_string", [separator]) => joined(array, separator.character_text()?, None),
        ("array_to_string", [separator, null_text]) => joined(
            array,
            separator.character_text()?,
            null_text.character_text(),
        ),
        ("array_length" | "array_upper" | "array_lower", [dimension]) => {
            match dimension_is_one(dimension) {
                Some(true) if !array.items.is_empty() => match name {
                    "array_length" => integer(array.items.len() as i64),
                    "array_upper" => integer(array.upper_bound().unwrap_or(0)),
                    _ => integer(i64::from(array.lower_bound)),
                },
                Some(_) => Ok(Value::Null),
                None if dimension.is_null() => Ok(Value::Null),
                None => return None,
            }
        }
        ("array_ndims", []) if array.items.is_empty() => Ok(Value::Null),
        ("array_ndims", []) => integer(1),
        ("cardinality", []) => integer(array.items.len() as i64),
        ("array_subscript", [Value::Null]) => Ok(Value::Null),
        ("array_subscript", [subscript]) => subscripted(array, subscript),
        _ => return None,
    })
}

fn subscripted(array: &ArrayValue, subscript: &Value) -> SqlResult<Value> {
    let whole = match subscript {
        Value::Numeric(number) => number.to_i128_rounded(),
        other => match other.as_integer() {
            Some(whole) => Some(i128::from(whole)),
            None => {
                return Err(SqlError::new(
                    SqlState::DATATYPE_MISMATCH,
                    "array subscript must have type integer",
                ));
            }
        },
    };
    let subscript = whole
        .and_then(|whole| i32::try_from(whole).ok())
        .ok_or_else(|| crate::types::out_of_range(SqlType::Integer))?;
    let index = i64::from(subscript) - i64::from(array.lower_bound);
    Ok(usize::try_from(index)
        .ok()
        .and_then(|index| array.items.get(index))
        .cloned()
        .unwrap_or(Value::Null))
}

fn is_unknown(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::Literal(Literal::String(_) | Literal::Null)
    )
}

fn common_element_type(arguments: &[Expr], scope: Scope<'_>) -> Option<Option<SqlType>> {
    let mut common: Option<SqlType> = None;
    for argument in arguments {
        if is_unknown(argument) {
            continue;
        }
        let found = crate::typing::static_type(argument, scope)?;
        common = Some(match common {
            None => found,
            Some(known) => crate::set_operations::common_type(known, found)?,
        });
    }
    Some(common)
}

pub(crate) fn construct(
    evaluator: &Evaluator<'_>,
    arguments: &[Expr],
    target: Option<ArrayElement>,
) -> SqlResult<Value> {
    let element = match target {
        Some(element) => element,
        None => {
            let sql_type = match common_element_type(arguments, evaluator.scope) {
                Some(Some(sql_type)) => sql_type,
                Some(None) if !arguments.is_empty() => SqlType::Text,
                Some(None) => {
                    return Err(SqlError::new(
                        SqlState::INDETERMINATE_DATATYPE,
                        "cannot determine type of empty array",
                    )
                    .with_hint(
                        "Explicitly cast to the desired type, for example ARRAY[]::integer[].",
                    ));
                }
                None => {
                    return construct_from_values(evaluator, arguments);
                }
            };
            if let SqlType::Array(_) = sql_type {
                return Err(multidimensional());
            }
            ArrayElement::required(sql_type)?
        }
    };
    let mut builder = ArrayBuilder::new(element);
    for argument in arguments {
        let value = evaluator.eval(argument, Some(element.sql_type()))?;
        builder.push(value)?;
    }
    Ok(Value::Array(Box::new(builder.finish())))
}

pub(crate) fn cast_constructor(
    evaluator: &Evaluator<'_>,
    constructor: &Expr,
    element: ArrayElement,
) -> Option<SqlResult<Value>> {
    let ExprKind::Function { arguments, .. } = &constructor.kind else {
        return None;
    };
    if arguments.iter().all(is_unknown) {
        return Some(construct(evaluator, arguments, Some(element)));
    }
    Some(
        construct(evaluator, arguments, None)
            .and_then(|built| convert(built, SqlType::Array(element), Conversion::Explicit)),
    )
}

fn construct_from_values(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Value> {
    let mut values = Vec::with_capacity(arguments.len());
    let mut common: Option<SqlType> = None;
    let mut held = crate::memory::RowBytes::default();
    for argument in arguments {
        let value = evaluator.eval(argument, common)?;
        held.add(&value)?;
        if let Some(found) = value.sql_type() {
            common = Some(match common {
                None => found,
                Some(known) => {
                    crate::set_operations::common_type(known, found).ok_or_else(|| {
                        SqlError::new(
                            SqlState::DATATYPE_MISMATCH,
                            format!(
                                "ARRAY types {} and {} cannot be matched",
                                known.name(),
                                found.name()
                            ),
                        )
                    })?
                }
            });
        }
        values.push(value);
    }
    let sql_type = common.unwrap_or(SqlType::Text);
    if let SqlType::Array(_) = sql_type {
        return Err(multidimensional());
    }
    let mut builder = ArrayBuilder::new(ArrayElement::required(sql_type)?);
    for value in values {
        builder.push(value)?;
    }
    Ok(Value::Array(Box::new(builder.finish())))
}

pub(crate) fn from_rows(values: Vec<Value>, sql_type: Option<SqlType>) -> SqlResult<Value> {
    let sql_type = sql_type
        .or_else(|| values.iter().find_map(Value::sql_type))
        .unwrap_or(SqlType::Text);
    if let SqlType::Array(_) = sql_type {
        return Err(multidimensional());
    }
    let mut builder = ArrayBuilder::new(ArrayElement::required(sql_type)?);
    for value in values {
        builder.push(value)?;
    }
    Ok(Value::Array(Box::new(builder.finish())))
}

fn requires_array(position: usize) -> SqlError {
    SqlError::new(
        SqlState::WRONG_OBJECT_TYPE,
        "op ANY/ALL (array) requires array on right side",
    )
    .at(position)
}

pub(crate) fn quantified(
    evaluator: &Evaluator<'_>,
    arguments: &[Expr],
    all: bool,
) -> SqlResult<Value> {
    let [left, array, operator] = arguments else {
        return Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "a quantified comparison is missing an operand",
        ));
    };
    let ExprKind::Literal(Literal::String(symbol)) = &operator.kind else {
        return Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "a quantified comparison is missing its operator",
        ));
    };
    let (left_value, array_value) = if is_unknown(left) && !is_unknown(array) {
        let array_value = evaluator.eval(array, None)?;
        let hint = array_of(&array_value).map(|found| found.element.sql_type());
        (evaluator.eval(left, hint)?, array_value)
    } else {
        let left_value = evaluator.eval(left, None)?;
        let hint = left_value
            .sql_type()
            .or_else(|| evaluator.declared_type(left))
            .and_then(ArrayElement::of)
            .map(SqlType::Array);
        (left_value, evaluator.eval(array, hint)?)
    };
    let items = match &array_value {
        Value::Null => return Ok(Value::Null),
        Value::Array(found) => &found.items,
        _ => return Err(requires_array(array.position)),
    };
    let mut result = Some(all);
    for item in items {
        let truth = compare_truth(&left_value, item, symbol)?;
        result = if all {
            and(result, truth)
        } else {
            or(result, truth)
        };
        if result == Some(!all) {
            break;
        }
    }
    Ok(truth_value(result))
}

fn no_concatenation(left: &str, right: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("operator does not exist: {left} || {right}"),
    )
    .with_hint(
        "No operator matches the given name and argument types. You might need to add explicit type casts.",
    )
}

#[derive(Clone, Copy)]
enum Operand {
    Array(ArrayElement),
    Element(SqlType),
    Unknown,
}

impl Operand {
    fn of(value: &Value, declared: impl FnOnce() -> Option<SqlType>) -> Self {
        match value {
            Value::Array(array) => Self::Array(array.element),
            Value::Null => match declared() {
                Some(SqlType::Array(element)) => Self::Array(element),
                Some(sql_type) => Self::Element(sql_type),
                None => Self::Unknown,
            },
            value => value
                .sql_type()
                .or_else(declared)
                .map_or(Self::Unknown, Self::Element),
        }
    }

    fn of_type(sql_type: Option<SqlType>) -> Self {
        match sql_type {
            Some(SqlType::Array(element)) => Self::Array(element),
            Some(sql_type) => Self::Element(sql_type),
            None => Self::Unknown,
        }
    }

    fn shown(self) -> String {
        match self {
            Self::Array(element) => element.type_name(),
            Self::Element(sql_type) => sql_type.name(),
            Self::Unknown => "unknown".to_owned(),
        }
    }
}

fn joined_element(first: ArrayElement, second: ArrayElement) -> Option<ArrayElement> {
    let (first_type, second_type) = (first.sql_type(), second.sql_type());
    if first == second || (first_type.is_character() && second_type.is_character()) {
        return Some(first);
    }
    if first_type.is_numeric() && second_type.is_numeric() {
        return crate::set_operations::common_type(first_type, second_type)
            .and_then(ArrayElement::of);
    }
    None
}

fn concatenated_element(left: Operand, right: Operand) -> Option<Option<ArrayElement>> {
    let element_of = |sql_type: SqlType| ArrayElement::of(sql_type);
    Some(match (left, right) {
        (Operand::Array(left), Operand::Array(right)) => joined_element(left, right),
        (Operand::Array(array), Operand::Element(element)) => {
            element_of(element).and_then(|element| joined_element(array, element))
        }
        (Operand::Element(element), Operand::Array(array)) => {
            element_of(element).and_then(|element| joined_element(element, array))
        }
        (Operand::Array(array), Operand::Unknown) | (Operand::Unknown, Operand::Array(array)) => {
            Some(array)
        }
        _ => return None,
    })
}

pub(crate) fn concatenation(
    evaluator: &Evaluator<'_>,
    (left, left_concatenated): (&Expr, Concatenated),
    (right, right_concatenated): (&Expr, Concatenated),
) -> SqlResult<Concatenated> {
    let operand = |expr: &Expr, concatenated: &Concatenated| {
        if is_unknown(expr) {
            return Operand::Unknown;
        }
        Operand::of(&concatenated.value, || match concatenated.array_element {
            Some(element) => Some(SqlType::Array(element)),
            None => evaluator
                .declared_type(expr)
                .or_else(|| crate::typing::static_type(expr, evaluator.scope)),
        })
    };
    let left_operand = operand(left, &left_concatenated);
    let right_operand = operand(right, &right_concatenated);
    let (left_value, right_value) = (left_concatenated.value, right_concatenated.value);
    let array_type = match (left_operand, right_operand) {
        (Operand::Array(element), _) | (_, Operand::Array(element)) => SqlType::Array(element),
        _ => {
            return Err(no_concatenation(
                &left_operand.shown(),
                &right_operand.shown(),
            ));
        }
    };
    let reread = |expr: &Expr, value: Value| {
        if is_unknown(expr) && !value.is_null() {
            evaluator.eval(expr, Some(array_type))
        } else {
            Ok(value)
        }
    };
    let left_value = reread(left, left_value)?;
    let right_value = reread(right, right_value)?;
    let left_operand = match left_operand {
        Operand::Unknown => Operand::of(&left_value, || None),
        known => known,
    };
    let right_operand = match right_operand {
        Operand::Unknown => Operand::of(&right_value, || None),
        known => known,
    };
    let Some(Some(element)) = concatenated_element(left_operand, right_operand) else {
        return Err(no_concatenation(
            &left_operand.shown(),
            &right_operand.shown(),
        ));
    };
    concatenate(
        element,
        (left_operand, left_value),
        (right_operand, right_value),
    )
    .map(|value| Concatenated {
        value,
        array_element: Some(element),
    })
}

fn concatenate(
    element: ArrayElement,
    left: (Operand, Value),
    right: (Operand, Value),
) -> SqlResult<Value> {
    let is_array = |operand: Operand| matches!(operand, Operand::Array(_) | Operand::Unknown);
    if left.1.is_null() && right.1.is_null() && is_array(left.0) && is_array(right.0) {
        return Ok(Value::Null);
    }
    let lower_bound = match (&left.1, &right.1) {
        (Value::Array(array), _) if !array.items.is_empty() => array.lower_bound,
        (_, Value::Array(array)) => array.lower_bound,
        _ => 1,
    };
    let mut builder = ArrayBuilder::new(element);
    for (operand, value) in [left, right] {
        match value {
            Value::Array(array) if array.element == element && builder.items.is_empty() => {
                builder.bytes = held_bytes(&array);
                builder.items = array.items;
            }
            Value::Array(array) => {
                for item in array.items {
                    builder.push(item)?;
                }
            }
            Value::Null if is_array(operand) => {}
            value => builder.push(value)?,
        }
    }
    let mut joined = builder.finish();
    joined.lower_bound = lower_bound;
    Ok(Value::Array(Box::new(joined)))
}

pub(crate) fn function_type(
    name: &str,
    arguments: &[Expr],
    scope: Scope<'_>,
) -> Option<Option<SqlType>> {
    Some(match name {
        "array_constructor" => match common_element_type(arguments, scope) {
            Some(Some(sql_type)) => ArrayElement::of(sql_type).map(SqlType::Array),
            Some(None) if !arguments.is_empty() => Some(SqlType::Array(ArrayElement::Text)),
            _ => None,
        },
        "array_element_cast" => arguments
            .first()
            .and_then(|array| crate::typing::static_type(array, scope)),
        "array_subscript" => match arguments
            .first()
            .and_then(|array| crate::typing::static_type(array, scope))
        {
            Some(SqlType::Array(element)) => Some(element.sql_type()),
            _ => None,
        },
        "array_any" | "array_all" => Some(SqlType::Boolean),
        "array_to_string" => Some(SqlType::Text),
        "array_length" | "array_upper" | "array_lower" | "array_ndims" | "cardinality" => {
            Some(SqlType::Integer)
        }
        _ => return None,
    })
}

pub(crate) fn element_cast(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Value> {
    let [array, element_target] = arguments else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            "function array_element_cast() does not exist",
        ));
    };
    let ExprKind::Cast {
        sql_type: element_type,
        ..
    } = element_target.kind
    else {
        return evaluator.eval(array, None);
    };
    match evaluator.eval(array, None)? {
        Value::Array(mut array) => {
            for item in &mut array.items {
                if !item.is_null() {
                    *item = convert(
                        std::mem::replace(item, Value::Null),
                        element_type,
                        Conversion::Explicit,
                    )?;
                }
            }
            Ok(Value::Array(array))
        }
        other => Ok(other),
    }
}

pub(crate) fn concat_type(left: Option<SqlType>, right: Option<SqlType>) -> Option<SqlType> {
    let (left, right) = (Operand::of_type(left), Operand::of_type(right));
    let first_array = match (left, right) {
        (Operand::Array(element), _) | (_, Operand::Array(element)) => element,
        _ => return None,
    };
    let element = concatenated_element(left, right)
        .flatten()
        .unwrap_or(first_array);
    Some(SqlType::Array(element))
}
