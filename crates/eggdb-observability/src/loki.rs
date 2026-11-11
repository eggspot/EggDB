use crate::line_protocol::{FieldValue, Point};
use crate::remote_write::{Field, Reader, text};
use eggdb_sql::json::{Json, Node, parse_document};

pub(crate) const PUSH_PATH: &str = "/loki/api/v1/push";
const MEASUREMENT: &str = "logs";
const MAX_DECOMPRESSED: usize = 64 << 20;
const MOST_LINES: usize = 1 << 20;
const MAX_LABELS: usize = 64;

fn label_pairs(written: &str) -> Result<Vec<(String, String)>, String> {
    let inner = written
        .trim()
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .ok_or_else(|| format!("labels {written} are not in braces"))?;
    let mut pairs = Vec::new();
    let mut characters = inner.chars().peekable();
    loop {
        while characters.peek().is_some_and(|next| next.is_whitespace() || *next == ',') {
            characters.next();
        }
        if characters.peek().is_none() {
            break;
        }
        let name: String = characters.by_ref().take_while(|next| *next != '=').collect();
        if characters.next() != Some('"') {
            return Err(format!("the label {} has no quoted value", name.trim()));
        }
        let mut value = String::new();
        loop {
            match characters.next() {
                Some('\\') => match characters.next() {
                    Some('n') => value.push('\n'),
                    Some(other) => value.push(other),
                    None => return Err("a label value ends in a backslash".to_owned()),
                },
                Some('"') => break,
                Some(other) => value.push(other),
                None => return Err("a label value is not closed".to_owned()),
            }
        }
        pairs.push((name.trim().to_owned(), value));
        if pairs.len() > MAX_LABELS {
            return Err(format!("a stream may have at most {MAX_LABELS} labels"));
        }
    }
    Ok(pairs)
}

fn point(labels: &[(String, String)], time_ns: i64, line: String) -> Point {
    Point {
        measurement: MEASUREMENT.to_owned(),
        tags: labels
            .iter()
            .filter(|(name, value)| !value.is_empty() && name != "line")
            .cloned()
            .collect(),
        fields: vec![("line".to_owned(), FieldValue::Text(line))],
        time_ns,
    }
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn from_json(body: &str) -> Result<Vec<Point>, String> {
    let document = parse_document(body).map_err(|error| error.message)?;
    let Some(Json::Array(streams)) = member(&document, "streams").map(|node| &node.value) else {
        return Err("a push needs a streams array".to_owned());
    };
    let mut points = Vec::new();
    for stream in streams {
        let mut labels = Vec::new();
        if let Some(Json::Object(members)) = member(stream, "stream").map(|node| &node.value) {
            for (name, value) in members {
                if let Json::String(value) = &value.value {
                    labels.push((name.clone(), value.clone()));
                }
            }
        }
        if labels.len() > MAX_LABELS {
            return Err(format!("a stream may have at most {MAX_LABELS} labels"));
        }
        let Some(Json::Array(values)) = member(stream, "values").map(|node| &node.value) else {
            continue;
        };
        for entry in values {
            let Json::Array(parts) = &entry.value else {
                return Err("each value is [\"<unix ns>\", \"<line>\"]".to_owned());
            };
            let (Some(Json::String(time)), Some(Json::String(line))) =
                (parts.first().map(|node| &node.value), parts.get(1).map(|node| &node.value))
            else {
                return Err("each value is [\"<unix ns>\", \"<line>\"]".to_owned());
            };
            let time_ns: i64 = time
                .parse()
                .map_err(|_| format!("{time} is not a timestamp in nanoseconds"))?;
            if points.len() >= MOST_LINES {
                return Err(format!("a push may hold at most {MOST_LINES} lines"));
            }
            points.push(point(&labels, time_ns, line.clone()));
        }
    }
    Ok(points)
}

fn from_protobuf(compressed: &[u8]) -> Result<Vec<Point>, String> {
    let body = eggdb_columnar::snappy_decompress(compressed, MAX_DECOMPRESSED)
        .map_err(|error| format!("the body is not a Snappy block: {}", error.0))?;
    let mut request = Reader {
        bytes: &body,
        at: 0,
    };
    let mut points = Vec::new();
    while let Some((number, field)) = request.next()? {
        let (1, Field::Bytes(stream)) = (number, field) else {
            continue;
        };
        let mut stream = Reader {
            bytes: stream,
            at: 0,
        };
        let mut labels = Vec::new();
        let mut entries = Vec::new();
        while let Some((number, field)) = stream.next()? {
            match (number, field) {
                (1, Field::Bytes(written)) => labels = label_pairs(&text(written)?)?,
                (2, Field::Bytes(entry)) => entries.push(entry),
                _ => {}
            }
        }
        for entry in entries {
            let mut entry = Reader {
                bytes: entry,
                at: 0,
            };
            let (mut seconds, mut nanos, mut line) = (0i64, 0i64, String::new());
            while let Some((number, field)) = entry.next()? {
                match (number, field) {
                    (1, Field::Bytes(timestamp)) => {
                        let mut timestamp = Reader {
                            bytes: timestamp,
                            at: 0,
                        };
                        while let Some((number, field)) = timestamp.next()? {
                            match (number, field) {
                                (1, Field::Varint(value)) => seconds = value as i64,
                                (2, Field::Varint(value)) => nanos = value as i64,
                                _ => {}
                            }
                        }
                    }
                    (2, Field::Bytes(written)) => line = String::from_utf8_lossy(written).into_owned(),
                    _ => {}
                }
            }
            if points.len() >= MOST_LINES {
                return Err(format!("a push may hold at most {MOST_LINES} lines"));
            }
            let time_ns = seconds.saturating_mul(1_000_000_000).saturating_add(nanos);
            points.push(point(&labels, time_ns, line));
        }
    }
    Ok(points)
}

/// The log lines of a Loki push request: JSON (`{"streams":[{"stream":{...},"values":[[ns,
/// line]]}]}`) or Snappy-compressed protobuf (`PushRequest`, as Promtail sends it).
pub(crate) fn decode(content_type: &str, body: &[u8]) -> Result<Vec<Point>, String> {
    if content_type.starts_with("application/x-protobuf") {
        from_protobuf(body)
    } else {
        let text = std::str::from_utf8(body).map_err(|_| "the body is not UTF-8".to_owned())?;
        from_json(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_and_label_strings_become_log_points() {
        let points = from_json(
            r#"{"streams":[{"stream":{"job":"api","level":"warn"},"values":[["1700000000000000000","disk almost full"]]}]}"#,
        )
        .unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].measurement, "logs");
        assert_eq!(points[0].time_ns, 1_700_000_000_000_000_000);
        assert_eq!(
            label_pairs(r#"{job="api", path="/a\"b"}"#).unwrap(),
            vec![
                ("job".to_owned(), "api".to_owned()),
                ("path".to_owned(), "/a\"b".to_owned())
            ]
        );
    }
}
