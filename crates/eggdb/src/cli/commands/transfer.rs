use super::{Access, Session, cache_for_value, open_failure, operation_failure};
use crate::cli::args::{CommandName, ConflictPolicy, ExportFile, Format};
use crate::cli::failure::{Exit, Failure, output_failure};
use crate::cli::jsonl::{self, ExportReader, Header, Item, ReadError};
use crate::cli::output::{Output, Style};
use crate::cli::render::{self, JsonObject, TimeStyle};
use eggdb::{
    Database, Durability, Error, ErrorKind, Options, ReadTransaction, Scan, WriteTransaction,
};
use eggdb_platform::{FileSystem, OsFileSystem};
use eggdb_storage::{BTree, PageSize};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const OUTPUT_BUFFER_BYTES: usize = 256 * 1024;
const BATCH_BYTES: usize = 4 << 20;
const BATCH_PAGE_BYTES: usize = 2 << 20;
const IMPORT_CACHE_BYTES: usize = 32 << 20;
const PROGRESS_EVERY: Duration = Duration::from_secs(1);
const SHOWN_KEY_BYTES: usize = 64;
const STAGING_SUFFIX: &str = ".importing";
const COLLECTION_KEY_PREFIX_BYTES: usize = 10;

impl Session<'_> {
    pub(super) fn export(&mut self) -> Result<Exit, Failure> {
        let db = self.open(Access::ReadOnly, None)?;
        let header = Header {
            page_size: Some(db.page_size()),
            eggdb: Some(eggdb::VERSION.to_owned()),
            exported_at: now(),
            collections: false,
        };
        let only = self.request.collection.clone();
        if let Some(name) = &only {
            db.begin_read()
                .and_then(|transaction| transaction.collection(name).map(drop))
                .map_err(|error| {
                    self.key_space_failure(&error, &format!("cannot export {}", self.file))
                })?;
        }
        let mut progress = Progress::new(self.stderr_is_terminal, "exported");
        let result = match self.request.export_file.clone() {
            Some(ExportFile::Path(path)) => {
                self.export_to_file(&db, &header, only.as_deref(), &path, &mut progress)
            }
            Some(ExportFile::Standard) | None => {
                self.export_to_stdout(&db, &header, only.as_deref(), &mut progress)
            }
        };
        progress.clear(&mut self.messages);
        result
    }

    fn export_to_stdout(
        &mut self,
        db: &Database,
        header: &Header,
        only: Option<&str>,
        progress: &mut Progress,
    ) -> Result<Exit, Failure> {
        let mut writer = BufWriter::with_capacity(OUTPUT_BUFFER_BYTES, self.out.sink()?);
        let written = write_export(db, header, only, &mut writer, progress, &mut self.messages);
        let exported = written.map_err(|failure| match failure {
            ExportFailure::Read(error) => {
                self.key_space_failure(&error, &format!("cannot export {}", self.file))
            }
            ExportFailure::Write(error) => output_failure(error),
            ExportFailure::SyncDirectory(error) => Failure::database(
                format!("cannot sync the output: {error}"),
                Exit::DatabaseError,
            ),
        })?;
        drop(writer);
        if self.stderr_is_terminal {
            progress.clear(&mut self.messages);
            self.messages.styled(
                Style::Dim,
                &format!(
                    "exported {} from {}",
                    render::entries(exported.entries),
                    self.file
                ),
            );
            self.messages.text("\n");
        }
        Ok(Exit::Done)
    }

    fn export_to_file(
        &mut self,
        db: &Database,
        header: &Header,
        only: Option<&str>,
        path: &Path,
        progress: &mut Progress,
    ) -> Result<Exit, Failure> {
        let out_display = path.display().to_string();
        let out = render::text(out_display.as_bytes());
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| {
                let message = if error.kind() == std::io::ErrorKind::AlreadyExists {
                    format!(
                        "{out} already exists; eggdb export never replaces a file. Choose another name, or remove it first."
                    )
                } else {
                    format!("cannot create {out}: {error}")
                };
                Failure::database(message, Exit::DatabaseError)
            })?;
        let mut writer = BufWriter::with_capacity(OUTPUT_BUFFER_BYTES, file);
        let written = write_export(db, header, only, &mut writer, progress, &mut self.messages)
            .and_then(|exported| {
                writer
                    .get_ref()
                    .sync_all()
                    .map(|()| exported)
                    .map_err(ExportFailure::Write)
            })
            .and_then(|exported| {
                OsFileSystem::new()
                    .sync_dir(directory_of(path))
                    .map(|()| exported)
                    .map_err(ExportFailure::SyncDirectory)
            });
        drop(writer);
        let exported = match written {
            Ok(exported) => exported,
            Err(failure) => {
                let _ = std::fs::remove_file(path);
                return Err(match failure {
                    ExportFailure::Read(error) => self.key_space_failure(
                        &error,
                        &format!("cannot export {}; {out} was removed", self.file),
                    ),
                    ExportFailure::Write(error) => Failure::database(
                        format!("cannot write {out}: {error}; the partial export was removed"),
                        Exit::DatabaseError,
                    ),
                    ExportFailure::SyncDirectory(error) => Failure::database(
                        format!(
                            "cannot sync the directory of {out}, so a power loss could lose the file: {error}; the export was removed"
                        ),
                        Exit::DatabaseError,
                    ),
                });
            }
        };
        progress.clear(&mut self.messages);
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("file", &self.file_display)
                    .string("out", &out_display)
                    .number("entries", exported.entries)
                    .number("collections", exported.collections)
                    .line(),
            ),
            _ => self.out.text(&format!(
                "exported {} from {} to {out}\n",
                render::entries(exported.entries),
                self.file
            )),
        }
        Ok(Exit::Done)
    }

    pub(super) fn import(&mut self) -> Result<Exit, Failure> {
        if let Some(source) = self.request.csv.source.clone() {
            return self.import_csv(source);
        }
        let usage = |message: String| Failure::usage(message, Some(CommandName::Import));
        let source = match &self.request.export_file {
            Some(ExportFile::Path(path)) => Some(path.clone()),
            Some(ExportFile::Standard) | None if self.stdin_is_terminal => {
                return Err(usage(
                    "import needs an export to read: give its file, or pipe it to stdin".to_owned(),
                ));
            }
            Some(ExportFile::Standard) | None => None,
        };
        let target_exists = std::fs::symlink_metadata(&self.request.file).is_ok();
        if target_exists && std::fs::metadata(&self.request.file).is_err() {
            return Err(Failure::database(
                format!(
                    "cannot import into {}: it is a symbolic link to a file that does not exist. Import into the path it should name, or remove the link.",
                    self.file
                ),
                Exit::DatabaseError,
            ));
        }
        if !target_exists && !self.request.create {
            return Err(Failure::database(
                format!(
                    "cannot open {}: no such file. Pass --create to make it, or check the path.",
                    self.file
                ),
                Exit::DatabaseError,
            ));
        }
        if target_exists && self.request.page_size.is_some() {
            return Err(usage(format!(
                "--page-size is for a new file; {} exists and keeps its page size",
                self.file
            )));
        }
        let source_display = source
            .as_ref()
            .map_or("-".to_owned(), |path| path.display().to_string());
        let source_shown = match &source {
            Some(_) => render::text(source_display.as_bytes()),
            None => "stdin".to_owned(),
        };
        let mut input = match &source {
            Some(path) => open_source(path, &source_shown)?,
            None => Input::ReadOnce(Box::new(&mut *self.stdin)),
        };
        let mut import = Import {
            target: &self.request.file,
            file: &self.file,
            file_display: &self.file_display,
            source_shown: source_shown.clone(),
            policy: self.request.on_conflict,
            target_collection: self.request.collection.clone(),
            cache_bytes: self.request.cache_bytes,
            requested_page_size: self.request.page_size,
            staging_opened: false,
            messages: &mut self.messages,
            progress: Progress::new(
                self.stderr_is_terminal,
                if self.request.dry_run {
                    "checked"
                } else {
                    "imported"
                },
            ),
            tally: Tally::default(),
        };
        let result = if self.request.dry_run {
            import.check_only(&mut input, target_exists)
        } else if target_exists {
            import.fill_existing(&mut input)
        } else {
            import.build_new(&mut input)
        };
        import.progress.clear(import.messages);
        let tally = import.tally;
        result?;
        let report = ImportReport {
            file: &self.file,
            file_display: &self.file_display,
            source_display: &source_display,
            source_shown: &source_shown,
            tally: &tally,
            dry_run: self.request.dry_run,
        };
        match self.format {
            Format::Json => self.out.text(&report.json()),
            _ => self.out.text(&report.text()),
        }
        Ok(Exit::Done)
    }
}

