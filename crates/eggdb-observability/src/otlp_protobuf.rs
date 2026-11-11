use crate::line_protocol::{FieldValue, Point};
use crate::remote_write::{Field, Reader};

const MOST_POINTS: usize = 1 << 20;
const MAX_ATTRIBUTES: usize = 128;

fn reader(bytes: &[u8]) -> Reader<'_> {
    Reader { bytes, at: 0 }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn column(key: &str) -> String {
    key.chars()
        .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
        .collect()
}

fn any_value(bytes: &[u8]) -> Result<Option<String>, String> {
    let mut value = reader(bytes);
    let mut found = None;
    while let Some((number, field)) = value.next()? {
        found = match (number, field) {
            (1, Field::Bytes(text)) => Some(String::from_utf8_lossy(text).into_owned()),
            (2, Field::Varint(flag)) => Some((flag != 0).to_string()),
            (3, Field::Varint(raw)) => Some((raw as i64).to_string()),
            (4, Field::Fixed64(bits)) => Some(f64::from_bits(bits).to_string()),
            _ => found,
        };
    }
    Ok(found)
}

fn key_value(bytes: &[u8], out: &mut Vec<(String, String)>) -> Result<(), String> {
    if out.len() >= MAX_ATTRIBUTES {
        return Err(format!("at most {MAX_ATTRIBUTES} attributes are kept per record"));
    }
    let mut pair = reader(bytes);
    let (mut key, mut value) = (None, None);
    while let Some((number, field)) = pair.next()? {
        match (number, field) {
            (1, Field::Bytes(text)) => key = Some(column(&String::from_utf8_lossy(text))),
            (2, Field::Bytes(any)) => value = any_value(any)?,
            _ => {}
        }
    }
    if let (Some(key), Some(value)) = (key, value)
        && !value.is_empty()
        && !out.iter().any(|(existing, _)| *existing == key)
    {
        out.push((key, value));
    }
    Ok(())
}

fn resource(bytes: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut labels = Vec::new();
    let mut resource = reader(bytes);
    while let Some((number, field)) = resource.next()? {
        if let (1, Field::Bytes(pair)) = (number, field) {
            key_value(pair, &mut labels)?;
        }
    }
    Ok(labels)
}

fn checked(points: &[Point]) -> Result<(), String> {
    if points.len() >= MOST_POINTS {
        return Err(format!("a request may hold at most {MOST_POINTS} records"));
    }
    Ok(())
}

fn groups<'b>(bytes: &'b [u8]) -> Result<Vec<(Vec<(String, String)>, Vec<&'b [u8]>)>, String> {
    let mut out = Vec::new();
    let mut request = reader(bytes);
    while let Some((number, field)) = request.next()? {
        let (1, Field::Bytes(group)) = (number, field) else {
            continue;
        };
        let mut group = reader(group);
        let mut labels = Vec::new();
        let mut scopes = Vec::new();
        while let Some((number, field)) = group.next()? {
            match (number, field) {
                (1, Field::Bytes(found)) => labels = resource(found)?,
                (2, Field::Bytes(scope)) => scopes.push(scope),
                _ => {}
            }
        }
        out.push((labels, scopes));
    }
    Ok(out)
}

fn scope_items(scope: &[u8]) -> Result<Vec<&[u8]>, String> {
    let mut items = Vec::new();
    let mut scope = reader(scope);
    while let Some((number, field)) = scope.next()? {
        if let (2, Field::Bytes(item)) = (number, field) {
            items.push(item);
        }
    }
    Ok(items)
}

/// An OTLP `ExportLogsServiceRequest` in protobuf, as [`crate::otlp::logs`] reads JSON.
pub(crate) fn logs(bytes: &[u8]) -> Result<Vec<Point>, String> {
    let mut points = Vec::new();
    for (labels, scopes) in groups(bytes)? {
        for scope in scopes {
            for record in scope_items(scope)? {
                checked(&points)?;
                let mut tags = labels.clone();
                let (mut time, mut observed, mut line) = (0u64, 0u64, String::new());
                let mut record = reader(record);
                while let Some((number, field)) = record.next()? {
                    match (number, field) {
                        (1, Field::Fixed64(value)) => time = value,
                        (11, Field::Fixed64(value)) => observed = value,
                        (3, Field::Bytes(text)) => {
                            tags.push(("severity".to_owned(), String::from_utf8_lossy(text).into_owned()));
                        }
                        (5, Field::Bytes(any)) => line = any_value(any)?.unwrap_or_default(),
                        (6, Field::Bytes(pair)) => key_value(pair, &mut tags)?,
                        _ => {}
                    }
                }
                tags.retain(|(name, value)| name != "line" && !value.is_empty());
                let time = if time == 0 { observed } else { time };
                points.push(Point {
                    measurement: crate::loki::MEASUREMENT.to_owned(),
                    tags,
                    fields: vec![("line".to_owned(), FieldValue::Text(line))],
                    time_ns: i64::try_from(time).unwrap_or(i64::MAX),
                });
            }
        }
    }
    Ok(points)
}

