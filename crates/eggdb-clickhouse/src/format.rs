use eggdb_sql::json::{Json, parse_document};
use eggdb_sql::{QueryResult, SqlType, Value};

/// One of ClickHouse's text formats, by the names its `FORMAT` clause takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `TabSeparated` (`TSV`), with `\N` for null and backslash escapes.
    TabSeparated { names: bool, types: bool, raw: bool },
    /// `CSV`, strings in double quotes.
    Csv { names: bool, types: bool },
    /// `JSONEachRow` (`JSONLines`, `NDJSON`): one object per line.
    JsonEachRow,
    /// `JSONCompactEachRow`: one array per line.
    JsonCompactEachRow,
    /// `JSON`: metadata, rows as objects, and counts.
    Json,
    /// `JSONCompact`: metadata, rows as arrays, and counts.
    JsonCompact,
    /// `Values`: SQL row literals, for inserts.
    Values,
    /// `Null`: the query runs and nothing is written.
    Null,
}

impl Format {
    /// The format ClickHouse calls `name`, ignoring case as its aliases do.
    pub fn named(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "tabseparated" | "tsv" => Self::TabSeparated { names: false, types: false, raw: false },
            "tabseparatedraw" | "tsvraw" | "raw" => Self::TabSeparated { names: false, types: false, raw: true },
            "tabseparatedwithnames" | "tsvwithnames" => Self::TabSeparated { names: true, types: false, raw: false },
            "tabseparatedwithnamesandtypes" | "tsvwithnamesandtypes" => {
                Self::TabSeparated { names: true, types: true, raw: false }
            }
            "csv" => Self::Csv { names: false, types: false },
            "csvwithnames" => Self::Csv { names: true, types: false },
            "csvwithnamesandtypes" => Self::Csv { names: true, types: true },
            "jsoneachrow" | "jsonlines" | "ndjson" => Self::JsonEachRow,
            "jsoncompacteachrow" => Self::JsonCompactEachRow,
            "json" => Self::Json,
            "jsoncompact" => Self::JsonCompact,
            "values" => Self::Values,
            "null" => Self::Null,
            _ => return None,
        })
    }

    /// The name ClickHouse reports in `X-ClickHouse-Format`.
    pub fn name(self) -> &'static str {
        match self {
            Self::TabSeparated { raw: true, .. } => "TabSeparatedRaw",
            Self::TabSeparated { names: true, types: true, .. } => "TabSeparatedWithNamesAndTypes",
            Self::TabSeparated { names: true, .. } => "TabSeparatedWithNames",
            Self::TabSeparated { .. } => "TabSeparated",
            Self::Csv { names: true, types: true } => "CSVWithNamesAndTypes",
            Self::Csv { names: true, .. } => "CSVWithNames",
            Self::Csv { .. } => "CSV",
            Self::JsonEachRow => "JSONEachRow",
            Self::JsonCompactEachRow => "JSONCompactEachRow",
            Self::Json => "JSON",
            Self::JsonCompact => "JSONCompact",
            Self::Values => "Values",
            Self::Null => "Null",
        }
    }

    /// The response's `Content-Type`.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::TabSeparated { .. } => "text/tab-separated-values; charset=UTF-8",
            Self::Csv { .. } => "text/csv; charset=UTF-8; header=absent",
            Self::JsonEachRow | Self::JsonCompactEachRow => "application/x-ndjson; charset=UTF-8",
            Self::Json | Self::JsonCompact => "application/json; charset=UTF-8",
            Self::Values | Self::Null => "text/plain; charset=UTF-8",
        }
    }
}

/// The ClickHouse type name of a result column.
pub fn type_name(sql_type: Option<&SqlType>) -> String {
    let base = match sql_type {
        Some(SqlType::SmallInt) => "Int16".to_owned(),
        Some(SqlType::Integer) => "Int32".to_owned(),
        Some(SqlType::BigInt) => "Int64".to_owned(),
        Some(SqlType::Real) => "Float32".to_owned(),
        Some(SqlType::Double) => "Float64".to_owned(),
        Some(SqlType::Boolean) => "Bool".to_owned(),
        Some(SqlType::Numeric(Some(modifier))) => {
            format!("Decimal({}, {})", modifier.precision, modifier.scale.max(0))
        }
        Some(SqlType::Numeric(None)) => "Decimal(38, 10)".to_owned(),
        Some(SqlType::Date) => "Date32".to_owned(),
        Some(SqlType::Timestamp(_)) => "DateTime64(6)".to_owned(),
        Some(SqlType::TimestampTz(_)) => "DateTime64(6, 'UTC')".to_owned(),
        Some(SqlType::Uuid) => "UUID".to_owned(),
        _ => "String".to_owned(),
    };
    format!("Nullable({base})")
}

fn plain_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Boolean(flag) => Some(flag.to_string()),
        other => other.to_text(),
    }
}

fn tab_escaped(text: &str, out: &mut String) {
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            other => out.push(other),
        }
    }
}

fn is_number(value: &Value) -> bool {
    matches!(
        value,
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) | Value::Numeric(_) | Value::Double(_) | Value::Real(_) | Value::Boolean(_)
    )
}

