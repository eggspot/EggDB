use crate::cursors::Cursors;
use crate::server::{ServerConfig, Session};
use eggdb_auth::scram::{ScramExchange, ScramFailure, ScramVerifier};
use eggdb_bson::{Bson, Document, compare};
use eggdb_documents::{
    DocumentError, Filter, Modification, Projection, SortSpec, Store, UpdateOptions, parse_filter, parse_projection,
    parse_sort, parse_update,
};
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_WIRE_VERSION: i32 = 17;
const COMPATIBLE_VERSION: &str = "6.0.0";
const DEFAULT_FIRST_BATCH: usize = 101;
const MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;
const MAX_WRITE_BATCH: usize = 100_000;

/// What every connection of one server shares.
pub(crate) struct Shared<'s> {
    pub(crate) store: &'s Store<'s>,
    pub(crate) config: &'s ServerConfig,
    pub(crate) cursors: Mutex<Cursors>,
    pub(crate) started: Instant,
}

fn text(value: &str) -> Bson {
    Bson::String(value.to_owned())
}

fn document(entries: Vec<(&str, Bson)>) -> Document {
    Document::from_entries(entries.into_iter().map(|(name, value)| (name.to_owned(), value)).collect())
}

fn ok(mut reply: Document) -> Document {
    reply.insert("ok", Bson::Double(1.0));
    reply
}

pub(crate) fn failure(error: &DocumentError) -> Document {
    document(vec![
        ("ok", Bson::Double(0.0)),
        ("errmsg", text(&error.message)),
        ("code", Bson::Int32(error.code)),
        ("codeName", text(error.code_name)),
    ])
}

fn command_failure(code: i32, code_name: &'static str, message: impl Into<String>) -> Document {
    failure(&DocumentError { code, code_name, message: message.into() })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

fn collection_name<'b>(body: &'b Document, command: &str) -> Result<&'b str, DocumentError> {
    match body.get(command) {
        Some(Bson::String(name)) => {
            eggdb_documents::check_collection_name(name)?;
            Ok(name)
        }
        _ => Err(DocumentError::bad_value(format!("collection name has invalid type {}", body.get(command).map_or("missing", Bson::type_name)))),
    }
}

fn filter_of(body: &Document, name: &str) -> Result<Filter, DocumentError> {
    match body.get(name) {
        None | Some(Bson::Null) => parse_filter(&Document::new()),
        Some(Bson::Document(filter)) => parse_filter(filter),
        Some(other) => Err(DocumentError::type_mismatch(format!("BSON field '{name}' is the wrong type '{}', expected type 'object'", other.type_name()))),
    }
}

fn sort_of(body: &Document, name: &str) -> Result<SortSpec, DocumentError> {
    match body.get(name) {
        None | Some(Bson::Null) => Ok(SortSpec::default()),
        Some(Bson::Document(sort)) => parse_sort(sort),
        Some(other) => Err(DocumentError::type_mismatch(format!("BSON field '{name}' is the wrong type '{}', expected type 'object'", other.type_name()))),
    }
}

fn projection_of(body: &Document, name: &str) -> Result<Option<Projection>, DocumentError> {
    match body.get(name) {
        None | Some(Bson::Null) => Ok(None),
        Some(Bson::Document(spec)) if spec.is_empty() => Ok(None),
        Some(Bson::Document(spec)) => parse_projection(spec).map(Some),
        Some(other) => Err(DocumentError::type_mismatch(format!("BSON field '{name}' is the wrong type '{}', expected type 'object'", other.type_name()))),
    }
}

fn count_of(body: &Document, name: &str) -> Result<Option<i64>, DocumentError> {
    match body.get(name) {
        None | Some(Bson::Null) => Ok(None),
        Some(value) => value.as_i64().map(Some).ok_or_else(|| DocumentError::type_mismatch(format!("BSON field '{name}' needs a number"))),
    }
}

