use crate::line_protocol::{FieldValue, Point};
use eggdb_sql::json::{Json, Node, parse_document};

pub(crate) const LOGS_PATH: &str = "/v1/logs";
pub(crate) const TRACES_PATH: &str = "/v1/traces";
pub(crate) const METRICS_PATH: &str = "/v1/metrics";
const MOST_POINTS: usize = 1 << 20;
const MAX_ATTRIBUTES: usize = 128;

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn items<'n>(node: &'n Node, name: &str) -> &'n [Node] {
    match member(node, name).map(|found| &found.value) {
        Some(Json::Array(items)) => items,
        _ => &[],
    }
}

fn string_of(node: Option<&Node>) -> Option<String> {
    match &node?.value {
        Json::String(text) => Some(text.clone()),
        Json::Number(text) => Some(text.clone()),
        _ => None,
    }
}

fn nanos(node: &Node, name: &str) -> i64 {
    string_of(member(node, name))
        .and_then(|text| text.parse::<i64>().ok())
        .unwrap_or(0)
}

fn any_value(node: &Node) -> Option<String> {
    let Json::Object(members) = &node.value else {
        return None;
    };
    let (kind, value) = members.first()?;
    match (kind.as_str(), &value.value) {
        ("stringValue", Json::String(text)) => Some(text.clone()),
        ("intValue", Json::String(text) | Json::Number(text)) => Some(text.clone()),
        ("doubleValue", Json::Number(text)) => Some(text.clone()),
        ("boolValue", Json::Boolean(flag)) => Some(flag.to_string()),
        _ => None,
    }
}

fn attributes(node: &Node, prefix: &str, out: &mut Vec<(String, String)>) -> Result<(), String> {
    for attribute in items(node, "attributes") {
        if out.len() >= MAX_ATTRIBUTES {
            return Err(format!("at most {MAX_ATTRIBUTES} attributes are kept per record"));
        }
        let (Some(key), Some(value)) = (
            string_of(member(attribute, "key")),
            member(attribute, "value").and_then(any_value),
        ) else {
            continue;
        };
        let column: String = format!("{prefix}{key}")
            .chars()
            .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
            .collect();
        if !value.is_empty() && !out.iter().any(|(existing, _)| *existing == column) {
            out.push((column, value));
        }
    }
    Ok(())
}

fn resource_labels(resource_group: &Node) -> Result<Vec<(String, String)>, String> {
    let mut labels = Vec::new();
    if let Some(resource) = member(resource_group, "resource") {
        attributes(resource, "", &mut labels)?;
    }
    Ok(labels)
}

fn document(body: &[u8]) -> Result<Node, String> {
    let text = std::str::from_utf8(body).map_err(|_| "the body is not UTF-8".to_owned())?;
    parse_document(text).map_err(|error| error.message)
}

fn checked(points: &[Point]) -> Result<(), String> {
    if points.len() >= MOST_POINTS {
        return Err(format!("a request may hold at most {MOST_POINTS} records"));
    }
    Ok(())
}

/// OTLP/HTTP JSON logs: each log record a row of `logs` with its resource and record
/// attributes as labels, `severity` and the body as `line`.
pub(crate) fn logs(body: &[u8]) -> Result<Vec<Point>, String> {
    let request = document(body)?;
    let mut points = Vec::new();
    for resource in items(&request, "resourceLogs") {
        let labels = resource_labels(resource)?;
        for scope in items(resource, "scopeLogs") {
            for record in items(scope, "logRecords") {
                checked(&points)?;
                let mut tags = labels.clone();
                attributes(record, "", &mut tags)?;
                if let Some(severity) = string_of(member(record, "severityText")) {
                    tags.push(("severity".to_owned(), severity));
                }
                if tags.iter().any(|(name, _)| name == "line") {
                    tags.retain(|(name, _)| name != "line");
                }
                let line = member(record, "body").and_then(any_value).unwrap_or_default();
                let mut time_ns = nanos(record, "timeUnixNano");
                if time_ns == 0 {
                    time_ns = nanos(record, "observedTimeUnixNano");
                }
                points.push(Point {
                    measurement: crate::loki::MEASUREMENT.to_owned(),
                    tags,
                    fields: vec![("line".to_owned(), FieldValue::Text(line))],
                    time_ns,
                });
            }
        }
    }
    Ok(points)
}

