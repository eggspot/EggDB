use crate::arrays::ArrayValue;
use crate::set_returning::RowSink;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{
    Conversion, DefinedKind, DefinedType, DefinedValue, SqlType, Value, compare_values,
};
use crate::user_types::Attribute;
use std::cmp::Ordering;

pub(crate) const RECORD_OID: u32 = 2249;
pub(crate) const ROW_FUNCTION: &str = "*row*";
pub(crate) const FIELD_FUNCTION: &str = "*field*";
pub(crate) const EXPAND_ARRAY_FUNCTION: &str = "_pg_expandarray";
pub(crate) const EXPANDED_FIELDS: [&str; 2] = ["x", "n"];
const MAX_FIELDS: usize = crate::encoding::MAX_COLUMNS;

/// A row of a composite type or a `record`: its type, its fields' values in order and, for a
/// `record`, its fields' names. A named composite type's fields are named by its definition.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordValue {
    /// The OID of the composite type, or 2249 for `record`.
    pub type_oid: u32,
    /// The fields' values.
    pub fields: Vec<Value>,
    /// The fields' names for a `record`; empty for a named composite type.
    pub names: Vec<String>,
}

pub(crate) fn record_value(type_oid: u32, fields: Vec<Value>, names: Vec<String>) -> Value {
    Value::Defined(Box::new(DefinedValue::Record(RecordValue {
        type_oid,
        fields,
        names,
    })))
}

pub(crate) fn anonymous(fields: Vec<Value>) -> Value {
    let names = (1..=fields.len()).map(|at| format!("f{at}")).collect();
    record_value(RECORD_OID, fields, names)
}

fn needs_quotes(text: &str) -> bool {
    text.is_empty()
        || text
            .chars()
            .any(|c| matches!(c, '"' | '\\' | '(' | ')' | ',') || c.is_ascii_whitespace())
}

pub(crate) fn format_text(record: &RecordValue) -> String {
    let mut out = String::from("(");
    for (at, field) in record.fields.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        let Some(text) = field.to_text() else {
            continue;
        };
        if needs_quotes(&text) {
            out.push('"');
            for character in text.chars() {
                if character == '"' || character == '\\' {
                    out.push(character);
                }
                out.push(character);
            }
            out.push('"');
        } else {
            out.push_str(&text);
        }
    }
    out.push(')');
    out
}

fn malformed(text: &str, detail: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("malformed record literal: \"{text}\""),
    )
    .with_detail(detail.to_owned())
}

fn split_fields(text: &str) -> SqlResult<Vec<Option<String>>> {
    let mut characters = text.chars().peekable();
    while characters.next_if(|c| c.is_ascii_whitespace()).is_some() {}
    if characters.next() != Some('(') {
        return Err(malformed(text, "Missing left parenthesis."));
    }
    let mut fields: Vec<Option<String>> = Vec::new();
    loop {
        if fields.len() >= MAX_FIELDS {
            return Err(malformed(text, "Too many columns."));
        }
        match characters.peek() {
            Some(',') | Some(')') => fields.push(None),
            None => return Err(malformed(text, "Unexpected end of input.")),
            Some(_) => {
                let mut field = String::new();
                let mut quoted = false;
                loop {
                    match characters.peek().copied() {
                        None => return Err(malformed(text, "Unexpected end of input.")),
                        Some('"') => {
                            characters.next();
                            if quoted && characters.peek() == Some(&'"') {
                                characters.next();
                                field.push('"');
                            } else {
                                quoted = !quoted;
                            }
                        }
                        Some('\\') => {
                            characters.next();
                            match characters.next() {
                                Some(escaped) => field.push(escaped),
                                None => return Err(malformed(text, "Unexpected end of input.")),
                            }
                        }
                        Some(',' | ')') if !quoted => break,
                        Some(other) => {
                            characters.next();
                            field.push(other);
                        }
                    }
                }
                fields.push(Some(field));
            }
        }
        match characters.next() {
            Some(',') => {}
            Some(')') => break,
            _ => return Err(malformed(text, "Unexpected end of input.")),
        }
    }
    if characters.any(|c| !c.is_ascii_whitespace()) {
        return Err(malformed(text, "Junk after right parenthesis."));
    }
    Ok(fields)
}

pub(crate) fn parse_text(text: &str, type_oid: u32, attributes: &[Attribute]) -> SqlResult<Value> {
    let fields = split_fields(text)?;
    if fields.len() < attributes.len() {
        return Err(malformed(text, "Too few columns."));
    }
    if fields.len() > attributes.len() {
        return Err(malformed(text, "Too many columns."));
    }
    let mut values = Vec::with_capacity(fields.len());
    let mut held = crate::memory::RowBytes::default();
    for (field, attribute) in fields.into_iter().zip(attributes) {
        let value = match field {
            None => Value::Null,
            Some(field) => crate::types::parse_input(&field, attribute.sql_type)?,
        };
        held.add(&value)?;
        values.push(value);
    }
    Ok(record_value(type_oid, values, Vec::new()))
}

pub(crate) fn compare(left: &RecordValue, right: &RecordValue) -> Ordering {
    for (a, b) in left.fields.iter().zip(&right.fields) {
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
    left.fields.len().cmp(&right.fields.len())
}

pub(crate) fn field_names(record: &RecordValue) -> Vec<String> {
    if !record.names.is_empty() {
        return record.names.clone();
    }
    crate::user_types::composite_attributes(record.type_oid)
        .map(|attributes| attributes.into_iter().map(|attribute| attribute.name).collect())
        .unwrap_or_default()
}

fn missing_field(name: &str, type_name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_COLUMN,
        format!("column \"{name}\" not found in data type {type_name}"),
    )
}