enum ExportFailure {
    Read(Error),
    Write(std::io::Error),
    SyncDirectory(eggdb_platform::Error),
}

fn write_export(
    db: &Database,
    header: &Header,
    only: Option<&str>,
    out: &mut dyn Write,
    progress: &mut Progress,
    messages: &mut Output<'_>,
) -> Result<Exported, ExportFailure> {
    let transaction = db.begin_read().map_err(ExportFailure::Read)?;
    let mut written = Written {
        out: &mut *out,
        progress: &mut *progress,
        messages: &mut *messages,
        exported: Exported::default(),
    };
    match only {
        Some(name) => {
            let collection = transaction
                .collection(name)
                .map_err(ExportFailure::Read)?;
            jsonl::write_header(written.out, header).map_err(ExportFailure::Write)?;
            written.all(None, collection.scan(..))?;
        }
        None => {
            let names = transaction
                .collection_names()
                .map_err(ExportFailure::Read)?;
            let header = Header {
                collections: !names.is_empty(),
                ..header.clone()
            };
            jsonl::write_header(written.out, &header).map_err(ExportFailure::Write)?;
            written.all(None, transaction.scan(..))?;
            for name in &names {
                let collection = transaction
                    .collection(name)
                    .map_err(ExportFailure::Read)?;
                written.exported.collections += 1;
                if !written.all(Some(name), collection.scan(..))? {
                    jsonl::write_collection_line(written.out, name)
                        .map_err(ExportFailure::Write)?;
                }
            }
        }
    }
    let exported = written.exported;
    jsonl::write_trailer(out, exported.entries).map_err(ExportFailure::Write)?;
    out.flush().map_err(ExportFailure::Write)?;
    Ok(exported)
}

