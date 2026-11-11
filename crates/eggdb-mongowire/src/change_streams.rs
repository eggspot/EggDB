use crate::commands::{Shared, document, ok, text};
use eggdb_bson::{Bson, Document};
use eggdb_documents::{DocumentError, parse_pipeline, run_pipeline};
use std::time::{Duration, Instant};

const EVENTS_PER_READ: usize = 1_000;
const DEFAULT_AWAIT: Duration = Duration::from_secs(1);
const LONGEST_AWAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);

/// Where a change stream reads next and what it watches: one collection, one database, or
/// every database (`allChangesForCluster`).
#[derive(Debug, Clone)]
pub(crate) struct ChangeStream {
    next: u64,
    database: Option<String>,
    collection: Option<String>,
    full_document: bool,
    stages: Vec<Bson>,
}

fn token(sequence: u64) -> Bson {
    Bson::Document(document(vec![("_data", text(&format!("{sequence:016X}")))]))
}

fn resumed_after(spec: &Document) -> Result<Option<u64>, DocumentError> {
    let Some(token) = spec
        .get_document("resumeAfter")
        .or_else(|| spec.get_document("startAfter"))
    else {
        return Ok(None);
    };
    token
        .get_str("_data")
        .and_then(|data| u64::from_str_radix(data, 16).ok())
        .map(|sequence| Some(sequence + 1))
        .ok_or_else(|| DocumentError {
            code: 260,
            code_name: "InvalidResumeToken",
            message: "the resume token was not made by this server".to_owned(),
        })
}

fn event(stream: &ChangeStream, stored: &Document) -> Option<Document> {
    let sequence = u64::try_from(stored.get_i64("sequence")?).ok()?;
    let database = stored.get_str("db")?;
    let collection = stored.get_str("coll")?;
    if stream.database.as_deref().is_some_and(|wanted| wanted != database)
        || stream.collection.as_deref().is_some_and(|wanted| wanted != collection)
    {
        return None;
    }
    let operation = stored.get_str("operationType")?;
    let wall = match stored.get("wallTime") {
        Some(Bson::DateTime(millis)) => *millis,
        _ => 0,
    };
    let mut entries = vec![
        ("_id", token(sequence)),
        ("operationType", text(operation)),
        (
            "clusterTime",
            Bson::Timestamp {
                time: u32::try_from(wall / 1000).unwrap_or(0),
                increment: sequence as u32,
            },
        ),
        ("wallTime", Bson::DateTime(wall)),
        (
            "ns",
            Bson::Document(document(vec![("db", text(database)), ("coll", text(collection))])),
        ),
        ("documentKey", stored.get("documentKey").cloned().unwrap_or(Bson::Null)),
    ];
    if let Some(full) = stored.get("fullDocument") {
        entries.push(("fullDocument", full.clone()));
    }
    if let Some(description) = stored.get("updateDescription") {
        entries.push(("updateDescription", description.clone()));
        if stream.full_document
            && let Some(after) = stored.get("afterChange")
        {
            entries.push(("fullDocument", after.clone()));
        }
    }
    Some(document(entries))
}

fn read(shared: &Shared<'_>, stream: &mut ChangeStream) -> Result<Vec<Bson>, DocumentError> {
    let stored = shared.store.changes_from(stream.next, EVENTS_PER_READ)?;
    let mut events = Vec::new();
    for change in &stored {
        if let Some(sequence) = change.get_i64("sequence").and_then(|value| u64::try_from(value).ok()) {
            stream.next = stream.next.max(sequence + 1);
        }
        if let Some(event) = event(stream, change) {
            events.push(event);
        }
    }
    if events.is_empty() || stream.stages.is_empty() {
        return Ok(events.into_iter().map(Bson::Document).collect());
    }
    let stages = parse_pipeline(&stream.stages)?;
    let nothing = |_: &str| -> eggdb_documents::DocumentResult<Vec<Document>> { Ok(Vec::new()) };
    Ok(run_pipeline(&stages, events, &nothing)?
        .into_iter()
        .map(Bson::Document)
        .collect())
}