/// An OTLP `ExportTraceServiceRequest` in protobuf, as [`crate::otlp::traces`] reads JSON.
pub(crate) fn traces(bytes: &[u8]) -> Result<Vec<Point>, String> {
    let mut points = Vec::new();
    for (labels, scopes) in groups(bytes)? {
        let service = labels
            .iter()
            .find(|(name, _)| name == "service_name")
            .map(|(_, value)| value.clone());
        for scope in scopes {
            for span in scope_items(scope)? {
                checked(&points)?;
                let mut tags = Vec::new();
                let mut attributes = Vec::new();
                let (mut start, mut end) = (0u64, 0u64);
                let mut span = reader(span);
                while let Some((number, field)) = span.next()? {
                    match (number, field) {
                        (1, Field::Bytes(id)) => tags.push(("trace_id".to_owned(), hex(id))),
                        (2, Field::Bytes(id)) => tags.push(("span_id".to_owned(), hex(id))),
                        (4, Field::Bytes(id)) if !id.is_empty() => {
                            tags.push(("parent_id".to_owned(), hex(id)));
                        }
                        (5, Field::Bytes(name)) => {
                            tags.push(("name".to_owned(), String::from_utf8_lossy(name).into_owned()));
                        }
                        (6, Field::Varint(kind)) => {
                            let kind = match kind {
                                2 => "SERVER",
                                3 => "CLIENT",
                                4 => "PRODUCER",
                                5 => "CONSUMER",
                                _ => "",
                            };
                            if !kind.is_empty() {
                                tags.push(("kind".to_owned(), kind.to_owned()));
                            }
                        }
                        (7, Field::Fixed64(value)) => start = value,
                        (8, Field::Fixed64(value)) => end = value,
                        (9, Field::Bytes(pair)) => key_value(pair, &mut attributes)?,
                        _ => {}
                    }
                }
                if !tags.iter().any(|(name, _)| name == "trace_id")
                    || !tags.iter().any(|(name, _)| name == "span_id")
                {
                    return Err("a span has no trace_id or span_id".to_owned());
                }
                if let Some(service) = &service {
                    tags.push(("service".to_owned(), service.clone()));
                }
                let rendered: Vec<String> = attributes
                    .iter()
                    .map(|(key, value)| format!("{key:?}:{value:?}"))
                    .collect();
                points.push(Point {
                    measurement: crate::zipkin::MEASUREMENT.to_owned(),
                    tags,
                    fields: vec![
                        (
                            "duration_us".to_owned(),
                            FieldValue::Integer(i64::try_from(end.saturating_sub(start) / 1000).unwrap_or(i64::MAX)),
                        ),
                        ("tags".to_owned(), FieldValue::Text(format!("{{{}}}", rendered.join(",")))),
                    ],
                    time_ns: i64::try_from(start).unwrap_or(i64::MAX),
                });
            }
        }
    }
    Ok(points)
}

/// An OTLP `ExportMetricsServiceRequest` in protobuf, as [`crate::otlp::metrics`] reads JSON.
pub(crate) fn metrics(bytes: &[u8]) -> Result<Vec<Point>, String> {
    let mut points = Vec::new();
    for (labels, scopes) in groups(bytes)? {
        for scope in scopes {
            for metric in scope_items(scope)? {
                let mut name = String::new();
                let mut series = Vec::new();
                let mut metric = reader(metric);
                while let Some((number, field)) = metric.next()? {
                    match (number, field) {
                        (1, Field::Bytes(text)) => name = column(&String::from_utf8_lossy(text)),
                        (5 | 7, Field::Bytes(data)) => series.push(data),
                        _ => {}
                    }
                }
                if name.is_empty() {
                    continue;
                }
                for data in series {
                    let mut data = reader(data);
                    while let Some((number, field)) = data.next()? {
                        let (1, Field::Bytes(point)) = (number, field) else {
                            continue;
                        };
                        checked(&points)?;
                        let mut tags = labels.clone();
                        let (mut time, mut value) = (0u64, None);
                        let mut point = reader(point);
                        while let Some((number, field)) = point.next()? {
                            match (number, field) {
                                (3, Field::Fixed64(raw)) => time = raw,
                                (4, Field::Fixed64(bits)) => value = Some(f64::from_bits(bits)),
                                (6, Field::Fixed64(raw)) => value = Some(raw as i64 as f64),
                                (7, Field::Bytes(pair)) => key_value(pair, &mut tags)?,
                                _ => {}
                            }
                        }
                        let Some(value) = value.filter(|value| value.is_finite()) else {
                            continue;
                        };
                        tags.retain(|(label, _)| label != "value" && label != "time");
                        points.push(Point {
                            measurement: name.clone(),
                            tags,
                            fields: vec![("value".to_owned(), FieldValue::Float(value))],
                            time_ns: i64::try_from(time).unwrap_or(i64::MAX),
                        });
                    }
                }
            }
        }
    }
    Ok(points)
}
