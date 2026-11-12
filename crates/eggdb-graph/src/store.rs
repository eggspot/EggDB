use crate::value::{Node, Relationship, Value, from_bson, to_bson};
use crate::{GraphError, GraphResult};
use eggdb_bson::{Bson, Document};
use eggdb_keyspace::{KeysRead, KeysWrite};

const COUNTER: &[u8] = b"\x00next";
const NODE: u8 = 1;
const RELATIONSHIP: u8 = 2;
const OUTGOING: u8 = 3;
const INCOMING: u8 = 4;
const SCHEMA_RULE: u8 = 5;
const LABEL_INDEX: u8 = 6;
const PROPERTY_INDEX: u8 = 7;
const INDEXES_BUILT: &[u8] = b"\x00indexed";

/// What a schema rule asks of nodes with its label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchemaKind {
    Unique,
    Exists,
    Index,
}

/// A constraint or an index on a label's properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaRule {
    pub(crate) name: String,
    pub(crate) kind: SchemaKind,
    pub(crate) label: String,
    pub(crate) properties: Vec<String>,
}

impl SchemaRule {
    /// Whether the rule keeps index entries: indexes and uniqueness constraints do.
    pub(crate) fn indexes(&self) -> bool {
        matches!(self.kind, SchemaKind::Index | SchemaKind::Unique)
    }
}

fn label_prefix(label: &str) -> Vec<u8> {
    let mut key = vec![LABEL_INDEX];
    key.extend_from_slice(label.as_bytes());
    key.push(0);
    key
}

const INTEGER_TAG: u8 = b'i';
const FLOAT_TAG: u8 = b'f';
const STRING_TAG: u8 = b's';
const BOOLEAN_TAG: u8 = b'b';
const STRING_END: [u8; 2] = [0, 1];
const ESCAPED_ZERO: [u8; 2] = [0, 0xFF];
const ORDERED_INDEXES: u8 = 2;

fn ordered_integer(number: i64) -> [u8; 8] {
    (number as u64 ^ (1 << 63)).to_be_bytes()
}

fn ordered_float(number: f64) -> [u8; 8] {
    let bits = number.to_bits();
    let ordered = if bits >> 63 == 1 { !bits } else { bits | (1 << 63) };
    ordered.to_be_bytes()
}

fn tagged(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 1);
    out.push(tag);
    out.extend_from_slice(body);
    out
}

fn escaped_text(text: &str, out: &mut Vec<u8>) {
    for &byte in text.as_bytes() {
        if byte == 0 {
            out.extend_from_slice(&ESCAPED_ZERO);
        } else {
            out.push(byte);
        }
    }
}

fn index_bytes(value: &Value) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match value {
        Value::Integer(number) => {
            out.push(INTEGER_TAG);
            out.extend_from_slice(&ordered_integer(*number));
        }
        Value::Float(number) if number.fract() == 0.0 && (-9.2e18..9.2e18).contains(number) => {
            out.push(INTEGER_TAG);
            out.extend_from_slice(&ordered_integer(*number as i64));
        }
        Value::Float(number) if !number.is_nan() => {
            out.push(FLOAT_TAG);
            out.extend_from_slice(&ordered_float(*number));
        }
        Value::String(text) => {
            out.push(STRING_TAG);
            escaped_text(text, &mut out);
            out.extend_from_slice(&STRING_END);
        }
        Value::Boolean(flag) => out.extend_from_slice(&[BOOLEAN_TAG, u8::from(*flag)]),
        _ => return None,
    }
    Some(out)
}