fn batch(remaining: &mut VecDeque<Document>, most: usize) -> Vec<Bson> {
    let mut out = Vec::new();
    let mut bytes = 0;
    while out.len() < most {
        let Some(next) = remaining.front() else {
            break;
        };
        let size = next.to_bytes().len();
        if !out.is_empty() && bytes + size > MAX_BATCH_BYTES {
            break;
        }
        bytes += size;
        if let Some(next) = remaining.pop_front() {
            out.push(Bson::Document(next));
        }
    }
    out
}

pub(crate) fn cursor_reply(shared: &Shared<'_>, namespace: String, mut remaining: VecDeque<Document>, first_batch: usize, single: bool) -> Document {
    let first = batch(&mut remaining, first_batch);
    let id = if remaining.is_empty() || single {
        0
    } else {
        shared.cursors.lock().unwrap_or_else(std::sync::PoisonError::into_inner).open(namespace.clone(), remaining)
    };
    ok(document(vec![(
        "cursor",
        Bson::Document(document(vec![("firstBatch", Bson::Array(first)), ("id", Bson::Int64(id)), ("ns", text(&namespace))])),
    )]))
}

fn hello(session: &Session, body: &Document, legacy: bool) -> Document {
    let mut reply = Document::new();
    reply.insert("helloOk", Bson::Boolean(true));
    reply.insert(if legacy { "ismaster" } else { "isWritablePrimary" }, Bson::Boolean(true));
    reply.insert("maxBsonObjectSize", Bson::Int32(eggdb_documents::MAX_DOCUMENT_BYTES as i32));
    reply.insert("maxMessageSizeBytes", Bson::Int32(crate::MAX_MESSAGE_BYTES as i32));
    reply.insert("maxWriteBatchSize", Bson::Int32(MAX_WRITE_BATCH as i32));
    reply.insert("localTime", Bson::DateTime(now_ms()));
    reply.insert("logicalSessionTimeoutMinutes", Bson::Int32(30));
    reply.insert("connectionId", Bson::Int32(session.id));
    reply.insert("minWireVersion", Bson::Int32(0));
    reply.insert("maxWireVersion", Bson::Int32(MAX_WIRE_VERSION));
    reply.insert("readOnly", Bson::Boolean(false));
    if body.get_str("saslSupportedMechs").is_some() {
        reply.insert("saslSupportedMechs", Bson::Array(vec![text(eggdb_auth::scram::SCRAM_SHA_256)]));
    }
    ok(reply)
}

fn sasl_user(client_first: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(client_first).ok()?;
    let bare = text.splitn(3, ',').nth(2)?;
    let name = bare.split(',').next()?.strip_prefix("n=")?;
    Some(name.replace("=2C", ",").replace("=3D", "="))
}

fn binary_payload(body: &Document) -> Option<Vec<u8>> {
    match body.get("payload") {
        Some(Bson::Binary { bytes, .. }) => Some(bytes.clone()),
        Some(Bson::String(text)) => Some(text.as_bytes().to_vec()),
        _ => None,
    }
}

fn authentication_failed() -> Document {
    command_failure(18, "AuthenticationFailed", "Authentication failed.")
}

fn payload_reply(done: bool, payload: Vec<u8>) -> Document {
    ok(document(vec![
        ("conversationId", Bson::Int32(1)),
        ("done", Bson::Boolean(done)),
        ("payload", Bson::Binary { subtype: 0, bytes: payload }),
    ]))
}

fn sasl_start(shared: &Shared<'_>, session: &mut Session, body: &Document) -> Document {
    let mechanism = body.get_str("mechanism").unwrap_or_default();
    if mechanism != eggdb_auth::scram::SCRAM_SHA_256 {
        return command_failure(334, "MechanismUnavailable", format!("Received authentication for mechanism {mechanism} which is not enabled"));
    }
    let Some(payload) = binary_payload(body) else {
        return authentication_failed();
    };
    let user = sasl_user(&payload).unwrap_or_default();
    let verifier = if user == shared.config.user {
        shared.config.verifier.clone()
    } else {
        ScramVerifier::for_unknown_user(&shared.config.unknown_user_secret, &user, shared.config.verifier.iterations())
    };
    let mut nonce = [0u8; 18];
    if (shared.config.random)(&mut nonce).is_err() {
        return command_failure(1, "InternalError", "could not generate a random nonce");
    }
    let mut exchange = ScramExchange::new(verifier, &nonce);
    match exchange.client_first(&payload) {
        Ok(server_first) => {
            let skip_empty = body.get_document("options").and_then(|options| options.get_bool("skipEmptyExchange")) == Some(true);
            session.conversation = Some(crate::server::Conversation::AwaitingFinal { exchange, skip_empty, user });
            payload_reply(false, server_first.into_bytes())
        }
        Err(_) => authentication_failed(),
    }
}

