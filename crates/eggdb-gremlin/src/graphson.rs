use crate::json::Json;
use eggdb_graph::{Node, Relationship, Value};

const MAX_DEPTH: usize = 32;
const DEFAULT_LABEL: &str = "vertex";

fn whole(json: &Json) -> Option<i64> {
    match json {
        Json::Integer(number) => Some(*number),
        Json::Float(number) if number.fract() == 0.0 && number.abs() < 9.2e18 => Some(*number as i64),
        Json::Object(_) => whole(json.field("@value")?),
        _ => None,
    }
}

fn id_of(json: &Json) -> Result<u64, String> {
    whole(json)
        .and_then(|id| u64::try_from(id).ok())
        .ok_or_else(|| "EggDB's element ids are whole numbers from 0".to_owned())
}

/// The value a GraphSON 3.0 value stands for; plain JSON reads as itself.
pub(crate) fn value_of(json: &Json, depth: usize) -> Result<Value, String> {
    if depth > MAX_DEPTH {
        return Err("the GraphSON value nests too deeply".into());
    }
    let items = |json: &Json| match json {
        Json::Array(items) => items.iter().map(|item| value_of(item, depth + 1)).collect::<Result<Vec<_>, _>>(),
        _ => Err("a GraphSON list holds an array".to_owned()),
    };
    Ok(match json {
        Json::Null => Value::Null,
        Json::Bool(flag) => Value::Boolean(*flag),
        Json::Integer(number) => Value::Integer(*number),
        Json::Float(number) => Value::Float(*number),
        Json::Text(text) => Value::String(text.clone()),
        Json::Array(_) => Value::List(items(json)?),
        Json::Object(fields) => {
            let Some(kind) = json.field("@type").and_then(Json::text) else {
                return Ok(Value::Map(
                    fields.iter().map(|(name, value)| Ok((name.clone(), value_of(value, depth + 1)?))).collect::<Result<_, String>>()?,
                ));
            };
            let value = json.field("@value").unwrap_or(&Json::Null);
            match kind {
                "g:Int32" | "g:Int64" | "gx:Int16" | "gx:Byte" | "g:Date" | "g:Timestamp" | "gx:BigInteger" => {
                    Value::Integer(whole(value).ok_or_else(|| format!("{kind} holds a whole number"))?)
                }
                "g:Float" | "g:Double" | "gx:BigDecimal" => match value {
                    Json::Integer(number) => Value::Float(*number as f64),
                    Json::Float(number) => Value::Float(*number),
                    Json::Text(text) if text == "NaN" => Value::Float(f64::NAN),
                    Json::Text(text) if text == "Infinity" => Value::Float(f64::INFINITY),
                    Json::Text(text) if text == "-Infinity" => Value::Float(f64::NEG_INFINITY),
                    _ => return Err(format!("{kind} holds a number")),
                },
                "g:UUID" | "gx:Char" => Value::String(value.text().unwrap_or_default().to_owned()),
                "g:List" | "g:Set" | "gx:ByteBuffer" if matches!(value, Json::Array(_)) => Value::List(items(value)?),
                "g:Map" => {
                    let Json::Array(flat) = value else {
                        return Err("g:Map holds its keys and values in one array".into());
                    };
                    let mut entries = Vec::with_capacity(flat.len() / 2);
                    for pair in flat.chunks(2) {
                        let [key, item] = pair else {
                            return Err("g:Map holds a key without a value".into());
                        };
                        let key = match value_of(key, depth + 1)? {
                            Value::String(text) => text,
                            other => eggdb_graph::display(&other),
                        };
                        entries.push((key, value_of(item, depth + 1)?));
                    }
                    Value::Map(entries)
                }
                "g:Vertex" => Value::Node(Box::new(Node {
                    id: id_of(value.field("id").unwrap_or(&Json::Null))?,
                    labels: value.field("label").and_then(Json::text).map(str::to_owned).into_iter().collect(),
                    properties: Vec::new(),
                })),
                "g:Edge" => Value::Relationship(Box::new(Relationship {
                    id: id_of(value.field("id").unwrap_or(&Json::Null))?,
                    start: id_of(value.field("outV").unwrap_or(&Json::Null))?,
                    end: id_of(value.field("inV").unwrap_or(&Json::Null))?,
                    kind: value.field("label").and_then(Json::text).unwrap_or_default().to_owned(),
                    properties: Vec::new(),
                })),
                _ => return Err(format!("the GraphSON type {kind} is not supported")),
            }
        }
    })
}

