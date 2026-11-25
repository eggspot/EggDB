use crate::QueryResult;
use crate::execute::display;
use crate::value::{Node, Relationship, Value};

/// A query's result as one JSON object: `columns`, the column names, and `rows`, one array of
/// values per row. Nodes are objects with `id`, `labels` and `properties`; relationships with
/// `id`, `type`, `start`, `end` and `properties`; paths with `nodes` and `relationships`;
/// temporal values and points are their Cypher text; a float that is not finite is its text.
pub fn result_json(result: &QueryResult) -> String {
    let mut out = String::from("{\"columns\":[");
    for (index, column) in result.columns.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_string(&mut out, column);
    }
    out.push_str("],\"rows\":[");
    for (index, row) in result.rows.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('[');
        for (position, value) in row.iter().enumerate() {
            if position > 0 {
                out.push(',');
            }
            push_value(&mut out, value);
        }
        out.push(']');
    }
    out.push_str("]}");
    out
}

/// One value as JSON, as [`result_json`] writes it.
pub fn value_json(value: &Value) -> String {
    let mut out = String::new();
    push_value(&mut out, value);
    out
}

fn push_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Boolean(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Integer(number) => out.push_str(&number.to_string()),
        Value::Float(number) if number.is_finite() => out.push_str(&display(value)),
        Value::Float(_) | Value::Temporal(_) | Value::Point(_) => push_string(out, &display(value)),
        Value::String(text) => push_string(out, text),
        Value::List(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_value(out, item);
            }
            out.push(']');
        }
        Value::Map(entries) => push_properties(out, entries),
        Value::Node(node) => push_node(out, node),
        Value::Relationship(relationship) => push_relationship(out, relationship),
        Value::Path(path) => {
            out.push_str("{\"nodes\":[");
            for (index, node) in path.nodes.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_node(out, node);
            }
            out.push_str("],\"relationships\":[");
            for (index, relationship) in path.relationships.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_relationship(out, relationship);
            }
            out.push_str("]}");
        }
    }
}

fn push_node(out: &mut String, node: &Node) {
    out.push_str("{\"id\":");
    out.push_str(&node.id.to_string());
    out.push_str(",\"labels\":[");
    for (index, label) in node.labels.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_string(out, label);
    }
    out.push_str("],\"properties\":");
    push_properties(out, &node.properties);
    out.push('}');
}

fn push_relationship(out: &mut String, relationship: &Relationship) {
    out.push_str("{\"id\":");
    out.push_str(&relationship.id.to_string());
    out.push_str(",\"type\":");
    push_string(out, &relationship.kind);
    out.push_str(",\"start\":");
    out.push_str(&relationship.start.to_string());
    out.push_str(",\"end\":");
    out.push_str(&relationship.end.to_string());
    out.push_str(",\"properties\":");
    push_properties(out, &relationship.properties);
    out.push('}');
}

fn push_properties(out: &mut String, entries: &[(String, Value)]) {
    out.push('{');
    for (index, (name, value)) in entries.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_string(out, name);
        out.push(':');
        push_value(out, value);
    }
    out.push('}');
}

fn push_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if u32::from(control) < 0x20 || control == '\u{2028}' || control == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", u32::from(control)));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::{result_json, value_json};
    use crate::QueryResult;
    use crate::value::{Node, Value};

    #[test]
    fn nodes_lists_and_text_become_json() {
        let node = Value::Node(Box::new(Node {
            id: 7,
            labels: vec!["Person".to_owned()],
            properties: vec![("name".to_owned(), Value::String("Ada \"L\"\n".to_owned()))],
        }));
        let result = QueryResult {
            columns: vec!["n".to_owned(), "xs".to_owned()],
            rows: vec![vec![
                node,
                Value::List(vec![Value::Integer(1), Value::Float(2.5), Value::Null]),
            ]],
        };
        assert_eq!(
            result_json(&result),
            r#"{"columns":["n","xs"],"rows":[[{"id":7,"labels":["Person"],"properties":{"name":"Ada \"L\"\n"}},[1,2.5,null]]]}"#
        );
        assert_eq!(value_json(&Value::Float(f64::NAN)), "\"NaN\"");
    }
}
