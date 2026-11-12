use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

pub(crate) const FUNCTIONS: [&str; 27] = [
    "eggdb_pdf_table",
    "fhir_validate",
    "opcua_decode",
    "eggdb_route_dijkstra",
    "eggdb_route_astar",
    "eggdb_route_reach",
    "eggdb_route_components",
    "eggdb_route_tsp",
    "eggdb_route_tsp_euclidean",
    "eggdb_route_node_network",
    "eggdb_route_spanning_tree",
    "eggdb_route_max_flow",
    "modbus_decode",
    "spreadsheet_recalculate",
    "row_pattern_matches",
    "geotiff_info",
    "image_metadata",
    "icalendar_parse",
    "vcard_parse",
    "dicom_parse",
    "sparkplug_decode",
    "iso20022_parse",
    "fix_parse",
    "fix_field",
    "hl7_parse",
    "edifact_parse",
    "extract_text",
];

const MAX_EXTRACTED_BYTES: usize = 64 << 20;
const MOST_MESSAGE_BYTES: usize = 4 << 20;

fn invalid(standard: &str, message: impl std::fmt::Display) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("invalid {standard} message: {message}"),
    )
}

fn string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    crate::json::write_string(text, &mut out);
    out
}

fn array(items: impl IntoIterator<Item = String>) -> String {
    format!("[{}]", items.into_iter().collect::<Vec<_>>().join(", "))
}

fn jsonb(text: String) -> SqlResult<Value> {
    crate::memory::check_value_bytes(text.len())?;
    crate::json::normalize(&text).map(Value::Jsonb)
}

fn fix_fields(message: &str) -> SqlResult<Vec<(u32, &str)>> {
    let delimiter = if message.contains('\u{1}') {
        '\u{1}'
    } else {
        '|'
    };
    let mut fields = Vec::new();
    for part in message
        .trim_end_matches(['\r', '\n'])
        .split(delimiter)
        .filter(|part| !part.is_empty())
    {
        let (tag, value) = part
            .split_once('=')
            .ok_or_else(|| invalid("FIX", format!("the field {part:?} has no =")))?;
        let number =
            (!tag.is_empty() && tag.len() <= 9 && tag.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| tag.parse::<u32>().ok())
                .flatten()
                .ok_or_else(|| invalid("FIX", format!("the tag {tag:?} is not a number")))?;
        fields.push((number, value));
    }
    if fields.is_empty() {
        return Err(invalid("FIX", "it has no fields"));
    }
    check_fix_trailer(&fields)?;
    Ok(fields)
}

fn field_bytes(tag: u32, value: &str) -> usize {
    tag.to_string().len() + 1 + value.len() + 1
}

fn check_fix_trailer(fields: &[(u32, &str)]) -> SqlResult<()> {
    if let Some((10, checksum)) = fields.last() {
        let sum = fields[..fields.len() - 1]
            .iter()
            .fold(0u32, |sum, (tag, value)| {
                let field = format!("{tag}={value}\u{1}");
                field
                    .bytes()
                    .fold(sum, |sum, byte| (sum + u32::from(byte)) % 256)
            });
        if checksum.parse::<u32>().ok() != Some(sum) {
            return Err(invalid(
                "FIX",
                format!("CheckSum(10) is {checksum}, but the message sums to {sum:03}"),
            ));
        }
    }
    if let [(8, _), (9, length), rest @ ..] = fields {
        let body: usize = rest
            .iter()
            .filter(|(tag, _)| *tag != 10)
            .map(|(tag, value)| field_bytes(*tag, value))
            .sum();
        if length.parse::<usize>().ok() != Some(body) {
            return Err(invalid(
                "FIX",
                format!("BodyLength(9) is {length}, but the body has {body} bytes"),
            ));
        }
    }
    Ok(())
}

fn fix_parse(message: &str) -> SqlResult<Value> {
    let fields = fix_fields(message)?;
    jsonb(array(fields.iter().map(|(tag, value)| {
        format!("{{\"tag\": {tag}, \"value\": {}}}", string(value))
    })))
}

fn fix_field(message: &str, tag: &Value) -> SqlResult<Value> {
    let wanted = match tag {
        Value::SmallInt(number) => i64::from(*number),
        Value::Integer(number) => i64::from(*number),
        Value::BigInt(number) => *number,
        _ => return Err(invalid("FIX", "the tag must be an integer")),
    };
    let fields = fix_fields(message)?;
    Ok(fields
        .iter()
        .find(|(tag, _)| i64::from(*tag) == wanted)
        .map_or(Value::Null, |(_, value)| Value::Text((*value).to_owned())))
}

