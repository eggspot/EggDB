use super::{Access, Session, operation_failure};
use crate::cli::args::{CollectionAction, CommandName, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::output::Style;
use crate::cli::render::{self, JsonObject};

impl Session<'_> {
    pub(super) fn collection_command(&mut self) -> Result<Exit, Failure> {
        match self.request.collection_action {
            Some(CollectionAction::List) => self.list_collections(),
            Some(CollectionAction::Add) => self.add_collection(),
            Some(CollectionAction::Delete) => self.delete_collection(),
            None => Err(Failure::usage(
                "collection needs list, add or delete",
                Some(CommandName::Collection),
            )),
        }
    }

    fn list_collections(&mut self) -> Result<Exit, Failure> {
        let db = self.open(Access::ReadOnly, None)?;
        let names = db.collection_names().map_err(|error| {
            operation_failure(
                &error,
                &format!("cannot list the collections of {}", self.file),
            )
        })?;
        drop(db);
        match self.format {
            Format::Json => {
                let list = names
                    .iter()
                    .map(|name| render::json_string(name))
                    .collect::<Vec<_>>()
                    .join(",");
                self.out.text(
                    &JsonObject::new()
                        .string("file", &self.file_display)
                        .raw("collections", &format!("[{list}]"))
                        .line(),
                );
            }
            _ => {
                for name in &names {
                    self.out.text(name);
                    self.out.text("\n");
                }
                self.out.finish()?;
                self.messages
                    .styled(Style::Dim, &collections_count(names.len()));
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
        let deleted = db.drop_collection(&name).map_err(|error| {
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
    use super::collections_count;

    #[test]
    fn the_count_reads_as_words() {
        assert_eq!(collections_count(0), "no collections");
        assert_eq!(collections_count(1), "1 collection");
        assert_eq!(collections_count(1200), "1,200 collections");
    }
}