fn value_of_index_bytes(bytes: &[u8]) -> Option<Value> {
    let (tag, rest) = bytes.split_first()?;
    Some(match *tag {
        INTEGER_TAG => Value::Integer((u64::from_be_bytes(rest.try_into().ok()?) ^ (1 << 63)) as i64),
        FLOAT_TAG => {
            let ordered = u64::from_be_bytes(rest.try_into().ok()?);
            let bits = if ordered >> 63 == 1 { ordered & !(1 << 63) } else { !ordered };
            Value::Float(f64::from_bits(bits))
        }
        STRING_TAG => {
            let body = rest.strip_suffix(&STRING_END)?;
            let mut text = Vec::with_capacity(body.len());
            let mut at = 0;
            while at < body.len() {
                if body[at] == 0 {
                    if body.get(at + 1) != Some(&0xFF) {
                        return None;
                    }
                    text.push(0);
                    at += 2;
                } else {
                    text.push(body[at]);
                    at += 1;
                }
            }
            Value::String(String::from_utf8(text).ok()?)
        }
        BOOLEAN_TAG => Value::Boolean(rest.first().copied()? != 0),
        _ => return None,
    })
}

/// The bounds `WHERE` puts on one indexed property: a lowest and a highest value, each
/// inclusive or not, and a text prefix.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct IndexRange {
    lower: Option<(Value, bool)>,
    upper: Option<(Value, bool)>,
    prefix: Option<String>,
    impossible: bool,
    unusable: bool,
}

fn compare_indexed(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    let number = |value: &Value| match value {
        Value::Integer(number) => Some(*number as f64),
        Value::Float(number) => Some(*number),
        _ => None,
    };
    match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => Some(left.cmp(right)),
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        (Value::Boolean(left), Value::Boolean(right)) => Some(left.cmp(right)),
        _ => number(left)?.partial_cmp(&number(right)?),
    }
}

impl IndexRange {
    /// Narrows the range by `property <operator> value`. A `NULL` bound matches nothing, as
    /// the comparison does; a bound the index cannot hold (a list, a temporal value) makes the
    /// range unusable, so the label is scanned instead.
    pub(crate) fn narrow(&mut self, operator: crate::cypher::Operator, value: Value) {
        use crate::cypher::Operator;
        if value == Value::Null {
            self.impossible = true;
            return;
        }
        if index_bytes(&value).is_none() {
            self.unusable = true;
            return;
        }
        match operator {
            Operator::Greater | Operator::GreaterOrEqual => {
                let inclusive = operator == Operator::GreaterOrEqual;
                let tighter = match &self.lower {
                    None => true,
                    Some((known, _)) => compare_indexed(&value, known) == Some(std::cmp::Ordering::Greater),
                };
                if tighter {
                    self.lower = Some((value, inclusive));
                }
            }
            Operator::Less | Operator::LessOrEqual => {
                let inclusive = operator == Operator::LessOrEqual;
                let tighter = match &self.upper {
                    None => true,
                    Some((known, _)) => compare_indexed(&value, known) == Some(std::cmp::Ordering::Less),
                };
                if tighter {
                    self.upper = Some((value, inclusive));
                }
            }
            Operator::StartsWith => match value {
                Value::String(prefix) => {
                    if self.prefix.as_ref().is_none_or(|known| prefix.len() > known.len()) {
                        self.prefix = Some(prefix);
                    }
                }
                _ => self.impossible = true,
            },
            _ => {}
        }
    }

    /// Whether the index cannot serve the range, or the range bounds nothing, so a label scan
    /// is as good.
    pub(crate) fn is_open(&self) -> bool {
        self.unusable || (!self.impossible && self.lower.is_none() && self.upper.is_none() && self.prefix.is_none())
    }

