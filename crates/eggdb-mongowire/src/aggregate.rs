use crate::commands::{Shared, cursor_reply};
use eggdb_bson::{Bson, Document};
use eggdb_documents::{DocumentError, SortSpec, Stage, check_collection_name, parse_filter, parse_pipeline, run_pipeline};

pub(crate) use eggdb_documents::field_value;

const DEFAULT_FIRST_BATCH: usize = 101;

/// Runs the `aggregate` command on a collection: the pipeline's first `$match` chooses the
/// documents read, and the rest runs over them.
pub(crate) fn aggregate(shared: &Shared<'_>, database: &str, body: &Document) -> Result<Document, DocumentError> {
    let collection = match body.get("aggregate") {
        Some(Bson::String(name)) => {
            check_collection_name(name)?;
            name.clone()
        }
        _ => return Err(DocumentError::not_implemented("aggregate without a collection is not supported yet")),
    };
    let Some(pipeline) = body.get_array("pipeline") else {
        return Err(DocumentError::failed_to_parse("BSON field 'aggregate.pipeline' is missing but a required field"));
    };
    if body.get_bool("explain") == Some(true) {
        return Err(DocumentError::not_implemented("explain for aggregate is not supported yet"));
    }
    if pipeline.iter().any(|stage| stage.as_document().is_some_and(|stage| stage.contains_key("$out") || stage.contains_key("$merge"))) {
        return Err(DocumentError::not_implemented("$out and $merge are not supported yet"));
    }
    let stages = parse_pipeline(pipeline)?;
    let (first_filter, rest) = match stages.first().and_then(Stage::as_match) {
        Some(filter) => (filter.clone(), &stages[1..]),
        None => (parse_filter(&Document::new())?, &stages[..]),
    };
    let input = shared.store.find(database, &collection, &first_filter, &SortSpec::default())?;
    let everything = parse_filter(&Document::new())?;
    let load = |name: &str| {
        check_collection_name(name)?;
        shared.store.find(database, name, &everything, &SortSpec::default())
    };
    let output = run_pipeline(rest, input, &load)?;
    let first_batch = body
        .get_document("cursor")
        .and_then(|cursor| cursor.get_i64("batchSize"))
        .filter(|size| *size >= 0)
        .map_or(DEFAULT_FIRST_BATCH, |size| size as usize);
    Ok(cursor_reply(shared, format!("{database}.{collection}"), output.into(), first_batch, false))
}
