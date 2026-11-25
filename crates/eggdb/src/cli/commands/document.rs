use super::{Access, Session};
use crate::cli::args::{CommandName, Format};
use crate::cli::failure::{Exit, Failure};
use eggdb::Database;
use eggdb_bson::{Bson, Document};
use eggdb_documents::{Filter, SortSpec, Store};

const EVERY_DOCUMENT: &str = "{}";

impl Session<'_> {
    pub(super) fn document_command(&mut self) -> Result<Exit, Failure> {
        let Some((namespace, query)) = self.request.document.clone() else {
            return Err(Failure::usage("document needs get", Some(CommandName::Document)));
        };
        let db = self.open(Access::ReadOnly, None)?;
        let outcome = query_documents(&db, &namespace, query.as_deref().unwrap_or(EVERY_DOCUMENT));
        self.close(db)?;
        match outcome {
            Ok(documents) => {
                match self.format {
                    Format::Json => {
                        self.out.text(&documents_json(&documents));
                        self.out.text("\n");
                    }
                    _ => {
                        for document in &documents {
                            self.out.text(&eggdb_bson::relaxed_json(document));
                            self.out.text("\n");
                        }
                    }
                }
                Ok(Exit::Done)
            }
            Err(message) => Err(Failure::coded("document_error", message, Exit::DatabaseError)),
        }
    }
}

/// The documents one query reads from `database.collection`: a filter object, or an
/// aggregation pipeline array whose stages only read.
pub(super) fn query_documents(db: &Database, namespace: &str, query: &str) -> Result<Vec<Document>, String> {
    let Some((database, collection)) = namespace.split_once('.').filter(|(database, collection)| {
        !database.is_empty() && !collection.is_empty()
    }) else {
        return Err(format!("{namespace} is not a namespace: give database.collection, such as shop.orders"));
    };
    let wrapped = eggdb_documents::parse_extended_json(&format!("{{\"query\": {query}}}"))
        .map_err(|error| format!("the query is not Extended JSON: {error}"))?;
    let keyspace = super::serve_resp::DatabaseKeyspace {
        db,
        prefix: super::serve_resp::MONGO_COLLECTIONS,
    };
    let store = Store::new(&keyspace, super::serve_mongo::store_seed());
    let every = Filter::And(Vec::new());
    let unsorted = SortSpec::default();
    match wrapped.get("query") {
        Some(Bson::Document(filter)) => {
            let filter = eggdb_documents::parse_filter(filter).map_err(|error| error.to_string())?;
            store
                .find(database, collection, &filter, &unsorted)
                .map_err(|error| error.to_string())
        }
        Some(Bson::Array(stages)) => {
            let (reading, output) = eggdb_documents::split_output(stages).map_err(|error| error.to_string())?;
            if output.is_some() {
                return Err("$out and $merge write: this command only reads".to_owned());
            }
            let stages = eggdb_documents::parse_pipeline(reading).map_err(|error| error.to_string())?;
            let input = store
                .find(database, collection, &every, &unsorted)
                .map_err(|error| error.to_string())?;
            let load = |other: &str| store.find(database, other, &every, &unsorted);
            eggdb_documents::run_pipeline(&stages, input, &load).map_err(|error| error.to_string())
        }
        _ => Err("the query is a filter object or an aggregation pipeline array".to_owned()),
    }
}

/// The documents as one JSON object, `{"documents": [...]}`, each in relaxed Extended JSON.
pub(super) fn documents_json(documents: &[Document]) -> String {
    let items: Vec<String> = documents.iter().map(eggdb_bson::relaxed_json).collect();
    format!("{{\"documents\":[{}]}}", items.join(","))
}