/// OTLP/HTTP JSON traces: each span a row of `spans`, as Zipkin spans are stored.
pub(crate) fn traces(body: &[u8]) -> Result<Vec<Point>, String> {
    let request = document(body)?;
    let mut points = Vec::new();
    for resource in items(&request, "resourceSpans") {
        let labels = resource_labels(resource)?;
        let service = labels
            .iter()
            .find(|(name, _)| name == "service_name")
            .map(|(_, value)| value.clone());
        for scope in items(resource, "scopeSpans") {
            for span in items(scope, "spans") {
                checked(&points)?;
                let id = |name: &str| string_of(member(span, name)).map(|id| id.to_ascii_lowercase());
                let (Some(trace_id), Some(span_id)) = (id("traceId"), id("spanId")) else {
                    return Err("a span has no traceId or spanId".to_owned());
                };
                let mut tags = vec![("trace_id".to_owned(), trace_id), ("span_id".to_owned(), span_id)];
                if let Some(parent) = id("parentSpanId").filter(|parent| !parent.is_empty()) {
                    tags.push(("parent_id".to_owned(), parent));
                }
                if let Some(name) = string_of(member(span, "name")) {
                    tags.push(("name".to_owned(), name));
                }
                let kind = match string_of(member(span, "kind")).as_deref() {
                    Some("2" | "SPAN_KIND_SERVER") => Some("SERVER"),
                    Some("3" | "SPAN_KIND_CLIENT") => Some("CLIENT"),
                    Some("4" | "SPAN_KIND_PRODUCER") => Some("PRODUCER"),
                    Some("5" | "SPAN_KIND_CONSUMER") => Some("CONSUMER"),
                    _ => None,
                };
                if let Some(kind) = kind {
                    tags.push(("kind".to_owned(), kind.to_owned()));
                }
                if let Some(service) = &service {
                    tags.push(("service".to_owned(), service.clone()));
                }
                let start = nanos(span, "startTimeUnixNano");
                let end = nanos(span, "endTimeUnixNano");
                let mut attributes_found = Vec::new();
                attributes(span, "", &mut attributes_found)?;
                let rendered: Vec<String> = attributes_found
                    .iter()
                    .map(|(key, value)| format!("{}:{}", quoted(key), quoted(value)))
                    .collect();
                points.push(Point {
                    measurement: crate::zipkin::MEASUREMENT.to_owned(),
                    tags,
                    fields: vec![
                        (
                            "duration_us".to_owned(),
                            FieldValue::Integer(end.saturating_sub(start).max(0) / 1000),
                        ),
                        ("tags".to_owned(), FieldValue::Text(format!("{{{}}}", rendered.join(",")))),
                    ],
                    time_ns: start,
                });
            }
        }
    }
    Ok(points)
}

fn quoted(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// OTLP/HTTP JSON metrics: gauge and sum data points, each a sample of its metric (dots in
/// names become underscores) with the resource and point attributes as labels.
pub(crate) fn metrics(body: &[u8]) -> Result<Vec<Point>, String> {
    let request = document(body)?;
    let mut points = Vec::new();
    for resource in items(&request, "resourceMetrics") {
        let labels = resource_labels(resource)?;
        for scope in items(resource, "scopeMetrics") {
            for metric in items(scope, "metrics") {
                let Some(name) = string_of(member(metric, "name")) else {
                    continue;
                };
                let name: String = name
                    .chars()
                    .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
                    .collect();
                for kind in ["gauge", "sum"] {
                    let Some(series) = member(metric, kind) else {
                        continue;
                    };
                    for point in items(series, "dataPoints") {
                        checked(&points)?;
                        let value = match (member(point, "asDouble"), member(point, "asInt")) {
                            (Some(value), _) => string_of(Some(value)).and_then(|text| text.parse::<f64>().ok()),
                            (None, Some(value)) => string_of(Some(value)).and_then(|text| text.parse::<f64>().ok()),
                            _ => None,
                        };
                        let Some(value) = value.filter(|value| value.is_finite()) else {
                            continue;
                        };
                        let mut tags = labels.clone();
                        attributes(point, "", &mut tags)?;
                        tags.retain(|(label, _)| label != "value" && label != "time");
                        points.push(Point {
                            measurement: name.clone(),
                            tags,
                            fields: vec![("value".to_owned(), FieldValue::Float(value))],
                            time_ns: nanos(point, "timeUnixNano"),
                        });
                    }
                }
            }
        }
    }
    Ok(points)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otlp_json_logs_spans_and_metrics_become_points() {
        let logs = logs(br#"{"resourceLogs":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"api"}}]},"scopeLogs":[{"logRecords":[{"timeUnixNano":"1700000000000000000","severityText":"WARN","body":{"stringValue":"slow"}}]}]}]}"#).unwrap();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].tags.contains(&("service_name".to_owned(), "api".to_owned())));
        let spans = traces(br#"{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"api"}}]},"scopeSpans":[{"spans":[{"traceId":"5B8EFFF798038103D269B633813FC60C","spanId":"EEE19B7EC3C1B174","name":"GET","kind":2,"startTimeUnixNano":"1000","endTimeUnixNano":"3000000"}]}]}]}"#).unwrap();
        assert!(spans[0].tags.contains(&("kind".to_owned(), "SERVER".to_owned())));
        let samples = metrics(br#"{"resourceMetrics":[{"scopeMetrics":[{"metrics":[{"name":"http.requests","sum":{"dataPoints":[{"asInt":"7","timeUnixNano":"1000000"}]}}]}]}]}"#).unwrap();
        assert_eq!(samples[0].measurement, "http_requests");
    }
}
