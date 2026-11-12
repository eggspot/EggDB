use crate::{Database, ErrorKind, ReadCollection, WriteCollection};
use eggdb_keyspace::{EntryVisitor, KeysRead, KeysWrite, Keyspace};

/// One protocol's key-value databases inside an EggDB file: database `n` is the collection
/// `<prefix><n>`, so the messaging and key-value servers, and programs that embed EggDB, share
/// the same stored queues, keys and streams.
pub struct DatabaseKeyspace<'d> {
    /// The database file.
    pub db: &'d Database,
    /// The collection-name prefix of the protocol, such as [`QUEUE_COLLECTIONS`].
    pub prefix: &'static str,
}

/// The RESP (Redis protocol) databases.
pub const RESP_COLLECTIONS: &str = "resp";
/// The memcached items.
pub const MEMCACHED_COLLECTIONS: &str = "memcached";
/// The Elasticsearch-compatible indices.
pub const ES_COLLECTIONS: &str = "es";
/// The MongoDB-compatible databases.
pub const MONGO_COLLECTIONS: &str = "mongo";
/// The property graphs.
pub const GRAPH_COLLECTIONS: &str = "graph";
/// The RDF quads.
pub const RDF_COLLECTIONS: &str = "rdf";
/// The mailboxes and messages.
pub const MAIL_COLLECTIONS: &str = "mail";
/// The Kafka-compatible topics.
pub const KAFKA_COLLECTIONS: &str = "kafka";
/// The message queues (AMQP, STOMP, SQS and the in-process inbox).
pub const QUEUE_COLLECTIONS: &str = "queue";
/// The MQTT sessions and retained messages.
pub const MQTT_COLLECTIONS: &str = "mqtt";
/// The NATS JetStream streams.
pub const NATS_COLLECTIONS: &str = "nats";
/// The ZooKeeper-compatible tree.
pub const ZOOKEEPER_COLLECTIONS: &str = "zookeeper";
/// The S3-compatible objects.
pub const OBJECT_COLLECTIONS: &str = "objects";
/// The DynamoDB-compatible tables.
pub const DYNAMODB_COLLECTIONS: &str = "dynamodb";

fn failed(error: &crate::Error) -> String {
    format!("ERR {error}")
}

fn visit_scanned(
    scan: impl Iterator<Item = Result<(Vec<u8>, Vec<u8>), crate::Error>>,
    visit: &mut dyn FnMut(Vec<u8>) -> Result<bool, String>,
) -> Result<(), String> {
    for entry in scan {
        let (key, _) = entry.map_err(|error| failed(&error))?;
        if !visit(key)? {
            break;
        }
    }
    Ok(())
}

fn visit_entries(
    scan: impl Iterator<Item = Result<(Vec<u8>, Vec<u8>), crate::Error>>,
    visit: &mut EntryVisitor<'_>,
) -> Result<(), String> {
    for entry in scan {
        let (key, value) = entry.map_err(|error| failed(&error))?;
        if !visit(&key, &value)? {
            break;
        }
    }
    Ok(())
}

struct ReadKeys<'c> {
    collection: Option<ReadCollection<'c>>,
}

impl KeysRead for ReadKeys<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        match &self.collection {
            None => Ok(None),
            Some(collection) => collection.get(key).map_err(|error| failed(&error)),
        }
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
        let Some(collection) = &self.collection else {
            return Ok(Vec::new());
        };
        collection
            .scan_prefix(b"")
            .map(|entry| entry.map(|(key, _)| key).map_err(|error| failed(&error)))
            .collect()
    }

    fn visit_keys(
        &self,
        visit: &mut dyn FnMut(Vec<u8>) -> Result<bool, String>,
    ) -> Result<(), String> {
        let Some(collection) = &self.collection else {
            return Ok(());
        };
        visit_scanned(collection.scan_prefix(b""), visit)
    }

    fn scan(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, String> {
        let Some(collection) = &self.collection else {
            return Ok(Vec::new());
        };
        collection
            .scan_prefix(prefix)
            .map(|entry| entry.map_err(|error| failed(&error)))
            .collect()
    }

    fn visit_prefix(&self, prefix: &[u8], visit: &mut EntryVisitor<'_>) -> Result<(), String> {
        let Some(collection) = &self.collection else {
            return Ok(());
        };
        visit_entries(collection.scan_prefix(prefix), visit)
    }
}