    /// The key ranges after a rule's prefix that hold every value in the range: one per kind
    /// of value the bounds compare with (integers and floats are kept apart, so a number range
    /// reads both), each from its lowest possible key to the first key after its highest.
    fn key_ranges(&self) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        if self.impossible {
            return Vec::new();
        }
        if let Some(prefix) = &self.prefix {
            let mut start = vec![STRING_TAG];
            escaped_text(prefix, &mut start);
            let end = eggdb_keyspace::keys_after_prefix(&start);
            return vec![(start, end)];
        }
        let lower = self.lower.as_ref().map(|(value, _)| value);
        let upper = self.upper.as_ref().map(|(value, _)| value);
        let number = |value: &Value| match value {
            Value::Integer(number) => Some(*number as f64),
            Value::Float(number) => Some(*number),
            _ => None,
        };
        let numeric = lower.or(upper).is_some_and(|value| number(value).is_some());
        if numeric {
            let low = lower.and_then(number);
            let high = upper.and_then(number);
            if low.is_some_and(f64::is_nan) || high.is_some_and(f64::is_nan) {
                return Vec::new();
            }
            let integer_bound = |bound: f64| {
                if bound <= i64::MIN as f64 {
                    i64::MIN
                } else if bound >= i64::MAX as f64 {
                    i64::MAX
                } else {
                    bound as i64
                }
            };
            let integers_start = low.map_or_else(
                || vec![INTEGER_TAG],
                |low| tagged(INTEGER_TAG, &ordered_integer(integer_bound(low.floor()))),
            );
            let integers_end = match high {
                Some(high) => eggdb_keyspace::keys_after_prefix(&tagged(INTEGER_TAG, &ordered_integer(integer_bound(high.ceil())))),
                None => Some(vec![INTEGER_TAG + 1]),
            };
            let floats_start = low.map_or_else(|| vec![FLOAT_TAG], |low| tagged(FLOAT_TAG, &ordered_float(low)));
            let floats_end = match high {
                Some(high) => eggdb_keyspace::keys_after_prefix(&tagged(FLOAT_TAG, &ordered_float(high))),
                None => Some(vec![FLOAT_TAG + 1]),
            };
            return vec![(floats_start, floats_end), (integers_start, integers_end)];
        }
        let tag = match lower.or(upper) {
            Some(Value::String(_)) => STRING_TAG,
            Some(Value::Boolean(_)) => BOOLEAN_TAG,
            _ => return Vec::new(),
        };
        let bound = |value: Option<&Value>| match value {
            Some(Value::String(text)) => {
                let mut key = vec![STRING_TAG];
                escaped_text(text, &mut key);
                Some(key)
            }
            Some(Value::Boolean(flag)) => Some(vec![BOOLEAN_TAG, u8::from(*flag)]),
            _ => None,
        };
        let start = bound(lower).unwrap_or_else(|| vec![tag]);
        let end = match bound(upper) {
            Some(high) => eggdb_keyspace::keys_after_prefix(&high),
            None => Some(vec![tag + 1]),
        };
        vec![(start, end)]
    }

    fn holds(&self, value: &Value) -> bool {
        if self.impossible {
            return false;
        }
        if let Some((lower, inclusive)) = &self.lower {
            match compare_indexed(value, lower) {
                Some(std::cmp::Ordering::Greater) => {}
                Some(std::cmp::Ordering::Equal) if *inclusive => {}
                _ => return false,
            }
        }
        if let Some((upper, inclusive)) = &self.upper {
            match compare_indexed(value, upper) {
                Some(std::cmp::Ordering::Less) => {}
                Some(std::cmp::Ordering::Equal) if *inclusive => {}
                _ => return false,
            }
        }
        match &self.prefix {
            Some(prefix) => matches!(value, Value::String(text) if text.starts_with(prefix.as_str())),
            None => true,
        }
    }
}

fn index_prefix(rule: &str, values: &[&Value]) -> Option<Vec<u8>> {
    let mut key = rule_prefix(rule);
    for value in values {
        key.extend_from_slice(&index_bytes(value)?);
    }
    Some(key)
}

fn rule_prefix(rule: &str) -> Vec<u8> {
    let mut key = vec![PROPERTY_INDEX];
    key.extend_from_slice(rule.as_bytes());
    key.push(0);
    key
}

fn with_id(mut key: Vec<u8>, id: u64) -> Vec<u8> {
    key.extend_from_slice(&id.to_be_bytes());
    key
}

