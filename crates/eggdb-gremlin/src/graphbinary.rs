use crate::json::Json;

pub(crate) const MIME: &str = "application/vnd.graphbinary-v1.0";
const VERSION: u8 = 0x81;
const PRESENT: u8 = 0x00;
const NULL: u8 = 0x01;
const MAX_DEPTH: usize = 64;
const MAX_ITEMS: usize = 1 << 24;

const INT: u8 = 0x01;
const LONG: u8 = 0x02;
const STRING: u8 = 0x03;
const DATE: u8 = 0x04;
const TIMESTAMP: u8 = 0x05;
const DOUBLE: u8 = 0x07;
const FLOAT: u8 = 0x08;
const LIST: u8 = 0x09;
const MAP: u8 = 0x0A;
const SET: u8 = 0x0B;
const UUID: u8 = 0x0C;
const EDGE: u8 = 0x0D;
const PATH: u8 = 0x0E;
const PROPERTY: u8 = 0x0F;
const VERTEX: u8 = 0x11;
const VERTEX_PROPERTY: u8 = 0x12;
const BARRIER: u8 = 0x13;
const BINDING: u8 = 0x14;
const BYTECODE: u8 = 0x15;
const CARDINALITY: u8 = 0x16;
const COLUMN: u8 = 0x17;
const DIRECTION: u8 = 0x18;
const OPERATOR: u8 = 0x19;
const ORDER: u8 = 0x1A;
const PICK: u8 = 0x1B;
const POP: u8 = 0x1C;
const PREDICATE: u8 = 0x1E;
const SCOPE: u8 = 0x1F;
const TOKEN: u8 = 0x20;
const TRAVERSER: u8 = 0x21;
const BYTE: u8 = 0x24;
const SHORT: u8 = 0x26;
const BOOLEAN: u8 = 0x27;
const TEXT_PREDICATE: u8 = 0x28;
const CHAR: u8 = 0x80;
const UNSPECIFIED_NULL: u8 = 0xFE;

fn enum_type(code: u8) -> Option<&'static str> {
    Some(match code {
        BARRIER => "g:Barrier",
        CARDINALITY => "g:Cardinality",
        COLUMN => "g:Column",
        DIRECTION => "g:Direction",
        OPERATOR => "g:Operator",
        ORDER => "g:Order",
        PICK => "g:Pick",
        POP => "g:Pop",
        SCOPE => "g:Scope",
        TOKEN => "g:T",
        _ => return None,
    })
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

type Read<T> = Result<T, String>;