struct Hl7Separators {
    field: char,
    component: char,
    repetition: char,
    escape: char,
    subcomponent: char,
}

impl Hl7Separators {
    fn from_header(header: &str) -> SqlResult<Self> {
        let mut characters = header.chars().skip(3);
        let field = characters
            .next()
            .ok_or_else(|| invalid("HL7", "MSH has no field separator"))?;
        let encoding: Vec<char> = characters
            .take_while(|character| *character != field)
            .take(4)
            .collect();
        let pick = |at: usize, default: char| encoding.get(at).copied().unwrap_or(default);
        let separators = Self {
            field,
            component: pick(0, '^'),
            repetition: pick(1, '~'),
            escape: pick(2, '\\'),
            subcomponent: pick(3, '&'),
        };
        let all = [
            separators.field,
            separators.component,
            separators.repetition,
            separators.escape,
            separators.subcomponent,
        ];
        if all
            .iter()
            .enumerate()
            .any(|(at, character)| character.is_alphanumeric() || all[..at].contains(character))
        {
            return Err(invalid(
                "HL7",
                "MSH's separators must be five different symbols",
            ));
        }
        Ok(separators)
    }

    fn unescape(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find(self.escape) {
            out.push_str(&rest[..start]);
            let after = &rest[start + self.escape.len_utf8()..];
            let Some(end) = after.find(self.escape) else {
                out.push_str(&rest[start..]);
                return out;
            };
            match &after[..end] {
                "F" => out.push(self.field),
                "S" => out.push(self.component),
                "T" => out.push(self.subcomponent),
                "R" => out.push(self.repetition),
                "E" => out.push(self.escape),
                other => {
                    out.push(self.escape);
                    out.push_str(other);
                    out.push(self.escape);
                }
            }
            rest = &after[end + self.escape.len_utf8()..];
        }
        out.push_str(rest);
        out
    }

    fn value(&self, text: &str) -> String {
        let nested = |text: &str, separator: char, inner: &dyn Fn(&str) -> String| {
            if text.contains(separator) {
                array(text.split(separator).map(inner))
            } else {
                inner(text)
            }
        };
        let subcomponents = |text: &str| {
            nested(text, self.subcomponent, &|leaf: &str| {
                string(&self.unescape(leaf))
            })
        };
        let components = |text: &str| nested(text, self.component, &subcomponents);
        nested(text, self.repetition, &components)
    }
}

fn segment_name(name: &str, standard: &str) -> SqlResult<()> {
    if name.len() == 3 && name.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(invalid(standard, format!("{name:?} is not a segment name")))
    }
}

fn hl7_parse(message: &str) -> SqlResult<Value> {
    let mut segments = message
        .split(['\r', '\n'])
        .filter(|segment| !segment.trim().is_empty());
    let header = segments
        .next()
        .ok_or_else(|| invalid("HL7", "it is empty"))?;
    if !header.starts_with("MSH") {
        return Err(invalid("HL7", "it does not start with an MSH segment"));
    }
    let separators = Hl7Separators::from_header(header)?;
    let mut out = Vec::new();
    for segment in std::iter::once(header).chain(segments) {
        let mut parts = segment.split(separators.field);
        let name = parts.next().unwrap_or_default();
        segment_name(name, "HL7")?;
        let fields: Vec<String> = if name == "MSH" {
            let mut fields = vec![
                string(&separators.field.to_string()),
                string(parts.next().unwrap_or_default()),
            ];
            fields.extend(parts.map(|field| separators.value(field)));
            fields
        } else {
            parts.map(|field| separators.value(field)).collect()
        };
        out.push(format!(
            "{{\"segment\": {}, \"fields\": {}}}",
            string(name),
            array(fields)
        ));
    }
    jsonb(array(out))
}

struct EdifactSyntax {
    component: char,
    element: char,
    release: char,
    terminator: char,
}