#[derive(Debug, Default, Clone, Copy)]
struct Exported {
    entries: u64,
    collections: u64,
}

struct Written<'w, 'o> {
    out: &'w mut dyn Write,
    progress: &'w mut Progress,
    messages: &'w mut Output<'o>,
    exported: Exported,
}

impl Written<'_, '_> {
    fn all(&mut self, collection: Option<&str>, mut scan: Scan<'_>) -> Result<bool, ExportFailure> {
        let mut any = false;
        while let Some((key, value)) = scan.next_entry().map_err(ExportFailure::Read)? {
            jsonl::write_entry_in(self.out, collection, key, value)
                .map_err(ExportFailure::Write)?;
            any = true;
            self.exported.entries += 1;
            self.progress.tick(self.messages, self.exported.entries);
        }
        Ok(any)
    }
}

fn now() -> Option<String> {
    let since_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?;
    let micros = u64::try_from(since_epoch.as_micros()).ok()?;
    Some(render::utc_time(micros, TimeStyle::Rfc3339))
}

struct Progress {
    enabled: bool,
    verb: &'static str,
    last: Instant,
    width: usize,
}

impl Progress {
    fn new(enabled: bool, verb: &'static str) -> Self {
        Self {
            enabled,
            verb,
            last: Instant::now(),
            width: 0,
        }
    }

    fn tick(&mut self, messages: &mut Output<'_>, entries: u64) {
        if !self.enabled || self.last.elapsed() < PROGRESS_EVERY {
            return;
        }
        self.last = Instant::now();
        let line = format!("{} {}…", self.verb, render::entries(entries));
        let width = line.chars().count();
        messages.text("\r");
        messages.text(&line);
        messages.text(&" ".repeat(self.width.saturating_sub(width)));
        self.width = width;
        let _ = messages.finish();
    }

    fn clear(&mut self, messages: &mut Output<'_>) {
        if self.width == 0 {
            return;
        }
        messages.text("\r");
        messages.text(&" ".repeat(self.width));
        messages.text("\r");
        self.width = 0;
        let _ = messages.finish();
    }
}

enum Input<'s> {
    RegularFile(std::fs::File),
    ReadOnce(Box<dyn Read + 's>),
}

fn open_source<'s>(path: &Path, shown: &str) -> Result<Input<'s>, Failure> {
    let unreadable = |error: std::io::Error| {
        Failure::usage(
            format!("cannot read {shown}: {error}"),
            Some(CommandName::Import),
        )
    };
    let file = std::fs::File::open(path).map_err(unreadable)?;
    let regular = file.metadata().map_err(unreadable)?.is_file();
    Ok(if regular {
        Input::RegularFile(file)
    } else {
        Input::ReadOnce(Box::new(file))
    })
}

fn directory_of(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

#[derive(Debug, Default)]
struct Tally {
    entries: u64,
    imported: u64,
    skipped: u64,
    committed: u64,
    largest_entry_bytes: usize,
    created_page_size: Option<usize>,
    collections_added: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Write,
    WriteAgain,
    Skip,
}

enum KeyLimit {
    Known(usize),
    FromHeader,
}

#[derive(Default)]
struct KeyOrder {
    previous_space: Option<String>,
    previous: Vec<u8>,
    previous_line: u64,
    started: bool,
}

impl KeyOrder {
    fn compare(&self, space: Option<&str>, key: &[u8]) -> std::cmp::Ordering {
        space
            .cmp(&self.previous_space.as_deref())
            .then_with(|| key.cmp(&self.previous))
    }

    fn remember(&mut self, space: Option<&str>, key: &[u8], line: u64) {
        if self.previous_space.as_deref() != space {
            self.previous_space = space.map(str::to_owned);
        }
        self.previous.clear();
        self.previous.extend_from_slice(key);
        self.previous_line = line;
        self.started = true;
    }
}

#[derive(Clone, Copy)]
struct EntryAt<'e> {
    line: u64,
    source: Option<&'e str>,
    target: Option<&'e str>,
    key: &'e [u8],
}

