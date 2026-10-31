use crate::keys::{encode, item_key, items_prefix, key_value, member, table_key, text};
use eggdb_auth::sigv4::{self, SignedRequest};
use eggdb_http::{Request, Response};
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};
use eggdb_sql::json::{Json, Node, parse_document};
use std::time::{SystemTime, UNIX_EPOCH};

const TYPE_PREFIX: &str = "com.amazonaws.dynamodb.v20120810#";
const MAX_RESULT: usize = 1000;

/// The DynamoDB-compatible endpoint over one keyspace.
pub struct Endpoint<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
    access_key: String,
    secret: String,
}

impl std::fmt::Debug for Endpoint<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("access_key", &self.access_key).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
struct Schema {
    partition: (String, String),
    sort: Option<(String, String)>,
    created: u64,
}

impl Schema {
    fn encode(&self) -> String {
        let mut parts = vec![self.created.to_string(), self.partition.0.clone(), self.partition.1.clone()];
        if let Some((name, kind)) = &self.sort {
            parts.push(name.clone());
            parts.push(kind.clone());
        }
        parts.join("\t")
    }

    fn decode(text: &str) -> Option<Self> {
        let parts: Vec<&str> = text.split('\t').collect();
        let created = parts.first()?.parse().ok()?;
        let partition = ((*parts.get(1)?).to_owned(), (*parts.get(2)?).to_owned());
        let sort = match (parts.get(3), parts.get(4)) {
            (Some(name), Some(kind)) => Some(((*name).to_owned(), (*kind).to_owned())),
            _ => None,
        };
        Some(Self { partition, sort, created })
    }

    fn describe(&self, table: &str, items: usize) -> String {
        let mut key_schema = vec![format!("{{\"AttributeName\":{},\"KeyType\":\"HASH\"}}", quoted(&self.partition.0))];
        let mut definitions = vec![format!("{{\"AttributeName\":{},\"AttributeType\":{}}}", quoted(&self.partition.0), quoted(&self.partition.1))];
        if let Some((name, kind)) = &self.sort {
            key_schema.push(format!("{{\"AttributeName\":{},\"KeyType\":\"RANGE\"}}", quoted(name)));
            definitions.push(format!("{{\"AttributeName\":{},\"AttributeType\":{}}}", quoted(name), quoted(kind)));
        }
        format!(
            "{{\"TableName\":{},\"TableStatus\":\"ACTIVE\",\"KeySchema\":[{}],\"AttributeDefinitions\":[{}],\"ItemCount\":{items},\"TableSizeBytes\":0,\"CreationDateTime\":{},\"TableArn\":{},\"BillingModeSummary\":{{\"BillingMode\":\"PAY_PER_REQUEST\"}}}}",
            quoted(table),
            key_schema.join(","),
            definitions.join(","),
            self.created,
            quoted(&format!("arn:aws:dynamodb:eggdb:000000000000:table/{table}"))
        )
    }
}

fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
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

fn json(status: u16, body: String) -> Response {
    Response { status, content_type: "application/x-amz-json-1.0".to_owned(), body: body.into_bytes(), headers: Vec::new() }
}

fn failure(kind: &str, message: &str) -> Response {
    json(400, format!("{{\"__type\":{},\"message\":{}}}", quoted(&format!("{TYPE_PREFIX}{kind}")), quoted(message)))
}

fn invalid(message: &str) -> Response {
    failure("ValidationException", message)
}

fn slice<'t>(node: &Node, text: &'t str) -> &'t str {
    text.get(node.start..node.end).unwrap_or("null")
}

type Item = Vec<(String, String)>;

fn item_of(node: &Node, text: &str) -> Option<Item> {
    match &node.value {
        Json::Object(members) => Some(members.iter().map(|(name, value)| (name.clone(), slice(value, text).to_owned())).collect()),
        _ => None,
    }
}

fn render(item: &Item) -> String {
    let fields: Vec<String> = item.iter().map(|(name, value)| format!("{}:{value}", quoted(name))).collect();
    format!("{{{}}}", fields.join(","))
}

fn now_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

struct Expression<'r> {
    names: Option<&'r Node>,
    values: Option<&'r Node>,
}

