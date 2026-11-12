use crate::arrays::{ArrayElement, ArrayValue};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{self, Json, Node};
use crate::json_functions::RecordSink;
use crate::session::OutputColumn;
use crate::types::{Conversion, DefinedKind, DefinedType, SqlType, Value, convert};

/// The functions that turn a JSON object (or an array of objects) into rows whose columns the
/// caller declares in a column definition list: `AS x(a int, b text)`.
pub(crate) const FUNCTIONS: [&str; 9] = [
    "read_parquet",
    "read_bam",
    "read_vcf",
    "read_arrow",
    "read_csv",
    "json_to_record",
    "jsonb_to_record",
    "json_to_recordset",
    "jsonb_to_recordset",
];

/// Whether `name` is one of [`FUNCTIONS`].
pub(crate) fn is_to_record(name: &str) -> bool {
    FUNCTIONS.contains(&name)
}

/// The error for a record-returning function called without a column definition list.
pub(crate) fn definition_list_required() -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        "a column definition list is required for functions returning \"record\"",
    )
}

/// Checks the arguments: a document followed by one typed placeholder per declared column.
pub(crate) fn result_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    let Some((document, columns)) = types.split_first() else {
        return Err(definition_list_required());
    };
    let accepted = document.is_none_or(|sql_type| {
        matches!(sql_type, SqlType::Json | SqlType::Jsonb) || sql_type.is_character()
    });
    if !accepted {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            "function json_to_record does not exist",
        ));
    }
    if columns.is_empty() {
        return Err(definition_list_required());
    }
    Ok(SqlType::Text)
}

/// The declared columns, each with its declared type.
pub(crate) fn columns(names: &[String], types: &[Option<SqlType>]) -> Vec<OutputColumn> {
    names
        .iter()
        .zip(types.iter().skip(1))
        .map(|(name, sql_type)| OutputColumn {
            name: name.clone(),
            sql_type: *sql_type,
        })
        .collect()
}

fn not_an_object(name: &str, found: &Json) -> SqlError {
    let what = match found {
        Json::Array(_) => "an array",
        _ => "a scalar",
    };
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("cannot call {name} on {what}"),
    )
}

fn not_an_array_of_objects(name: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("argument of {name} must be an array of objects"),
    )
}

/// Produces the rows of `name`: one for an object (`*_to_record`), one per element of an array
/// of objects (`*_to_recordset`), each column read from the member with the column's name.
pub(crate) fn rows(
    name: &str,
    values: &[Value],
    names: &[String],
    types: &[Option<SqlType>],
    sink: &mut RecordSink<'_>,
) -> SqlResult<()> {
    if name == "read_parquet" || name == "read_arrow" {
        return crate::file_readers::rows(values, names, types, sink, name == "read_arrow");
    }
    if name == "read_csv" {
        return crate::file_readers::csv_rows(values, names, types, sink);
    }
    if name == "read_vcf" {
        return crate::file_readers::vcf_rows(values, names, types, sink);
    }
    if name == "read_bam" {
        return crate::file_readers::bam_rows(values, names, types, sink);
    }
    let Some(document) = values.first() else {
        return Err(definition_list_required());
    };
    let text = match document {
        Value::Null => return Ok(()),
        Value::Json(text) | Value::Jsonb(text) | Value::Text(text) => text.as_str(),
        _ => return Err(definition_list_required()),
    };
    let binary = name.starts_with("jsonb");
    let parsed = json::parse(text, binary)?;
    let declared: Vec<(&str, Option<SqlType>)> = names
        .iter()
        .map(String::as_str)
        .zip(types.iter().skip(1).copied())
        .collect();
    let record = |object: &Node| row(text, object, &declared, binary);
    if name.ends_with("set") {
        let Json::Array(elements) = &parsed.value else {
            return Err(not_an_array_of_objects(name));
        };
        for element in elements {
            if !matches!(element.value, Json::Object(_)) {
                return Err(not_an_array_of_objects(name));
            }
            sink(record(element)?)?;
        }
        return Ok(());
    }
    if !matches!(parsed.value, Json::Object(_)) {
        return Err(not_an_object(name, &parsed.value));
    }
    sink(record(&parsed)?)
}