impl EntryAt<'_> {
    fn shown(&self) -> String {
        match self.target {
            Some(collection) => format!("{} in collection {collection}", shown_key(self.key)),
            None => shown_key(self.key),
        }
    }
}

struct WriteState {
    cache: usize,
    max_key: usize,
    check_existing: bool,
    order: KeyOrder,
    pending: bool,
    ensured: BTreeSet<String>,
}

#[derive(Default)]
struct Batch {
    entries: u64,
    keys: u64,
    bytes: usize,
}

enum WriteOutcome {
    Finished,
    NeedsCache(usize),
}

struct Import<'s, 'a> {
    target: &'s Path,
    file: &'s str,
    file_display: &'s str,
    source_shown: String,
    policy: ConflictPolicy,
    target_collection: Option<String>,
    cache_bytes: Option<usize>,
    requested_page_size: Option<usize>,
    staging_opened: bool,
    messages: &'s mut Output<'a>,
    progress: Progress,
    tally: Tally,
}

impl Import<'_, '_> {
    fn check_only(&mut self, input: &mut Input<'_>, target_exists: bool) -> Result<(), Failure> {
        if !target_exists {
            self.check_pass(input, KeyLimit::FromHeader, None)?;
            return Ok(());
        }
        let options = Options::new()
            .read_only(true)
            .create_if_missing(false)
            .cache_bytes(self.cache_bytes.unwrap_or(IMPORT_CACHE_BYTES));
        let db = Database::open_with(self.target, options).map_err(|error| {
            open_failure(self.file, self.file_display, &error, Access::ReadOnly)
        })?;
        let view = self.view_if_not_empty(&db)?;
        self.check_pass(input, KeyLimit::Known(db.max_key_bytes()), view.as_ref())?;
        Ok(())
    }

    fn fill_existing(&mut self, input: &mut Input<'_>) -> Result<(), Failure> {
        let fixed_cache = self.cache_bytes.is_some();
        let mut cache = self.cache_bytes.unwrap_or(IMPORT_CACHE_BYTES);
        let target = self.target.to_path_buf();
        let mut db = self.open(&target, cache, None)?;
        let check_existing = self.view_if_not_empty(&db)?.is_some();
        let max_key = db.max_key_bytes();
        if let Input::RegularFile(_) = input {
            let view = if check_existing {
                Some(
                    db.begin_read()
                        .map_err(|error| self.database_failure(&error))?,
                )
            } else {
                None
            };
            let checked = self.check_pass(input, KeyLimit::Known(max_key), view.as_ref());
            drop(view);
            if let Err(failure) = checked {
                let _ = db.close();
                return Err(with_note(failure, "Nothing was imported."));
            }
            let needed = cache_for_entry(self.tally.largest_entry_bytes);
            if !fixed_cache && needed > cache {
                self.close(db)?;
                cache = needed;
                db = self.open(&target, cache, None)?;
            }
            self.tally = Tally::default();
        }
        let written = self.with_reader(input, |import, reader| {
            reader
                .read_header()
                .map_err(|error| import.read_failure(error))?;
            import.write_pass(reader, &target, db, cache, max_key, check_existing)
        });
        written.map_err(|failure| {
            let note = match self.tally.committed {
                0 => "Nothing was imported.".to_owned(),
                committed => format!(
                    "The {} before it were committed; after fixing the export, import it again with --on-conflict skip.",
                    render::entries(committed)
                ),
            };
            with_note(failure, &note)
        })
    }

    fn build_new(&mut self, input: &mut Input<'_>) -> Result<(), Failure> {
        let staging = staging_path(self.target);
        let staging_shown = render::text(staging.display().to_string().as_bytes());
        if std::fs::symlink_metadata(&staging).is_ok() {
            return Err(Failure::database(
                format!(
                    "cannot import into {}: {staging_shown} exists, left by an import that did not finish or one still running. Remove it and import again.",
                    self.file
                ),
                Exit::DatabaseError,
            ));
        }
        let fixed_cache = self.cache_bytes.is_some();
        let mut cache = self.cache_bytes.unwrap_or(IMPORT_CACHE_BYTES);
        let not_created = format!("Nothing was imported and {} was not created.", self.file);
        if let Input::RegularFile(_) = input {
            self.check_pass(input, KeyLimit::FromHeader, None)
                .map_err(|failure| with_note(failure, &not_created))?;
            if !fixed_cache {
                cache = cache.max(cache_for_entry(self.tally.largest_entry_bytes));
            }
            self.tally = Tally::default();
        }
        let written = self.with_reader(input, |import, reader| {
            let header = reader
                .read_header()
                .map_err(|error| import.read_failure(error))?;
            let page_size = import.new_page_size(&header);
            let db = import.open(&staging, cache, Some(page_size))?;
            import.staging_opened = true;
            import.tally.created_page_size = Some(page_size);
            let max_key = db.max_key_bytes();
            import.write_pass(reader, &staging, db, cache, max_key, false)
        });
        if let Err(failure) = written {
            if self.staging_opened {
                let _ = std::fs::remove_file(&staging);
            }
            return Err(with_note(failure, &not_created));
        }
        self.move_into_place(&staging, &staging_shown)
    }