fn edifact_syntax(message: &str) -> SqlResult<(EdifactSyntax, &str)> {
    let Some(advice) = message.strip_prefix("UNA") else {
        return Ok((
            EdifactSyntax {
                component: ':',
                element: '+',
                release: '?',
                terminator: '\'',
            },
            message,
        ));
    };
    let characters: Vec<char> = advice.chars().take(6).collect();
    let [component, element, _decimal, release, _reserved, terminator] = characters[..] else {
        return Err(invalid(
            "EDIFACT",
            "the UNA service string advice needs six characters",
        ));
    };
    let used = characters
        .iter()
        .map(|character| character.len_utf8())
        .sum::<usize>();
    Ok((
        EdifactSyntax {
            component,
            element,
            release,
            terminator,
        },
        &advice[used..],
    ))
}

fn edifact_parse(message: &str) -> SqlResult<Value> {
    let (syntax, body) = edifact_syntax(message)?;
    let mut segments: Vec<Vec<Vec<String>>> = Vec::new();
    let mut elements: Vec<Vec<String>> = vec![vec![String::new()]];
    let mut characters = body.chars();
    let mut started = false;
    while let Some(character) = characters.next() {
        if !started && character.is_whitespace() {
            continue;
        }
        started = true;
        let current = elements
            .last_mut()
            .and_then(|components| components.last_mut());
        match character {
            _ if character == syntax.release => {
                let escaped = characters
                    .next()
                    .ok_or_else(|| invalid("EDIFACT", "it ends with a release character"))?;
                if let Some(text) = current {
                    text.push(escaped);
                }
            }
            _ if character == syntax.terminator => {
                segments.push(std::mem::replace(&mut elements, vec![vec![String::new()]]));
                started = false;
            }
            _ if character == syntax.element => elements.push(vec![String::new()]),
            _ if character == syntax.component => {
                if let Some(components) = elements.last_mut() {
                    components.push(String::new());
                }
            }
            _ => {
                if let Some(text) = current {
                    text.push(character);
                }
            }
        }
    }
    if started {
        segments.push(elements);
    }
    if segments.is_empty() {
        return Err(invalid("EDIFACT", "it has no segments"));
    }
    let mut out = Vec::with_capacity(segments.len());
    for segment in segments {
        let mut elements = segment.into_iter();
        let tag = elements
            .next()
            .and_then(|components| components.into_iter().next())
            .unwrap_or_default();
        segment_name(&tag, "EDIFACT")?;
        let values = elements.map(|components| match components.as_slice() {
            [single] => string(single),
            _ => array(components.iter().map(|component| string(component))),
        });
        out.push(format!(
            "{{\"tag\": {}, \"elements\": {}}}",
            string(&tag),
            array(values)
        ));
    }
    jsonb(array(out))
}

const ISO20022_NAMESPACE: &str = "urn:iso:std:iso:20022:tech:xsd:";

