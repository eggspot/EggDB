pub(super) use eggdb::keyspace::{DatabaseKeyspace, RESP_COLLECTIONS, MEMCACHED_COLLECTIONS, ES_COLLECTIONS, MONGO_COLLECTIONS, GRAPH_COLLECTIONS, RDF_COLLECTIONS, MAIL_COLLECTIONS, DAV_COLLECTIONS, KAFKA_COLLECTIONS, QUEUE_COLLECTIONS, MQTT_COLLECTIONS, NATS_COLLECTIONS, ZOOKEEPER_COLLECTIONS, ETCD_COLLECTIONS, CONNECT_COLLECTIONS, OBJECT_COLLECTIONS, DYNAMODB_COLLECTIONS};

/// The database's stored roles as RESP users (D1364): a role that may log in and has a
/// password logs in with `AUTH <role> <password>`, reaching what `eggdb_sql::protocol_access`
/// gives it.
pub(super) struct RoleLogins<'d> {
    pub(super) db: &'d eggdb::Database,
}

impl RoleLogins<'_> {
    fn access(&self, user: &str, password: &[u8]) -> Option<eggdb_sql::ProtocolAccess> {
        let store = eggdb::sql::Store(self.db);
        let reader = eggdb_sql::KeyValueStore::begin_read(&store).ok()?;
        let verifier = eggdb_pgwire::ScramVerifier::from_stored(&eggdb_sql::login_verifier(&reader, user).ok()??)?;
        if !verifier.matches_password(password) {
            return None;
        }
        eggdb_sql::protocol_access(&reader, user).ok()?
    }
}

impl eggdb_resp::Logins for RoleLogins<'_> {
    fn check(&self, user: &[u8], password: &[u8]) -> Option<eggdb_resp::LoginAccess> {
        match self.access(std::str::from_utf8(user).ok()?, password)? {
            eggdb_sql::ProtocolAccess::Read => Some(eggdb_resp::LoginAccess::Read),
            eggdb_sql::ProtocolAccess::ReadWrite => Some(eggdb_resp::LoginAccess::ReadWrite),
        }
    }
}

impl eggdb_esrest::RoleLogins for RoleLogins<'_> {
    fn check(&self, user: &str, password: &str) -> Option<eggdb_esrest::RoleAccess> {
        match self.access(user, password.as_bytes())? {
            eggdb_sql::ProtocolAccess::Read => Some(eggdb_esrest::RoleAccess::Read),
            eggdb_sql::ProtocolAccess::ReadWrite => Some(eggdb_esrest::RoleAccess::ReadWrite),
        }
    }
}