    fn move_into_place(&mut self, staging: &Path, staging_shown: &str) -> Result<(), Failure> {
        if std::fs::symlink_metadata(self.target).is_ok() {
            return Err(Failure::database(
                format!(
                    "cannot import into {}: a file appeared there while the import ran. The imported database is {staging_shown}; rename it yourself.",
                    self.file
                ),
                Exit::DatabaseError,
            ));
        }
        let file_system = OsFileSystem::new();
        file_system.rename(staging, self.target).map_err(|error| {
            Failure::database(
                format!(
                    "cannot rename {staging_shown} to {}: {error}. The imported database is {staging_shown}.",
                    self.file
                ),
                Exit::DatabaseError,
            )
        })?;
        file_system.sync_dir(directory_of(self.target)).map_err(|error| {
            Failure::database(
                format!(
                    "imported into {}, but cannot sync its directory, so a power loss could still undo the rename: {error}",
                    self.file
                ),
                Exit::DatabaseError,
            )
        })
    }

    fn with_reader<T>(
        &mut self,
        input: &mut Input<'_>,
        read: impl FnOnce(&mut Self, &mut ExportReader<'_>) -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        match input {
            Input::RegularFile(file) => {
                file.rewind().map_err(|error| {
                    Failure::database(
                        format!("cannot read {}: {error}", self.source_shown),
                        Exit::DatabaseError,
                    )
                })?;
                let mut reader = ExportReader::new(file);
                read(self, &mut reader)
            }
            Input::ReadOnce(stream) => {
                let mut reader = ExportReader::new(&mut **stream);
                read(self, &mut reader)
            }
        }
    }

    fn check_pass(
        &mut self,
        input: &mut Input<'_>,
        limit: KeyLimit,
        existing: Option<&ReadTransaction<'_>>,
    ) -> Result<(), Failure> {
        self.with_reader(input, |import, reader| {
            let header = reader
                .read_header()
                .map_err(|error| import.read_failure(error))?;
            let max_key = match limit {
                KeyLimit::Known(max_key) => max_key,
                KeyLimit::FromHeader => {
                    let page_size = import.new_page_size(&header);
                    import.tally.created_page_size = Some(page_size);
                    max_key_bytes(page_size)
                }
            };
            let mut order = KeyOrder::default();
            let mut seen = BTreeSet::new();
            let flatten_into = import.target_collection.clone();
            loop {
                let item = reader
                    .read_item()
                    .map_err(|error| import.read_failure(error))?;
                let line = reader.line();
                match item {
                    Item::End => break,
                    Item::Collection => {
                        if flatten_into.is_none() {
                            let name = reader.current_collection().unwrap_or_default();
                            import.would_add(line, name, existing, &mut seen)?;
                        }
                        continue;
                    }
                    Item::Entry => {}
                }
                let (key, value) = reader.current_entry();
                let source = reader.current_collection();
                let entry = EntryAt {
                    line,
                    source,
                    target: flatten_into.as_deref().or(source),
                    key,
                };
                if let Some(name) = entry.target {
                    import.would_add(line, name, existing, &mut seen)?;
                }
                let decision = import.decide(entry, max_key, &mut order, || {
                    existing.map_or(Ok(false), |view| contains_in(view, entry.target, key))
                })?;
                import.count(decision);
                let entry_bytes = key.len().saturating_add(value.len());
                import.tally.largest_entry_bytes =
                    import.tally.largest_entry_bytes.max(entry_bytes);
                import.tally.entries = reader.entries();
                import.progress.tick(import.messages, reader.entries());
            }
            import.tally.entries = reader.entries();
            Ok(())
        })
    }

    fn would_add(
        &mut self,
        line: u64,
        name: &str,
        existing: Option<&ReadTransaction<'_>>,
        seen: &mut BTreeSet<String>,
    ) -> Result<(), Failure> {
        if seen.contains(name) {
            return Ok(());
        }
        seen.insert(name.to_owned());
        let exists = match existing.map(|view| view.collection(name)) {
            None => false,
            Some(Ok(_)) => true,
            Some(Err(error)) if error.kind() == ErrorKind::CollectionNotFound => false,
            Some(Err(error)) => return Err(self.collection_refused(line, name, &error)),
        };
        if !exists {
            self.tally.collections_added += 1;
        }
        Ok(())
    }

    fn ensure(
        &mut self,
        line: u64,
        open: &mut WriteTransaction<'_>,
        ensured: &mut BTreeSet<String>,
        name: &str,
    ) -> Result<(), Failure> {
        if ensured.contains(name) {
            return Ok(());
        }
        let created = open
            .create_collection(name)
            .map_err(|error| self.collection_refused(line, name, &error))?;
        if created {
            self.tally.collections_added += 1;
        }
        ensured.insert(name.to_owned());
        Ok(())
    }