fn sasl_continue(session: &mut Session, body: &Document) -> Document {
    let payload = binary_payload(body).unwrap_or_default();
    match session.conversation.take() {
        Some(crate::server::Conversation::AwaitingFinal { mut exchange, skip_empty, user }) => match exchange.client_final(&payload) {
            Ok(server_final) => {
                if skip_empty {
                    session.authenticated_user = Some(user);
                    payload_reply(true, server_final.into_bytes())
                } else {
                    session.conversation = Some(crate::server::Conversation::AwaitingEmpty { user });
                    payload_reply(false, server_final.into_bytes())
                }
            }
            Err(ScramFailure::WrongProof | ScramFailure::Malformed(_) | ScramFailure::Unsupported(_)) => {
                session.failed_logins += 1;
                authentication_failed()
            }
        },
        Some(crate::server::Conversation::AwaitingEmpty { user }) => {
            session.authenticated_user = Some(user);
            payload_reply(true, Vec::new())
        }
        None => command_failure(17, "ProtocolError", "No SASL session state found"),
    }
}

fn may_run_unauthenticated(command: &str) -> bool {
    matches!(
        command,
        "hello" | "isMaster" | "ismaster" | "ping" | "buildInfo" | "buildinfo" | "saslStart" | "saslContinue" | "getnonce"
            | "endSessions" | "connectionStatus" | "logout" | "whatsmyuri"
    )
}