fn reply(batch_name: &str, events: Vec<Bson>, id: i64, namespace: &str, next: u64) -> Document {
    ok(document(vec![(
        "cursor",
        Bson::Document(document(vec![
            (batch_name, Bson::Array(events)),
            ("id", Bson::Int64(id)),
            ("ns", text(namespace)),
            ("postBatchResumeToken", token(next.saturating_sub(1))),
        ])),
    )]))
}

/// `aggregate` whose pipeline starts with `$changeStream`: opens a cursor on the change log
/// from now, or from just after a resume token, on the collection named, the whole database
/// (`aggregate: 1`), or every database (`allChangesForCluster` on `admin`); the stages after
/// `$changeStream` filter and reshape each event.
pub(crate) fn open(
    shared: &Shared<'_>,
    database: &str,
    body: &Document,
    pipeline: &[Bson],
) -> Result<Document, DocumentError> {
    let Some(Bson::Document(first)) = pipeline.first() else {
        return Err(DocumentError::failed_to_parse("the pipeline is empty"));
    };
    let spec = match first.get("$changeStream") {
        Some(Bson::Document(spec)) => spec.clone(),
        _ => return Err(DocumentError::failed_to_parse("$changeStream takes an object")),
    };
    let collection = match body.get("aggregate") {
        Some(Bson::String(name)) => Some(name.clone()),
        _ => None,
    };
    let every_database = spec.get_bool("allChangesForCluster") == Some(true);
    if every_database && (database != "admin" || collection.is_some()) {
        return Err(DocumentError::failed_to_parse(
            "allChangesForCluster is only allowed on the admin database with aggregate: 1",
        ));
    }
    let stages: Vec<Bson> = pipeline[1..].to_vec();
    parse_pipeline(&stages)?;
    let next = match resumed_after(&spec)? {
        Some(next) => next,
        None => shared.store.change_position()?,
    };
    let mut stream = ChangeStream {
        next,
        database: (!every_database).then(|| database.to_owned()),
        collection: collection.clone(),
        full_document: matches!(
            spec.get_str("fullDocument"),
            Some("updateLookup" | "whenAvailable" | "required")
        ),
        stages,
    };
    let namespace = match &collection {
        Some(collection) => format!("{database}.{collection}"),
        None => format!("{database}.$cmd.aggregate"),
    };
    let events = read(shared, &mut stream)?;
    let next = stream.next;
    let id = shared
        .cursors
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .open_change_stream(namespace.clone(), stream)
        .ok_or_else(|| DocumentError {
            code: 146,
            code_name: "ExceededMemoryLimit",
            message: "the server's memory budget has no room for another cursor".to_owned(),
        })?;
    Ok(reply("firstBatch", events, id, &namespace, next))
}

/// `getMore` on a change stream: the events since the last batch, waiting up to the request's
/// `maxTimeMS` (one second when absent, at most ten) for the first one.
pub(crate) fn get_more(
    shared: &Shared<'_>,
    id: i64,
    namespace: &str,
    mut stream: ChangeStream,
    body: &Document,
) -> Result<(Document, ChangeStream), DocumentError> {
    let wait = body
        .get("maxTimeMS")
        .and_then(Bson::as_i64)
        .and_then(|millis| u64::try_from(millis).ok())
        .map_or(DEFAULT_AWAIT, Duration::from_millis)
        .min(LONGEST_AWAIT);
    let deadline = Instant::now() + wait;
    loop {
        let events = read(shared, &mut stream)?;
        if !events.is_empty() || Instant::now() >= deadline {
            let next = stream.next;
            return Ok((reply("nextBatch", events, id, namespace, next), stream));
        }
        std::thread::sleep(POLL);
    }
}
