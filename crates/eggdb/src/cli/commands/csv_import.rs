use super::copy_io::{StreamCopy, bulk_cache};
use super::sql::{exit_for, print_error};
use super::{Access, Session};
use crate::cli::args::{CommandName, CsvOptions, ExportFile, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject};
use eggdb::sql::Store as DatabaseStore;
use eggdb_sql::{
    BcpCharacterReader, BcpTerminators, Dialect, KeyValueStore, QueryResult, SqlResult,
};
use std::cell::Cell;
use std::fs::File;
use std::io::Read;

fn sql_string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn loaded_rows(result: &QueryResult) -> u64 {
    result
        .tag
        .rsplit(' ')
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
}

fn load_bcp<S: KeyValueStore>(
    session: &mut eggdb_sql::Session<'_, S>,
    statement: &str,
    input: &mut dyn Read,
    terminators: BcpTerminators,
    options: &CsvOptions,
) -> SqlResult<u64> {
    session.execute(statement, &mut |_| {})?;
    let mut reader = BcpCharacterReader::new(input, terminators).skipping(u64::from(options.header));
    let mut loaded: SqlResult<u64> = Ok(0);
    session.execute_bulk_load(&mut reader, &mut |step| {
        loaded = step.outcome.as_ref().map(loaded_rows).map_err(Clone::clone);
        true
    })?;
    loaded
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
        let table = options.table.clone().unwrap_or_default();
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
        let bcp = self.request.dialect == Dialect::SqlServer;
        let terminators = if bcp {
            Some(
                BcpTerminators::written(
                    options.delimiter.as_deref(),
                    options.row_terminator.as_deref(),
                )
                .map_err(|error| usage(error.message().to_owned()))?,
            )
        } else {
            None
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
        let sql = if bcp {
            let keep_nulls = if options.keep_nulls {
                " WITH (KEEP_NULLS)"
            } else {
                ""
            };
            format!("INSERT BULK {table}{keep_nulls}")
        } else {
            format!("COPY {table} FROM STDIN ({})", copy_options.join(", "))
        };
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
            if let Some(terminators) = terminators {
                load_bcp(&mut session, &sql, input, terminators, &options).map(|count| {
                    rows = count;
                })
            } else {
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
            }
        };
        let outcome = match outcome {
            Ok(()) if !session.autocommit() => session.execute("COMMIT", &mut |_| {}),
            outcome => outcome,
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
