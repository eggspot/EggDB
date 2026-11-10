use crate::commands::{Shared, cursor_reply};
use eggdb_bson::{Bson, Document};
use eggdb_documents::{
    DocumentError, PipelineOutput, SortSpec, Stage, check_collection_name, check_database_name,
    parse_filter, parse_pipeline, pipeline_with_time, run_pipeline, split_output,
};

pub(crate) use eggdb_documents::field_value;

const DEFAULT_FIRST_BATCH: usize = 101;

/// Runs the `aggregate` command on a collection: the pipeline's first `$match` chooses the
/// documents read, and the rest runs over them.
pub(crate) fn aggregate(
    shared: &Shared<'_>,
    database: &str,
    body: &Document,
) -> Result<Document, DocumentError> {
    let collection = match body.get("aggregate") {
        Some(Bson::String(name)) => {
            check_collection_name(name)?;
            name.clone()
        }
        _ => {
            return Err(DocumentError::not_implemented(
                "aggregate without a collection is not supported yet",
            ));
        }
    };
    let Some(pipeline) = body.get_array("pipeline") else {
        return Err(DocumentError::failed_to_parse(
            "BSON field 'aggregate.pipeline' is missing but a required field",
        ));
    };
    if body.get_bool("explain") == Some(true) {
        return Err(DocumentError::not_implemented(
            "explain for aggregate is not supported yet",
        ));
    }
    let bound = pipeline_with_time(pipeline, crate::commands::now_ms());
    let (bound, written_to) = split_output(&bound)?;
    let stages = parse_pipeline(bound)?;
    let (first_filter, rest) = match stages.first().and_then(Stage::as_match) {
        Some(filter) => (filter.clone(), &stages[1..]),
        None => (parse_filter(&Document::new())?, &stages[..]),
    };
    let input = shared
        .store
        .find(database, &collection, &first_filter, &SortSpec::default())?;
    let everything = parse_filter(&Document::new())?;
    let load = |name: &str| {
        check_collection_name(name)?;
        shared
            .store
            .find(database, name, &everything, &SortSpec::default())
    };
    let mut output = run_pipeline(rest, input, &load)?;
    if let Some(written_to) = written_to {
        write_output(shared, database, written_to, std::mem::take(&mut output))?;
    }
    let first_batch = body
        .get_document("cursor")
        .and_then(|cursor| cursor.get_i64("batchSize"))
        .filter(|size| *size >= 0)
        .map_or(DEFAULT_FIRST_BATCH, |size| size as usize);
    Ok(cursor_reply(
        shared,
        format!("{database}.{collection}"),
        output.into(),
        first_batch,
        false,
    ))
}

fn write_output(
    shared: &Shared<'_>,
    database: &str,
    written_to: PipelineOutput,
    documents: Vec<Document>,
) -> Result<(), DocumentError> {
    match written_to {
        PipelineOutput::Out { database: target, collection } => {
            let target = target.as_deref().unwrap_or(database);
            check_database_name(target)?;
            shared.store.replace_documents(target, &collection, documents)?;
        }
        PipelineOutput::Merge { database: target, collection, when_matched, when_not_matched } => {
            let target = target.as_deref().unwrap_or(database);
            check_database_name(target)?;
            shared.store.merge_documents(
                target,
                &collection,
                documents,
                &when_matched,
                when_not_matched,
            )?;
        }
    }
    Ok(())
}