fn row(
    document: &str,
    object: &Node,
    declared: &[(&str, Option<SqlType>)],
    binary: bool,
) -> SqlResult<Vec<Value>> {
    let Json::Object(members) = &object.value else {
        return Ok(vec![Value::Null; declared.len()]);
    };
    declared
        .iter()
        .map(|(column, sql_type)| {
            let found = members
                .iter()
                .rev()
                .find(|(key, _)| key == column)
                .map(|(_, node)| node);
            match found {
                None => Ok(Value::Null),
                Some(node) => field(document, node, *sql_type, binary),
            }
        })
        .collect()
}

fn source_text(document: &str, node: &Node, binary: bool) -> SqlResult<String> {
    if binary {
        return json::normalized_value(&node.value);
    }
    match document.get(node.start..node.end) {
        Some(text) => Ok(text.to_owned()),
        None => json::normalized_value(&node.value),
    }
}

/// Reads one member as a column of `sql_type`, through the type's text input, as PostgreSQL's
/// `populate_record` machinery does.
pub(crate) fn field(
    document: &str,
    node: &Node,
    sql_type: Option<SqlType>,
    binary: bool,
) -> SqlResult<Value> {
    match (&node.value, sql_type) {
        (Json::Null, _) => Ok(Value::Null),
        (_, Some(SqlType::Json)) => Ok(Value::Json(source_text(document, node, binary)?)),
        (_, Some(SqlType::Jsonb)) => Ok(Value::Jsonb(json::normalized_value(&node.value)?)),
        (Json::Array(elements), Some(SqlType::Array(element))) => {
            array(document, elements, element, binary)
        }
        (Json::Object(members), Some(SqlType::Defined(defined)))
            if defined.kind == DefinedKind::Composite =>
        {
            composite(document, members, defined, binary)
        }
        (Json::String(text), target) => typed(Value::Text(text.clone()), target),
        (Json::Number(text), target) => typed(Value::Text(text.clone()), target),
        (Json::Boolean(flag), target) => typed(
            Value::Text(if *flag { "true" } else { "false" }.to_owned()),
            target,
        ),
        (Json::Array(_) | Json::Object(_), target) => {
            typed(Value::Text(source_text(document, node, binary)?), target)
        }
    }
}

fn array(
    document: &str,
    elements: &[Node],
    element: ArrayElement,
    binary: bool,
) -> SqlResult<Value> {
    let nested = elements
        .iter()
        .filter(|node| matches!(node.value, Json::Array(_)))
        .count();
    if nested > 0 && nested == elements.len() {
        return Err(SqlError::not_supported(
            "multidimensional arrays are not supported yet",
        ));
    }
    if nested > 0 {
        return Err(
            SqlError::new(SqlState::INVALID_TEXT_REPRESENTATION, "expected JSON array")
                .with_detail("Every element of a one-dimensional array must be a scalar."),
        );
    }
    let items = elements
        .iter()
        .map(|node| field(document, node, Some(element.sql_type()), binary))
        .collect::<SqlResult<Vec<Value>>>()?;
    crate::arrays::convert_to(
        Value::Array(Box::new(ArrayValue::new(element, items))),
        element,
        Conversion::Assignment,
    )
}

fn composite(
    document: &str,
    members: &[(String, Node)],
    defined: DefinedType,
    binary: bool,
) -> SqlResult<Value> {
    let Some(attributes) = crate::user_types::composite_attributes(defined.oid) else {
        return Err(SqlError::not_supported(
            "populating an anonymous record from a JSON object is not supported",
        ));
    };
    let fields = attributes
        .iter()
        .map(|attribute| {
            let found = members
                .iter()
                .rev()
                .find(|(key, _)| *key == attribute.name)
                .map(|(_, node)| node);
            match found {
                None => Ok(Value::Null),
                Some(node) => field(document, node, Some(attribute.sql_type), binary),
            }
        })
        .collect::<SqlResult<Vec<Value>>>()?;
    Ok(crate::records::record_value(
        defined.oid,
        fields,
        Vec::new(),
    ))
}