/// Runs one command for a session; returns the reply document.
pub(crate) fn run(shared: &Shared<'_>, session: &mut Session, database: &str, body: &Document) -> Document {
    let Some((command, _)) = body.first() else {
        return command_failure(59, "CommandNotFound", "no command given");
    };
    let command = command.clone();
    if session.authenticated_user.is_none() && !may_run_unauthenticated(&command) {
        return command_failure(13, "Unauthorized", format!("command {command} requires authentication"));
    }
    if let Err(error) = eggdb_documents::check_database_name(database) {
        return failure(&error);
    }
    let outcome = match command.as_str() {
        "hello" => Ok(hello(session, body, false)),
        "isMaster" | "ismaster" => Ok(hello(session, body, true)),
        "ping" => Ok(ok(Document::new())),
        "buildInfo" | "buildinfo" => Ok(ok(document(vec![
            ("version", text(COMPATIBLE_VERSION)),
            ("gitVersion", text("eggdb")),
            ("versionArray", Bson::Array(vec![Bson::Int32(6), Bson::Int32(0), Bson::Int32(0), Bson::Int32(0)])),
            ("bits", Bson::Int32(64)),
            ("maxBsonObjectSize", Bson::Int32(eggdb_documents::MAX_DOCUMENT_BYTES as i32)),
            ("storageEngines", Bson::Array(vec![text("eggdb")])),
        ]))),
        "saslStart" => Ok(sasl_start(shared, session, body)),
        "saslContinue" => Ok(sasl_continue(session, body)),
        "logout" => {
            session.authenticated_user = None;
            Ok(ok(Document::new()))
        }
        "connectionStatus" => {
            let users: Vec<Bson> = session
                .authenticated_user
                .iter()
                .map(|user| Bson::Document(document(vec![("user", text(user)), ("db", text("admin"))])))
                .collect();
            let roles: Vec<Bson> = if users.is_empty() {
                Vec::new()
            } else {
                vec![Bson::Document(document(vec![("role", text("root")), ("db", text("admin"))]))]
            };
            Ok(ok(document(vec![(
                "authInfo",
                Bson::Document(document(vec![("authenticatedUsers", Bson::Array(users)), ("authenticatedUserRoles", Bson::Array(roles))])),
            )])))
        }
        "whatsmyuri" => Ok(ok(document(vec![("you", text(&session.peer))]))),
        "endSessions" | "killSessions" | "refreshSessions" | "killAllSessions" => Ok(ok(Document::new())),
        "getLog" => Ok(ok(document(vec![("totalLinesWritten", Bson::Int32(0)), ("log", Bson::Array(Vec::new()))]))),
        "hostInfo" => Ok(ok(document(vec![
            ("system", Bson::Document(document(vec![("hostname", text("eggdb"))]))),
            ("os", Bson::Document(Document::new())),
            ("extra", Bson::Document(Document::new())),
        ]))),
        "getCmdLineOpts" => Ok(ok(document(vec![("argv", Bson::Array(vec![text("eggdb")])), ("parsed", Bson::Document(Document::new()))]))),
        "getParameter" => Ok(ok(document(vec![(
            "featureCompatibilityVersion",
            Bson::Document(document(vec![("version", text("6.0"))])),
        )]))),
        "serverStatus" => Ok(ok(document(vec![
            ("host", text("eggdb")),
            ("version", text(COMPATIBLE_VERSION)),
            ("process", text("eggdb")),
            ("uptime", Bson::Double(shared.started.elapsed().as_secs_f64())),
            ("localTime", Bson::DateTime(now_ms())),
        ]))),
        "listDatabases" => list_databases(shared, body),
        "listCollections" => list_collections(shared, database, body),
        "create" => collection_name(body, "create").and_then(|name| {
            shared.store.create_collection(database, name)?;
            Ok(ok(Document::new()))
        }),
        "drop" => collection_name(body, "drop").and_then(|name| {
            let indexes = shared.store.indexes(database, name)?.map_or(0, |indexes| indexes.len());
            shared.store.drop_collection(database, name)?;
            Ok(ok(document(vec![("nIndexesWas", Bson::Int32(indexes as i32)), ("ns", text(&format!("{database}.{name}")))])))
        }),
        "dropDatabase" => {
            shared.store.drop_database(database).map(|()| ok(document(vec![("dropped", text(database))])))
        }
        "insert" => insert(shared, database, body),
        "find" => find(shared, database, body),
        "getMore" => get_more(shared, database, body),
        "killCursors" => kill_cursors(shared, body),
        "update" => update(shared, database, body),
        "delete" => delete(shared, database, body),
        "findAndModify" | "findandmodify" => find_and_modify(shared, database, body, &command),
        "count" => count(shared, database, body),
        "distinct" => distinct(shared, database, body),
        "createIndexes" => create_indexes(shared, database, body),
        "listIndexes" => collection_name(body, "listIndexes").and_then(|name| {
            let Some(indexes) = shared.store.indexes(database, name)? else {
                return Err(DocumentError::namespace_not_found(format!("ns does not exist: {database}.{name}")));
            };
            Ok(cursor_reply(shared, format!("{database}.{name}"), indexes.into(), usize::MAX, true))
        }),
        "dropIndexes" | "deleteIndexes" => drop_indexes(shared, database, body, &command),
        "dbStats" => db_stats(shared, database),
        "collStats" => collection_name(body, "collStats").and_then(|name| {
            let count = shared.store.find(database, name, &parse_filter(&Document::new())?, &SortSpec::default())?.len();
            Ok(ok(document(vec![("ns", text(&format!("{database}.{name}"))), ("count", Bson::Int64(count as i64))])))
        }),
        "aggregate" => crate::aggregate::aggregate(shared, database, body),
        other => Err(DocumentError {
            code: 59,
            code_name: "CommandNotFound",
            message: format!("no such command: '{other}'"),
        }),
    };
    outcome.unwrap_or_else(|error| failure(&error))
}

