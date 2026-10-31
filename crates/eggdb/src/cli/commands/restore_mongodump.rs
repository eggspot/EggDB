use super::{Access, Session};
use crate::cli::args::{CommandName, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject};
use std::path::{Path, PathBuf};

const DUMP_SUFFIX: &str = ".bson";
const METADATA_SUFFIX: &str = ".metadata.json";

pub(super) enum Layout {
    Databases,
    OneDatabase,
    OneCollection,
}

fn holds_bson(directory: &Path) -> bool {
    std::fs::read_dir(directory).is_ok_and(|entries| {
        entries.flatten().any(|entry| entry.file_name().to_string_lossy().ends_with(DUMP_SUFFIX))
    })
}

pub(super) fn layout_of(path: &Path) -> Option<Layout> {
    if path.is_file() {
        return path.to_string_lossy().ends_with(DUMP_SUFFIX).then_some(Layout::OneCollection);
    }
    if !path.is_dir() {
        return None;
    }
    if holds_bson(path) {
        return Some(Layout::OneDatabase);
    }
    let any_database = std::fs::read_dir(path)
        .ok()?
        .flatten()
        .any(|entry| entry.path().is_dir() && holds_bson(&entry.path()));
    any_database.then_some(Layout::Databases)
}

fn name_of(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

fn collections_in(directory: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut found: Vec<(String, PathBuf)> = std::fs::read_dir(directory)?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let collection = name.strip_suffix(DUMP_SUFFIX)?.to_owned();
            entry.path().is_file().then(|| (collection, entry.path()))
        })
        .collect();
    found.sort();
    Ok(found)
}

#[derive(Default)]
struct Totals {
    databases: u64,
    collections: u64,
    documents: u64,
    failed: u64,
    indexes: u64,
    skipped: Vec<String>,
}

impl Session<'_> {
    pub(super) fn restore_mongodump(&mut self, dump: &Path, layout: Layout) -> Result<Exit, Failure> {
        let shown = render::text(dump.display().to_string().as_bytes());
        let usage = |message: String| Failure::usage(message, Some(CommandName::Restore));
        let mut plan: Vec<(String, Vec<(String, PathBuf)>)> = Vec::new();
        let read_failure = |error: std::io::Error| usage(format!("cannot read {shown}: {error}"));
        match layout {
            Layout::OneCollection => {
                let collection = name_of(dump).strip_suffix(DUMP_SUFFIX).unwrap_or_default().to_owned();
                let database = dump.parent().map(name_of).filter(|name| !name.is_empty()).unwrap_or_else(|| "test".to_owned());
                plan.push((database, vec![(collection, dump.to_path_buf())]));
            }
            Layout::OneDatabase => plan.push((name_of(dump), collections_in(dump).map_err(read_failure)?)),
            Layout::Databases => {
                let mut databases: Vec<PathBuf> =
                    std::fs::read_dir(dump).map_err(read_failure)?.flatten().map(|entry| entry.path()).filter(|path| path.is_dir()).collect();
                databases.sort();
                for database in databases {
                    let name = name_of(&database);
                    if matches!(name.as_str(), "admin" | "config" | "local") {
                        continue;
                    }
                    plan.push((name, collections_in(&database).map_err(read_failure)?));
                }
            }
        }
        let db = self.open(Access::Write { create: true }, None)?;
        let keyspace = super::serve_resp::DatabaseKeyspace { db: &db, prefix: super::serve_resp::MONGO_COLLECTIONS };
        let store = eggdb_documents::Store::new(&keyspace, super::serve_mongo::store_seed());
        let mut totals = Totals::default();
        let mut first_error: Option<String> = None;
        for (database, collections) in &plan {
            totals.databases += 1;
            for (collection, file) in collections {
                if collection.starts_with("system.") {
                    continue;
                }
                let documents = std::fs::File::open(file).map_err(read_failure)?;
                let metadata_path = file.with_file_name(format!("{collection}{METADATA_SUFFIX}"));
                let metadata = std::fs::read_to_string(&metadata_path).ok();
                let restored = eggdb_documents::restore_collection(
                    &store,
                    database,
                    collection,
                    std::io::BufReader::new(documents),
                    metadata.as_deref(),
                )
                .map_err(|error| {
                    Failure::database(
                        format!("cannot restore {database}.{collection} from {shown}: {}", error.message),
                        Exit::DatabaseError,
                    )
                })?;
                totals.collections += 1;
                totals.documents += restored.documents;
                totals.failed += restored.failed;
                totals.indexes += restored.indexes;
                totals.skipped.extend(restored.skipped_indexes.into_iter().map(|reason| format!("{database}.{collection} {reason}")));
                if first_error.is_none() {
                    first_error = restored.first_error.map(|error| format!("{database}.{collection}: {error}"));
                }
            }
        }
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("format", "mongodump")
                    .number("databases", totals.databases)
                    .number("collections", totals.collections)
                    .number("documents", totals.documents)
                    .number("failed", totals.failed)
                    .number("indexes", totals.indexes)
                    .line(),
            ),
            _ => {
                self.out.text(&format!(
                    "restored {shown} (mongodump): {} databases, {} collections, {} documents, {} failed, {} indexes\n",
                    totals.databases, totals.collections, totals.documents, totals.failed, totals.indexes
                ));
                for skipped in &totals.skipped {
                    self.out.text(&format!("index left out: {}\n", render::text(skipped.as_bytes())));
                }
                if let Some(error) = &first_error {
                    self.out.text(&format!("first failure: {}\n", render::text(error.as_bytes())));
                }
            }
        }
        self.close(db)?;
        Ok(if totals.failed == 0 { Exit::Done } else { Exit::DatabaseError })
    }
}
