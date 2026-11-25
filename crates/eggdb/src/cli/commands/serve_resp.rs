pub(super) use eggdb::keyspace::{DatabaseKeyspace, RESP_COLLECTIONS, MEMCACHED_COLLECTIONS, ES_COLLECTIONS, MONGO_COLLECTIONS, GRAPH_COLLECTIONS, RDF_COLLECTIONS, MAIL_COLLECTIONS, DAV_COLLECTIONS, KAFKA_COLLECTIONS, QUEUE_COLLECTIONS, MQTT_COLLECTIONS, NATS_COLLECTIONS, ZOOKEEPER_COLLECTIONS, ETCD_COLLECTIONS, CONNECT_COLLECTIONS, OBJECT_COLLECTIONS, DYNAMODB_COLLECTIONS};

/// The database's stored roles as RESP users (D1364): a role that may log in and has a
/// password logs in with `AUTH <role> <password>`, reaching what `eggdb_sql::protocol_access`
/// gives it.
pub(super) struct RoleLogins<'d> {
    pub(super) db: &'d eggdb::Database,
}

impl eggdb_resp::Logins for RoleLogins<'_> {
    fn check(&self, user: &[u8], password: &[u8]) -> Option<eggdb_resp::LoginAccess> {
        let user = std::str::from_utf8(user).ok()?;
        let store = eggdb::sql::Store(self.db);
        let reader = eggdb_sql::KeyValueStore::begin_read(&store).ok()?;
        let verifier = eggdb_pgwire::ScramVerifier::from_stored(&eggdb_sql::login_verifier(&reader, user).ok()??)?;
        if !verifier.matches_password(password) {
            return None;
        }
        match eggdb_sql::protocol_access(&reader, user).ok()?? {
            eggdb_sql::ProtocolAccess::Read => Some(eggdb_resp::LoginAccess::Read),
            eggdb_sql::ProtocolAccess::ReadWrite => Some(eggdb_resp::LoginAccess::ReadWrite),
        }
    }
}
