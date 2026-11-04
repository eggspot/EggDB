use super::{Access, Session, operation_failure};
use crate::cli::args::{CollectionAction, CommandName, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::output::Style;
use crate::cli::progress::ProgressLine;
use crate::cli::render::{self, JsonObject};
use crate::cli::reports;
use eggdb::{CollectionStats, Database, DropProgress, ErrorKind};
use std::time::Duration;

pub(super) struct CountedCollections {
    pub(super) stats: Vec<CollectionStats>,
    pub(super) uncounted: bool,
}

impl Session<'_> {
    pub(super) fn collection_command(&mut self) -> Result<Exit, Failure> {
        match self.request.collection_action {
            Some(CollectionAction::List) => self.list_collections(),
            Some(CollectionAction::Add) => self.add_collection(),
            Some(CollectionAction::Delete) => self.delete_collection(),
            Some(CollectionAction::Move) => self.move_collection(),
            None => Err(Failure::usage(
                "collection needs list, add, delete or move",
                Some(CommandName::Collection),
            )),
        }
    }

    pub(super) fn counted_collections(
        &self,
        db: &Database,
        context: &str,
    ) -> Result<CountedCollections, Failure> {
        let stats = db
            .collection_stats()
            .map_err(|error| operation_failure(&error, context))?;
        let uncounted = db.is_read_only()
            && self.shared.is_none()
            && db.has_uncounted_collections().unwrap_or(false);
        Ok(CountedCollections { stats, uncounted })
    }

    pub(super) fn keep_collection_counts(&self, counted: &CountedCollections) {
        if !counted.uncounted {
            return;
        }
        let options = self
            .database_options(Access::Write { create: false }, None)
            .busy_timeout(Duration::ZERO);
        if let Ok(db) = Database::open_with(&self.request.file, options) {
            let _ = db.collection_stats();
            let _ = db.close();
        }
    }

    fn list_collections(&mut self) -> Result<Exit, Failure> {
        let db = self.open(Access::ReadOnly, None)?;
        let context = format!("cannot list the collections of {}", self.file);
        let counted = self.counted_collections(&db, &context)?;
        drop(db);
        self.keep_collection_counts(&counted);
        let stats = &counted.stats;
        match self.format {
            Format::Json => {
                let list = stats
                    .iter()
                    .map(|one| render::json_string(&one.name))
                    .collect::<Vec<_>>()
                    .join(",");
                self.out.text(
                    &JsonObject::new()
                        .string("file", &self.file_display)
                        .raw("collections", &format!("[{list}]"))
                        .raw("counts", &reports::collections_json(stats))
                        .line(),
                );
            }
            _ => {
                self.out.text(&listed_with_counts(stats));
                self.out.finish()?;
                self.messages
                    .styled(Style::Dim, &collections_count(stats.len()));
                self.messages.text("\n");
            }
        }
        Ok(Exit::Done)
    }

    fn add_collection(&mut self) -> Result<Exit, Failure> {
        let name = self.request.collection.clone().unwrap_or_default();
        let db = self.open(Access::Write { create: false }, None)?;
        let added = db.create_collection(&name).map_err(|error| {
            operation_failure(
                &error,
                &format!("cannot add the collection {name} to {}", self.file),
            )
        })?;
        self.close(db)?;
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("file", &self.file_display)
                    .string("collection", &name)
                    .boolean("added", added)
                    .line(),
            ),
            _ if added => self
                .out
                .text(&format!("added collection {name} to {}\n", self.file)),
            _ => self.error_message(&format!(
                "nothing to add: {name} is already a collection in {}",
                self.file
            )),
        }
        Ok(if added { Exit::Done } else { Exit::NotFound })
    }

    fn delete_collection(&mut self) -> Result<Exit, Failure> {
        let name = self.request.collection.clone().unwrap_or_default();
        let db = self.open(Access::Write { create: false }, None)?;
        let mut progress = ProgressLine::new(self.stderr_is_terminal);
        let messages = &mut self.messages;
        let dropped = db.drop_collection_with_progress(&name, |step| {
            progress.show(messages, || drop_progress_line(&name, step));
        });
        progress.clear(&mut self.messages);
        let deleted = dropped.map_err(|error| {
            operation_failure(
                &error,
                &format!("cannot delete the collection {name} from {}", self.file),
            )
        })?;
        self.close(db)?;
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("file", &self.file_display)
                    .string("collection", &name)
                    .boolean("deleted", deleted)
                    .line(),
            ),
            _ if deleted => self
                .out
                .text(&format!("deleted collection {name} from {}\n", self.file)),
            _ => self.error_message(&format!(
                "nothing to delete: {name} is not a collection in {}",
                self.file
            )),
        }
        Ok(if deleted { Exit::Done } else { Exit::NotFound })
    }

    fn move_collection(&mut self) -> Result<Exit, Failure> {
        let name = self.request.collection.clone().unwrap_or_default();
        let new_name = self.request.new_collection_name.clone().unwrap_or_default();
        let db = self.open(Access::Write { create: false }, None)?;
        let moved = match db.rename_collection(&name, &new_name) {
            Ok(()) => true,
            Err(error) if error.kind() == ErrorKind::CollectionNotFound => false,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return Err(Failure::coded(
                    "already_exists",
                    format!(
                        "cannot rename the collection {name} to {new_name} in {}: {new_name} is already a collection there, and a rename never replaces one. Choose another name, or delete {new_name} first.",
                        self.file
                    ),
                    Exit::DatabaseError,
                ));
            }
            Err(error) => {
                return Err(operation_failure(
                    &error,
                    &format!(
                        "cannot rename the collection {name} to {new_name} in {}",
                        self.file
                    ),
                ));
            }
        };
        self.close(db)?;
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("file", &self.file_display)
                    .string("collection", &name)
                    .string("new_name", &new_name)
                    .boolean("moved", moved)
                    .line(),
            ),
            _ if moved => self.out.text(&format!(
                "renamed collection {name} to {new_name} in {}\n",
                self.file
            )),
            _ => self.error_message(&format!(
                "nothing to rename: {name} is not a collection in {}",
                self.file
            )),
        }
        Ok(if moved { Exit::Done } else { Exit::NotFound })
    }
}