fn long(number: u64) -> Json {
    Json::typed("g:Int64", Json::Integer(i64::try_from(number).unwrap_or(i64::MAX)))
}

fn label_of(node: &Node) -> String {
    node.labels.first().cloned().unwrap_or_else(|| DEFAULT_LABEL.to_owned())
}

fn property_id(node: u64, index: usize) -> Json {
    let packed = node.wrapping_shl(16) | ((index as u64) & 0xFFFF);
    Json::typed("g:Int64", Json::Integer((packed & (i64::MAX as u64)) as i64))
}

fn vertex(node: &Node) -> Json {
    let properties = node
        .properties
        .iter()
        .enumerate()
        .map(|(index, (name, value))| {
            let property = Json::typed(
                "g:VertexProperty",
                Json::object(vec![("id", property_id(node.id, index)), ("value", to_json(value)), ("label", Json::Text(name.clone()))]),
            );
            (name.clone(), Json::Array(vec![property]))
        })
        .collect();
    Json::typed(
        "g:Vertex",
        Json::object(vec![("id", long(node.id)), ("label", Json::Text(label_of(node))), ("properties", Json::Object(properties))]),
    )
}

fn edge(relationship: &Relationship) -> Json {
    let properties = relationship
        .properties
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                Json::typed("g:Property", Json::object(vec![("key", Json::Text(name.clone())), ("value", to_json(value))])),
            )
        })
        .collect();
    Json::typed(
        "g:Edge",
        Json::object(vec![
            ("id", long(relationship.id)),
            ("label", Json::Text(relationship.kind.clone())),
            ("inVLabel", Json::Text(DEFAULT_LABEL.to_owned())),
            ("outVLabel", Json::Text(DEFAULT_LABEL.to_owned())),
            ("inV", long(relationship.end)),
            ("outV", long(relationship.start)),
            ("properties", Json::Object(properties)),
        ]),
    )
}

fn map(entries: Vec<(Json, Json)>) -> Json {
    Json::typed("g:Map", Json::Array(entries.into_iter().flat_map(|(key, value)| [key, value]).collect()))
}

/// A value in GraphSON 3.0.
pub(crate) fn to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Boolean(flag) => Json::Bool(*flag),
        Value::Integer(number) => Json::typed("g:Int64", Json::Integer(*number)),
        Value::Float(number) => Json::typed("g:Double", Json::Float(*number)),
        Value::String(text) => Json::Text(text.clone()),
        Value::List(items) => Json::typed("g:List", Json::Array(items.iter().map(to_json).collect())),
        Value::Map(entries) => map(entries.iter().map(|(key, value)| (Json::Text(key.clone()), to_json(value))).collect()),
        Value::Node(node) => vertex(node),
        Value::Relationship(relationship) => edge(relationship),
        Value::Path(path) => {
            let mut objects = Vec::new();
            for (index, node) in path.nodes.iter().enumerate() {
                if index > 0
                    && let Some(relationship) = path.relationships.get(index - 1)
                {
                    objects.push(edge(relationship));
                }
                objects.push(vertex(node));
            }
            path_json(objects)
        }
        other => Json::Text(eggdb_graph::display(other)),
    }
}

fn path_json(objects: Vec<Json>) -> Json {
    let labels = objects.iter().map(|_| Json::typed("g:Set", Json::Array(Vec::new()))).collect();
    Json::typed(
        "g:Path",
        Json::object(vec![("labels", Json::typed("g:List", Json::Array(labels))), ("objects", Json::typed("g:List", Json::Array(objects)))]),
    )
}