/// `text` as a JSON string.
pub fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Boolean(flag) => flag.to_string(),
        Value::SmallInt(_) | Value::Integer(_) | Value::Numeric(_) => value.to_text().unwrap_or_default(),
        Value::Double(number) if number.is_finite() => number.to_string(),
        Value::Real(number) if number.is_finite() => number.to_string(),
        Value::Double(_) | Value::Real(_) => "null".to_owned(),
        other => json_string(&other.to_text().unwrap_or_default()),
    }
}

fn csv_field(value: &Value, out: &mut String) {
    match plain_text(value) {
        None => out.push_str("\\N"),
        Some(text) if is_number(value) => out.push_str(&text),
        Some(text) => {
            out.push('"');
            out.push_str(&text.replace('"', "\"\""));
            out.push('"');
        }
    }
}

fn names_of(result: &QueryResult) -> Vec<String> {
    result.columns.iter().flatten().map(|column| column.name.clone()).collect()
}

fn types_of(result: &QueryResult) -> Vec<String> {
    result
        .columns
        .iter()
        .flatten()
        .map(|column| type_name(column.sql_type.as_ref()))
        .collect()
}

fn tab_line(fields: &[String], out: &mut String, raw: bool) {
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            out.push('\t');
        }
        if raw {
            out.push_str(field);
        } else {
            tab_escaped(field, out);
        }
    }
    out.push('\n');
}

fn csv_line(fields: &[String], out: &mut String) {
    let quoted: Vec<String> = fields
        .iter()
        .map(|field| format!("\"{}\"", field.replace('"', "\"\"")))
        .collect();
    out.push_str(&quoted.join(","));
    out.push('\n');
}

fn meta_json(result: &QueryResult) -> String {
    let fields: Vec<String> = names_of(result)
        .iter()
        .zip(types_of(result))
        .map(|(name, kind)| format!("{{\"name\":{},\"type\":{}}}", json_string(name), json_string(&kind)))
        .collect();
    format!("[{}]", fields.join(","))
}

/// The rows of `result` written in `format`; `elapsed` is the query's time in seconds for
/// the JSON formats' statistics.
pub fn render(result: &QueryResult, format: Format, elapsed: f64) -> String {
    let mut out = String::new();
    let names = names_of(result);
    match format {
        Format::Null | Format::Values => {
            if format == Format::Values {
                let rows: Vec<String> = result
                    .rows
                    .iter()
                    .map(|row| {
                        let values: Vec<String> = row
                            .iter()
                            .map(|value| match plain_text(value) {
                                None => "NULL".to_owned(),
                                Some(text) if is_number(value) => text,
                                Some(text) => format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'")),
                            })
                            .collect();
                        format!("({})", values.join(","))
                    })
                    .collect();
                out.push_str(&rows.join(","));
            }
        }
        Format::TabSeparated { names: with_names, types, raw } => {
            if with_names {
                tab_line(&names, &mut out, raw);
            }
            if types {
                tab_line(&types_of(result), &mut out, raw);
            }
            for row in &result.rows {
                for (index, value) in row.iter().enumerate() {
                    if index > 0 {
                        out.push('\t');
                    }
                    match plain_text(value) {
                        None => out.push_str("\\N"),
                        Some(text) if raw => out.push_str(&text),
                        Some(text) => tab_escaped(&text, &mut out),
                    }
                }
                out.push('\n');
            }
        }
        Format::Csv { names: with_names, types } => {
            if with_names {
                csv_line(&names, &mut out);
            }
            if types {
                csv_line(&types_of(result), &mut out);
            }
            for row in &result.rows {
                for (index, value) in row.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    csv_field(value, &mut out);
                }
                out.push('\n');
            }
        }
        Format::JsonEachRow => {
            for row in &result.rows {
                out.push_str(&object_of(&names, row));
                out.push('\n');
            }
        }
        Format::JsonCompactEachRow => {
            for row in &result.rows {
                out.push_str(&array_of(row));
                out.push('\n');
            }
        }
        Format::Json | Format::JsonCompact => {
            let rows: Vec<String> = result
                .rows
                .iter()
                .map(|row| {
                    if format == Format::Json {
                        object_of(&names, row)
                    } else {
                        array_of(row)
                    }
                })
                .collect();
            out.push_str(&format!(
                "{{\"meta\":{},\"data\":[{}],\"rows\":{},\"statistics\":{{\"elapsed\":{elapsed},\"rows_read\":{},\"bytes_read\":0}}}}\n",
                meta_json(result),
                rows.join(","),
                result.rows.len(),
                result.rows.len()
            ));
        }
    }
    out
}

fn object_of(names: &[String], row: &[Value]) -> String {
    let fields: Vec<String> = names
        .iter()
        .zip(row)
        .map(|(name, value)| format!("{}:{}", json_string(name), json_value(value)))
        .collect();
    format!("{{{}}}", fields.join(","))
}

fn array_of(row: &[Value]) -> String {
    let values: Vec<String> = row.iter().map(json_value).collect();
    format!("[{}]", values.join(","))
}

