use super::copy_io::{StreamCopy, bulk_cache};
use super::sql::{exit_for, print_error};
use super::{Access, Session};
use crate::cli::args::{CommandName, ExportFile, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject};
use eggdb::sql::Store as DatabaseStore;
use eggdb_sql::QueryResult;
use std::cell::Cell;
use std::fs::File;
use std::io::Read;

fn sql_string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn is_one_name(text: &str) -> bool {
    let mut quoted = false;
    let bytes = text.as_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b';' if !quoted => return false,
            b'-' | b'/' if !quoted && matches!(bytes.get(index + 1), Some(b'-' | b'*')) => {
                return false;
            }
            _ => {}
        }
    }
    !quoted && !text.trim().is_empty()
}

impl Session<'_> {
    pub(super) fn import_csv(&mut self, source: ExportFile) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Import));
        let options = self.request.csv.clone();
        let table = options.table.unwrap_or_default();
        if !is_one_name(&table) {
            return Err(usage(format!(
                "--table takes one table name as SQL writes it, such as users or app.\"Users\", not '{table}'"
            )));
        }
        let (mut file, size, shown) = match &source {
            ExportFile::Path(path) => {
                let shown = render::text(path.display().to_string().as_bytes());
                let file = File::open(path)
                    .map_err(|error| usage(format!("cannot read {shown}: {error}")))?;
                let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                (Some(file), size, shown)
            }
            ExportFile::Standard if self.stdin_is_terminal => {
                return Err(usage(
                    "import --csv - reads the CSV from stdin: pipe it in, or give its file"
                        .to_owned(),
                ));
            }
            ExportFile::Standard => (None, 0, "stdin".to_owned()),
        };
        let mut copy_options = vec!["FORMAT csv".to_owned()];
        if options.header {
            copy_options.push("HEADER".to_owned());
        }
        if let Some(delimiter) = &options.delimiter {
            copy_options.push(format!("DELIMITER {}", sql_string(delimiter)));
        }
        if let Some(null) = &options.null {
            copy_options.push(format!("NULL {}", sql_string(null)));
        }
        let sql = format!("COPY {table} FROM STDIN ({})", copy_options.join(", "));
        let db = self.open(Access::Write { create: false }, Some(bulk_cache(size)))?;
        let store = DatabaseStore(&db);
        let mut session = eggdb_sql::Session::with_dialect(&store, self.request.dialect);
        let copied_out = Cell::new(false);
        let mut rows = 0u64;
        let outcome = {
            let input: &mut dyn Read = match &mut file {
                Some(file) => file,
                None => &mut *self.stdin,
            };
            let mut copy = StreamCopy {
                input: Some(input),
                output: None,
                copied_out: &copied_out,
            };
            session.execute_with_copy(
                &sql,
                &mut |result: QueryResult| {
                    if let Some(count) = result.tag.strip_prefix("COPY ") {
                        rows = count.parse().unwrap_or(0);
                    }
                    true
                },
                &mut copy,
            )
        };
        let closed = session.close();
        let exit = match outcome.and(closed.map(|_| ())) {
            Ok(()) => Exit::Done,
            Err(error) => {
                print_error(&mut self.messages, &error, &sql);
                self.messages
                    .text("Nothing was imported: the rows load in one transaction.\n");
                exit_for(&error)
            }
        };
        self.close(db)?;
        if exit != Exit::Done {
            return Ok(exit);
        }
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("file", &self.file_display)
                    .string("from", &shown)
                    .string("table", &table)
                    .number("rows", rows)
                    .line(),
            ),
            _ => self.out.text(&format!(
                "imported {} {} from {shown} into {table} in {}\n",
                render::count(rows),
                if rows == 1 { "row" } else { "rows" },
                self.file
            )),
        }
        Ok(Exit::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::{is_one_name, sql_string};

    #[test]
    fn a_table_option_is_one_name_and_strings_are_quoted() {
        for name in ["users", "app.\"My; Table\"", "t (a, b)", "\"a--b\""] {
            assert!(is_one_name(name), "{name}");
        }
        for name in ["", "t; DROP TABLE x", "t -- comment", "t /* x */", "\"open"] {
            assert!(!is_one_name(name), "{name}");
        }
        assert_eq!(sql_string("it's"), "'it''s'");
    }
}
