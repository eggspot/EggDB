use super::{Access, Session, cache_for_value, open_failure, operation_failure};
use crate::cli::args::{CommandName, ConflictPolicy, ExportFile, Format};
use crate::cli::failure::{Exit, Failure, output_failure};
use crate::cli::jsonl::{self, ExportReader, Header, ReadError};
use crate::cli::output::{Output, Style};
use crate::cli::render::{self, JsonObject, TimeStyle};
use eggdb::{Database, Durability, Error, Options, ReadTransaction, Scan, WriteTransaction};
use eggdb_platform::{FileSystem, OsFileSystem};
use eggdb_storage::{BTree, PageSize};
use std::ffi::OsString;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const OUTPUT_BUFFER_BYTES: usize = 256 * 1024;
const BATCH_BYTES: usize = 4 << 20;
const BATCH_PAGE_BYTES: usize = 2 << 20;
const IMPORT_CACHE_BYTES: usize = 32 << 20;
const PROGRESS_EVERY: Duration = Duration::from_secs(1);
const SHOWN_KEY_BYTES: usize = 64;
const STAGING_SUFFIX: &str = ".importing";

impl Session<'_> {
    pub(super) fn export(&mut self) -> Result<Exit, Failure> {
        let db = self.open(Access::ReadOnly, None)?;
        let header = Header {
            page_size: Some(db.page_size()),
            eggdb: Some(eggdb::VERSION.to_owned()),
            exported_at: now(),
        };
        let mut progress = Progress::new(self.stderr_is_terminal, "exported");
        let result = match self.request.export_file.clone() {
            Some(ExportFile::Path(path)) => self.export_to_file(&db, &header, &path, &mut progress),
            Some(ExportFile::Standard) | None => self.export_to_stdout(&db, &header, &mut progress),
        };
        progress.clear(&mut self.messages);
        result
    }

    fn export_to_stdout(
        &mut self,
        db: &Database,
        header: &Header,
        progress: &mut Progress,
    ) -> Result<Exit, Failure> {
        let mut writer = BufWriter::with_capacity(OUTPUT_BUFFER_BYTES, self.out.sink()?);
        let written = write_export(db, header, &mut writer, progress, &mut self.messages);
        let entries = written.map_err(|failure| match failure {
            ExportFailure::Read(error) => {
                operation_failure(&error, &format!("cannot export {}", self.file))
            }
            ExportFailure::Write(error) => output_failure(error),
        })?;
        drop(writer);
        if self.stderr_is_terminal {
            progress.clear(&mut self.messages);
            self.messages.styled(
                Style::Dim,
                &format!("exported {} from {}", render::entries(entries), self.file),
            );
            self.messages.text("\n");
        }
        Ok(Exit::Done)
    }

    fn export_to_file(
        &mut self,
        db: &Database,
        header: &Header,
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
        let written = write_export(db, header, &mut writer, progress, &mut self.messages).and_then(
            |entries| {
                writer
                    .get_ref()
                    .sync_all()
                    .map(|()| entries)
                    .map_err(ExportFailure::Write)
            },
        );
        drop(writer);
        let entries = match written {
            Ok(entries) => entries,
            Err(failure) => {
                let _ = std::fs::remove_file(path);
                return Err(match failure {
                    ExportFailure::Read(error) => operation_failure(
                        &error,
                        &format!("cannot export {}; {out} was removed", self.file),
                    ),
                    ExportFailure::Write(error) => Failure::database(
                        format!("cannot write {out}: {error}; the partial export was removed"),
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
                    .number("entries", entries)
                    .line(),
            ),
            _ => self.out.text(&format!(
                "exported {} from {} to {out}\n",
                render::entries(entries),
                self.file
            )),
        }
        Ok(Exit::Done)
    }

    pub(super) fn import(&mut self) -> Result<Exit, Failure> {
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
        let mut import = Import {
            target: &self.request.file,
            file: &self.file,
            file_display: &self.file_display,
            source_shown: source_shown.clone(),
            policy: self.request.on_conflict,
            cache_bytes: self.request.cache_bytes,
            page_size: self.request.page_size,
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
        let mut input = match &source {
            Some(path) => Input::File(path),
            None => Input::Stdin(&mut *self.stdin),
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
}

fn write_export(
    db: &Database,
    header: &Header,
    out: &mut dyn Write,
    progress: &mut Progress,
    messages: &mut Output<'_>,
) -> Result<u64, ExportFailure> {
    let transaction = db.begin_read().map_err(ExportFailure::Read)?;
    jsonl::write_header(out, header).map_err(ExportFailure::Write)?;
    let mut scan = transaction.scan(..);
    let mut entries = 0u64;
    while let Some((key, value)) = scan.next_entry().map_err(ExportFailure::Read)? {
        jsonl::write_entry(out, key, value).map_err(ExportFailure::Write)?;
        entries += 1;
        progress.tick(messages, entries);
    }
    jsonl::write_trailer(out, entries).map_err(ExportFailure::Write)?;
    out.flush().map_err(ExportFailure::Write)?;
    Ok(entries)
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
    File(&'s Path),
    Stdin(&'s mut dyn Read),
}

#[derive(Debug, Default)]
struct Tally {
    entries: u64,
    imported: u64,
    skipped: u64,
    committed: u64,
    largest_entry_bytes: usize,
    created_page_size: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Write,
    Skip,
}

enum KeyLimit {
    Of(usize),
    FromHeader,
}

#[derive(Default)]
struct KeyOrder {
    previous: Vec<u8>,
    previous_line: u64,
    started: bool,
}

struct Writing {
    cache: usize,
    max_key: usize,
    check_existing: bool,
    order: KeyOrder,
    pending: bool,
}

#[derive(Default)]
struct Batch {
    entries: u64,
    bytes: usize,
}

enum Batches {
    Finished,
    NeedsCache(usize),
}

struct Import<'s, 'a> {
    target: &'s Path,
    file: &'s str,
    file_display: &'s str,
    source_shown: String,
    policy: ConflictPolicy,
    cache_bytes: Option<usize>,
    page_size: Option<usize>,
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
        self.check_pass(input, KeyLimit::Of(db.max_key_bytes()), view.as_ref())?;
        Ok(())
    }

    fn fill_existing(&mut self, input: &mut Input<'_>) -> Result<(), Failure> {
        let fixed_cache = self.cache_bytes.is_some();
        let mut cache = self.cache_bytes.unwrap_or(IMPORT_CACHE_BYTES);
        let target = self.target.to_path_buf();
        let mut db = self.open(&target, cache, None)?;
        let check_existing = self.view_if_not_empty(&db)?.is_some();
        let max_key = db.max_key_bytes();
        if let Input::File(_) = input {
            let view = if check_existing {
                Some(
                    db.begin_read()
                        .map_err(|error| self.database_failure(&error))?,
                )
            } else {
                None
            };
            let checked = self.check_pass(input, KeyLimit::Of(max_key), view.as_ref());
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
        if let Input::File(_) = input {
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
            import.tally.created_page_size = Some(page_size);
            let max_key = db.max_key_bytes();
            import.write_pass(reader, &staging, db, cache, max_key, false)
        });
        if let Err(failure) = written {
            let _ = std::fs::remove_file(&staging);
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
        let directory = self
            .target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        file_system.sync_dir(directory).map_err(|error| {
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
            Input::File(path) => {
                let mut file = self.open_source(path)?;
                let mut reader = ExportReader::new(&mut file);
                read(self, &mut reader)
            }
            Input::Stdin(stdin) => {
                let mut reader = ExportReader::new(&mut **stdin);
                read(self, &mut reader)
            }
        }
    }

    fn open_source(&self, path: &Path) -> Result<std::fs::File, Failure> {
        std::fs::File::open(path).map_err(|error| {
            Failure::usage(
                format!("cannot read {}: {error}", self.source_shown),
                Some(CommandName::Import),
            )
        })
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
                KeyLimit::Of(max_key) => max_key,
                KeyLimit::FromHeader => {
                    let page_size = import.new_page_size(&header);
                    import.tally.created_page_size = Some(page_size);
                    max_key_bytes(page_size)
                }
            };
            let mut order = KeyOrder::default();
            while reader
                .read_entry()
                .map_err(|error| import.read_failure(error))?
            {
                let (key, value) = reader.current_entry();
                let line = reader.line();
                let decision =
                    import.decide(line, key, max_key, &mut order, || match existing {
                        Some(view) => contains(view.scan(key..=key)),
                        None => Ok(false),
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

    fn write_pass(
        &mut self,
        reader: &mut ExportReader<'_>,
        path: &Path,
        mut db: Database,
        cache: usize,
        max_key: usize,
        check_existing: bool,
    ) -> Result<(), Failure> {
        let mut writing = Writing {
            cache,
            max_key,
            check_existing,
            order: KeyOrder::default(),
            pending: false,
        };
        loop {
            match self.write_batches(reader, &db, &mut writing) {
                Ok(Batches::Finished) => return self.close(db),
                Ok(Batches::NeedsCache(needed)) => {
                    self.close(db)?;
                    writing.cache = needed;
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
        writing: &mut Writing,
    ) -> Result<Batches, Failure> {
        let fixed_cache = self.cache_bytes.is_some();
        let batch_entries = (BATCH_PAGE_BYTES / db.page_size()).max(1) as u64;
        let mut transaction: Option<WriteTransaction<'_>> = None;
        let mut batch = Batch::default();
        loop {
            if !writing.pending {
                if !reader
                    .read_entry()
                    .map_err(|error| self.read_failure(error))?
                {
                    break;
                }
                writing.pending = true;
            }
            let (key, value) = reader.current_entry();
            let line = reader.line();
            let entry_bytes = key.len().saturating_add(value.len());
            let needed = cache_for_entry(entry_bytes);
            if !fixed_cache && needed > writing.cache {
                self.commit(transaction.take(), &mut batch)?;
                return Ok(Batches::NeedsCache(needed));
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
            let decision = self.decide(line, key, writing.max_key, &mut writing.order, || {
                if writing.check_existing {
                    contains(open.scan(key..=key))
                } else {
                    Ok(false)
                }
            })?;
            if decision == Decision::Write {
                open.put(key, value).map_err(|error| {
                    operation_failure(
                        &error,
                        &format!(
                            "cannot import {}: line {line}: cannot store {} in {}",
                            self.source_shown,
                            shown_key(key),
                            self.file
                        ),
                    )
                })?;
                batch.entries += 1;
                batch.bytes = batch.bytes.saturating_add(entry_bytes);
            }
            self.count(decision);
            writing.pending = false;
            self.tally.entries = reader.entries();
            self.progress.tick(self.messages, self.tally.imported);
            if batch.bytes >= BATCH_BYTES {
                self.commit(transaction.take(), &mut batch)?;
            }
        }
        self.commit(transaction.take(), &mut batch)?;
        Ok(Batches::Finished)
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
            self.tally.committed += batch.entries;
        }
        *batch = Batch::default();
        Ok(())
    }

    fn decide(
        &self,
        line: u64,
        key: &[u8],
        max_key: usize,
        order: &mut KeyOrder,
        exists: impl FnOnce() -> Result<bool, Error>,
    ) -> Result<Decision, Failure> {
        if key.len() > max_key {
            return Err(self.invalid(
                line,
                format!(
                    "key {} is {} bytes; {} stores keys up to {} bytes with its page size, and a larger page size (up to 64k, given when the file is created) stores longer ones",
                    shown_key(key),
                    render::count(key.len() as u64),
                    self.file,
                    render::count(max_key as u64)
                ),
            ));
        }
        if order.started {
            match key.cmp(&order.previous) {
                std::cmp::Ordering::Less => {
                    return Err(self.invalid(
                        line,
                        format!(
                            "key {} comes after {} on line {}; an export lists its keys in ascending byte order, as eggdb export writes them",
                            shown_key(key),
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
                                shown_key(key)
                            ),
                        )),
                        ConflictPolicy::Overwrite => Ok(Decision::Write),
                        ConflictPolicy::Skip => Ok(Decision::Skip),
                    };
                }
                std::cmp::Ordering::Greater => {}
            }
        }
        order.previous.clear();
        order.previous.extend_from_slice(key);
        order.previous_line = line;
        order.started = true;
        let exists = exists().map_err(|error| self.database_failure(&error))?;
        if !exists {
            return Ok(Decision::Write);
        }
        match self.policy {
            ConflictPolicy::Error => Err(self.invalid(
                line,
                format!(
                    "key {} is already in {}. Choose what happens to keys already there: --on-conflict overwrite or --on-conflict skip",
                    shown_key(key),
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
            Decision::Skip => self.tally.skipped += 1,
        }
    }

    fn new_page_size(&self, header: &Header) -> usize {
        self.page_size
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
        let empty = !contains(view.scan(..)).map_err(|error| self.database_failure(&error))?;
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
            open_failure(&render::text(display.as_bytes()), &display, &error, access)
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