    fn collection_refused(&self, line: u64, name: &str, error: &Error) -> Failure {
        match error.kind() {
            ErrorKind::InvalidCollectionName | ErrorKind::Corrupt | ErrorKind::Incompatible => {
                Failure::database(
                    format!(
                        "cannot import {}: line {line}: cannot add the collection {name} to {}: {error}",
                        self.source_shown, self.file
                    ),
                    Exit::for_error(error.kind()),
                )
            }
            _ => self.database_failure(error),
        }
    }

    fn write_pass(
        &mut self,
        reader: &mut ExportReader<'_>,
        path: &Path,
        mut db: Database,
        cache: usize,
        max_key: usize,
        check_existing: bool,
    ) -> Result<(), Failure> {
        let mut state = WriteState {
            cache,
            max_key,
            check_existing: check_existing || self.target_collection.is_some(),
            order: KeyOrder::default(),
            pending: false,
            ensured: BTreeSet::new(),
        };
        loop {
            match self.write_batches(reader, &db, &mut state) {
                Ok(WriteOutcome::Finished) => return self.close(db),
                Ok(WriteOutcome::NeedsCache(needed)) => {
                    self.close(db)?;
                    state.cache = needed;
                    db = self.open(path, needed, None)?;
                }
                Err(failure) => {
                    let _ = db.close();
                    return Err(failure);
                }
            }
        }
    }

    fn write_batches(
        &mut self,
        reader: &mut ExportReader<'_>,
        db: &Database,
        state: &mut WriteState,
    ) -> Result<WriteOutcome, Failure> {
        let fixed_cache = self.cache_bytes.is_some();
        let batch_entries = (BATCH_PAGE_BYTES / db.page_size()).max(1) as u64;
        let mut transaction: Option<WriteTransaction<'_>> = None;
        let mut batch = Batch::default();
        let flatten_into = self.target_collection.clone();
        loop {
            if !state.pending {
                let item = reader
                    .read_item()
                    .map_err(|error| self.read_failure(error))?;
                match item {
                    Item::End => break,
                    Item::Collection => {
                        if flatten_into.is_none() {
                            let line = reader.line();
                            let name = reader.current_collection().unwrap_or_default();
                            let open = match transaction.take() {
                                Some(open) => open,
                                None => db
                                    .begin_write()
                                    .map_err(|error| self.database_failure(&error))?,
                            };
                            let open = transaction.insert(open);
                            self.ensure(line, open, &mut state.ensured, name)?;
                        }
                        continue;
                    }
                    Item::Entry => state.pending = true,
                }
            }
            let (key, value) = reader.current_entry();
            let source = reader.current_collection();
            let entry = EntryAt {
                line: reader.line(),
                source,
                target: flatten_into.as_deref().or(source),
                key,
            };
            let entry_bytes = key.len().saturating_add(value.len());
            let needed = cache_for_entry(entry_bytes);
            if !fixed_cache && needed > state.cache {
                self.commit(transaction.take(), &mut batch)?;
                return Ok(WriteOutcome::NeedsCache(needed));
            }
            let batch_full = batch.entries >= batch_entries
                || batch.bytes.saturating_add(entry_bytes) > BATCH_BYTES;
            if batch.entries > 0 && batch_full {
                self.commit(transaction.take(), &mut batch)?;
            }
            let began = match transaction.take() {
                Some(open) => open,
                None => db
                    .begin_write()
                    .map_err(|error| self.database_failure(&error))?,
            };
            let open = transaction.insert(began);
            if let Some(name) = entry.target {
                self.ensure(entry.line, open, &mut state.ensured, name)?;
            }
            let check_existing = state.check_existing;
            let decision = self.decide(entry, state.max_key, &mut state.order, || {
                if check_existing {
                    contains_while_writing(open, entry.target, key)
                } else {
                    Ok(false)
                }
            })?;
            if decision != Decision::Skip {
                let stored = match entry.target {
                    None => open.put(key, value),
                    Some(name) => open
                        .collection(name)
                        .and_then(|mut collection| collection.put(key, value)),
                };
                stored.map_err(|error| {
                    operation_failure(
                        &error,
                        &format!(
                            "cannot import {}: line {}: cannot store {} in {}",
                            self.source_shown,
                            entry.line,
                            entry.shown(),
                            self.file
                        ),
                    )
                })?;
                batch.entries += 1;
                batch.keys += u64::from(decision == Decision::Write);
                batch.bytes = batch.bytes.saturating_add(entry_bytes);
            }
            self.count(decision);
            state.pending = false;
            self.tally.entries = reader.entries();
            self.progress.tick(self.messages, self.tally.imported);
            if batch.bytes >= BATCH_BYTES {
                self.commit(transaction.take(), &mut batch)?;
            }
        }
        self.commit(transaction.take(), &mut batch)?;
        Ok(WriteOutcome::Finished)
    }

