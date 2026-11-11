use crate::line_protocol::{FieldValue, Point};
use eggdb_sql::json::{Json, Node, parse_document};

pub(crate) const SPANS_PATH: &str = "/api/v2/spans";
pub(crate) const MEASUREMENT: &str = "spans";
const MOST_SPANS: usize = 1 << 20;

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Json> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| &value.value),
        _ => None,
    }
}

fn text(node: &Node, name: &str) -> Option<String> {
    match member(node, name)? {
        Json::String(value) => Some(value.clone()),
        Json::Number(value) => Some(value.clone()),
        _ => None,
    }
}

fn number(node: &Node, name: &str) -> Option<i64> {
    match member(node, name)? {
        Json::Number(value) => value.parse::<f64>().ok().map(|value| value as i64),
        _ => None,
    }
}

fn is_hex_id(id: &str, lengths: &[usize]) -> bool {
    lengths.contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn tags_json(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The spans of a Zipkin v2 JSON `POST /api/v2/spans` body, each as a point of `spans`: ids,
/// name, kind and service as tags, and `duration_us` and the span's own tags (as JSON) as fields.
pub(crate) fn decode(body: &[u8]) -> Result<Vec<Point>, String> {
    let text_body = std::str::from_utf8(body).map_err(|_| "the body is not UTF-8".to_owned())?;
    let document = parse_document(text_body).map_err(|error| error.message)?;
    let Json::Array(spans) = &document.value else {
        return Err("a Zipkin v2 body is a JSON array of spans".to_owned());
    };
    if spans.len() > MOST_SPANS {
        return Err(format!("a request may hold at most {MOST_SPANS} spans"));
    }
    let mut points = Vec::with_capacity(spans.len());
    for span in spans {
        let trace_id = text(span, "traceId").ok_or("a span has no traceId")?.to_ascii_lowercase();
        let id = text(span, "id").ok_or("a span has no id")?.to_ascii_lowercase();
        if !is_hex_id(&trace_id, &[16, 32]) || !is_hex_id(&id, &[16]) {
            return Err(format!("span {id} of trace {trace_id} has malformed ids"));
        }
        let mut tags = vec![("trace_id".to_owned(), trace_id), ("span_id".to_owned(), id)];
        if let Some(parent) = text(span, "parentId") {
            tags.push(("parent_id".to_owned(), parent.to_ascii_lowercase()));
        }
        for (name, column) in [("name", "name"), ("kind", "kind")] {
            if let Some(value) = text(span, name) {
                tags.push((column.to_owned(), value));
            }
        }
        if let Some(endpoint) = span_endpoint(span, "localEndpoint") {
            tags.push(("service".to_owned(), endpoint));
        }
        if let Some(endpoint) = span_endpoint(span, "remoteEndpoint") {
            tags.push(("remote_service".to_owned(), endpoint));
        }
        tags.retain(|(_, value)| !value.is_empty());
        let mut fields = vec![(
            "duration_us".to_owned(),
            FieldValue::Integer(number(span, "duration").unwrap_or(0).max(0)),
        )];
        if let Some(Json::Object(entries)) = member(span, "tags") {
            let rendered: Vec<String> = entries
                .iter()
                .filter_map(|(key, value)| match &value.value {
                    Json::String(value) => Some(format!("{}:{}", tags_json(key), tags_json(value))),
                    _ => None,
                })
                .collect();
            fields.push(("tags".to_owned(), FieldValue::Text(format!("{{{}}}", rendered.join(",")))));
        }
        let time_ns = number(span, "timestamp").unwrap_or(0).saturating_mul(1000);
        points.push(Point {
            measurement: MEASUREMENT.to_owned(),
            tags,
            fields,
            time_ns,
        });
    }
    Ok(points)
}

fn span_endpoint(span: &Node, name: &str) -> Option<String> {
    let Json::Object(members) = member(span, name)? else {
        return None;
    };
    members
        .iter()
        .find(|(key, _)| key == "serviceName")
        .and_then(|(_, value)| match &value.value {
            Json::String(service) => Some(service.clone()),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zipkin_spans_become_points() {
        let points = decode(
            br#"[{"traceId":"5af7183fb1d4cf5f","id":"352bff9a74ca9ad2","name":"get /api","timestamp":1556604172355737,"duration":1431,"localEndpoint":{"serviceName":"backend"},"tags":{"http.method":"GET"}}]"#,
        )
        .unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].time_ns, 1_556_604_172_355_737_000);
        assert!(points[0].tags.contains(&("service".to_owned(), "backend".to_owned())));
        assert!(decode(br#"[{"traceId":"zz","id":"1"}]"#).is_err());
    }
}