impl Reader<'_> {
    fn take(&mut self, count: usize) -> Read<&[u8]> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or("a GraphBinary value is cut short")?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn byte(&mut self) -> Read<u8> {
        Ok(self.take(1)?[0])
    }

    fn int(&mut self) -> Read<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().map_err(|_| "an int is cut short")?))
    }

    fn long(&mut self) -> Read<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().map_err(|_| "a long is cut short")?))
    }

    fn count(&mut self) -> Read<usize> {
        usize::try_from(self.int()?).ok().filter(|count| *count <= MAX_ITEMS).ok_or_else(|| "a GraphBinary length is out of range".to_owned())
    }

    fn string(&mut self) -> Read<String> {
        let length = self.count()?;
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| "a GraphBinary string is not UTF-8".to_owned())
    }

    fn uuid(&mut self) -> Read<String> {
        let bytes = self.take(16)?;
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
    }

    fn map_entries(&mut self, depth: usize) -> Read<Vec<Json>> {
        let count = self.count()?;
        let mut flat = Vec::with_capacity(count.min(1024) * 2);
        for _ in 0..count {
            flat.push(self.value(depth + 1)?);
            flat.push(self.value(depth + 1)?);
        }
        Ok(flat)
    }

    fn items(&mut self, depth: usize) -> Read<Vec<Json>> {
        let count = self.count()?;
        let mut items = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            items.push(self.value(depth + 1)?);
        }
        Ok(items)
    }

    fn bare_map(&mut self, depth: usize) -> Read<Json> {
        let flat = self.map_entries(depth)?;
        let mut fields = Vec::with_capacity(flat.len() / 2);
        for pair in flat.chunks(2) {
            let [key, value] = pair else {
                return Err("a GraphBinary map holds a key without a value".into());
            };
            let key = match key {
                Json::Text(text) => text.clone(),
                other => other.to_text(),
            };
            fields.push((key, value.clone()));
        }
        Ok(Json::Object(fields))
    }

    fn bytecode(&mut self, depth: usize) -> Read<Json> {
        let instructions = |reader: &mut Self| -> Read<Vec<Json>> {
            let count = reader.count()?;
            let mut out = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                let mut instruction = vec![Json::Text(reader.string()?)];
                instruction.extend(reader.items(depth)?);
                out.push(Json::Array(instruction));
            }
            Ok(out)
        };
        let steps = instructions(self)?;
        let sources = instructions(self)?;
        Ok(Json::typed("g:Bytecode", Json::object(vec![("step", Json::Array(steps)), ("source", Json::Array(sources))])))
    }

    fn value(&mut self, depth: usize) -> Read<Json> {
        if depth > MAX_DEPTH {
            return Err("a GraphBinary value nests too deeply".into());
        }
        let code = self.byte()?;
        if code == UNSPECIFIED_NULL {
            self.byte()?;
            return Ok(Json::Null);
        }
        match self.byte()? {
            PRESENT => {}
            NULL => return Ok(Json::Null),
            other => return Err(format!("the GraphBinary value flag {other:#04x} is not known")),
        }
        if let Some(kind) = enum_type(code) {
            return match self.value(depth + 1)? {
                Json::Text(name) => Ok(Json::typed(kind, Json::Text(name))),
                _ => Err("a GraphBinary enum holds its name as a string".into()),
            };
        }
        Ok(match code {
            INT => Json::typed("g:Int32", Json::Integer(i64::from(self.int()?))),
            LONG | DATE | TIMESTAMP => Json::typed("g:Int64", Json::Integer(self.long()?)),
            STRING => Json::Text(self.string()?),
            DOUBLE => Json::typed("g:Double", Json::Float(f64::from_bits(self.long()? as u64))),
            FLOAT => Json::typed("g:Float", Json::Float(f64::from(f32::from_bits(self.int()? as u32)))),
            BYTE => Json::typed("g:Int32", Json::Integer(i64::from(self.byte()? as i8))),
            SHORT => Json::typed("g:Int32", Json::Integer(i64::from(i16::from_be_bytes(self.take(2)?.try_into().map_err(|_| "a short is cut short")?)))),
            BOOLEAN => Json::Bool(self.byte()? != 0),
            CHAR => {
                let first = self.byte()?;
                let length = match first {
                    0x00..=0x7F => 1,
                    0xC0..=0xDF => 2,
                    0xE0..=0xEF => 3,
                    _ => 4,
                };
                let mut bytes = vec![first];
                bytes.extend_from_slice(self.take(length - 1)?);
                Json::Text(String::from_utf8(bytes).map_err(|_| "a GraphBinary char is not UTF-8")?)
            }
            UUID => Json::typed("g:UUID", Json::Text(self.uuid()?)),
            LIST => Json::typed("g:List", Json::Array(self.items(depth)?)),
            SET => Json::typed("g:Set", Json::Array(self.items(depth)?)),
            MAP => Json::typed("g:Map", Json::Array(self.map_entries(depth)?)),
            VERTEX => {
                let id = self.value(depth + 1)?;
                let label = self.string()?;
                let _properties = self.value(depth + 1)?;
                Json::typed("g:Vertex", Json::object(vec![("id", id), ("label", Json::Text(label))]))
            }
            EDGE => {
                let id = self.value(depth + 1)?;
                let label = self.string()?;
                let in_vertex = self.value(depth + 1)?;
                let _in_label = self.string()?;
                let out_vertex = self.value(depth + 1)?;
                let _out_label = self.string()?;
                let _parent = self.value(depth + 1)?;
                let _properties = self.value(depth + 1)?;
                Json::typed(
                    "g:Edge",
                    Json::object(vec![("id", id), ("label", Json::Text(label)), ("inV", in_vertex), ("outV", out_vertex)]),
                )
            }
            BYTECODE => self.bytecode(depth)?,
            PREDICATE | TEXT_PREDICATE => {
                let name = self.string()?;
                let values = self.items(depth)?;
                let value = match values.as_slice() {
                    [single] if !matches!(name.as_str(), "within" | "without" | "between" | "inside" | "outside") => single.clone(),
                    _ => Json::typed("g:List", Json::Array(values)),
                };
                let kind = if code == PREDICATE { "g:P" } else { "g:TextP" };
                Json::typed(kind, Json::object(vec![("predicate", Json::Text(name)), ("value", value)]))
            }
            BINDING => {
                let _name = self.string()?;
                self.value(depth + 1)?
            }
            other => return Err(format!("the GraphBinary type {other:#04x} is not supported")),
        })
    }
}

