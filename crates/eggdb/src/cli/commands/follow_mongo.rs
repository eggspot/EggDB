use super::follow::{FollowCounts, copy_stopped, source_failed};
use super::{Access, Session};
use crate::cli::args::CommandName;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::JsonObject;
use eggdb_bson::{Bson, Document};
use eggdb_documents::{Store, Update, UpdateOptions, parse_filter};
use eggdb_keyspace::{Overlay, StagedChanges};
use eggdb_mongowire::client::{MongoClient, MongoClientError, MongoUrl, parse_mongodb_url};
use eggdb_platform::stop_signals;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PASSWORD_VARIABLE: &str = "MONGODB_PASSWORD";
const STATE_DATABASE: &str = "eggdb_follow";
const STATE_COLLECTION: &str = "state";
const STATE_ID: &str = "mongodb";
const SYSTEM_DATABASES: [&str; 4] = ["admin", "config", "local", STATE_DATABASE];
const COPY_BATCH: i32 = 1000;
const STREAM_WAIT_MS: i64 = 1000;
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(30);

struct SavedPosition {
    source: String,
    identity: String,
    resume_token: Document,
}

enum Ending {
    Interrupt,
    CaughtUp,
}

fn entries(pairs: Vec<(&str, Bson)>) -> Document {
    Document::from_entries(pairs.into_iter().map(|(name, value)| (name.to_owned(), value)).collect())
}

fn mongo_failure(target: &str, error: &MongoClientError) -> Failure {
    source_failed(match error {
        MongoClientError::Io(error) => format!(
            "cannot reach {target}: {error}. Check the host and port, and that mongod or mongos listens there (net.bindIp)"
        ),
        MongoClientError::Server { code: 18, .. } => format!(
            "cannot sign in to {target}: the user or password is wrong, or the user is not defined in authSource"
        ),
        MongoClientError::Server { code: 13, message, .. } => format!(
            "{target} refused a command: {message}. The user needs the read role on the followed databases and changeStream on the cluster (readAnyDatabase covers both)"
        ),
        other => format!("{target}: {other}"),
    })
}

fn store_failure(file: &str, error: &eggdb_documents::DocumentError) -> Failure {
    source_failed(format!("cannot write to {file}: {error}"))
}

fn cursor_parts(reply: &Document, batch: &str) -> (i64, Vec<Document>, Option<Document>) {
    let Some(cursor) = reply.get_document("cursor") else {
        return (0, Vec::new(), None);
    };
    let id = cursor.get("id").and_then(Bson::as_i64).unwrap_or(0);
    let documents = cursor
        .get_array(batch)
        .map(|items| items.iter().filter_map(|item| item.as_document().cloned()).collect())
        .unwrap_or_default();
    let token = cursor.get_document("postBatchResumeToken").cloned();
    (id, documents, token)
}

fn by_id(id: Bson) -> Document {
    entries(vec![("_id", id)])
}

fn namespace(event: &Document, member: &str) -> Option<(String, String)> {
    let ns = event.get_document(member)?;
    Some((ns.get_str("db")?.to_owned(), ns.get_str("coll").unwrap_or_default().to_owned()))
}

fn identity_of(hello: &Document) -> Option<String> {
    if let Some(set) = hello.get_str("setName") {
        return Some(format!("replica set {set}"));
    }
    (hello.get_str("msg") == Some("isdbgrid")).then(|| "sharded cluster".to_owned())
}

