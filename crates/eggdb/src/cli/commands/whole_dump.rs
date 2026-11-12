use super::sql::{exit_for, print_error};
use super::{Access, Session, operation_failure};
use crate::cli::args::{ExportFile, Format};
use crate::cli::failure::{Exit, Failure, output_failure};
use crate::cli::jsonl::{self, ExportReader, Header, Item, ReadError};
use crate::cli::render::{self, JsonObject};
use eggdb::sql::Store as DatabaseStore;
use eggdb::{Database, Error, ReadTransaction};
use std::collections::BTreeSet;
use std::io::{BufRead, Read, Write};

const MAGIC: &[u8] = b"-- eggdb-dump 1, sql ";
const MAGIC_END: &[u8] = b" bytes\n";
const MOST_HEADER_BYTES: usize = 64;
const BATCH_ENTRIES: u64 = 10_000;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct EntriesLoaded {
    pub(super) entries: u64,
    pub(super) collections: u64,
}

pub(super) fn is_whole_dump(head: &[u8]) -> bool {
    head.starts_with(MAGIC)
}

fn header_line(sql_bytes: usize) -> Vec<u8> {
    let mut line = MAGIC.to_vec();
    line.extend_from_slice(sql_bytes.to_string().as_bytes());
    line.extend_from_slice(MAGIC_END);
    line
}

pub(super) fn read_header_line(input: &mut dyn BufRead) -> Result<u64, String> {
    let mut line = Vec::new();
    Read::take(&mut *input, MOST_HEADER_BYTES as u64)
        .read_until(b'\n', &mut line)
        .map_err(|error| format!("cannot read the dump's first line: {error}"))?;
    line.strip_prefix(MAGIC)
        .and_then(|rest| rest.strip_suffix(MAGIC_END))
        .and_then(|digits| std::str::from_utf8(digits).ok())
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|digits| digits.parse().ok())
        .ok_or_else(|| "the dump's first line is not \"-- eggdb-dump 1, sql <n> bytes\"".to_owned())
}

fn write_entries(
    transaction: &ReadTransaction<'_>,
    header: &Header,
    out: &mut dyn Write,
) -> Result<EntriesLoaded, DumpFailure> {
    jsonl::write_header(out, header).map_err(DumpFailure::Write)?;
    let mut written = EntriesLoaded::default();
    let mut scan = transaction.scan(..);
    while let Some((key, value)) = scan.next_entry().map_err(DumpFailure::Read)? {
        if eggdb_sql::is_sql_key(key) {
            continue;
        }
        jsonl::write_entry_in(out, None, key, value).map_err(DumpFailure::Write)?;
        written.entries += 1;
    }
    for name in transaction.collection_names().map_err(DumpFailure::Read)? {
        let collection = transaction.collection(&name).map_err(DumpFailure::Read)?;
        written.collections += 1;
        let mut any = false;
        let mut scan = collection.scan(..);
        while let Some((key, value)) = scan.next_entry().map_err(DumpFailure::Read)? {
            jsonl::write_entry_in(out, Some(&name), key, value).map_err(DumpFailure::Write)?;
            written.entries += 1;
            any = true;
        }
        if !any {
            jsonl::write_collection_line(out, &name).map_err(DumpFailure::Write)?;
        }
    }
    jsonl::write_trailer(out, written.entries).map_err(DumpFailure::Write)?;
    out.flush().map_err(DumpFailure::Write)?;
    Ok(written)
}

enum DumpFailure {
    Read(Error),
    Write(std::io::Error),
}

fn sql_part(db: &Database) -> Result<(Vec<u8>, eggdb_sql::DumpReport), eggdb_sql::SqlError> {
    let store = DatabaseStore(db);
    let mut session = eggdb_sql::Session::new(&store);
    let mut written = Vec::new();
    let outcome = eggdb_sql::dump_sql(&mut session, eggdb_sql::Dialect::PostgreSql, &mut written);
    let _ = session.close();
    outcome.map(|report| (written, report))
}

pub(super) fn load_entries(db: &Database, input: &mut dyn Read) -> Result<EntriesLoaded, String> {
    let mut reader = ExportReader::new(input);
    let mut loaded = EntriesLoaded::default();
    let mut ensured: BTreeSet<String> = BTreeSet::new();
    let mut transaction = db.begin_write().map_err(|error| error.to_string())?;
    let mut pending: u64 = 0;
    loop {
        let item = reader.read_item().map_err(|error| match error {
            ReadError::Input(error) => format!("cannot read the dump's entries: {error}"),
            ReadError::Invalid { line, problem } => format!("line {line} of the dump's entries: {problem}"),
        })?;
        match item {
            Item::End => break,
            Item::Collection => {
                let name = reader.current_collection().unwrap_or_default().to_owned();
                if ensured.insert(name.clone()) {
                    transaction.create_collection(&name).map_err(|error| error.to_string())?;
                    loaded.collections += 1;
                }
            }
            Item::Entry => {
                let (key, value) = reader.current_entry();
                let stored = match reader.current_collection() {
                    None => transaction.put(key, value),
                    Some(name) => {
                        if ensured.insert(name.to_owned()) {
                            transaction.create_collection(name).map_err(|error| error.to_string())?;
                            loaded.collections += 1;
                        }
                        transaction.collection(name).and_then(|mut collection| collection.put(key, value))
                    }
                };
                stored.map_err(|error| format!("line {}: {error}", reader.line()))?;
                loaded.entries += 1;
                pending += 1;
                if pending >= BATCH_ENTRIES {
                    transaction.commit().map_err(|error| error.to_string())?;
                    transaction = db.begin_write().map_err(|error| error.to_string())?;
                    pending = 0;
                }
            }
        }
    }
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(loaded)
}