struct WriteKeys<'t, 'db> {
    collection: WriteCollection<'t, 'db>,
}

impl KeysRead for WriteKeys<'_, '_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.collection.get(key).map_err(|error| failed(&error))
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
        self.collection
            .scan_prefix(b"")
            .map(|entry| entry.map(|(key, _)| key).map_err(|error| failed(&error)))
            .collect()
    }

    fn visit_keys(
        &self,
        visit: &mut dyn FnMut(Vec<u8>) -> Result<bool, String>,
    ) -> Result<(), String> {
        visit_scanned(self.collection.scan_prefix(b""), visit)
    }

    fn scan(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, String> {
        self.collection
            .scan_prefix(prefix)
            .map(|entry| entry.map_err(|error| failed(&error)))
            .collect()
    }

    fn visit_prefix(&self, prefix: &[u8], visit: &mut EntryVisitor<'_>) -> Result<(), String> {
        visit_entries(self.collection.scan_prefix(prefix), visit)
    }
}

impl KeysWrite for WriteKeys<'_, '_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), String> {
        self.collection
            .put(key, value)
            .map_err(|error| failed(&error))
    }

    fn delete(&mut self, key: &[u8]) -> Result<bool, String> {
        self.collection.delete(key).map_err(|error| failed(&error))
    }

    fn clear(&mut self) -> Result<(), String> {
        for key in self.keys()? {
            self.delete(&key)?;
        }
        Ok(())
    }
}

impl Keyspace for DatabaseKeyspace<'_> {
    fn read(
        &self,
        db: u32,
        body: &mut dyn FnMut(&dyn KeysRead) -> Result<(), String>,
    ) -> Result<(), String> {
        let transaction = self.db.begin_read().map_err(|error| failed(&error))?;
        let collection = match transaction.collection(&format!("{}{db}", self.prefix)) {
            Ok(collection) => Some(collection),
            Err(error) if error.kind() == ErrorKind::CollectionNotFound => None,
            Err(error) => return Err(failed(&error)),
        };
        body(&ReadKeys { collection })
    }

    fn write(
        &self,
        db: u32,
        body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut transaction = self.db.begin_write().map_err(|error| failed(&error))?;
        let name = format!("{}{db}", self.prefix);
        let outcome = transaction
            .create_collection(&name)
            .map_err(|error| failed(&error))
            .and_then(|_| {
                let collection = transaction
                    .collection(&name)
                    .map_err(|error| failed(&error))?;
                body(&mut WriteKeys { collection })
            });
        match outcome {
            Ok(()) => transaction
                .commit()
                .map(|_| ())
                .map_err(|error| failed(&error)),
            Err(error) => {
                let _ = transaction.rollback();
                Err(error)
            }
        }
    }

    fn read_record(&self, name: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let transaction = self.db.begin_read().map_err(|error| failed(&error))?;
        match transaction.collection(&records_collection(self.prefix)) {
            Ok(collection) => collection.get(name).map_err(|error| failed(&error)),
            Err(error) if error.kind() == ErrorKind::CollectionNotFound => Ok(None),
            Err(error) => Err(failed(&error)),
        }
    }

    fn write_record(&self, name: &[u8], value: Option<&[u8]>) -> Result<(), String> {
        let mut transaction = self.db.begin_write().map_err(|error| failed(&error))?;
        let collection_name = records_collection(self.prefix);
        let outcome = transaction
            .create_collection(&collection_name)
            .map_err(|error| failed(&error))
            .and_then(|_| {
                let mut collection = transaction
                    .collection(&collection_name)
                    .map_err(|error| failed(&error))?;
                match value {
                    Some(value) => collection.put(name, value).map_err(|error| failed(&error)),
                    None => collection
                        .delete(name)
                        .map(|_| ())
                        .map_err(|error| failed(&error)),
                }
            });
        match outcome {
            Ok(()) => transaction
                .commit()
                .map(|_| ())
                .map_err(|error| failed(&error)),
            Err(error) => {
                let _ = transaction.rollback();
                Err(error)
            }
        }
    }
}

fn records_collection(prefix: &str) -> String {
    format!("{prefix}_records")
}