fn typed(text: Value, target: Option<SqlType>) -> SqlResult<Value> {
    match target {
        None => Ok(text),
        Some(sql_type) => convert(text, sql_type, Conversion::Explicit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collected(
        name: &str,
        document: &str,
        names: &[&str],
        types: &[SqlType],
    ) -> SqlResult<Vec<Vec<Value>>> {
        let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        let mut declared = vec![None];
        declared.extend(types.iter().copied().map(Some));
        let mut found = Vec::new();
        rows(
            name,
            &[Value::Text(document.to_owned())],
            &names,
            &declared,
            &mut |row| {
                found.push(row);
                Ok(())
            },
        )?;
        Ok(found)
    }

    #[test]
    fn to_record_reads_each_declared_column() {
        let found = collected(
            "jsonb_to_record",
            r#"{"a": 1, "b": "x", "c": {"d": true}, "e": null}"#,
            &["a", "b", "c", "e", "missing"],
            &[
                SqlType::Integer,
                SqlType::Text,
                SqlType::Jsonb,
                SqlType::Text,
                SqlType::Text,
            ],
        )
        .unwrap();
        assert_eq!(
            found,
            [vec![
                Value::Integer(1),
                Value::Text("x".to_owned()),
                Value::Jsonb(r#"{"d": true}"#.to_owned()),
                Value::Null,
                Value::Null,
            ]]
        );
    }

    #[test]
    fn to_recordset_returns_a_row_per_object() {
        let found = collected(
            "json_to_recordset",
            r#"[{"a": 1}, {"a": 2, "b": [1, 2]}]"#,
            &["a", "b"],
            &[SqlType::BigInt, SqlType::Text],
        )
        .unwrap();
        assert_eq!(
            found,
            [
                vec![Value::BigInt(1), Value::Null],
                vec![Value::BigInt(2), Value::Text("[1, 2]".to_owned())],
            ]
        );
    }

    #[test]
    fn wrong_shapes_and_bad_values_are_errors() {
        let state = |name: &str, document: &str, sql_type: SqlType| {
            collected(name, document, &["a"], &[sql_type])
                .unwrap_err()
                .state()
                .code()
                .to_owned()
        };
        assert_eq!(state("json_to_record", "[1]", SqlType::Integer), "22023");
        assert_eq!(state("json_to_record", "3", SqlType::Integer), "22023");
        assert_eq!(
            state("jsonb_to_recordset", r#"{"a": 1}"#, SqlType::Integer),
            "22023"
        );
        assert_eq!(
            state("jsonb_to_recordset", "[1]", SqlType::Integer),
            "22023"
        );
        assert_eq!(
            state("jsonb_to_record", r#"{"a": "x"}"#, SqlType::Integer),
            "22P02"
        );
        assert_eq!(state("jsonb_to_record", "{", SqlType::Integer), "22P02");
        assert_eq!(result_type(&[None]).unwrap_err().state().code(), "42601");
    }

    fn sql(session: &mut crate::Session<'_>, statement: &str) -> Result<Vec<String>, SqlError> {
        let mut outputs = Vec::new();
        session.execute(statement, &mut |result| {
            outputs.push(crate::tests::render(&result))
        })?;
        let output = outputs.pop().unwrap_or_default();
        let lines: Vec<String> = output.lines().map(str::to_owned).collect();
        Ok(match lines.as_slice() {
            [_, body @ .., _] => body.to_vec(),
            _ => lines,
        })
    }

    #[test]
    fn postgresql_documented_example_fills_arrays_and_composites() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        sql(&mut session, "CREATE TYPE myrowtype AS (a int, b text)").unwrap();
        assert_eq!(
            sql(
                &mut session,
                r#"select * from json_to_record('{"a":1,"b":[1,2,3],"c":[1,2,3],"e":"bar","r": {"a": 123, "b": "a b c"}}') as x(a int, b text, c int[], d text, r myrowtype)"#
            )
            .unwrap(),
            [r#"1|[1,2,3]|{1,2,3}|NULL|(123,"a b c")"#]
        );
        assert_eq!(
            sql(
                &mut session,
                r#"select c from jsonb_to_record('{"c": ["x", null, "y z"]}') as x(c text[])"#
            )
            .unwrap(),
            [r#"{x,NULL,"y z"}"#]
        );
        let state = |session: &mut crate::Session<'_>, statement: &str| {
            sql(session, statement)
                .unwrap_err()
                .state()
                .code()
                .to_owned()
        };
        assert_eq!(
            state(
                &mut session,
                r#"select * from json_to_record('{"c": [[1, 2], [3, 4]]}') as x(c int[])"#
            ),
            "0A000"
        );
        assert_eq!(
            state(
                &mut session,
                r#"select * from json_to_record('{"c": [1, [2]]}') as x(c int[])"#
            ),
            "22P02"
        );
        assert_eq!(
            state(
                &mut session,
                r#"select * from json_to_record('{"c": [1, "x"]}') as x(c int[])"#
            ),
            "22P02"
        );
    }
}