impl Expression<'_> {
    fn name(&self, token: &str) -> String {
        match token.strip_prefix('#') {
            Some(_) => self.names.and_then(|names| text(names, token)).unwrap_or(token).to_owned(),
            None => token.to_owned(),
        }
    }

    fn value(&self, token: &str) -> Option<&Node> {
        self.values.and_then(|values| member(values, token))
    }
}

fn tokens(expression: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for character in expression.chars() {
        match character {
            '(' | ')' | ',' => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                out.push(character.to_string());
            }
            whitespace if whitespace.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[derive(Debug)]
enum SortCondition {
    Compare(String, Vec<u8>),
    Between(Vec<u8>, Vec<u8>),
    BeginsWith(String),
}

impl<'k> Endpoint<'k> {
    /// An endpoint over `keyspace` whose requests must be signed with `access_key` and `secret`.
    pub fn new(keyspace: &'k (dyn Keyspace + Sync), access_key: &str, secret: &str) -> Self {
        Self { keyspace, access_key: access_key.to_owned(), secret: secret.to_owned() }
    }

    fn read<T>(&self, body: impl FnOnce(&dyn KeysRead) -> Result<T, String>) -> Result<T, String> {
        let mut body = Some(body);
        let mut out = None;
        self.keyspace.read(0, &mut |keys| {
            if let Some(body) = body.take() {
                out = Some(body(keys));
            }
            Ok(())
        })?;
        out.unwrap_or_else(|| Err("the read did not run".to_owned()))
    }

    fn write(&self, body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>) -> Result<(), String> {
        self.keyspace.write(0, body)
    }

    fn schema(&self, table: &str) -> Result<Option<Schema>, String> {
        self.read(|keys| Ok(keys.get(&table_key(table))?.and_then(|bytes| String::from_utf8(bytes).ok()).and_then(|text| Schema::decode(&text))))
    }

    /// Answers one HTTP request.
    pub fn answer(&self, request: &Request) -> Response {
        let signed = SignedRequest { method: &request.method, path: &request.path, query: request.query.as_deref(), headers: &request.headers, body: &request.body };
        let now = i64::try_from(now_seconds()).unwrap_or(i64::MAX);
        if let Err(error) = sigv4::verify(&signed, &self.access_key, &self.secret, now) {
            return json(400, format!("{{\"__type\":\"com.amazon.coral.service#UnrecognizedClientException\",\"message\":{}}}", quoted(&error.to_string())));
        }
        let Some(action) = request.header("x-amz-target").and_then(|target| target.strip_prefix("DynamoDB_20120810.")) else {
            return invalid("send X-Amz-Target: DynamoDB_20120810.<Action>");
        };
        let text = String::from_utf8_lossy(&request.body).into_owned();
        let Ok(body) = parse_document(if text.trim().is_empty() { "{}" } else { &text }) else {
            return failure("SerializationException", "the request body is not valid JSON");
        };
        let text = if text.trim().is_empty() { "{}".to_owned() } else { text };
        let outcome = match action {
            "CreateTable" => self.create_table(&body),
            "DescribeTable" => self.describe_table(&body),
            "ListTables" => self.list_tables(),
            "DeleteTable" => self.delete_table(&body),
            "PutItem" => self.put_item(&body, &text),
            "GetItem" => self.get_item(&body, &text),
            "DeleteItem" => self.delete_item(&body, &text),
            "UpdateItem" => self.update_item(&body, &text),
            "Query" => self.query(&body, &text),
            "Scan" => self.scan(&body, &text),
            other => Err(invalid(&format!("the action {other} is not supported yet"))),
        };
        outcome.unwrap_or_else(|response| response)
    }

    fn table_of(&self, body: &Node) -> Result<(String, Schema), Response> {
        let table = text(body, "TableName").ok_or_else(|| invalid("TableName is required"))?.to_owned();
        let schema = self.schema(&table).map_err(|message| failure("InternalServerError", &message))?;
        let schema = schema.ok_or_else(|| failure("ResourceNotFoundException", &format!("Requested resource not found: Table: {table} not found")))?;
        Ok((table, schema))
    }

    fn create_table(&self, body: &Node) -> Result<Response, Response> {
        let table = text(body, "TableName").ok_or_else(|| invalid("TableName is required"))?;
        if table.is_empty() || table.len() > 255 || !table.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')) {
            return Err(invalid("TableName must be 1 to 255 letters, digits, '_', '-' or '.'"));
        }
        let definitions: Vec<(String, String)> = match member(body, "AttributeDefinitions").map(|node| &node.value) {
            Some(Json::Array(items)) => items.iter().filter_map(|item| Some((text(item, "AttributeName")?.to_owned(), text(item, "AttributeType")?.to_owned()))).collect(),
            _ => return Err(invalid("AttributeDefinitions is required")),
        };
        let mut partition = None;
        let mut sort = None;
        if let Some(Json::Array(items)) = member(body, "KeySchema").map(|node| &node.value) {
            for item in items {
                let (Some(name), Some(kind)) = (text(item, "AttributeName"), text(item, "KeyType")) else {
                    continue;
                };
                let defined = definitions.iter().find(|(defined, _)| defined == name).map(|(_, kind)| kind.clone()).ok_or_else(|| invalid(&format!("{name} has no attribute definition")))?;
                if !matches!(defined.as_str(), "S" | "N" | "B") {
                    return Err(invalid("key attributes must be of type S, N or B"));
                }
                match kind {
                    "HASH" => partition = Some((name.to_owned(), defined)),
                    "RANGE" => sort = Some((name.to_owned(), defined)),
                    _ => return Err(invalid("KeyType must be HASH or RANGE")),
                }
            }
        }
        let partition = partition.ok_or_else(|| invalid("KeySchema needs a HASH key"))?;
        let schema = Schema { partition, sort, created: now_seconds() };
        let encoded = schema.encode();
        let mut existed = false;
        self.write(&mut |keys| {
            existed = keys.get(&table_key(table))?.is_some();
            if !existed {
                keys.put(&table_key(table), encoded.as_bytes())?;
            }
            Ok(())
        })
        .map_err(|message| failure("InternalServerError", &message))?;
        if existed {
            return Err(failure("ResourceInUseException", &format!("Table already exists: {table}")));
        }
        Ok(json(200, format!("{{\"TableDescription\":{}}}", schema.describe(table, 0))))
    }

    fn count(&self, table: &str) -> usize {
        self.read(|keys| Ok(keys.scan(&items_prefix(table))?.len())).unwrap_or(0)
    }

    fn describe_table(&self, body: &Node) -> Result<Response, Response> {
        let (table, schema) = self.table_of(body)?;
        Ok(json(200, format!("{{\"Table\":{}}}", schema.describe(&table, self.count(&table)))))
    }

    fn list_tables(&self) -> Result<Response, Response> {
        let names = self
            .read(|keys| Ok(keys.scan(&[crate::keys::TABLE])?.into_iter().filter_map(|(key, _)| String::from_utf8(key[1..].to_vec()).ok()).collect::<Vec<_>>()))
            .map_err(|message| failure("InternalServerError", &message))?;
        let listed: Vec<String> = names.iter().map(|name| quoted(name)).collect();
        Ok(json(200, format!("{{\"TableNames\":[{}]}}", listed.join(","))))
    }

    fn delete_table(&self, body: &Node) -> Result<Response, Response> {
        let (table, schema) = self.table_of(body)?;
        self.write(&mut |keys| {
            for (key, _) in keys.scan(&items_prefix(&table))? {
                keys.delete(&key)?;
            }
            keys.delete(&table_key(&table))?;
            Ok(())
        })
        .map_err(|message| failure("InternalServerError", &message))?;
        Ok(json(200, format!("{{\"TableDescription\":{}}}", schema.describe(&table, 0))))
    }

    fn key_of(schema: &Schema, table: &str, key: &Node) -> Result<Vec<u8>, Response> {
        let partition_node = member(key, &schema.partition.0).ok_or_else(|| invalid(&format!("the key is missing {}", schema.partition.0)))?;
        let partition = key_value(partition_node, &schema.partition.1).map_err(|message| invalid(&message))?;
        let sort = match &schema.sort {
            Some((name, kind)) => Some(key_value(member(key, name).ok_or_else(|| invalid(&format!("the key is missing {name}")))?, kind).map_err(|message| invalid(&message))?),
            None => None,
        };
        Ok(item_key(table, &partition, sort.as_ref()))
    }

    fn stored(&self, key: &[u8]) -> Result<Option<String>, Response> {
        self.read(|keys| Ok(keys.get(key)?.and_then(|bytes| String::from_utf8(bytes).ok()))).map_err(|message| failure("InternalServerError", &message))
    }

    fn put_item(&self, body: &Node, text_body: &str) -> Result<Response, Response> {
        let (table, schema) = self.table_of(body)?;
        let item_node = member(body, "Item").ok_or_else(|| invalid("Item is required"))?;
        let item = item_of(item_node, text_body).ok_or_else(|| invalid("Item must be a map of attributes"))?;
        let key = Self::key_of(&schema, &table, item_node)?;
        let condition = text(body, "ConditionExpression").map(tokens);
        let rendered = render(&item);
        let previous = self.stored(&key)?;
        if let Some(condition) = condition {
            match condition.as_slice() {
                [function, open, _, close] if function == "attribute_not_exists" && open == "(" && close == ")" => {
                    if previous.is_some() {
                        return Err(failure("ConditionalCheckFailedException", "The conditional request failed"));
                    }
                }
                _ => return Err(invalid("only attribute_not_exists(...) conditions are supported yet")),
            }
        }
        self.write(&mut |keys| keys.put(&key, rendered.as_bytes())).map_err(|message| failure("InternalServerError", &message))?;
        let returned = (text(body, "ReturnValues") == Some("ALL_OLD")).then_some(previous).flatten();
        Ok(json(200, returned.map_or_else(|| "{}".to_owned(), |old| format!("{{\"Attributes\":{old}}}"))))
    }

    fn get_item(&self, body: &Node, _: &str) -> Result<Response, Response> {
        let (table, schema) = self.table_of(body)?;
        let key = Self::key_of(&schema, &table, member(body, "Key").ok_or_else(|| invalid("Key is required"))?)?;
        Ok(json(200, self.stored(&key)?.map_or_else(|| "{}".to_owned(), |item| format!("{{\"Item\":{item}}}"))))
    }

    fn delete_item(&self, body: &Node, _: &str) -> Result<Response, Response> {
        let (table, schema) = self.table_of(body)?;
        let key = Self::key_of(&schema, &table, member(body, "Key").ok_or_else(|| invalid("Key is required"))?)?;
        let previous = self.stored(&key)?;
        self.write(&mut |keys| keys.delete(&key).map(|_| ())).map_err(|message| failure("InternalServerError", &message))?;
        let returned = (text(body, "ReturnValues") == Some("ALL_OLD")).then_some(previous).flatten();
        Ok(json(200, returned.map_or_else(|| "{}".to_owned(), |old| format!("{{\"Attributes\":{old}}}"))))
    }

    fn update_item(&self, body: &Node, text_body: &str) -> Result<Response, Response> {
        let (table, schema) = self.table_of(body)?;
        let key_node = member(body, "Key").ok_or_else(|| invalid("Key is required"))?;
        let key = Self::key_of(&schema, &table, key_node)?;
        let expression = Expression { names: member(body, "ExpressionAttributeNames"), values: member(body, "ExpressionAttributeValues") };
        let mut item: Item = match self.stored(&key)? {
            Some(stored) => {
                let node = parse_document(&stored).map_err(|_| failure("InternalServerError", "a stored item is damaged"))?;
                item_of(&node, &stored).unwrap_or_default()
            }
            None => item_of(key_node, text_body).unwrap_or_default(),
        };
        let words = tokens(text(body, "UpdateExpression").ok_or_else(|| invalid("UpdateExpression is required"))?);
        let mut at = 0;
        let mut mode = "";
        while at < words.len() {
            match words[at].to_ascii_uppercase().as_str() {
                "SET" | "REMOVE" => {
                    mode = if words[at].eq_ignore_ascii_case("SET") { "SET" } else { "REMOVE" };
                    at += 1;
                }
                "," => at += 1,
                _ if mode == "SET" => {
                    let name = expression.name(&words[at]);
                    if words.get(at + 1).map(String::as_str) != Some("=") {
                        return Err(invalid("SET takes name = :value"));
                    }
                    let value = words.get(at + 2).and_then(|token| expression.value(token)).ok_or_else(|| invalid("a SET value is missing from ExpressionAttributeValues"))?;
                    let rendered = slice(value, text_body).to_owned();
                    match item.iter_mut().find(|(existing, _)| *existing == name) {
                        Some(entry) => entry.1 = rendered,
                        None => item.push((name, rendered)),
                    }
                    at += 3;
                }
                _ if mode == "REMOVE" => {
                    let name = expression.name(&words[at]);
                    item.retain(|(existing, _)| *existing != name);
                    at += 1;
                }
                _ => return Err(invalid("UpdateExpression supports SET and REMOVE")),
            }
        }
        let rendered = render(&item);
        self.write(&mut |keys| keys.put(&key, rendered.as_bytes())).map_err(|message| failure("InternalServerError", &message))?;
        Ok(json(200, if text(body, "ReturnValues") == Some("ALL_NEW") { format!("{{\"Attributes\":{rendered}}}") } else { "{}".to_owned() }))
    }

    fn sort_condition(schema: &Schema, expression: &Expression<'_>, words: &[String]) -> Result<Option<SortCondition>, Response> {
        let Some((sort_name, sort_kind)) = &schema.sort else {
            return if words.is_empty() { Ok(None) } else { Err(invalid("the table has no sort key")) };
        };
        let encoded = |token: &String| -> Result<Vec<u8>, Response> {
            let node = expression.value(token).ok_or_else(|| invalid(&format!("{token} is missing from ExpressionAttributeValues")))?;
            let value = key_value(node, sort_kind).map_err(|message| invalid(&message))?;
            let mut out = Vec::new();
            encode(&value, &mut out);
            Ok(out)
        };
        match words {
            [] => Ok(None),
            [function, open, name, comma, value, close] if function.eq_ignore_ascii_case("begins_with") && open == "(" && comma == "," && close == ")" && expression.name(name) == *sort_name => {
                let node = expression.value(value).ok_or_else(|| invalid("the begins_with value is missing"))?;
                Ok(Some(SortCondition::BeginsWith(text(node, "S").or_else(|| text(node, "B")).ok_or_else(|| invalid("begins_with takes a string"))?.to_owned())))
            }
            [name, between, low, and, high] if between.eq_ignore_ascii_case("BETWEEN") && and.eq_ignore_ascii_case("AND") && expression.name(name) == *sort_name => Ok(Some(SortCondition::Between(encoded(low)?, encoded(high)?))),
            [name, operator, value] if expression.name(name) == *sort_name && matches!(operator.as_str(), "=" | "<" | "<=" | ">" | ">=") => Ok(Some(SortCondition::Compare(operator.clone(), encoded(value)?))),
            _ => Err(invalid("the sort key condition is not one this endpoint reads")),
        }
    }

    fn query(&self, body: &Node, text_body: &str) -> Result<Response, Response> {
        let (table, schema) = self.table_of(body)?;
        let expression = Expression { names: member(body, "ExpressionAttributeNames"), values: member(body, "ExpressionAttributeValues") };
        let words = tokens(text(body, "KeyConditionExpression").ok_or_else(|| invalid("KeyConditionExpression is required"))?);
        let (first, rest) = match words.iter().enumerate().find(|(at, word)| word.eq_ignore_ascii_case("AND") && words.get(at.saturating_sub(2)).is_none_or(|before| !before.eq_ignore_ascii_case("BETWEEN"))) {
            Some((at, _)) => (&words[..at], &words[at + 1..]),
            None => (&words[..], &[][..]),
        };
        let (partition_words, sort_words) = if first.first().is_some_and(|name| expression.name(name) == schema.partition.0) { (first, rest) } else { (rest, first) };
        let [name, equals, value] = partition_words else {
            return Err(invalid("the key condition needs partition_key = :value"));
        };
        if expression.name(name) != schema.partition.0 || equals != "=" {
            return Err(invalid("the key condition needs partition_key = :value"));
        }
        let partition_node = expression.value(value).ok_or_else(|| invalid(&format!("{value} is missing from ExpressionAttributeValues")))?;
        let partition = key_value(partition_node, &schema.partition.1).map_err(|message| invalid(&message))?;
        let condition = Self::sort_condition(&schema, &expression, sort_words)?;
        let prefix = item_key(&table, &partition, None);
        let forward = !matches!(member(body, "ScanIndexForward").map(|node| &node.value), Some(Json::Boolean(false)));
        let start = self.start_key(&schema, &table, body)?;
        let entries = self.read(|keys| keys.scan(&prefix)).map_err(|message| failure("InternalServerError", &message))?;
        let mut matched: Vec<(Vec<u8>, String)> = entries
            .into_iter()
            .filter(|(key, value)| {
                let sort = &key[prefix.len()..];
                match &condition {
                    None => true,
                    Some(SortCondition::Compare(operator, wanted)) => match operator.as_str() {
                        "=" => sort == wanted.as_slice(),
                        "<" => sort < wanted.as_slice(),
                        "<=" => sort <= wanted.as_slice(),
                        ">" => sort > wanted.as_slice(),
                        _ => sort >= wanted.as_slice(),
                    },
                    Some(SortCondition::Between(low, high)) => sort >= low.as_slice() && sort <= high.as_slice(),
                    Some(SortCondition::BeginsWith(start)) => schema.sort.as_ref().is_some_and(|(name, _)| {
                        parse_document(&String::from_utf8_lossy(value)).ok().and_then(|node| member(&node, name).and_then(|attribute| text(attribute, "S").or_else(|| text(attribute, "B")).map(|text| text.starts_with(start.as_str())))).unwrap_or(false)
                    }),
                }
            })
            .map(|(key, value)| (key, String::from_utf8_lossy(&value).into_owned()))
            .collect();
        if !forward {
            matched.reverse();
        }
        if let Some(start) = start {
            matched.retain(|(key, _)| if forward { *key > start } else { *key < start });
        }
        Ok(self.page(&schema, body, matched))
    }

    fn start_key(&self, schema: &Schema, table: &str, body: &Node) -> Result<Option<Vec<u8>>, Response> {
        member(body, "ExclusiveStartKey").map(|key| Self::key_of(schema, table, key)).transpose()
    }

    fn page(&self, schema: &Schema, body: &Node, matched: Vec<(Vec<u8>, String)>) -> Response {
        let limit = match member(body, "Limit").map(|node| &node.value) {
            Some(Json::Number(number)) => number.parse::<usize>().unwrap_or(MAX_RESULT).clamp(1, MAX_RESULT),
            _ => MAX_RESULT,
        };
        let total = matched.len();
        let shown: Vec<&(Vec<u8>, String)> = matched.iter().take(limit).collect();
        let items: Vec<&str> = shown.iter().map(|(_, item)| item.as_str()).collect();
        let mut out = format!("{{\"Items\":[{}],\"Count\":{},\"ScannedCount\":{}", items.join(","), items.len(), items.len());
        if total > limit
            && let Some((_, last)) = shown.last()
            && let Ok(node) = parse_document(last)
        {
            let mut key_fields = Vec::new();
            for name in std::iter::once(&schema.partition.0).chain(schema.sort.as_ref().map(|(name, _)| name)) {
                if let Some(attribute) = member(&node, name) {
                    key_fields.push(format!("{}:{}", quoted(name), slice(attribute, last)));
                }
            }
            out.push_str(&format!(",\"LastEvaluatedKey\":{{{}}}", key_fields.join(",")));
        }
        out.push('}');
        json(200, out)
    }

    fn scan(&self, body: &Node, _: &str) -> Result<Response, Response> {
        let (table, schema) = self.table_of(body)?;
        let start = self.start_key(&schema, &table, body)?;
        let entries = self.read(|keys| keys.scan(&items_prefix(&table))).map_err(|message| failure("InternalServerError", &message))?;
        let matched: Vec<(Vec<u8>, String)> = entries
            .into_iter()
            .filter(|(key, _)| start.as_ref().is_none_or(|start| key > start))
            .map(|(key, value)| (key, String::from_utf8_lossy(&value).into_owned()))
            .collect();
        Ok(self.page(&schema, body, matched))
    }
}