impl Session<'_> {
    pub(super) fn export_whole_dump(&mut self) -> Result<Exit, Failure> {
        let target = match self.request.export_file.clone() {
            Some(ExportFile::Path(path)) => Some(path),
            _ => None,
        };
        if let Some(path) = &target
            && std::fs::symlink_metadata(path).is_ok()
        {
            return Err(Failure::coded(
                "already_exists",
                format!(
                    "{} exists; eggdb export never replaces a file",
                    render::text(path.display().to_string().as_bytes())
                ),
                Exit::DatabaseError,
            ));
        }
        let db = self.open(Access::ReadOnly, None)?;
        let sql = match sql_part(&db) {
            Ok(sql) => sql,
            Err(error) => {
                self.close(db)?;
                print_error(&mut self.messages, &error, "export --dump");
                return Ok(exit_for(&error));
            }
        };
        let header = Header {
            page_size: Some(db.page_size()),
            eggdb: Some(eggdb::VERSION.to_owned()),
            exported_at: None,
            collections: true,
        };
        let (sql_text, report) = sql;
        let written = match &target {
            Some(path) => self.whole_dump_to_file(&db, &header, &sql_text, path),
            None => {
                let file = self.file.clone();
                let out = self.out.sink()?;
                write_whole_dump(&db, &header, &sql_text, out).map_err(|failure| match failure {
                    DumpFailure::Read(error) => operation_failure(&error, &format!("cannot export {file}")),
                    DumpFailure::Write(error) => output_failure(error),
                })
            }
        };
        self.close(db)?;
        let entries = written?;
        let summary = format!(
            "dumped {} tables, {} rows, {} and {} collections\n",
            render::count(report.tables),
            render::count(report.rows),
            render::entries(entries.entries),
            render::count(entries.collections)
        );
        match (&target, self.format) {
            (Some(_), Format::Json) => self.out.text(
                &JsonObject::new()
                    .number("tables", report.tables)
                    .number("rows", report.rows)
                    .number("entries", entries.entries)
                    .number("collections", entries.collections)
                    .line(),
            ),
            (Some(_), _) => self.out.text(&summary),
            (None, _) => self.messages.text(&summary),
        }
        Ok(Exit::Done)
    }

    fn whole_dump_to_file(
        &mut self,
        db: &Database,
        header: &Header,
        sql_text: &[u8],
        path: &std::path::Path,
    ) -> Result<EntriesLoaded, Failure> {
        let shown = render::text(path.display().to_string().as_bytes());
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| Failure::database(format!("cannot write {shown}: {error}"), Exit::DatabaseError))?;
        let mut out = std::io::BufWriter::new(file);
        let written = write_whole_dump(db, header, sql_text, &mut out).and_then(|entries| {
            out.into_inner()
                .map_err(|error| DumpFailure::Write(error.into_error()))
                .and_then(|file| file.sync_all().map_err(DumpFailure::Write))
                .map(|()| entries)
        });
        written.map_err(|failure| {
            let _ = std::fs::remove_file(path);
            match failure {
                DumpFailure::Read(error) => operation_failure(&error, &format!("cannot export {}", self.file)),
                DumpFailure::Write(error) => {
                    Failure::database(format!("cannot write {shown}: {error}"), Exit::DatabaseError)
                }
            }
        })
    }
}

fn write_whole_dump(
    db: &Database,
    header: &Header,
    sql_text: &[u8],
    out: &mut dyn Write,
) -> Result<EntriesLoaded, DumpFailure> {
    out.write_all(&header_line(sql_text.len())).map_err(DumpFailure::Write)?;
    out.write_all(sql_text).map_err(DumpFailure::Write)?;
    let transaction = db.begin_read().map_err(DumpFailure::Read)?;
    write_entries(&transaction, header, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_line_names_the_sql_part_s_length() {
        let line = header_line(1234);
        assert_eq!(line, b"-- eggdb-dump 1, sql 1234 bytes\n");
        assert!(is_whole_dump(&line));
        let mut input = std::io::Cursor::new([line.as_slice(), b"SELECT 1;"].concat());
        assert_eq!(read_header_line(&mut input), Ok(1234));
        let mut rest = String::new();
        input.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "SELECT 1;");
        for bad in [&b"-- eggdb-dump 1, sql  bytes\n"[..], b"-- eggdb-dump 1, sql 12x bytes\n", b"-- eggdb-dump 1, sql 12"] {
            assert!(read_header_line(&mut std::io::Cursor::new(bad)).is_err());
        }
    }
}