    fn commit(
        &mut self,
        transaction: Option<WriteTransaction<'_>>,
        batch: &mut Batch,
    ) -> Result<(), Failure> {
        if let Some(transaction) = transaction {
            transaction.commit().map_err(|error| {
                operation_failure(&error, &format!("cannot commit to {}", self.file))
            })?;
            self.tally.committed += batch.keys;
        }
        *batch = Batch::default();
        Ok(())
    }

    fn decide(
        &self,
        entry: EntryAt<'_>,
        max_key: usize,
        order: &mut KeyOrder,
        exists: impl FnOnce() -> Result<bool, Error>,
    ) -> Result<Decision, Failure> {
        let EntryAt { line, source, key, .. } = entry;
        let max_key = match entry.target {
            Some(_) => max_key.saturating_sub(COLLECTION_KEY_PREFIX_BYTES),
            None => max_key,
        };
        if key.len() > max_key {
            let space = match entry.target {
                Some(_) => "keys in a collection",
                None => "keys",
            };
            return Err(self.invalid(
                line,
                format!(
                    "key {} is {} bytes; {} stores {space} up to {} bytes with its page size, and a larger page size (up to 64k, given when the file is created) stores longer ones",
                    entry.shown(),
                    render::count(key.len() as u64),
                    self.file,
                    render::count(max_key as u64)
                ),
            ));
        }
        if order.started {
            match order.compare(source, key) {
                std::cmp::Ordering::Less => {
                    return Err(self.invalid(
                        line,
                        format!(
                            "key {} comes after {} on line {}; an export lists its keys in ascending byte order, as eggdb export writes them",
                            entry.shown(),
                            shown_key(&order.previous),
                            order.previous_line
                        ),
                    ));
                }
                std::cmp::Ordering::Equal => {
                    let first_line = order.previous_line;
                    order.previous_line = line;
                    return match self.policy {
                        ConflictPolicy::Error => Err(self.invalid(
                            line,
                            format!(
                                "key {} appears twice, on lines {first_line} and {line}. Keep the last with --on-conflict overwrite, or the first with --on-conflict skip",
                                entry.shown()
                            ),
                        )),
                        ConflictPolicy::Overwrite => Ok(Decision::WriteAgain),
                        ConflictPolicy::Skip => Ok(Decision::Skip),
                    };
                }
                std::cmp::Ordering::Greater => {}
            }
        }
        order.remember(source, key, line);
        if self.policy == ConflictPolicy::Overwrite {
            return Ok(Decision::Write);
        }
        let exists = exists().map_err(|error| self.database_failure(&error))?;
        if !exists {
            return Ok(Decision::Write);
        }
        match self.policy {
            ConflictPolicy::Error => Err(self.invalid(
                line,
                format!(
                    "key {} is already in {}. Choose what happens to keys already there: --on-conflict overwrite or --on-conflict skip",
                    entry.shown(),
                    self.file
                ),
            )),
            ConflictPolicy::Overwrite => Ok(Decision::Write),
            ConflictPolicy::Skip => Ok(Decision::Skip),
        }
    }

    fn count(&mut self, decision: Decision) {
        match decision {
            Decision::Write => self.tally.imported += 1,
            Decision::WriteAgain => {}
            Decision::Skip => self.tally.skipped += 1,
        }
    }

    fn new_page_size(&self, header: &Header) -> usize {
        self.requested_page_size
            .or(header.page_size)
            .unwrap_or(PageSize::DEFAULT.bytes())
    }

    fn view_if_not_empty<'db>(
        &self,
        db: &'db Database,
    ) -> Result<Option<ReadTransaction<'db>>, Failure> {
        let view = db
            .begin_read()
            .map_err(|error| self.database_failure(&error))?;
        let empty = !contains(view.scan(..)).map_err(|error| self.database_failure(&error))?
            && view
                .collection_names()
                .map_err(|error| self.database_failure(&error))?
                .is_empty();
        Ok((!empty).then_some(view))
    }

    fn open(
        &self,
        path: &Path,
        cache: usize,
        new_page_size: Option<usize>,
    ) -> Result<Database, Failure> {
        let mut options = Options::new()
            .cache_bytes(cache)
            .durability(Durability::Normal)
            .create_if_missing(new_page_size.is_some());
        if let Some(page_size) = new_page_size {
            options = options.page_size_bytes(page_size);
        }
        let access = Access::Write {
            create: new_page_size.is_some(),
        };
        Database::open_with(path, options).map_err(|error| {
            let display = path.display().to_string();
            let shown = render::text(display.as_bytes());
            if error.kind() == ErrorKind::NotFound && new_page_size.is_none() {
                return Failure::database(
                    format!(
                        "cannot open {shown}: no such file. Pass --create to make it, or check the path."
                    ),
                    Exit::DatabaseError,
                );
            }
            open_failure(&shown, &display, &error, access)
        })
    }

    fn close(&self, db: Database) -> Result<(), Failure> {
        db.close()
            .map_err(|error| operation_failure(&error, &format!("cannot close {}", self.file)))
    }

    fn invalid(&self, line: u64, problem: String) -> Failure {
        Failure::database(
            format!(
                "cannot import {}: line {line}: {problem}",
                self.source_shown
            ),
            Exit::DatabaseError,
        )
    }

    fn read_failure(&self, error: ReadError) -> Failure {
        match error {
            ReadError::Input(error) => Failure::database(
                format!("cannot read {}: {error}", self.source_shown),
                Exit::DatabaseError,
            ),
            ReadError::Invalid { line, problem } => self.invalid(line, problem),
        }
    }

    fn database_failure(&self, error: &Error) -> Failure {
        operation_failure(error, &format!("cannot import into {}", self.file))
    }
}