fn ids_under(keys: &dyn KeysRead, prefix: &[u8]) -> GraphResult<Vec<u64>> {
    Ok(keys
        .scan(prefix)
        .map_err(storage)?
        .into_iter()
        .filter(|(key, _)| key.len() == prefix.len() + 8)
        .filter_map(|(key, _)| Some(u64::from_be_bytes(key.get(prefix.len()..)?.try_into().ok()?)))
        .collect())
}

fn index_entries(node: &Node, rules: &[SchemaRule]) -> Vec<Vec<u8>> {
    let mut entries: Vec<Vec<u8>> = node.labels.iter().map(|label| with_id(label_prefix(label), node.id)).collect();
    for rule in rules.iter().filter(|rule| rule.indexes() && node.labels.contains(&rule.label)) {
        let values: Option<Vec<&Value>> = rule.properties.iter().map(|property| property_of(node, property)).collect();
        if let Some(prefix) = values.and_then(|values| index_prefix(&rule.name, &values)) {
            entries.push(with_id(prefix, node.id));
        }
    }
    entries
}

fn rule_key(name: &str) -> Vec<u8> {
    let mut key = vec![SCHEMA_RULE];
    key.extend_from_slice(name.as_bytes());
    key
}

fn property_of<'n>(node: &'n Node, name: &str) -> Option<&'n Value> {
    node.properties
        .iter()
        .find(|(property, _)| property == name)
        .map(|(_, value)| value)
        .filter(|value| !matches!(value, Value::Null))
}

fn violation(rule: &SchemaRule, node: u64) -> GraphError {
    GraphError::new(
        "Neo.ClientError.Schema.ConstraintValidationFailed",
        match rule.kind {
            SchemaKind::Exists => format!(
                "Node({node}) with label `{}` must have the property `{}`",
                rule.label,
                rule.properties.join("`, `")
            ),
            _ => format!(
                "Node({node}) already exists with label `{}` and properties `{}`",
                rule.label,
                rule.properties.join("`, `")
            ),
        },
    )
}

fn key(kind: u8, id: u64) -> Vec<u8> {
    let mut key = vec![kind];
    key.extend_from_slice(&id.to_be_bytes());
    key
}

fn adjacency(kind: u8, node: u64, relationship: u64) -> Vec<u8> {
    let mut key = key(kind, node);
    key.extend_from_slice(&relationship.to_be_bytes());
    key
}

fn storage(message: String) -> GraphError {
    GraphError::new("Neo.DatabaseError.General.UnknownError", message)
}

fn decode(bytes: &[u8]) -> GraphResult<Document> {
    Document::from_bytes(bytes)
        .map_err(|error| storage(format!("a stored graph record is damaged: {error}")))
}

fn properties_of(document: &Document) -> Vec<(String, Value)> {
    let mut properties: Vec<(String, Value)> = document
        .get_document("p")
        .map(|map| {
            map.iter()
                .map(|(name, value)| (name.clone(), from_bson(value)))
                .collect()
        })
        .unwrap_or_default();
    properties.sort_by(|a, b| a.0.cmp(&b.0));
    properties
}

fn properties_document(properties: &[(String, Value)]) -> GraphResult<Document> {
    let mut map = Document::new();
    for (name, value) in properties {
        if matches!(value, Value::Null) {
            continue;
        }
        let stored = to_bson(value).ok_or_else(|| {
            GraphError::new(
                "Neo.ClientError.Statement.TypeError",
                format!(
                    "Property values can only be of primitive types or arrays thereof, not {}",
                    value.type_name()
                ),
            )
        })?;
        map.insert(name.clone(), stored);
    }
    Ok(map)
}

/// Reads the graph within a transaction.
pub(crate) struct GraphRead<'k> {
    pub(crate) keys: &'k dyn KeysRead,
}

