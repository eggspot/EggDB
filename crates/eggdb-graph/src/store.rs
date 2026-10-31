use crate::value::{Node, Relationship, Value, from_bson, to_bson};
use crate::{GraphError, GraphResult};
use eggdb_bson::{Bson, Document};
use eggdb_keyspace::{KeysRead, KeysWrite};

const COUNTER: &[u8] = b"\x00next";
const NODE: u8 = 1;
const RELATIONSHIP: u8 = 2;
const OUTGOING: u8 = 3;
const INCOMING: u8 = 4;

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
    Document::from_bytes(bytes).map_err(|error| storage(format!("a stored graph record is damaged: {error}")))
}

fn properties_of(document: &Document) -> Vec<(String, Value)> {
    let mut properties: Vec<(String, Value)> = document
        .get_document("p")
        .map(|map| map.iter().map(|(name, value)| (name.clone(), from_bson(value))).collect())
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
                format!("Property values can only be of primitive types or arrays thereof, not {}", value.type_name()),
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
        let labels = document.get_array("l").map(|labels| labels.iter().filter_map(|label| label.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        Ok(Some(Node { id, labels, properties: properties_of(&document) }))
    }

    pub(crate) fn relationship(&self, id: u64) -> GraphResult<Option<Relationship>> {
        let Some(bytes) = self.keys.get(&key(RELATIONSHIP, id)).map_err(storage)? else {
            return Ok(None);
        };
        let document = decode(&bytes)?;
        let number = |name: &str| document.get_i64(name).map(|value| value as u64).unwrap_or(0);
        Ok(Some(Relationship {
            id,
            start: number("s"),
            end: number("e"),
            kind: document.get_str("t").unwrap_or_default().to_owned(),
            properties: properties_of(&document),
        }))
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

    pub(crate) fn relationship_ids(&self) -> GraphResult<Vec<u64>> {
        Ok(self
            .keys
            .scan(&[RELATIONSHIP])
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

    fn next_id(&mut self) -> GraphResult<u64> {
        let current = self.keys.get(COUNTER).map_err(storage)?.and_then(|bytes| bytes.try_into().ok()).map_or(0, u64::from_be_bytes);
        self.keys.put(COUNTER, &(current + 1).to_be_bytes()).map_err(storage)?;
        Ok(current)
    }

    pub(crate) fn put_node(&mut self, id: Option<u64>, labels: &[String], properties: &[(String, Value)]) -> GraphResult<u64> {
        let id = match id {
            Some(id) => id,
            None => self.next_id()?,
        };
        let mut document = Document::new();
        let mut sorted = labels.to_vec();
        sorted.sort();
        sorted.dedup();
        document.insert("l", Bson::Array(sorted.into_iter().map(Bson::String).collect()));
        document.insert("p", Bson::Document(properties_document(properties)?));
        self.keys.put(&key(NODE, id), &document.to_bytes()).map_err(storage)?;
        Ok(id)
    }

    pub(crate) fn put_relationship(&mut self, id: Option<u64>, start: u64, end: u64, kind: &str, properties: &[(String, Value)]) -> GraphResult<u64> {
        let id = match id {
            Some(id) => id,
            None => self.next_id()?,
        };
        let mut document = Document::new();
        document.insert("s", Bson::Int64(start as i64));
        document.insert("e", Bson::Int64(end as i64));
        document.insert("t", Bson::String(kind.to_owned()));
        document.insert("p", Bson::Document(properties_document(properties)?));
        self.keys.put(&key(RELATIONSHIP, id), &document.to_bytes()).map_err(storage)?;
        self.keys.put(&adjacency(OUTGOING, start, id), &[]).map_err(storage)?;
        self.keys.put(&adjacency(INCOMING, end, id), &[]).map_err(storage)?;
        Ok(id)
    }

    pub(crate) fn delete_relationship(&mut self, id: u64) -> GraphResult<bool> {
        let Some(relationship) = self.read().relationship(id)? else {
            return Ok(false);
        };
        self.keys.delete(&key(RELATIONSHIP, id)).map_err(storage)?;
        self.keys.delete(&adjacency(OUTGOING, relationship.start, id)).map_err(storage)?;
        self.keys.delete(&adjacency(INCOMING, relationship.end, id)).map_err(storage)?;
        Ok(true)
    }

    pub(crate) fn delete_node(&mut self, id: u64, detach: bool) -> GraphResult<bool> {
        let touching: Vec<u64> = [true, false].iter().map(|outgoing| self.read().touching(id, *outgoing)).collect::<GraphResult<Vec<_>>>()?.concat();
        if !touching.is_empty() && !detach {
            return Err(GraphError::new(
                "Neo.ClientError.Schema.ConstraintValidationFailed",
                format!("Cannot delete node<{id}>, because it still has relationships. To delete this node, you must first delete its relationships."),
            ));
        }
        for relationship in touching {
            self.delete_relationship(relationship)?;
        }
        self.keys.delete(&key(NODE, id)).map_err(storage)
    }
}