/// One value of an inserted row, as SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    /// `NULL`.
    Null,
    /// The column's default.
    Default,
    /// A literal: text the column's type converts.
    Text(String),
    /// A number or boolean written as is.
    Bare(String),
}

impl Field {
    /// The value in SQL.
    pub fn sql(&self) -> String {
        match self {
            Self::Null => "NULL".to_owned(),
            Self::Default => "DEFAULT".to_owned(),
            Self::Text(text) => format!("'{}'", text.replace('\'', "''")),
            Self::Bare(text) => text.clone(),
        }
    }
}

/// Rows read from an insert's data: the column names a header or the JSON keys gave (when
/// they did), then each row's fields.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Input {
    /// The column names, when the data named them.
    pub names: Option<Vec<String>>,
    /// The rows, each with its column names when they vary by row (JSON).
    pub rows: Vec<(Option<Vec<String>>, Vec<Field>)>,
}

fn tab_unescaped(field: &str) -> Field {
    if field == "\\N" {
        return Field::Null;
    }
    let mut out = String::with_capacity(field.len());
    let mut characters = field.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    Field::Text(out)
}

fn csv_records(data: &str) -> Result<Vec<Vec<Field>>, String> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut characters = data.chars().peekable();
    let finish = |field: &mut String, was_quoted: &mut bool, record: &mut Vec<Field>| {
        let text = std::mem::take(field);
        record.push(if *was_quoted {
            Field::Text(text)
        } else if text == "\\N" {
            Field::Null
        } else if text.is_empty() {
            Field::Default
        } else {
            Field::Text(text)
        });
        *was_quoted = false;
    };
    while let Some(character) = characters.next() {
        if quoted {
            if character == '"' {
                if characters.peek() == Some(&'"') {
                    characters.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(character);
            }
            continue;
        }
        match character {
            '"' if field.is_empty() => {
                quoted = true;
                was_quoted = true;
            }
            ',' => finish(&mut field, &mut was_quoted, &mut record),
            '\r' => {}
            '\n' => {
                finish(&mut field, &mut was_quoted, &mut record);
                records.push(std::mem::take(&mut record));
            }
            other => field.push(other),
        }
    }
    if quoted {
        return Err("a quoted CSV field is not closed".to_owned());
    }
    if !field.is_empty() || was_quoted || !record.is_empty() {
        finish(&mut field, &mut was_quoted, &mut record);
        records.push(record);
    }
    Ok(records)
}

fn json_field(node: &Json, text: &str, start: usize, end: usize) -> Field {
    match node {
        Json::Null => Field::Null,
        Json::Boolean(flag) => Field::Bare(if *flag { "TRUE" } else { "FALSE" }.to_owned()),
        Json::Number(number) => Field::Bare(number.clone()),
        Json::String(value) => Field::Text(value.clone()),
        _ => Field::Text(text.get(start..end).unwrap_or_default().to_owned()),
    }
}

fn field_texts(fields: &[Field]) -> Vec<String> {
    fields
        .iter()
        .map(|field| match field {
            Field::Text(text) | Field::Bare(text) => text.clone(),
            Field::Null | Field::Default => String::new(),
        })
        .collect()
}

/// The rows of an insert's `data` in `format`.
pub fn parse_input(format: Format, data: &str) -> Result<Input, String> {
    let mut input = Input::default();
    match format {
        Format::TabSeparated { names, types, .. } => {
            let mut lines = data.lines().filter(|line| !line.is_empty());
            if names {
                input.names = lines
                    .next()
                    .map(|line| field_texts(&line.split('\t').map(tab_unescaped).collect::<Vec<_>>()));
            }
            if types {
                lines.next();
            }
            for line in lines {
                input.rows.push((None, line.split('\t').map(tab_unescaped).collect()));
            }
        }
        Format::Csv { names, types } => {
            let mut records = csv_records(data)?.into_iter();
            if names {
                input.names = records.next().map(|record| field_texts(&record));
            }
            if types {
                records.next();
            }
            input.rows.extend(records.map(|record| (None, record)));
        }
        Format::JsonEachRow | Format::JsonCompactEachRow => {
            for (number, line) in data.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let document = parse_document(line)
                    .map_err(|error| format!("line {}: {}", number + 1, error.message))?;
                match document.value {
                    Json::Object(members) if format == Format::JsonEachRow => {
                        let names = members.iter().map(|(name, _)| name.clone()).collect();
                        let fields = members
                            .iter()
                            .map(|(_, node)| json_field(&node.value, line, node.start, node.end))
                            .collect();
                        input.rows.push((Some(names), fields));
                    }
                    Json::Array(items) if format == Format::JsonCompactEachRow => {
                        let fields = items
                            .iter()
                            .map(|node| json_field(&node.value, line, node.start, node.end))
                            .collect();
                        input.rows.push((None, fields));
                    }
                    _ => return Err(format!("line {} is not a row in {}", number + 1, format.name())),
                }
            }
        }
        Format::Json | Format::JsonCompact | Format::Values | Format::Null => {
            return Err(format!("{} is not read for inserts here", format.name()));
        }
    }
    Ok(input)
}