/// A list of the elements and values a traverser passed, as a GraphSON path.
pub(crate) fn path_of(value: &Value) -> Json {
    match value {
        Value::List(items) => path_json(items.iter().map(to_json).collect()),
        other => to_json(other),
    }
}

fn token(kind: &str, name: &str) -> Json {
    Json::typed(kind, Json::Text(name.to_owned()))
}

fn wanted<'p>(properties: &'p [(String, Value)], keys: &'p [String]) -> impl Iterator<Item = &'p (String, Value)> {
    properties.iter().filter(move |(name, _)| keys.is_empty() || keys.contains(name))
}

/// An element as `valueMap()` shows it: each property's values in a list, with the id and
/// label first when `tokens`.
pub(crate) fn value_map(value: &Value, tokens: bool, keys: &[String]) -> Json {
    let (id, label, properties) = match value {
        Value::Node(node) => (node.id, label_of(node), &node.properties),
        Value::Relationship(relationship) => (relationship.id, relationship.kind.clone(), &relationship.properties),
        other => return to_json(other),
    };
    let mut entries = Vec::new();
    if tokens {
        entries.push((token("g:T", "id"), long(id)));
        entries.push((token("g:T", "label"), Json::Text(label)));
    }
    let list = matches!(value, Value::Node(_));
    for (name, item) in wanted(properties, keys) {
        let shown = if list { Json::typed("g:List", Json::Array(vec![to_json(item)])) } else { to_json(item) };
        entries.push((Json::Text(name.clone()), shown));
    }
    map(entries)
}

/// An element as `elementMap()` shows it: its id, label and single property values, and for
/// an edge the ids of the vertices it leaves and enters.
pub(crate) fn element_map(value: &Value, keys: &[String]) -> Json {
    match value {
        Value::Node(node) => {
            let mut entries = vec![(token("g:T", "id"), long(node.id)), (token("g:T", "label"), Json::Text(label_of(node)))];
            entries.extend(wanted(&node.properties, keys).map(|(name, item)| (Json::Text(name.clone()), to_json(item))));
            map(entries)
        }
        Value::Relationship(relationship) => {
            let end = |id: u64| map(vec![(token("g:T", "id"), long(id)), (token("g:T", "label"), Json::Text(DEFAULT_LABEL.to_owned()))]);
            let mut entries = vec![
                (token("g:T", "id"), long(relationship.id)),
                (token("g:T", "label"), Json::Text(relationship.kind.clone())),
                (token("g:Direction", "IN"), end(relationship.end)),
                (token("g:Direction", "OUT"), end(relationship.start)),
            ];
            entries.extend(wanted(&relationship.properties, keys).map(|(name, item)| (Json::Text(name.clone()), to_json(item))));
            map(entries)
        }
        other => to_json(other),
    }
}

/// The rows of `groupCount()` (each a key and its count) as one GraphSON map.
pub(crate) fn group_count(rows: &[(Value, Value)]) -> Json {
    map(rows.iter().map(|(key, count)| (to_json(key), to_json(count))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphson_values_read_and_write_with_their_types() {
        let json = crate::json::parse(br#"{"@type":"g:List","@value":[{"@type":"g:Int32","@value":1},"a",{"@type":"g:Double","@value":1.5}]}"#).unwrap();
        assert_eq!(
            value_of(&json, 0).unwrap(),
            Value::List(vec![Value::Integer(1), Value::String("a".into()), Value::Float(1.5)])
        );
        let node = Value::Node(Box::new(Node { id: 1, labels: vec!["person".into()], properties: vec![("name".into(), Value::String("marko".into()))] }));
        let text = to_json(&node).to_text();
        assert!(text.starts_with(r#"{"@type":"g:Vertex","@value":{"id":{"@type":"g:Int64","@value":1},"label":"person","properties":{"name":[{"@type":"g:VertexProperty""#));
        assert_eq!(
            value_map(&node, false, &[]).to_text(),
            r#"{"@type":"g:Map","@value":["name",{"@type":"g:List","@value":["marko"]}]}"#
        );
    }
}
