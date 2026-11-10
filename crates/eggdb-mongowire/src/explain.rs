use crate::commands::{COMPATIBLE_VERSION, Shared, collection_name, document, filter_of, ok, text};
use eggdb_bson::{Bson, Document};
use eggdb_documents::{DocumentError, Filter, SortSpec, only_id, parse_filter};
use std::time::Instant;

const VERBOSITIES: [&str; 3] = ["queryPlanner", "executionStats", "allPlansExecution"];

struct Explained<'b> {
    command: &'b str,
    collection: &'b str,
    filter: Filter,
    parsed_query: Document,
    writes: bool,
}

fn statement_of<'b>(explained: &'b Document, field: &str) -> Result<&'b Document, DocumentError> {
    match explained
        .get_array(field)
        .and_then(|statements| statements.first())
    {
        Some(Bson::Document(statement)) => Ok(statement),
        _ => Err(DocumentError::failed_to_parse(format!(
            "explain of this command needs a non-empty '{field}' array of objects"
        ))),
    }
}

fn query_of(body: &Document, field: &str) -> Document {
    body.get_document(field).cloned().unwrap_or_default()
}

fn explained_of(explained: &Document) -> Result<Explained<'_>, DocumentError> {
    let Some((command, _)) = explained.first() else {
        return Err(DocumentError::failed_to_parse(
            "explain needs a command to explain",
        ));
    };
    let command = command.as_str();
    let collection = collection_name(explained, command)?;
    let (filter, parsed_query, writes) = match command {
        "find" => (
            filter_of(explained, "filter")?,
            query_of(explained, "filter"),
            false,
        ),
        "count" | "distinct" => (
            filter_of(explained, "query")?,
            query_of(explained, "query"),
            false,
        ),
        "findAndModify" | "findandmodify" => (
            filter_of(explained, "query")?,
            query_of(explained, "query"),
            true,
        ),
        "update" => {
            let statement = statement_of(explained, "updates")?;
            (filter_of(statement, "q")?, query_of(statement, "q"), true)
        }
        "delete" => {
            let statement = statement_of(explained, "deletes")?;
            (filter_of(statement, "q")?, query_of(statement, "q"), true)
        }
        "aggregate" => {
            let first_match = explained
                .get_array("pipeline")
                .and_then(|stages| stages.first())
                .and_then(Bson::as_document)
                .and_then(|stage| stage.get_document("$match"))
                .cloned()
                .unwrap_or_default();
            (parse_filter(&first_match)?, first_match, false)
        }
        other => {
            return Err(DocumentError {
                code: 59,
                code_name: "CommandNotFound",
                message: format!("Explain failed due to unknown command: {other}"),
            });
        }
    };
    Ok(Explained {
        command,
        collection,
        filter,
        parsed_query,
        writes,
    })
}

fn winning_plan(explained: &Explained<'_>) -> Document {
    let mut plan = if only_id(&explained.filter).is_some() {
        document(vec![("stage", text("IDHACK"))])
    } else {
        document(vec![
            ("stage", text("COLLSCAN")),
            ("filter", Bson::Document(explained.parsed_query.clone())),
            ("direction", text("forward")),
        ])
    };
    let wrapper = match explained.command {
        "update" => Some("UPDATE"),
        "delete" => Some("DELETE"),
        "count" => Some("COUNT"),
        "distinct" => Some("PROJECTION_DEFAULT"),
        _ => None,
    };
    if let Some(stage) = wrapper {
        plan = document(vec![
            ("stage", text(stage)),
            ("inputStage", Bson::Document(plan)),
        ]);
    }
    plan
}

/// Answers `explain` for `find`, `count`, `distinct`, `aggregate`, `update`, `delete` and
/// `findAndModify`: the plan is a collection scan, or the `_id` lookup when the filter is only
/// an `_id` equality, and the execution statistics come from running the filter.
pub(crate) fn explain(
    shared: &Shared<'_>,
    database: &str,
    body: &Document,
) -> Result<Document, DocumentError> {
    let Some(explained) = body.get_document("explain") else {
        return Err(DocumentError::type_mismatch(
            "BSON field 'explain.explain' is the wrong type, expected type 'object'",
        ));
    };
    let verbosity = match body.get("verbosity") {
        None => "allPlansExecution",
        Some(Bson::String(verbosity)) if VERBOSITIES.contains(&verbosity.as_str()) => verbosity.as_str(),
        Some(_) => {
            return Err(DocumentError::bad_value(
                "verbosity string must be one of {'queryPlanner', 'executionStats', 'allPlansExecution'}",
            ));
        }
    };
    explain_with(shared, database, explained, verbosity)
}

/// Answers `aggregate` with `explain: true`, which MongoDB explains at `queryPlanner`.
pub(crate) fn explain_aggregate(
    shared: &Shared<'_>,
    database: &str,
    body: &Document,
) -> Result<Document, DocumentError> {
    let mut explained = body.clone();
    explained.remove("explain");
    explain_with(shared, database, &explained, "queryPlanner")
}

fn explain_with(
    shared: &Shared<'_>,
    database: &str,
    explained_body: &Document,
    verbosity: &str,
) -> Result<Document, DocumentError> {
    let explained = explained_of(explained_body)?;
    let plan = winning_plan(&explained);
    let mut reply = document(vec![
        ("explainVersion", text("1")),
        (
            "queryPlanner",
            Bson::Document(document(vec![
                (
                    "namespace",
                    text(&format!("{database}.{}", explained.collection)),
                ),
                ("indexFilterSet", Bson::Boolean(false)),
                (
                    "parsedQuery",
                    Bson::Document(explained.parsed_query.clone()),
                ),
                ("maxIndexedOrSolutionsReached", Bson::Boolean(false)),
                ("maxIndexedAndSolutionsReached", Bson::Boolean(false)),
                ("maxScansToExplodeReached", Bson::Boolean(false)),
                ("winningPlan", Bson::Document(plan)),
                ("rejectedPlans", Bson::Array(Vec::new())),
            ])),
        ),
    ]);
    if verbosity != "queryPlanner" {
        let started = Instant::now();
        let matched = shared.store.find(
            database,
            explained.collection,
            &explained.filter,
            &SortSpec::default(),
        )?;
        let by_id = only_id(&explained.filter).is_some();
        let examined = if by_id {
            matched.len()
        } else {
            shared
                .store
                .find(
                    database,
                    explained.collection,
                    &parse_filter(&Document::new())?,
                    &SortSpec::default(),
                )?
                .len()
        };
        let returned = if explained.writes { 0 } else { matched.len() };
        let elapsed = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let mut statistics = document(vec![
            ("executionSuccess", Bson::Boolean(true)),
            ("nReturned", Bson::Int64(returned as i64)),
            ("executionTimeMillis", Bson::Int64(elapsed)),
            (
                "totalKeysExamined",
                Bson::Int64(if by_id { matched.len() as i64 } else { 0 }),
            ),
            ("totalDocsExamined", Bson::Int64(examined as i64)),
        ]);
        if verbosity == "allPlansExecution" {
            statistics.insert("allPlansExecution", Bson::Array(Vec::new()));
        }
        reply.insert("executionStats", Bson::Document(statistics));
    }
    reply.insert("command", Bson::Document(explained_body.clone()));
    reply.insert(
        "serverInfo",
        Bson::Document(document(vec![
            ("host", text("eggdb")),
            ("version", text(COMPATIBLE_VERSION)),
        ])),
    );
    Ok(ok(reply))
}