struct ImportReport<'r> {
    file: &'r str,
    file_display: &'r str,
    source_display: &'r str,
    source_shown: &'r str,
    tally: &'r Tally,
    dry_run: bool,
}

impl ImportReport<'_> {
    fn json(&self) -> String {
        JsonObject::new()
            .string("file", self.file_display)
            .string("from", self.source_display)
            .number("entries", self.tally.entries)
            .number("imported", self.tally.imported)
            .number("skipped", self.tally.skipped)
            .boolean("created", self.tally.created_page_size.is_some())
            .boolean("dry_run", self.dry_run)
            .number("collections_added", self.tally.collections_added)
            .line()
    }

    fn text(&self) -> String {
        let tally = self.tally;
        let skipped = match tally.skipped {
            0 => String::new(),
            skipped => format!(
                "; {} skipped, their keys already there",
                render::count(skipped)
            ),
        };
        if self.dry_run {
            let creates = match tally.created_page_size {
                Some(page_size) => {
                    format!(" (created with {} pages)", render::page_size(page_size))
                }
                None => String::new(),
            };
            return format!(
                "checked {}: {} would be imported into {}{creates}{skipped}\n",
                self.source_shown,
                render::entries(tally.imported),
                self.file
            );
        }
        match tally.created_page_size {
            Some(page_size) => format!(
                "created {} ({} pages) and imported {}{skipped}\n",
                self.file,
                render::page_size(page_size),
                render::entries(tally.imported)
            ),
            None => format!(
                "imported {} into {}{skipped}\n",
                render::entries(tally.imported),
                self.file
            ),
        }
    }
}

fn contains(mut scan: Scan<'_>) -> Result<bool, Error> {
    Ok(scan.next_key_and_value_len()?.is_some())
}

fn contains_in(
    view: &ReadTransaction<'_>,
    collection: Option<&str>,
    key: &[u8],
) -> Result<bool, Error> {
    match collection {
        None => contains(view.scan(key..=key)),
        Some(name) => match view.collection(name) {
            Ok(collection) => contains(collection.scan(key..=key)),
            Err(error) if error.kind() == ErrorKind::CollectionNotFound => Ok(false),
            Err(error) => Err(error),
        },
    }
}

fn contains_while_writing(
    open: &mut WriteTransaction<'_>,
    collection: Option<&str>,
    key: &[u8],
) -> Result<bool, Error> {
    match collection {
        None => contains(open.scan(key..=key)),
        Some(name) => {
            let collection = open.collection(name)?;
            contains(collection.scan(key..=key))
        }
    }
}

fn cache_for_entry(entry_bytes: usize) -> usize {
    cache_for_value(entry_bytes).max(IMPORT_CACHE_BYTES)
}

fn max_key_bytes(page_size: usize) -> usize {
    let log2 = u8::try_from(page_size.trailing_zeros()).unwrap_or(PageSize::MIN_LOG2);
    PageSize::from_log2(log2).map_or(
        BTree::max_key_bytes(PageSize::DEFAULT),
        BTree::max_key_bytes,
    )
}

fn staging_path(target: &Path) -> PathBuf {
    let mut name = OsString::from(target.as_os_str());
    name.push(STAGING_SUFFIX);
    PathBuf::from(name)
}

fn shown_key(key: &[u8]) -> String {
    if key.is_empty() {
        return "\"\"".to_owned();
    }
    match render::text_cut(key, SHOWN_KEY_BYTES) {
        Some(cut) => format!("{cut}… ({} bytes)", render::count(key.len() as u64)),
        None => render::text(key),
    }
}

fn with_note(failure: Failure, note: &str) -> Failure {
    match failure {
        Failure::Database {
            message,
            try_command,
            exit,
        } => Failure::Database {
            message: format!("{}. {note}", message.trim_end_matches('.')),
            try_command,
            exit,
        },
        other => other,
    }
}