/// A GraphBinary request (after its MIME type) as the same JSON object a GraphSON request is,
/// so both run through one path.
pub(crate) fn read_request(bytes: &[u8]) -> Read<Json> {
    let mut reader = Reader { bytes, at: 0 };
    if reader.byte()? != VERSION {
        return Err("the GraphBinary request is not version 1".into());
    }
    let id = reader.uuid()?;
    let op = reader.string()?;
    let processor = reader.string()?;
    let args = reader.bare_map(0)?;
    Ok(Json::object(vec![
        ("requestId", Json::Text(id)),
        ("op", Json::Text(op)),
        ("processor", Json::Text(processor)),
        ("args", args),
    ]))
}

fn write_int(out: &mut Vec<u8>, number: i32) {
    out.extend_from_slice(&number.to_be_bytes());
}

fn write_string(out: &mut Vec<u8>, text: &str) {
    write_int(out, i32::try_from(text.len()).unwrap_or(i32::MAX));
    out.extend_from_slice(text.as_bytes());
}

fn uuid_bytes(text: &str) -> Option<[u8; 16]> {
    let hex: String = text.chars().filter(|character| *character != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
}

fn typed(json: &Json) -> Option<(&str, &Json)> {
    Some((json.field("@type")?.text()?, json.field("@value")?))
}

fn whole(json: &Json) -> i64 {
    match json {
        Json::Integer(number) => *number,
        Json::Float(number) => *number as i64,
        _ => typed(json).map_or(0, |(_, value)| whole(value)),
    }
}

fn bare_label(json: &Json) -> String {
    json.text().unwrap_or("vertex").to_owned()
}

fn write_items(out: &mut Vec<u8>, items: &[Json]) {
    write_int(out, i32::try_from(items.len()).unwrap_or(i32::MAX));
    items.iter().for_each(|item| write_value(out, item));
}

fn write_null(out: &mut Vec<u8>) {
    out.extend_from_slice(&[UNSPECIFIED_NULL, NULL]);
}

fn write_vertex_properties(out: &mut Vec<u8>, properties: Option<&Json>) {
    let Some(Json::Object(fields)) = properties else {
        write_null(out);
        return;
    };
    let mut all = Vec::new();
    for (_, values) in fields {
        if let Json::Array(items) = values {
            all.extend(items.iter().cloned());
        }
    }
    out.extend_from_slice(&[LIST, PRESENT]);
    write_items(out, &all);
}

fn write_edge_properties(out: &mut Vec<u8>, properties: Option<&Json>) {
    let Some(Json::Object(fields)) = properties else {
        write_null(out);
        return;
    };
    let items: Vec<Json> = fields.iter().map(|(_, property)| property.clone()).collect();
    out.extend_from_slice(&[LIST, PRESENT]);
    write_items(out, &items);
}

/// Writes a GraphSON 3.0 value, as the server builds its results, fully qualified in
/// GraphBinary.
fn write_value(out: &mut Vec<u8>, json: &Json) {
    match json {
        Json::Null => write_null(out),
        Json::Bool(flag) => out.extend_from_slice(&[BOOLEAN, PRESENT, u8::from(*flag)]),
        Json::Integer(number) => {
            out.extend_from_slice(&[LONG, PRESENT]);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Json::Float(number) => {
            out.extend_from_slice(&[DOUBLE, PRESENT]);
            out.extend_from_slice(&number.to_bits().to_be_bytes());
        }
        Json::Text(text) => {
            out.extend_from_slice(&[STRING, PRESENT]);
            write_string(out, text);
        }
        Json::Array(items) => {
            out.extend_from_slice(&[LIST, PRESENT]);
            write_items(out, items);
        }
        Json::Object(fields) => {
            let Some((kind, value)) = typed(json) else {
                let flat: Vec<Json> = fields.iter().flat_map(|(key, item)| [Json::Text(key.clone()), item.clone()]).collect();
                out.extend_from_slice(&[MAP, PRESENT]);
                write_int(out, i32::try_from(fields.len()).unwrap_or(i32::MAX));
                flat.iter().for_each(|item| write_value(out, item));
                return;
            };
            write_typed(out, kind, value);
        }
    }
}

fn write_typed(out: &mut Vec<u8>, kind: &str, value: &Json) {
    match kind {
        "g:Int32" => {
            out.extend_from_slice(&[INT, PRESENT]);
            write_int(out, i32::try_from(whole(value)).unwrap_or(i32::MAX));
        }
        "g:Int64" => {
            out.extend_from_slice(&[LONG, PRESENT]);
            out.extend_from_slice(&whole(value).to_be_bytes());
        }
        "g:Double" | "g:Float" => {
            let number = match value {
                Json::Float(number) => *number,
                Json::Integer(number) => *number as f64,
                Json::Text(text) if text == "NaN" => f64::NAN,
                Json::Text(text) if text == "Infinity" => f64::INFINITY,
                _ => f64::NEG_INFINITY,
            };
            out.extend_from_slice(&[DOUBLE, PRESENT]);
            out.extend_from_slice(&number.to_bits().to_be_bytes());
        }
        "g:List" | "g:Set" => {
            out.extend_from_slice(&[if kind == "g:Set" { SET } else { LIST }, PRESENT]);
            match value {
                Json::Array(items) => write_items(out, items),
                _ => write_int(out, 0),
            }
        }
        "g:Map" => {
            out.extend_from_slice(&[MAP, PRESENT]);
            let items = match value {
                Json::Array(items) => items.as_slice(),
                _ => &[],
            };
            write_int(out, i32::try_from(items.len() / 2).unwrap_or(i32::MAX));
            items.iter().for_each(|item| write_value(out, item));
        }
        "g:UUID" => match value.text().and_then(uuid_bytes) {
            Some(bytes) => {
                out.extend_from_slice(&[UUID, PRESENT]);
                out.extend_from_slice(&bytes);
            }
            None => write_null(out),
        },
        "g:Vertex" => {
            out.extend_from_slice(&[VERTEX, PRESENT]);
            write_value(out, value.field("id").unwrap_or(&Json::Null));
            write_string(out, &bare_label(value.field("label").unwrap_or(&Json::Null)));
            write_vertex_properties(out, value.field("properties"));
        }
        "g:Edge" => {
            out.extend_from_slice(&[EDGE, PRESENT]);
            write_value(out, value.field("id").unwrap_or(&Json::Null));
            write_string(out, &bare_label(value.field("label").unwrap_or(&Json::Null)));
            write_value(out, value.field("inV").unwrap_or(&Json::Null));
            write_string(out, &bare_label(value.field("inVLabel").unwrap_or(&Json::Null)));
            write_value(out, value.field("outV").unwrap_or(&Json::Null));
            write_string(out, &bare_label(value.field("outVLabel").unwrap_or(&Json::Null)));
            write_null(out);
            write_edge_properties(out, value.field("properties"));
        }
        "g:VertexProperty" => {
            out.extend_from_slice(&[VERTEX_PROPERTY, PRESENT]);
            write_value(out, value.field("id").unwrap_or(&Json::Null));
            write_string(out, &bare_label(value.field("label").unwrap_or(&Json::Null)));
            write_value(out, value.field("value").unwrap_or(&Json::Null));
            write_null(out);
            write_null(out);
        }
        "g:Property" => {
            out.extend_from_slice(&[PROPERTY, PRESENT]);
            write_string(out, value.field("key").and_then(Json::text).unwrap_or_default());
            write_value(out, value.field("value").unwrap_or(&Json::Null));
            write_null(out);
        }
        "g:Path" => {
            out.extend_from_slice(&[PATH, PRESENT]);
            write_value(out, value.field("labels").unwrap_or(&Json::Null));
            write_value(out, value.field("objects").unwrap_or(&Json::Null));
        }
        "g:Traverser" => {
            out.extend_from_slice(&[TRAVERSER, PRESENT]);
            out.extend_from_slice(&whole(value.field("bulk").unwrap_or(&Json::Integer(1))).to_be_bytes());
            write_value(out, value.field("value").unwrap_or(&Json::Null));
        }
        "g:T" | "g:Direction" | "g:Order" | "g:Cardinality" | "g:Column" | "g:Pop" | "g:Scope" | "g:Operator" | "g:Barrier" | "g:Pick" => {
            let code = match kind {
                "g:T" => TOKEN,
                "g:Direction" => DIRECTION,
                "g:Order" => ORDER,
                "g:Cardinality" => CARDINALITY,
                "g:Column" => COLUMN,
                "g:Pop" => POP,
                "g:Scope" => SCOPE,
                "g:Operator" => OPERATOR,
                "g:Barrier" => BARRIER,
                _ => PICK,
            };
            out.extend_from_slice(&[code, PRESENT]);
            write_value(out, value);
        }
        _ => write_value(out, value),
    }
}

/// A GraphSON-shaped response object (`requestId`, `status`, `result`) in GraphBinary.
pub(crate) fn response_bytes(response: &Json) -> Vec<u8> {
    let mut out = vec![VERSION];
    match response.field("requestId").and_then(Json::text).and_then(uuid_bytes) {
        Some(bytes) => {
            out.push(PRESENT);
            out.extend_from_slice(&bytes);
        }
        None => out.push(NULL),
    }
    let status = response.field("status").unwrap_or(&Json::Null);
    write_int(&mut out, i32::try_from(whole(status.field("code").unwrap_or(&Json::Integer(500)))).unwrap_or(500));
    match status.field("message").and_then(Json::text).filter(|message| !message.is_empty()) {
        Some(message) => {
            out.push(PRESENT);
            write_string(&mut out, message);
        }
        None => out.push(NULL),
    }
    write_int(&mut out, 0);
    write_int(&mut out, 0);
    let data = response.field("result").and_then(|result| result.field("data")).unwrap_or(&Json::Null);
    write_value(&mut out, data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bytecode_request_reads_as_graphson_and_a_response_writes_back() {
        let mut request = vec![VERSION];
        request.extend_from_slice(&[0x41; 16]);
        write_string(&mut request, "bytecode");
        write_string(&mut request, "traversal");
        write_int(&mut request, 1);
        request.extend_from_slice(&[STRING, PRESENT]);
        write_string(&mut request, "gremlin");
        request.extend_from_slice(&[BYTECODE, PRESENT]);
        write_int(&mut request, 2);
        write_string(&mut request, "V");
        write_int(&mut request, 0);
        write_string(&mut request, "has");
        write_int(&mut request, 2);
        request.extend_from_slice(&[STRING, PRESENT]);
        write_string(&mut request, "age");
        request.extend_from_slice(&[PREDICATE, PRESENT]);
        write_string(&mut request, "gt");
        write_int(&mut request, 1);
        request.extend_from_slice(&[INT, PRESENT]);
        write_int(&mut request, 30);
        write_int(&mut request, 0);
        let json = read_request(&request).unwrap();
        assert_eq!(json.field("requestId").and_then(Json::text), Some("41414141-4141-4141-4141-414141414141"));
        let bytecode = json.field("args").and_then(|args| args.field("gremlin")).unwrap();
        let steps = crate::traversal::parse_bytecode(bytecode, 0).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].name, "has");
        let response = Json::object(vec![
            ("requestId", Json::Text("41414141-4141-4141-4141-414141414141".into())),
            ("status", Json::object(vec![("message", Json::Text(String::new())), ("code", Json::Integer(200))])),
            ("result", Json::object(vec![("data", Json::typed("g:List", Json::Array(vec![Json::typed("g:Int64", Json::Integer(7))])))])),
        ]);
        let bytes = response_bytes(&response);
        assert_eq!(&bytes[..2], &[VERSION, PRESENT]);
        assert_eq!(&bytes[18..22], &200i32.to_be_bytes());
        assert_eq!(bytes[22], NULL);
        assert_eq!(&bytes[31..], &[LIST, PRESENT, 0, 0, 0, 1, LONG, PRESENT, 0, 0, 0, 0, 0, 0, 0, 7]);
    }
}