pub(crate) fn field(value: &Value, name: &str) -> SqlResult<Value> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Record(record) => {
                let names = field_names(record);
                let at = names
                    .iter()
                    .position(|known| known == name)
                    .ok_or_else(|| missing_field(name, &value.type_name()))?;
                Ok(record.fields.get(at).cloned().unwrap_or(Value::Null))
            }
            _ => Err(missing_field(name, &value.type_name())),
        },
        other => Err(SqlError::new(
            SqlState::UNDEFINED_COLUMN,
            format!(
                "column notation .{name} applied to type {}, which is not a composite type",
                other.type_name()
            ),
        )),
    }
}

pub(crate) fn field_type(record_type: Option<SqlType>, name: &str) -> Option<SqlType> {
    let defined = record_type?.defined()?;
    if defined.kind != DefinedKind::Composite || defined.oid == RECORD_OID {
        return None;
    }
    crate::user_types::composite_attributes(defined.oid)?
        .into_iter()
        .find(|attribute| attribute.name == name)
        .map(|attribute| attribute.sql_type)
}

pub(crate) fn converted(
    record: &RecordValue,
    target_oid: u32,
    conversion: Conversion,
) -> Option<SqlResult<Value>> {
    if target_oid == RECORD_OID {
        return Some(Ok(record_value(
            RECORD_OID,
            record.fields.clone(),
            field_names(record),
        )));
    }
    let attributes = crate::user_types::composite_attributes(target_oid)?;
    if attributes.len() != record.fields.len() {
        return None;
    }
    let mut fields = Vec::with_capacity(attributes.len());
    for (field, attribute) in record.fields.iter().zip(&attributes) {
        match crate::types::convert(field.clone(), attribute.sql_type, conversion) {
            Ok(value) => fields.push(value),
            Err(error) => return Some(Err(error)),
        }
    }
    Some(Ok(record_value(target_oid, fields, Vec::new())))
}

pub(crate) fn expanded_array_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    match types {
        [Some(SqlType::Array(_)) | None] => Ok(SqlType::Defined(DefinedType::RECORD)),
        [Some(SqlType::Catalog(crate::pg_catalog::CatalogType::AnyArray))] => {
            Ok(SqlType::Defined(DefinedType::RECORD))
        }
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            "function information_schema._pg_expandarray does not exist for this argument",
        )
        .with_hint("It takes one array.")),
    }
}

pub(crate) fn expanded_array_rows(
    values: &[Value],
    _: SqlType,
    sink: &mut RowSink<'_>,
) -> SqlResult<()> {
    let Some(Value::Array(array)) = values.first() else {
        return Ok(());
    };
    expand(array, sink)
}

fn expand(array: &ArrayValue, sink: &mut RowSink<'_>) -> SqlResult<()> {
    for (at, item) in array.items.iter().enumerate() {
        let position = i32::try_from(at + 1).map_err(|_| {
            SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                "an array may hold at most 2147483647 elements",
            )
        })?;
        sink(record_value(
            RECORD_OID,
            vec![item.clone(), Value::Integer(position)],
            EXPANDED_FIELDS.iter().map(|name| (*name).to_owned()).collect(),
        ))?;
    }
    Ok(())
}

pub(crate) fn expanded_field_type(element: Option<SqlType>, field: &str) -> Option<SqlType> {
    match field {
        "x" => match element? {
            SqlType::Array(element) => Some(element.sql_type()),
            _ => None,
        },
        "n" => Some(SqlType::Integer),
        _ => None,
    }
}

pub(crate) fn construct(fields: Vec<Value>) -> SqlResult<Value> {
    if fields.len() > MAX_FIELDS {
        return Err(SqlError::new(
            SqlState::TOO_MANY_COLUMNS,
            format!("ROW expressions can have at most {MAX_FIELDS} entries"),
        ));
    }
    Ok(anonymous(fields))
}

#[cfg(test)]
mod tests {
    use super::{RECORD_OID, RecordValue, format_text, split_fields};
    use crate::types::Value;

    #[test]
    fn records_print_and_read_back_like_postgresql() {
        let record = RecordValue {
            type_oid: RECORD_OID,
            fields: vec![
                Value::Integer(1),
                Value::Text("a b".into()),
                Value::Null,
                Value::Text(String::new()),
                Value::Text("say \"hi\"\\".into()),
            ],
            names: Vec::new(),
        };
        let text = format_text(&record);
        assert_eq!(text, "(1,\"a b\",,\"\",\"say \"\"hi\"\"\\\\\")");
        let fields = split_fields(&text).unwrap();
        assert_eq!(
            fields,
            vec![
                Some("1".to_owned()),
                Some("a b".to_owned()),
                None,
                Some(String::new()),
                Some("say \"hi\"\\".to_owned()),
            ]
        );
    }

    #[test]
    fn malformed_records_are_errors_never_panics() {
        for text in ["", "1,2", "(1,2", "(1,2) x", "(\"open", "(\\", "((((", "(,,,"] {
            assert!(split_fields(text).is_err(), "{text}");
        }
        for text in ["()", " (1) ", "(,)", "(\"\")"] {
            assert!(split_fields(text).is_ok(), "{text}");
        }
    }
}
