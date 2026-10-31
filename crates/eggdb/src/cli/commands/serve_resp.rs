use eggdb::{Database, ErrorKind, ReadCollection, WriteCollection};
use eggdb_resp::{KeysRead, KeysWrite, Keyspace};

pub(super) struct DatabaseKeyspace<'d> {
    pub(super) db: &'d Database,
    pub(super) prefix: &'static str,
}

pub(super) const RESP_COLLECTIONS: &str = "resp";
pub(super) const MEMCACHED_COLLECTIONS: &str = "memcached";

fn failed(error: &eggdb::Error) -> String {
    format!("ERR {error}")
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
}

impl KeysWrite for WriteKeys<'_, '_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), String> {
        self.collection.put(key, value).map_err(|error| failed(&error))
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
    fn read(&self, db: u32, body: &mut dyn FnMut(&dyn KeysRead) -> Result<(), String>) -> Result<(), String> {
        let transaction = self.db.begin_read().map_err(|error| failed(&error))?;
        let collection = match transaction.collection(&format!("{}{db}", self.prefix)) {
            Ok(collection) => Some(collection),
            Err(error) if error.kind() == ErrorKind::CollectionNotFound => None,
            Err(error) => return Err(failed(&error)),
        };
        body(&ReadKeys { collection })
    }

    fn write(&self, db: u32, body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>) -> Result<(), String> {
        let mut transaction = self.db.begin_write().map_err(|error| failed(&error))?;
        let name = format!("{}{db}", self.prefix);
        let outcome = transaction
            .create_collection(&name)
            .map_err(|error| failed(&error))
            .and_then(|_| {
                let collection = transaction.collection(&name).map_err(|error| failed(&error))?;
                body(&mut WriteKeys { collection })
            });
        match outcome {
            Ok(()) => transaction.commit().map(|_| ()).map_err(|error| failed(&error)),
            Err(error) => {
                let _ = transaction.rollback();
                Err(error)
            }
        }
    }
}