fn list_databases(shared: &Shared<'_>, body: &Document) -> Result<Document, DocumentError> {
    let databases = shared.store.databases()?;
    let filter = filter_of(body, "filter")?;
    let name_only = body.get_bool("nameOnly") == Some(true);
    let mut listed = Vec::new();
    let mut total = 0u64;
    for (name, bytes) in databases {
        let mut entry = document(vec![("name", text(&name))]);
        if !name_only {
            entry.insert("sizeOnDisk", Bson::Int64(bytes as i64));
            entry.insert("empty", Bson::Boolean(bytes == 0));
        }
        if eggdb_documents::matches(&filter, &entry) {
            total += bytes;
            listed.push(Bson::Document(entry));
        }
    }
    let mut reply = document(vec![("databases", Bson::Array(listed))]);
    if !name_only {
        reply.insert("totalSize", Bson::Int64(total as i64));
        reply.insert("totalSizeMb", Bson::Int64((total / (1024 * 1024)) as i64));
    }
    Ok(ok(reply))
}

fn list_collections(shared: &Shared<'_>, database: &str, body: &Document) -> Result<Document, DocumentError> {
    let filter = filter_of(body, "filter")?;
    let name_only = body.get_bool("nameOnly") == Some(true);
    let mut listed = VecDeque::new();
    for (name, _) in shared.store.collections_of(database)? {
        let mut entry = document(vec![("name", text(&name)), ("type", text("collection"))]);
        if !name_only {
            entry.insert("options", Bson::Document(Document::new()));
            entry.insert("info", Bson::Document(document(vec![("readOnly", Bson::Boolean(false))])));
            entry.insert(
                "idIndex",
                Bson::Document(document(vec![
                    ("v", Bson::Int32(2)),
                    ("key", Bson::Document(document(vec![("_id", Bson::Int32(1))]))),
                    ("name", text("_id_")),
                ])),
            );
        }
        if eggdb_documents::matches(&filter, &entry) {
            listed.push_back(entry);
        }
    }
    Ok(cursor_reply(shared, format!("{database}.$cmd.listCollections"), listed, usize::MAX, true))
}

fn documents_field(body: &Document, name: &str) -> Result<Vec<Document>, DocumentError> {
    let Some(Bson::Array(items)) = body.get(name) else {
        return Err(DocumentError::failed_to_parse(format!("BSON field '{name}' is missing but a required field")));
    };
    if items.len() > MAX_WRITE_BATCH {
        return Err(DocumentError::bad_value(format!("Write batch sizes must be between 1 and {MAX_WRITE_BATCH}. Got {} operations.", items.len())));
    }
    items
        .iter()
        .map(|item| match item {
            Bson::Document(inner) => Ok(inner.clone()),
            other => Err(DocumentError::type_mismatch(format!("BSON field '{name}' holds a {} instead of an object", other.type_name()))),
        })
        .collect()
}

fn write_errors(errors: Vec<(usize, DocumentError)>) -> Bson {
    Bson::Array(
        errors
            .into_iter()
            .map(|(index, error)| {
                Bson::Document(document(vec![
                    ("index", Bson::Int32(index as i32)),
                    ("code", Bson::Int32(error.code)),
                    ("errmsg", text(&error.message)),
                ]))
            })
            .collect(),
    )
}

fn insert(shared: &Shared<'_>, database: &str, body: &Document) -> Result<Document, DocumentError> {
    let collection = collection_name(body, "insert")?;
    let documents = documents_field(body, "documents")?;
    let ordered = body.get_bool("ordered").unwrap_or(true);
    let outcome = shared.store.insert(database, collection, documents, ordered)?;
    let mut reply = document(vec![("n", Bson::Int32(outcome.inserted as i32))]);
    if !outcome.errors.is_empty() {
        reply.insert("writeErrors", write_errors(outcome.errors));
    }
    Ok(ok(reply))
}

