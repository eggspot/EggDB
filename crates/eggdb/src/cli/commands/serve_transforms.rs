use eggdb_sql::{KeyValueStore, Session, Value};

pub(super) const TRANSFORM_SCHEMA: &str = "storage_transforms";

pub(super) struct SqlObjectTransforms<'s, S: KeyValueStore> {
    pub(super) store: &'s S,
    pub(super) user: String,
    pub(super) database: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

impl<S: KeyValueStore + Sync> eggdb_s3::ObjectTransform for SqlObjectTransforms<'_, S> {
    fn run(&self, name: &str, bytes: &[u8], content_type: &str) -> Result<Vec<u8>, String> {
        if name.is_empty() || !name.chars().all(|character| character.is_ascii_alphanumeric() || character == '_') {
            return Err(format!("'{name}' is not a transform name"));
        }
        let mut session = Session::new(self.store);
        session.identify(&self.user, &self.database);
        let sql = format!(
            "SELECT {TRANSFORM_SCHEMA}.{name}(decode('{}', 'hex'), {})",
            hex(bytes),
            quoted(content_type)
        );
        let mut returned = None;
        session
            .execute(&sql, &mut |result| {
                if let Some(value) = result.rows.first().and_then(|row| row.first()) {
                    returned = Some(value.clone());
                }
            })
            .map_err(|error| format!("{TRANSFORM_SCHEMA}.{name}: {error}"))?;
        match returned {
            Some(Value::Bytea(bytes)) => Ok(bytes),
            Some(Value::Null) | None => Err(format!("{TRANSFORM_SCHEMA}.{name} returned NULL")),
            Some(_) => Err(format!("{TRANSFORM_SCHEMA}.{name} must return bytea")),
        }
    }
}
