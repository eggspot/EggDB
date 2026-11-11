//! EggDB's document model (D197): collections of BSON documents in named databases, stored on
//! EggDB's key-value transactions through [`eggdb_keyspace::Keyspace`], with query filters,
//! update operators (with positional `$`, `$[]` and `$[<identifier>]` paths and
//! pipeline-style updates), projections, sorting, aggregation expressions and pipeline stages
//! that behave as MongoDB's public documentation describes, so the MongoDB-compatible wire
//! protocol and later SQL over documents share one implementation. No MongoDB code is used or
//! read.

mod change_log;
mod error;
mod expression;
mod extended_json;
mod filter;
mod json_schema;
mod output;
mod path;
mod pipeline;
mod projection;
mod restore;
mod size;
mod sort;
mod store;
mod text_search;
mod update;

pub use change_log::KEPT_CHANGES;
pub use error::{DocumentError, DocumentResult};
pub use expression::{evaluate, field_value};
pub use extended_json::parse_extended_json;
pub use filter::{Condition, Filter, has_text, matched_array_index, matches, only_id, parse_filter, with_text_fields, without_text_score};
pub use text_search::TextSearch;
pub use output::{PipelineOutput, WhenMatched, WhenNotMatched, split_output};
pub use pipeline::{Stage, parse_pipeline, run_pipeline};
pub use projection::{Projection, parse_projection};
pub use restore::{
    ArchivedCollection, MAX_GUNZIPPED_DUMP_BYTES, MONGODUMP_ARCHIVE_MAGIC, RestoredCollection,
    gunzip_dump, read_mongodump_archive, restore_collection,
};
pub use sort::{SortSpec, compare_by, parse_sort, sort_documents};
pub use store::{
    InsertOutcome, MAX_DOCUMENT_BYTES, Modification, Store, UpdateOptions, UpdateOutcome,
    check_collection_name, check_database_name,
};
pub use update::{
    ArrayFilter, Operator, Update, apply_update, parse_update, parse_update_statement,
    filter_with_time, pipeline_with_time, upsert_document,
};

#[cfg(test)]
mod tests;