fn find(shared: &Shared<'_>, database: &str, body: &Document) -> Result<Document, DocumentError> {
    let collection = collection_name(body, "find")?;
    let filter = filter_of(body, "filter")?;
    let sort = sort_of(body, "sort")?;
    let projection = projection_of(body, "projection")?;
    let skip = count_of(body, "skip")?.unwrap_or(0);
    let limit = count_of(body, "limit")?.unwrap_or(0);
    let batch_size = count_of(body, "batchSize")?;
    if skip < 0 || batch_size.is_some_and(|size| size < 0) {
        return Err(DocumentError::bad_value("skip and batchSize may not be negative"));
    }
    let single = body.get_bool("singleBatch") == Some(true) || limit < 0;
    let found = shared.store.find(database, collection, &filter, &sort)?;
    let mut remaining: VecDeque<Document> = found
        .into_iter()
        .skip(skip as usize)
        .take(if limit == 0 { usize::MAX } else { limit.unsigned_abs() as usize })
        .collect();
    if let Some(projection) = projection {
        remaining = remaining.iter().map(|document| projection.apply(document)).collect();
    }
    let first = batch_size.map_or(DEFAULT_FIRST_BATCH, |size| size as usize);
    Ok(cursor_reply(shared, format!("{database}.{collection}"), remaining, first, single))
}

fn get_more(shared: &Shared<'_>, database: &str, body: &Document) -> Result<Document, DocumentError> {
    let Some(id) = body.get("getMore").and_then(Bson::as_i64) else {
        return Err(DocumentError::type_mismatch("BSON field 'getMore' is the wrong type, expected type 'long'"));
    };
    let collection = body.get_str("collection").unwrap_or_default();
    let namespace = format!("{database}.{collection}");
    let batch_size = count_of(body, "batchSize")?.filter(|size| *size > 0).map_or(usize::MAX, |size| size as usize);
    let mut cursors = shared.cursors.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(mut cursor) = cursors.take(id) else {
        return Err(DocumentError { code: 43, code_name: "CursorNotFound", message: format!("cursor id {id} not found") });
    };
    if cursor.namespace != namespace {
        let message = format!("Requested getMore on namespace '{namespace}', but cursor belongs to a different namespace {}", cursor.namespace);
        cursors.put_back(id, cursor);
        return Err(DocumentError { code: 13, code_name: "Unauthorized", message });
    }
    let next = batch(&mut cursor.remaining, batch_size);
    let id = if cursor.remaining.is_empty() {
        0
    } else {
        cursors.put_back(id, cursor);
        id
    };
    Ok(ok(document(vec![(
        "cursor",
        Bson::Document(document(vec![("nextBatch", Bson::Array(next)), ("id", Bson::Int64(id)), ("ns", text(&namespace))])),
    )])))
}

fn kill_cursors(shared: &Shared<'_>, body: &Document) -> Result<Document, DocumentError> {
    let ids = body.get_array("cursors").cloned().unwrap_or_default();
    let mut cursors = shared.cursors.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut killed = Vec::new();
    let mut not_found = Vec::new();
    for id in ids {
        match id.as_i64() {
            Some(number) if cursors.kill(number) => killed.push(Bson::Int64(number)),
            Some(number) => not_found.push(Bson::Int64(number)),
            None => {}
        }
    }
    Ok(ok(document(vec![
        ("cursorsKilled", Bson::Array(killed)),
        ("cursorsNotFound", Bson::Array(not_found)),
        ("cursorsAlive", Bson::Array(Vec::new())),
        ("cursorsUnknown", Bson::Array(Vec::new())),
    ])))
}

fn update_statement(
    shared: &Shared<'_>,
    database: &str,
    collection: &str,
    statement: &Document,
) -> Result<eggdb_documents::UpdateOutcome, DocumentError> {
    let filter = filter_of(statement, "q")?;
    let update = match statement.get("u") {
        Some(Bson::Document(update)) => parse_update(update)?,
        Some(Bson::Array(_)) => return Err(DocumentError::not_implemented("pipeline-style updates are not supported yet")),
        _ => return Err(DocumentError::failed_to_parse("BSON field 'update.updates.u' is missing but a required field")),
    };
    if statement.contains_key("arrayFilters") {
        return Err(DocumentError::not_implemented("arrayFilters are not supported yet"));
    }
    let options = UpdateOptions {
        multi: statement.get_bool("multi") == Some(true),
        upsert: statement.get_bool("upsert") == Some(true),
    };
    shared.store.update(database, collection, &filter, &update, options)
}