impl Session<'_> {
    pub(super) fn follow_mongo(&mut self) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Follow));
        if self.request.follow.publication.is_some()
            || self.request.follow.poll_seconds.is_some()
            || self.request.follow.sql_server_changes.is_some()
        {
            return Err(usage(
                "--publication, --poll and --changes are not for MongoDB sources, which stream their changes".to_owned(),
            ));
        }
        let mut url = parse_mongodb_url(&self.request.follow.source).map_err(usage)?;
        url.password = self.source_password(PASSWORD_VARIABLE)?;
        if url.user.is_some() && url.password.is_none() {
            return Err(usage(format!(
                "{} names a user: give its password in {PASSWORD_VARIABLE} or with --password-file",
                url.shown()
            )));
        }
        self.follow_mongo_from(&url)
    }

    fn follows_database(&self, database: &str) -> bool {
        if SYSTEM_DATABASES.contains(&database) {
            return false;
        }
        let chosen = &self.request.follow.schemas;
        chosen.is_empty() || chosen.iter().any(|name| name == database)
    }

    fn follow_mongo_from(&mut self, url: &MongoUrl) -> Result<Exit, Failure> {
        let target = url.shown();
        let existed = self.request.file.exists();
        let started = Instant::now();
        let mut client = MongoClient::connect(url).map_err(|error| mongo_failure(&target, &error))?;
        let hello = client
            .command("admin", entries(vec![("hello", Bson::Int32(1))]))
            .map_err(|error| mongo_failure(&target, &error))?;
        let identity = identity_of(&hello).ok_or_else(|| {
            source_failed(format!(
                "cannot follow {target}: it is a standalone server, and change streams need a replica set or a sharded cluster. Start mongod with --replSet and run rs.initiate(), then follow it"
            ))
        })?;
        self.follow_event(
            JsonObject::new()
                .string("event", "connected")
                .string("source", &target)
                .string("server", &identity)
                .string("tls", if url.tls { "TLS" } else { "no TLS" })
                .string("file", &self.file_display)
                .boolean("created", !existed),
            &format!(
                "following {target} into {}{}\n  connected       MongoDB {identity}, {}\n",
                self.file_display,
                if existed { "" } else { " (created)" },
                if url.tls { "TLS" } else { "no TLS" }
            ),
        );
        let _ = stop_signals::watch();
        if !existed && !self.request.create {
            return Err(Failure::usage(
                format!("{} does not exist; add --create to make it", self.file_display),
                Some(CommandName::Follow),
            ));
        }
        let db = self.open(Access::Write { create: self.request.create }, None)?;
        let keyspace = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::MONGO_COLLECTIONS,
        };
        let store = Store::new(&keyspace, super::serve_mongo::store_seed());
        let saved = self.saved_mongo_position(&store)?;
        let mut counts = FollowCounts::default();
        let mut token = match saved {
            Some(saved) => {
                if saved.source != target || saved.identity != identity {
                    return Err(source_failed(format!(
                        "{} follows {} ({}), not {target} ({identity}); follow into another file",
                        self.file_display, saved.source, saved.identity
                    )));
                }
                self.follow_event(
                    JsonObject::new().string("event", "resumed"),
                    "  resumed         from the saved change stream position\n",
                );
                saved.resume_token
            }
            None => {
                let (_, _, token) = self.open_change_stream(&mut client, &target, None)?;
                let token = token.ok_or_else(|| {
                    source_failed(format!("{target} gave no resume token when the change stream opened"))
                })?;
                self.copy_mongo(&mut client, &target, &store, &mut counts)?;
                self.save_mongo_position(&keyspace, &store, &target, &identity, &token, &[], &mut counts)?;
                self.follow_event(
                    JsonObject::new()
                        .string("event", "copied")
                        .number("collections", counts.tables)
                        .number("documents", counts.rows_copied)
                        .number("ms", started.elapsed().as_millis() as u64),
                    &format!(
                        "  copied          {} collections, {} documents in {:.1} s\n",
                        counts.tables,
                        counts.rows_copied,
                        started.elapsed().as_secs_f64()
                    ),
                );
                token
            }
        };
        let deadline_span = self.reconnect_for();
        let ending = loop {
            match self.stream_mongo(&mut client, &target, &identity, &keyspace, &store, &mut token, &mut counts) {
                Ok(ending) => break ending,
                Err(error) if error.connection_lost() => {
                    let deadline = Instant::now() + deadline_span;
                    let mut attempt = 0;
                    loop {
                        if !self.wait_to_reconnect(&mut attempt, &error.to_string(), deadline) {
                            return Err(source_failed(format!(
                                "the connection to {target} stayed lost; {} holds every change up to the saved position, and running the same command again resumes",
                                self.file_display
                            )));
                        }
                        if let Ok(reconnected) = MongoClient::connect(url) {
                            client = reconnected;
                            self.reconnected_event(attempt, "the saved resume token");
                            break;
                        }
                    }
                }
                Err(error) => return Err(mongo_failure(&target, &error)),
            }
        };
        let reason = match ending {
            Ending::Interrupt => ("interrupt", "stopped by Ctrl-C or SIGTERM"),
            Ending::CaughtUp => ("caught_up", "caught up, --until caught-up"),
        };
        self.follow_event(
            JsonObject::new()
                .string("event", "stopped")
                .string("reason", reason.0)
                .number("transactions", counts.transactions)
                .number("inserts", counts.inserts)
                .number("updates", counts.updates)
                .number("deletes", counts.deletes),
            &format!(
                "ok: {} holds every change {target} streamed up to the saved position ({}; {} inserts, {} updates, {} deletes)\n",
                self.file_display, reason.1, counts.inserts, counts.updates, counts.deletes
            ),
        );
        Ok(Exit::Done)
    }

    fn saved_mongo_position(&self, store: &Store<'_>) -> Result<Option<SavedPosition>, Failure> {
        let filter = parse_filter(&by_id(Bson::String(STATE_ID.to_owned())))
            .map_err(|error| store_failure(&self.file_display, &error))?;
        let found = store
            .find(STATE_DATABASE, STATE_COLLECTION, &filter, &Default::default())
            .map_err(|error| store_failure(&self.file_display, &error))?;
        Ok(found.into_iter().next().and_then(|state| {
            Some(SavedPosition {
                source: state.get_str("source")?.to_owned(),
                identity: state.get_str("identity")?.to_owned(),
                resume_token: state.get_document("resume_token")?.clone(),
            })
        }))
    }

    fn open_change_stream(
        &mut self,
        client: &mut MongoClient,
        target: &str,
        resume_after: Option<&Document>,
    ) -> Result<(i64, Vec<Document>, Option<Document>), Failure> {
        let mut stage = vec![
            ("allChangesForCluster", Bson::Boolean(true)),
            ("fullDocument", Bson::String("updateLookup".to_owned())),
        ];
        if let Some(token) = resume_after {
            stage.push(("resumeAfter", Bson::Document(token.clone())));
        }
        let reply = client
            .command(
                "admin",
                entries(vec![
                    ("aggregate", Bson::Int32(1)),
                    ("pipeline", Bson::Array(vec![Bson::Document(entries(vec![("$changeStream", Bson::Document(entries(stage)))]))])),
                    ("cursor", Bson::Document(entries(vec![("batchSize", Bson::Int32(0))]))),
                ]),
            )
            .map_err(|error| mongo_failure(target, &error))?;
        Ok(cursor_parts(&reply, "firstBatch"))
    }

    fn copy_mongo(
        &mut self,
        client: &mut MongoClient,
        target: &str,
        store: &Store<'_>,
        counts: &mut FollowCounts,
    ) -> Result<(), Failure> {
        let failed = |error: MongoClientError| mongo_failure(target, &error);
        let listed = client
            .command("admin", entries(vec![("listDatabases", Bson::Int32(1)), ("nameOnly", Bson::Boolean(true))]))
            .map_err(failed)?;
        let databases: Vec<String> = listed
            .get_array("databases")
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_document()?.get_str("name").map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        for database in databases.iter().filter(|name| self.follows_database(name)) {
            let reply = client
                .command(
                    database,
                    entries(vec![
                        ("listCollections", Bson::Int32(1)),
                        ("filter", Bson::Document(entries(vec![("type", Bson::String("collection".to_owned()))]))),
                    ]),
                )
                .map_err(failed)?;
            let (_, collections, _) = cursor_parts(&reply, "firstBatch");
            for info in collections {
                let Some(collection) = info.get_str("name").map(str::to_owned) else {
                    continue;
                };
                if collection.starts_with("system.") {
                    continue;
                }
                if stop_signals::requested() {
                    return Err(copy_stopped(&self.file_display));
                }
                let options = info.get_document("options").cloned().unwrap_or_default();
                store
                    .create_collection_with(database, &collection, &options)
                    .map_err(|error| store_failure(&self.file_display, &error))?;
                let copied = self.copy_collection(client, target, store, database, &collection)?;
                counts.tables += 1;
                counts.rows_copied += copied;
                self.follow_event(
                    JsonObject::new()
                        .string("event", "table_copied")
                        .string("collection", &format!("{database}.{collection}"))
                        .number("documents", copied),
                    &format!("  copied          {database}.{collection}: {copied} documents\n"),
                );
            }
        }
        Ok(())
    }

    fn copy_collection(
        &mut self,
        client: &mut MongoClient,
        target: &str,
        store: &Store<'_>,
        database: &str,
        collection: &str,
    ) -> Result<u64, Failure> {
        let failed = |error: MongoClientError| mongo_failure(target, &error);
        let indexes = client
            .command(database, entries(vec![("listIndexes", Bson::String(collection.to_owned()))]))
            .map_err(failed)?;
        let (_, definitions, _) = cursor_parts(&indexes, "firstBatch");
        let definitions: Vec<Document> =
            definitions.into_iter().filter(|definition| definition.get_str("name") != Some("_id_")).collect();
        if !definitions.is_empty() {
            store
                .create_indexes(database, collection, &definitions)
                .map_err(|error| store_failure(&self.file_display, &error))?;
        }
        let mut reply = client
            .command(
                database,
                entries(vec![
                    ("find", Bson::String(collection.to_owned())),
                    ("filter", Bson::Document(Document::new())),
                    ("batchSize", Bson::Int32(COPY_BATCH)),
                ]),
            )
            .map_err(failed)?;
        let mut batch_name = "firstBatch";
        let mut copied = 0u64;
        loop {
            let (cursor, documents, _) = cursor_parts(&reply, batch_name);
            copied += documents.len() as u64;
            if !documents.is_empty() {
                let outcome = store
                    .insert(database, collection, documents, false)
                    .map_err(|error| store_failure(&self.file_display, &error))?;
                if let Some((_, error)) = outcome.errors.first() {
                    return Err(store_failure(&self.file_display, error));
                }
            }
            if cursor == 0 {
                return Ok(copied);
            }
            if stop_signals::requested() {
                return Err(copy_stopped(&self.file_display));
            }
            reply = client
                .command(
                    database,
                    entries(vec![
                        ("getMore", Bson::Int64(cursor)),
                        ("collection", Bson::String(collection.to_owned())),
                        ("batchSize", Bson::Int32(COPY_BATCH)),
                    ]),
                )
                .map_err(failed)?;
            batch_name = "nextBatch";
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn save_mongo_position(
        &self,
        keyspace: &(dyn eggdb_keyspace::Keyspace + Sync),
        store: &Store<'_>,
        target: &str,
        identity: &str,
        token: &Document,
        events: &[Document],
        counts: &mut FollowCounts,
    ) -> Result<(), Failure> {
        let staged: Mutex<StagedChanges> = Mutex::new(StagedChanges::new());
        let overlay = Overlay::new(keyspace, &staged);
        let staged_store = store.over(&overlay);
        let failed = |error: eggdb_documents::DocumentError| store_failure(&self.file_display, &error);
        for event in events {
            self.apply_change(&staged_store, event, counts).map_err(failed)?;
        }
        let state = entries(vec![
            ("_id", Bson::String(STATE_ID.to_owned())),
            ("source", Bson::String(target.to_owned())),
            ("identity", Bson::String(identity.to_owned())),
            ("resume_token", Bson::Document(token.clone())),
        ]);
        let filter = parse_filter(&by_id(Bson::String(STATE_ID.to_owned()))).map_err(failed)?;
        staged_store
            .update(
                STATE_DATABASE,
                STATE_COLLECTION,
                &filter,
                &Update::Replacement(state),
                UpdateOptions { multi: false, upsert: true },
            )
            .map_err(failed)?;
        overlay
            .commit()
            .map_err(|reason| source_failed(format!("cannot save the follow position in {}: {reason}", self.file_display)))
    }

    fn apply_change(
        &self,
        store: &Store<'_>,
        event: &Document,
        counts: &mut FollowCounts,
    ) -> Result<(), eggdb_documents::DocumentError> {
        let Some((database, collection)) = namespace(event, "ns") else {
            return Ok(());
        };
        if !self.follows_database(&database) {
            return Ok(());
        }
        let id = event
            .get_document("documentKey")
            .and_then(|key| key.get("_id"))
            .cloned();
        match event.get_str("operationType").unwrap_or_default() {
            operation @ ("insert" | "update" | "replace") => {
                let Some(id) = id else {
                    return Ok(());
                };
                let filter = parse_filter(&by_id(id))?;
                match event.get_document("fullDocument") {
                    Some(document) => {
                        store.update(
                            &database,
                            &collection,
                            &filter,
                            &Update::Replacement(document.clone()),
                            UpdateOptions { multi: false, upsert: true },
                        )?;
                        if operation == "insert" {
                            counts.inserts += 1;
                        } else {
                            counts.updates += 1;
                        }
                    }
                    None => {
                        store.delete(&database, &collection, &filter, true)?;
                        counts.deletes += 1;
                    }
                }
            }
            "delete" => {
                if let Some(id) = id {
                    store.delete(&database, &collection, &parse_filter(&by_id(id))?, true)?;
                    counts.deletes += 1;
                }
            }
            "drop" => {
                store.drop_collection(&database, &collection)?;
                counts.schema_changes += 1;
            }
            "dropDatabase" => {
                store.drop_database(&database)?;
                counts.schema_changes += 1;
            }
            "rename" => {
                if let Some((to_database, to_collection)) = namespace(event, "to") {
                    store.rename_collection((&database, &collection), (&to_database, &to_collection), true)?;
                    counts.schema_changes += 1;
                }
            }
            _ => {}
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn stream_mongo(
        &mut self,
        client: &mut MongoClient,
        target: &str,
        identity: &str,
        keyspace: &(dyn eggdb_keyspace::Keyspace + Sync),
        store: &Store<'_>,
        token: &mut Document,
        counts: &mut FollowCounts,
    ) -> Result<Ending, MongoClientError> {
        client.set_read_timeout(Some(STREAM_READ_TIMEOUT))?;
        let opened = client.command(
            "admin",
            entries(vec![
                ("aggregate", Bson::Int32(1)),
                (
                    "pipeline",
                    Bson::Array(vec![Bson::Document(entries(vec![(
                        "$changeStream",
                        Bson::Document(entries(vec![
                            ("allChangesForCluster", Bson::Boolean(true)),
                            ("fullDocument", Bson::String("updateLookup".to_owned())),
                            ("resumeAfter", Bson::Document(token.clone())),
                        ])),
                    )]))]),
                ),
                ("cursor", Bson::Document(entries(vec![("batchSize", Bson::Int32(0))]))),
            ]),
        )?;
        let (cursor, _, _) = cursor_parts(&opened, "firstBatch");
        self.follow_event(
            JsonObject::new().string("event", "following"),
            "  following       the cluster's change stream\n",
        );
        loop {
            if stop_signals::requested() {
                return Ok(Ending::Interrupt);
            }
            let reply = client.command(
                "admin",
                entries(vec![
                    ("getMore", Bson::Int64(cursor)),
                    ("collection", Bson::String("$cmd.aggregate".to_owned())),
                    ("maxTimeMS", Bson::Int64(STREAM_WAIT_MS)),
                ]),
            )?;
            let (_, events, batch_token) = cursor_parts(&reply, "nextBatch");
            if events.iter().any(|event| event.get_str("operationType") == Some("invalidate")) {
                return Err(MongoClientError::Protocol(
                    "the change stream was invalidated; follow into a new file".to_owned(),
                ));
            }
            let next = batch_token
                .or_else(|| events.last().and_then(|event| event.get_document("_id").cloned()))
                .unwrap_or_else(|| token.clone());
            let caught_up = events.is_empty();
            if !caught_up || next != *token {
                if self
                    .save_mongo_position(keyspace, store, target, identity, &next, &events, counts)
                    .is_err()
                {
                    return Err(MongoClientError::Protocol(format!(
                        "cannot apply the changes to {}",
                        self.file_display
                    )));
                }
                if !caught_up {
                    counts.transactions += 1;
                }
                *token = next;
            }
            if caught_up && self.request.follow.until_caught_up {
                return Ok(Ending::CaughtUp);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_replica_sets_and_sharded_clusters_have_change_streams() {
        let replica = entries(vec![("setName", Bson::String("rs0".to_owned()))]);
        let router = entries(vec![("msg", Bson::String("isdbgrid".to_owned()))]);
        assert_eq!(identity_of(&replica).as_deref(), Some("replica set rs0"));
        assert_eq!(identity_of(&router).as_deref(), Some("sharded cluster"));
        assert_eq!(identity_of(&Document::new()), None);
        let reply = entries(vec![(
            "cursor",
            Bson::Document(entries(vec![
                ("id", Bson::Int64(7)),
                ("nextBatch", Bson::Array(vec![Bson::Document(Document::new())])),
            ])),
        )]);
        let (id, batch, token) = cursor_parts(&reply, "nextBatch");
        assert_eq!((id, batch.len(), token), (7, 1, None));
    }
}