/// An ISO 20022 XML message as `{"message": "pacs.008.001.08", "root": "Document",
/// "document": {...}}`: the message definition from the root's namespace, and the message as
/// JSON (elements by name, repeated ones as arrays, attributes such as `@Ccy`, text as strings).
fn iso20022_parse(text: &str) -> SqlResult<Value> {
    let (root, namespace, document) = crate::xml_path::document_json(text)?;
    let message = namespace
        .as_deref()
        .and_then(|namespace| namespace.strip_prefix(ISO20022_NAMESPACE))
        .ok_or_else(|| invalid("ISO 20022", format!("the root's namespace is not {ISO20022_NAMESPACE}...")))?;
    jsonb(format!(
        "{{\"message\": {}, \"root\": {}, \"document\": {document}}}",
        string(message),
        string(&root)
    ))
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    if name == "eggdb_pdf_table" {
        return Some(match values {
            [title, rows] => crate::report_pdf::table_pdf(&title.to_text().unwrap_or_default(), &rows.to_text().unwrap_or_default()).map(Value::Bytea),
            _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "eggdb_pdf_table takes a title and rows")),
        });
    }
    if matches!(name, "dicom_parse" | "sparkplug_decode" | "geotiff_info" | "image_metadata") {
        return Some(match values {
            [Value::Bytea(file)] if name == "image_metadata" => crate::image_metadata::metadata(file).and_then(jsonb),
            [Value::Bytea(file)] if name == "dicom_parse" => crate::dicom::parse(file).and_then(jsonb),
            [Value::Bytea(file)] if name == "geotiff_info" => crate::geotiff::info(file).and_then(jsonb),
            [Value::Bytea(payload)] => crate::sparkplug::decode(payload).and_then(jsonb),
            _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("{name} takes bytea"))),
        });
    }
    if name == "opcua_decode" {
        let encoded = match values.get(1).and_then(Value::to_text).as_deref().map(str::to_ascii_lowercase).as_deref() {
            None | Some("datavalue") => crate::opcua::Encoded::DataValue,
            Some("variant") => crate::opcua::Encoded::Variant,
            _ => {
                return Some(Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "opcua_decode: the second argument is 'datavalue' or 'variant'",
                )));
            }
        };
        return Some(match values.first() {
            Some(Value::Null) => Ok(Value::Null),
            Some(Value::Bytea(bytes)) => crate::opcua::decode(bytes, encoded).and_then(jsonb),
            _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "opcua_decode takes bytea and optionally 'datavalue' or 'variant'")),
        });
    }
    if name.starts_with("eggdb_route_") {
        let edges = values.first().and_then(Value::to_text);
        let whole = |at: usize| values.get(at).and_then(Value::as_integer);
        let flag = |at: usize| match values.get(at) {
            Some(Value::Boolean(flag)) => Some(*flag),
            None => Some(true),
            _ => None,
        };
        return Some(match (name, edges) {
            ("eggdb_route_dijkstra", Some(edges)) => match (whole(1), whole(2), flag(3)) {
                (Some(start), Some(finish), Some(directed)) => crate::routing::dijkstra(&edges, start, finish, directed).and_then(jsonb),
                _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "eggdb_route_dijkstra takes edges, a start, an end and whether the graph is directed")),
            },
            ("eggdb_route_reach", Some(edges)) => match (whole(1), values.get(2).and_then(|value| value.to_text()).and_then(|text| text.parse::<f64>().ok()), flag(3)) {
                (Some(start), Some(distance), Some(directed)) => crate::routing::driving_distance(&edges, start, distance, directed).and_then(jsonb),
                _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "eggdb_route_reach takes edges, a start, a distance and whether the graph is directed")),
            },
            ("eggdb_route_components", Some(edges)) => crate::routing::connected_components(&edges).and_then(jsonb),
            ("eggdb_route_spanning_tree", Some(edges)) => match values.get(1).and_then(Value::to_text).as_deref() {
                Some("kruskal") | None => crate::routing::spanning_tree(&edges, false).and_then(jsonb),
                Some("prim") => crate::routing::spanning_tree(&edges, true).and_then(jsonb),
                Some(_) => Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "eggdb_route_spanning_tree: the algorithm is 'kruskal' or 'prim'")),
            },
            ("eggdb_route_max_flow", Some(edges)) => match (whole(1), whole(2)) {
                (Some(source), Some(target)) => crate::routing::max_flow(&edges, source, target).and_then(jsonb),
                _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "eggdb_route_max_flow takes edges, a source and a target")),
            },
            ("eggdb_route_tsp", Some(matrix)) => match (whole(1).or(Some(0)), whole(2).or(Some(0))) {
                (Some(start), Some(last)) => crate::routing::tsp(&matrix, start, last).and_then(jsonb),
                _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "eggdb_route_tsp takes a cost matrix, a start and an end")),
            },
            ("eggdb_route_node_network", Some(edges)) => match values.get(1).and_then(Value::to_text).and_then(|text| text.parse::<f64>().ok()) {
                Some(tolerance) => crate::routing::node_network(&edges, tolerance).and_then(jsonb),
                None => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "eggdb_route_node_network takes edges and a tolerance")),
            },
            ("eggdb_route_tsp_euclidean", Some(points)) => match (whole(1).or(Some(0)), whole(2).or(Some(0))) {
                (Some(start), Some(last)) => crate::routing::tsp_euclidean(&points, start, last).and_then(jsonb),
                _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "eggdb_route_tsp_euclidean takes points, a start and an end")),
            },
            ("eggdb_route_astar", Some(edges)) => {
                let real = |at: usize, default: f64| match values.get(at) {
                    None => Some(default),
                    Some(value) => value.to_text().and_then(|text| text.parse::<f64>().ok()),
                };
                match (whole(1), whole(2), flag(3), whole(4).or(Some(5)), real(5, 1.0), real(6, 1.0)) {
                    (Some(start), Some(finish), Some(directed), Some(heuristic), Some(factor), Some(epsilon)) => {
                        let estimate = crate::routing::Estimate {
                            heuristic: u8::try_from(heuristic).unwrap_or(u8::MAX),
                            factor,
                            epsilon,
                        };
                        crate::routing::astar(&edges, start, finish, directed, estimate).and_then(jsonb)
                    }
                    _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "eggdb_route_astar takes edges, a start, an end, whether the graph is directed, a heuristic, a factor and an epsilon")),
                }
            }
            _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("{name} takes the edges as jsonb first"))),
        });
    }
    if name == "modbus_decode" {
        let direction = match values.get(1).and_then(Value::to_text).as_deref() {
            Some("request") => crate::modbus::Direction::Request,
            Some("response") => crate::modbus::Direction::Response,
            _ => return Some(Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "modbus_decode: the direction is 'request' or 'response'"))),
        };
        let framing = match values.get(2).and_then(Value::to_text).as_deref() {
            None | Some("tcp") => crate::modbus::Framing::Tcp,
            Some("rtu") => crate::modbus::Framing::Rtu,
            _ => return Some(Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "modbus_decode: the framing is 'tcp' or 'rtu'"))),
        };
        return Some(match values.first() {
            Some(Value::Bytea(frame)) if values.len() <= 3 => crate::modbus::decode(frame, direction, framing).and_then(jsonb),
            _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "modbus_decode takes a frame as bytea, a direction and optionally a framing")),
        });
    }
    if name == "row_pattern_matches" {
        return Some(match values {
            [Value::Jsonb(rows) | Value::Json(rows) | Value::Text(rows), Value::Text(pattern)] => crate::row_pattern::matches(rows, pattern).and_then(jsonb),
            _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "row_pattern_matches takes the rows as jsonb and the pattern as text")),
        });
    }
    if name == "spreadsheet_recalculate" {
        return Some(match values {
            [Value::Jsonb(sheet) | Value::Json(sheet) | Value::Text(sheet)] => crate::spreadsheet::recalculate(sheet).and_then(jsonb),
            _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "spreadsheet_recalculate takes the cells as jsonb")),
        });
    }
    if name == "fhir_validate" {
        return Some(match values {
            [resource] => match resource.to_text() {
                Some(text) if text.len() <= MOST_MESSAGE_BYTES => jsonb(crate::fhir::validate(&text)),
                Some(_) => Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!("fhir_validate reads resources of at most {MOST_MESSAGE_BYTES} bytes"),
                )),
                None => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "fhir_validate takes the resource as jsonb or text")),
            },
            _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "fhir_validate takes one resource")),
        });
    }
    if name == "extract_text" {
        return Some(match values {
            [Value::Bytea(file)] => eggdb_content::extract_text(file, MAX_EXTRACTED_BYTES)
                .map(Value::Text)
                .map_err(|error| {
                    SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("extract_text: {error}"),
                    )
                }),
            _ => Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                "extract_text takes the file as bytea",
            )),
        });
    }
    let text = match values.first() {
        Some(Value::Text(text) | Value::Bpchar(text)) => text.as_str(),
        _ => {
            return Some(Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("{name} takes the message as text"),
            )));
        }
    };
    if text.len() > MOST_MESSAGE_BYTES {
        return Some(Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("{name} reads messages of at most {MOST_MESSAGE_BYTES} bytes"),
        )));
    }
    Some(match (name, values) {
        ("fix_parse", [_]) => fix_parse(text),
        ("fix_field", [_, tag]) => fix_field(text, tag),
        ("hl7_parse", [_]) => hl7_parse(text),
        ("edifact_parse", [_]) => edifact_parse(text),
        ("iso20022_parse", [_]) => iso20022_parse(text),
        ("icalendar_parse", [_]) => crate::calendar_cards::icalendar(text).and_then(jsonb),
        ("vcard_parse", [_]) => crate::calendar_cards::vcard(text).and_then(jsonb),
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("{name} does not take {} arguments", values.len()),
        )),
    })
}

pub(crate) fn result_type(name: &str) -> crate::types::SqlType {
    if name == "eggdb_pdf_table" {
        crate::types::SqlType::Bytea
    } else if matches!(name, "fix_field" | "extract_text") {
        crate::types::SqlType::Text
    } else {
        crate::types::SqlType::Jsonb
    }
}

#[cfg(test)]
#[path = "standards_tests.rs"]
mod tests;