fn update(shared: &Shared<'_>, database: &str, body: &Document) -> Result<Document, DocumentError> {
    let collection = collection_name(body, "update")?;
    let statements = documents_field(body, "updates")?;
    let ordered = body.get_bool("ordered").unwrap_or(true);
    let mut matched = 0u64;
    let mut modified = 0u64;
    let mut upserted = Vec::new();
    let mut errors = Vec::new();
    for (index, statement) in statements.iter().enumerate() {
        let outcome = update_statement(shared, database, collection, statement);
        match outcome {
            Ok(outcome) => {
                matched += outcome.matched;
                modified += outcome.modified;
                if let Some(id) = outcome.upserted {
                    upserted.push(Bson::Document(document(vec![("index", Bson::Int32(index as i32)), ("_id", id)])));
                }
            }
            Err(error) if error.code == 1 => return Err(error),
            Err(error) => {
                errors.push((index, error));
                if ordered {
                    break;
                }
            }
        }
    }
    let mut reply = document(vec![
        ("n", Bson::Int32((matched + upserted.len() as u64) as i32)),
        ("nModified", Bson::Int32(modified as i32)),
    ]);
    if !upserted.is_empty() {
        reply.insert("upserted", Bson::Array(upserted));
    }
    if !errors.is_empty() {
        reply.insert("writeErrors", write_errors(errors));
    }
    Ok(ok(reply))
}

fn delete(shared: &Shared<'_>, database: &str, body: &Document) -> Result<Document, DocumentError> {
    let collection = collection_name(body, "delete")?;
    let statements = documents_field(body, "deletes")?;
    let ordered = body.get_bool("ordered").unwrap_or(true);
    let mut deleted = 0u64;
    let mut errors = Vec::new();
    for (index, statement) in statements.iter().enumerate() {
        let outcome = filter_of(statement, "q").and_then(|filter| {
            let only_one = match statement.get_i64("limit") {
                Some(0) => false,
                Some(1) => true,
                _ => return Err(DocumentError::failed_to_parse("The limit field in delete objects must be 0 or 1.")),
            };
            shared.store.delete(database, collection, &filter, only_one)
        });
        match outcome {
            Ok(count) => deleted += count,
            Err(error) if error.code == 1 => return Err(error),
            Err(error) => {
                errors.push((index, error));
                if ordered {
                    break;
                }
            }
        }
    }
    let mut reply = document(vec![("n", Bson::Int32(deleted as i32))]);
    if !errors.is_empty() {
        reply.insert("writeErrors", write_errors(errors));
    }
    Ok(ok(reply))
}

fn find_and_modify(shared: &Shared<'_>, database: &str, body: &Document, command: &str) -> Result<Document, DocumentError> {
    let collection = collection_name(body, command)?;
    let filter = filter_of(body, "query")?;
    let sort = sort_of(body, "sort")?;
    let projection = projection_of(body, "fields")?;
    let remove = body.get_bool("remove") == Some(true);
    let modification = match (remove, body.get("update")) {
        (true, None) => Modification::Remove,
        (false, Some(Bson::Document(update))) => Modification::Update {
            update: parse_update(update)?,
            upsert: body.get_bool("upsert") == Some(true),
            new: body.get_bool("new") == Some(true),
        },
        (false, Some(Bson::Array(_))) => return Err(DocumentError::not_implemented("pipeline-style updates are not supported yet")),
        (true, Some(_)) => return Err(DocumentError::failed_to_parse("Cannot specify both an update and remove=true")),
        (false, _) => return Err(DocumentError::failed_to_parse("Either an update or remove=true must be specified")),
    };
    let (found, outcome) = shared.store.find_and_modify(database, collection, &filter, &sort, &modification)?;
    let mut last = document(vec![("n", Bson::Int32(i32::from(found.is_some() || outcome.upserted.is_some())))]);
    if !remove {
        last.insert("updatedExisting", Bson::Boolean(outcome.matched > 0));
    }
    if let Some(id) = outcome.upserted {
        last.insert("upserted", id);
    }
    let value = match found {
        Some(found) => Bson::Document(projection.map_or(found.clone(), |projection| projection.apply(&found))),
        None => Bson::Null,
    };
    Ok(ok(document(vec![("lastErrorObject", Bson::Document(last)), ("value", value)])))
}