impl GraphRead<'_> {
    pub(crate) fn node(&self, id: u64) -> GraphResult<Option<Node>> {
        let Some(bytes) = self.keys.get(&key(NODE, id)).map_err(storage)? else {
            return Ok(None);
        };
        let document = decode(&bytes)?;
        let labels = document
            .get_array("l")
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|label| label.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Some(Node {
            id,
            labels,
            properties: properties_of(&document),
        }))
    }

    pub(crate) fn relationship(&self, id: u64) -> GraphResult<Option<Relationship>> {
        let Some(bytes) = self.keys.get(&key(RELATIONSHIP, id)).map_err(storage)? else {
            return Ok(None);
        };
        let document = decode(&bytes)?;
        let number = |name: &str| {
            document
                .get_i64(name)
                .map(|value| value as u64)
                .unwrap_or(0)
        };
        Ok(Some(Relationship {
            id,
            start: number("s"),
            end: number("e"),
            kind: document.get_str("t").unwrap_or_default().to_owned(),
            properties: properties_of(&document),
        }))
    }

    pub(crate) fn schema_rules(&self) -> GraphResult<Vec<SchemaRule>> {
        let mut rules = Vec::new();
        for (key, value) in self.keys.scan(&[SCHEMA_RULE]).map_err(storage)? {
            let document = decode(&value)?;
            let kind = match document.get_str("k").unwrap_or_default() {
                "unique" => SchemaKind::Unique,
                "exists" => SchemaKind::Exists,
                _ => SchemaKind::Index,
            };
            let properties = match document.get("p") {
                Some(Bson::Array(items)) => items
                    .iter()
                    .filter_map(|item| match item {
                        Bson::String(name) => Some(name.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            rules.push(SchemaRule {
                name: String::from_utf8_lossy(&key[1..]).into_owned(),
                kind,
                label: document.get_str("l").unwrap_or_default().to_owned(),
                properties,
            });
        }
        Ok(rules)
    }

    fn violated(&self, rule: &SchemaRule, node: &Node) -> GraphResult<bool> {
        if !node.labels.contains(&rule.label) {
            return Ok(false);
        }
        match rule.kind {
            SchemaKind::Index => Ok(false),
            SchemaKind::Exists => Ok(rule
                .properties
                .iter()
                .any(|property| property_of(node, property).is_none())),
            SchemaKind::Unique => {
                let Some(values) = rule
                    .properties
                    .iter()
                    .map(|property| property_of(node, property))
                    .collect::<Option<Vec<&Value>>>()
                else {
                    return Ok(false);
                };
                let candidates = match self.nodes_by_index(rule, &values)? {
                    Some(found) => found,
                    None => self.node_ids()?,
                };
                for other in candidates {
                    if other == node.id {
                        continue;
                    }
                    let Some(other) = self.node(other)? else {
                        continue;
                    };
                    if !other.labels.contains(&rule.label) {
                        continue;
                    }
                    let same = rule.properties.iter().zip(&values).all(|(property, value)| {
                        property_of(&other, property)
                            .is_some_and(|found| crate::value::equals(found, value) == Some(true))
                    });
                    if same {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    /// Whether the label and property indexes hold every node; until the first write after
    /// they came in, lookups scan instead.
    pub(crate) fn indexed(&self) -> GraphResult<bool> {
        Ok(self.keys.get(INDEXES_BUILT).map_err(storage)?.as_deref() == Some(&[ORDERED_INDEXES][..]))
    }

    /// The nodes with `label`, from the label index, or `None` while it is not built.
    pub(crate) fn nodes_with_label(&self, label: &str) -> GraphResult<Option<Vec<u64>>> {
        if !self.indexed()? {
            return Ok(None);
        }
        ids_under(self.keys, &label_prefix(label)).map(Some)
    }

    /// The nodes whose properties under `rule` equal `values`, from its index, or `None` when
    /// the index is not built or a value cannot be indexed.
    pub(crate) fn nodes_by_index(&self, rule: &SchemaRule, values: &[&Value]) -> GraphResult<Option<Vec<u64>>> {
        if !rule.indexes() || !self.indexed()? || self.keys.get(&rule_key(&rule.name)).map_err(storage)?.is_none() {
            return Ok(None);
        }
        let Some(prefix) = index_prefix(&rule.name, values) else {
            return Ok(None);
        };
        ids_under(self.keys, &prefix).map(Some)
    }

    /// The nodes whose one property under `rule` lies in `range`, read from the rule's index
    /// entries without reading the nodes, or `None` when the index is not built.
    pub(crate) fn nodes_in_range(&self, rule: &SchemaRule, range: &IndexRange) -> GraphResult<Option<Vec<u64>>> {
        if !rule.indexes() || rule.properties.len() != 1 || !self.indexed()? || self.keys.get(&rule_key(&rule.name)).map_err(storage)?.is_none() {
            return Ok(None);
        }
        let prefix = rule_prefix(&rule.name);
        let mut found = Vec::new();
        for (start, end) in range.key_ranges() {
            let start = [prefix.as_slice(), start.as_slice()].concat();
            let end = match end {
                Some(end) => Some([prefix.as_slice(), end.as_slice()].concat()),
                None => eggdb_keyspace::keys_after_prefix(&prefix),
            };
            self.keys
                .visit_range(&start, end.as_deref(), &mut |key, _| {
                    let Some(rest) = key.get(prefix.len()..) else {
                        return Ok(true);
                    };
                    let Some(split) = rest.len().checked_sub(8) else {
                        return Ok(true);
                    };
                    let (bytes, id) = rest.split_at(split);
                    if let (Some(value), Ok(id)) = (value_of_index_bytes(bytes), <[u8; 8]>::try_from(id))
                        && range.holds(&value)
                    {
                        found.push(u64::from_be_bytes(id));
                    }
                    Ok(true)
                })
                .map_err(storage)?;
        }
        Ok(Some(found))
    }

    pub(crate) fn node_ids(&self) -> GraphResult<Vec<u64>> {
        Ok(self
            .keys
            .scan(&[NODE])
            .map_err(storage)?
            .into_iter()
            .filter_map(|(key, _)| Some(u64::from_be_bytes(key.get(1..9)?.try_into().ok()?)))
            .collect())
    }

    /// The relationships leaving (`outgoing`) or entering a node.
    pub(crate) fn touching(&self, node: u64, outgoing: bool) -> GraphResult<Vec<u64>> {
        let prefix = key(if outgoing { OUTGOING } else { INCOMING }, node);
        Ok(self
            .keys
            .scan(&prefix)
            .map_err(storage)?
            .into_iter()
            .filter_map(|(key, _)| Some(u64::from_be_bytes(key.get(9..17)?.try_into().ok()?)))
            .collect())
    }
}

/// Changes the graph within a transaction.
pub(crate) struct GraphWrite<'k> {
    pub(crate) keys: &'k mut dyn KeysWrite,
}

impl GraphWrite<'_> {
    pub(crate) fn read(&self) -> GraphRead<'_> {
        GraphRead { keys: &*self.keys }
    }

    fn ensure_indexed(&mut self) -> GraphResult<()> {
        if self.read().indexed()? {
            return Ok(());
        }
        for (entry, _) in self.keys.scan(&[PROPERTY_INDEX]).map_err(storage)? {
            self.keys.delete(&entry).map_err(storage)?;
        }
        let rules = self.read().schema_rules()?;
        for id in self.read().node_ids()? {
            if let Some(node) = self.read().node(id)? {
                for entry in index_entries(&node, &rules) {
                    self.keys.put(&entry, &[]).map_err(storage)?;
                }
            }
        }
        self.keys.put(INDEXES_BUILT, &[ORDERED_INDEXES]).map_err(storage)
    }

    fn fill_rule_index(&mut self, rule: &SchemaRule) -> GraphResult<()> {
        if !rule.indexes() {
            return Ok(());
        }
        let ids = self.read().nodes_with_label(&rule.label)?.unwrap_or_default();
        for id in ids {
            if let Some(node) = self.read().node(id)? {
                for entry in index_entries(&node, std::slice::from_ref(rule)).into_iter().filter(|entry| entry.first() == Some(&PROPERTY_INDEX)) {
                    self.keys.put(&entry, &[]).map_err(storage)?;
                }
            }
        }
        Ok(())
    }

    fn next_id(&mut self) -> GraphResult<u64> {
        let current = self
            .keys
            .get(COUNTER)
            .map_err(storage)?
            .and_then(|bytes| bytes.try_into().ok())
            .map_or(0, u64::from_be_bytes);
        self.keys
            .put(COUNTER, &(current + 1).to_be_bytes())
            .map_err(storage)?;
        Ok(current)
    }

    pub(crate) fn put_node(
        &mut self,
        id: Option<u64>,
        labels: &[String],
        properties: &[(String, Value)],
    ) -> GraphResult<u64> {
        self.ensure_indexed()?;
        let id = match id {
            Some(id) => id,
            None => self.next_id()?,
        };
        let mut document = Document::new();
        let mut sorted = labels.to_vec();
        sorted.sort();
        sorted.dedup();
        document.insert(
            "l",
            Bson::Array(sorted.into_iter().map(Bson::String).collect()),
        );
        document.insert("p", Bson::Document(properties_document(properties)?));
        let rules = self.read().schema_rules()?;
        let mut kept = labels.to_vec();
        kept.sort();
        kept.dedup();
        let mut shown = properties.to_vec();
        shown.sort_by(|a, b| a.0.cmp(&b.0));
        let candidate = Node {
            id,
            labels: kept,
            properties: shown,
        };
        for rule in &rules {
            if self.read().violated(rule, &candidate)? {
                return Err(violation(rule, id));
            }
        }
        if let Some(previous) = self.read().node(id)? {
            for entry in index_entries(&previous, &rules) {
                self.keys.delete(&entry).map_err(storage)?;
            }
        }
        self.keys
            .put(&key(NODE, id), &document.to_bytes())
            .map_err(storage)?;
        for entry in index_entries(&candidate, &rules) {
            self.keys.put(&entry, &[]).map_err(storage)?;
        }
        Ok(id)
    }

    pub(crate) fn check_existing_nodes(&self, rule: &SchemaRule) -> GraphResult<()> {
        let read = self.read();
        for id in read.node_ids()? {
            if let Some(node) = read.node(id)?
                && read.violated(rule, &node)?
            {
                return Err(GraphError::new(
                    "Neo.ClientError.Schema.ConstraintCreationFailed",
                    format!(
                        "Unable to create constraint '{}': {}",
                        rule.name,
                        violation(rule, id).message
                    ),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn put_schema_rule(&mut self, rule: &SchemaRule) -> GraphResult<()> {
        self.ensure_indexed()?;
        let mut document = Document::new();
        document.insert(
            "k",
            Bson::String(
                match rule.kind {
                    SchemaKind::Unique => "unique",
                    SchemaKind::Exists => "exists",
                    SchemaKind::Index => "index",
                }
                .to_owned(),
            ),
        );
        document.insert("l", Bson::String(rule.label.clone()));
        document.insert(
            "p",
            Bson::Array(rule.properties.iter().cloned().map(Bson::String).collect()),
        );
        self.keys
            .put(&rule_key(&rule.name), &document.to_bytes())
            .map_err(storage)?;
        self.fill_rule_index(rule)
    }

    pub(crate) fn delete_schema_rule(&mut self, name: &str, index: bool) -> GraphResult<bool> {
        let Some(rule) = self
            .read()
            .schema_rules()?
            .into_iter()
            .find(|rule| rule.name == name)
        else {
            return Ok(false);
        };
        if (rule.kind == SchemaKind::Index) != index {
            return Ok(false);
        }
        self.keys.delete(&rule_key(name)).map_err(storage)?;
        for (entry, _) in self.keys.scan(&rule_prefix(name)).map_err(storage)? {
            self.keys.delete(&entry).map_err(storage)?;
        }
        Ok(true)
    }

    pub(crate) fn put_relationship(
        &mut self,
        id: Option<u64>,
        start: u64,
        end: u64,
        kind: &str,
        properties: &[(String, Value)],
    ) -> GraphResult<u64> {
        let id = match id {
            Some(id) => id,
            None => self.next_id()?,
        };
        let mut document = Document::new();
        document.insert("s", Bson::Int64(start as i64));
        document.insert("e", Bson::Int64(end as i64));
        document.insert("t", Bson::String(kind.to_owned()));
        document.insert("p", Bson::Document(properties_document(properties)?));
        self.keys
            .put(&key(RELATIONSHIP, id), &document.to_bytes())
            .map_err(storage)?;
        self.keys
            .put(&adjacency(OUTGOING, start, id), &[])
            .map_err(storage)?;
        self.keys
            .put(&adjacency(INCOMING, end, id), &[])
            .map_err(storage)?;
        Ok(id)
    }

    pub(crate) fn delete_relationship(&mut self, id: u64) -> GraphResult<bool> {
        let Some(relationship) = self.read().relationship(id)? else {
            return Ok(false);
        };
        self.keys.delete(&key(RELATIONSHIP, id)).map_err(storage)?;
        self.keys
            .delete(&adjacency(OUTGOING, relationship.start, id))
            .map_err(storage)?;
        self.keys
            .delete(&adjacency(INCOMING, relationship.end, id))
            .map_err(storage)?;
        Ok(true)
    }

    pub(crate) fn delete_node(&mut self, id: u64, detach: bool) -> GraphResult<bool> {
        let touching: Vec<u64> = [true, false]
            .iter()
            .map(|outgoing| self.read().touching(id, *outgoing))
            .collect::<GraphResult<Vec<_>>>()?
            .concat();
        if !touching.is_empty() && !detach {
            return Err(GraphError::new(
                "Neo.ClientError.Schema.ConstraintValidationFailed",
                format!(
                    "Cannot delete node<{id}>, because it still has relationships. To delete this node, you must first delete its relationships."
                ),
            ));
        }
        for relationship in touching {
            self.delete_relationship(relationship)?;
        }
        self.ensure_indexed()?;
        if let Some(node) = self.read().node(id)? {
            let rules = self.read().schema_rules()?;
            for entry in index_entries(&node, &rules) {
                self.keys.delete(&entry).map_err(storage)?;
            }
        }
        self.keys.delete(&key(NODE, id)).map_err(storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_keys_sort_as_their_values_and_read_back() {
        let values = [
            Value::Integer(i64::MIN),
            Value::Integer(-5),
            Value::Integer(0),
            Value::Integer(7),
            Value::Integer(i64::MAX),
        ];
        let keys: Vec<Vec<u8>> = values.iter().filter_map(index_bytes).collect();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        let floats = [Value::Float(-2.5), Value::Float(-0.25), Value::Float(0.5), Value::Float(1e300)];
        let keys: Vec<Vec<u8>> = floats.iter().filter_map(index_bytes).collect();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        let texts = [Value::String("a".into()), Value::String("a\0b".into()), Value::String("ab".into()), Value::String("b".into())];
        let keys: Vec<Vec<u8>> = texts.iter().filter_map(index_bytes).collect();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        for value in values.iter().chain(&floats).chain(&texts) {
            assert_eq!(value_of_index_bytes(&index_bytes(value).unwrap()).as_ref(), Some(value));
        }
        let mut range = IndexRange::default();
        range.narrow(crate::cypher::Operator::GreaterOrEqual, Value::Float(1.5));
        range.narrow(crate::cypher::Operator::Less, Value::Integer(4));
        let ranges = range.key_ranges();
        assert_eq!(ranges.len(), 2);
        let two = index_bytes(&Value::Integer(2)).unwrap();
        assert!(ranges.iter().any(|(start, end)| *start <= two && end.as_ref().is_none_or(|end| two < *end)));
    }
}