pub(crate) fn listed_with_counts(stats: &[CollectionStats]) -> String {
    let counts: Vec<String> = stats.iter().map(|one| render::entries(one.entries)).collect();
    let name_width = stats
        .iter()
        .map(|one| one.name.chars().count())
        .max()
        .unwrap_or(0);
    let count_width = counts
        .iter()
        .map(|count| count.chars().count())
        .max()
        .unwrap_or(0);
    stats
        .iter()
        .zip(&counts)
        .map(|(one, count)| format!("{:<name_width$}  {count:>count_width$}\n", one.name))
        .collect()
}

fn drop_progress_line(name: &str, step: DropProgress) -> String {
    match step.entries {
        Some(entries) => format!(
            "deleting collection {name}: {} of {}…",
            render::count(step.removed),
            render::entries(entries)
        ),
        None => format!(
            "deleting collection {name}: {}…",
            render::entries(step.removed)
        ),
    }
}

pub(crate) fn collections_count(count: usize) -> String {
    match count {
        0 => "no collections".to_owned(),
        1 => "1 collection".to_owned(),
        count => format!("{} collections", render::count(count as u64)),
    }
}

#[cfg(test)]
mod tests {
    use super::{collections_count, drop_progress_line, listed_with_counts};
    use eggdb::{CollectionStats, Database, DropProgress};

    #[test]
    fn the_list_puts_each_count_right_aligned_after_the_longest_name() {
        let db = Database::in_memory().unwrap();
        for name in ["cache", "sessions", "users"] {
            db.create_collection(name).unwrap();
        }
        db.write(|tx| {
            let mut cache = tx.collection("cache")?;
            for n in 0..1204u32 {
                cache.put(n.to_be_bytes(), "v")?;
            }
            tx.collection("users")?.put("u:1", "Ada")?;
            Ok(())
        })
        .unwrap();
        let stats: Vec<CollectionStats> = db.collection_stats().unwrap();
        assert_eq!(
            listed_with_counts(&stats),
            "cache     1,204 entries\nsessions     no entries\nusers           1 entry\n"
        );
        assert_eq!(listed_with_counts(&[]), "");
    }

    #[test]
    fn a_drop_s_progress_names_the_total_when_the_count_is_kept() {
        let db = Database::in_memory().unwrap();
        db.create_collection("cache").unwrap();
        db.collection("cache").put("k", "v").unwrap();
        let mut seen: Vec<DropProgress> = Vec::new();
        assert!(db.drop_collection_with_progress("cache", |step| seen.push(step)).unwrap());
        assert_eq!(
            seen.iter().map(|step| drop_progress_line("cache", *step)).collect::<Vec<_>>(),
            ["deleting collection cache: 1 of 1 entry…"]
        );
    }

    #[test]
    fn the_count_reads_as_words() {
        assert_eq!(collections_count(0), "no collections");
        assert_eq!(collections_count(1), "1 collection");
        assert_eq!(collections_count(1200), "1,200 collections");
    }
}
