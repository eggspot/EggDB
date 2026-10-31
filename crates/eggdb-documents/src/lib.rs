//! EggDB's document model (D197): collections of BSON documents in named databases, stored on
//! EggDB's key-value transactions through [`eggdb_keyspace::Keyspace`], with query filters,
//! update operators, projections, sorting, aggregation expressions and pipeline stages that
//! behave as MongoDB's public documentation
//! describes, so the MongoDB-compatible wire protocol and later SQL over documents share one
//! implementation. No MongoDB code is used or read.

mod error;
mod expression;
mod extended_json;
mod filter;
mod path;
mod pipeline;
mod projection;
mod restore;
mod sort;
mod store;
mod update;

pub use error::{DocumentError, DocumentResult};
pub use expression::{evaluate, field_value};
pub use extended_json::parse_extended_json;
pub use filter::{Condition, Filter, matches, only_id, parse_filter};
pub use pipeline::{Stage, parse_pipeline, run_pipeline};
pub use projection::{Projection, parse_projection};
pub use restore::{RestoredCollection, restore_collection};
pub use sort::{SortSpec, compare_by, parse_sort, sort_documents};
pub use store::{
    InsertOutcome, MAX_DOCUMENT_BYTES, Modification, Store, UpdateOptions, UpdateOutcome, check_collection_name,
    check_database_name,
};
pub use update::{Operator, Update, apply_update, parse_update, upsert_document};

#[cfg(test)]
mod tests;