fn count(shared: &Shared<'_>, database: &str, body: &Document) -> Result<Document, DocumentError> {
    let collection = collection_name(body, "count")?;
    let filter = filter_of(body, "query")?;
    let skip = count_of(body, "skip")?.unwrap_or(0).max(0) as usize;
    let limit = count_of(body, "limit")?.unwrap_or(0).unsigned_abs() as usize;
    let found = shared.store.find(database, collection, &filter, &SortSpec::default())?.len();
    let mut n = found.saturating_sub(skip);
    if limit > 0 {
        n = n.min(limit);
    }
    Ok(ok(document(vec![("n", Bson::Int32(n as i32))])))
}

fn distinct(shared: &Shared<'_>, database: &str, body: &Document) -> Result<Document, DocumentError> {
    let collection = collection_name(body, "distinct")?;
    let Some(key) = body.get_str("key") else {
        return Err(DocumentError::failed_to_parse("BSON field 'distinct.key' is missing but a required field"));
    };
    let filter = filter_of(body, "query")?;
    let mut values: Vec<Bson> = Vec::new();
    for found in shared.store.find(database, collection, &filter, &SortSpec::default())? {
        let mut add = |value: &Bson| {
            if !values.iter().any(|held| compare(held, value) == Ordering::Equal) {
                values.push(value.clone());
            }
        };
        if let Some(value) = crate::aggregate::field_value(&found, key) {
            match &value {
                Bson::Array(items) => items.iter().for_each(&mut add),
                other => add(other),
            }
        }
    }
    values.sort_by(compare);
    Ok(ok(document(vec![("values", Bson::Array(values))])))
}

fn create_indexes(shared: &Shared<'_>, database: &str, body: &Document) -> Result<Document, DocumentError> {
    let collection = collection_name(body, "createIndexes")?;
    let definitions = documents_field(body, "indexes")?;
    let existed = shared.store.indexes(database, collection)?.is_some();
    let (before, after) = shared.store.create_indexes(database, collection, &definitions)?;
    Ok(ok(document(vec![
        ("numIndexesBefore", Bson::Int32(before as i32)),
        ("numIndexesAfter", Bson::Int32(after as i32)),
        ("createdCollectionAutomatically", Bson::Boolean(!existed)),
    ])))
}

fn drop_indexes(shared: &Shared<'_>, database: &str, body: &Document, command: &str) -> Result<Document, DocumentError> {
    let collection = collection_name(body, command)?;
    let name = match body.get("index") {
        Some(Bson::String(name)) => name.clone(),
        Some(Bson::Document(key)) => {
            let indexes = shared.store.indexes(database, collection)?.unwrap_or_default();
            indexes
                .iter()
                .find(|index| index.get_document("key") == Some(key))
                .and_then(|index| index.get_str("name"))
                .map(str::to_owned)
                .ok_or_else(|| DocumentError { code: 27, code_name: "IndexNotFound", message: "can't find index with key".to_owned() })?
        }
        _ => return Err(DocumentError::failed_to_parse("BSON field 'index' is missing but a required field")),
    };
    let before = shared.store.drop_indexes(database, collection, &name)?;
    Ok(ok(document(vec![("nIndexesWas", Bson::Int32(before as i32))])))
}

fn db_stats(shared: &Shared<'_>, database: &str) -> Result<Document, DocumentError> {
    let collections = shared.store.collections_of(database)?;
    let mut objects = 0i64;
    for (name, _) in &collections {
        objects += shared.store.find(database, name, &parse_filter(&Document::new())?, &SortSpec::default())?.len() as i64;
    }
    let bytes = shared.store.databases()?.into_iter().find(|(name, _)| name == database).map_or(0, |(_, bytes)| bytes);
    Ok(ok(document(vec![
        ("db", text(database)),
        ("collections", Bson::Int32(collections.len() as i32)),
        ("objects", Bson::Int64(objects)),
        ("dataSize", Bson::Int64(bytes as i64)),
        ("storageSize", Bson::Int64(bytes as i64)),
    ])))
}
