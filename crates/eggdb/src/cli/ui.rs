use super::commands::sql::{error_lines, result_cells};
use super::render::{self, TimeStyle};
use super::sql_store::DatabaseStore;
use eggdb::{CheckReport, CommitStamp, Database, Options, Scan};
use eggdb_ui::{
    Change, CheckSummary, CountStep, ErrorKind, KeyRow, Source, SourceError, SqlFailure, SqlRun,
    SqlTable, Start, Summary, WriterBusy,
};
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const SECONDS_PER_DAY: u64 = 86_400;
const LONGEST_COUNT_STEP: Duration = Duration::from_millis(20);
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

type CheckThread = JoinHandle<eggdb::Result<CheckReport>>;

pub(crate) struct DatabaseSource {
    db: Option<Arc<Database>>,
    name: String,
    path: PathBuf,
    options: Options,
    check: Option<CheckThread>,
}

impl DatabaseSource {
    pub(crate) fn new(db: Database, name: String, options: Options) -> Self {
        Self {
            path: db.path().to_path_buf(),
            db: Some(Arc::new(db)),
            name,
            options,
            check: None,
        }
    }

    pub(crate) fn into_database(self) -> Option<Database> {
        if self.check.is_some() {
            return None;
        }
        Arc::into_inner(self.db?)
    }

    fn db(&self) -> Result<&Database, SourceError> {
        self.db.as_deref().ok_or_else(|| {
            SourceError::new(
                ErrorKind::NeedsReopen,
                format!("{} is closed; reopen it to go on", self.name),
            )
        })
    }
}

fn source_error(error: &eggdb::Error) -> SourceError {
    use eggdb::ErrorKind as Engine;
    let kind = match error.kind() {
        Engine::NotFound => ErrorKind::NotFound,
        Engine::PermissionDenied => ErrorKind::PermissionDenied,
        Engine::Locked => ErrorKind::Locked,
        Engine::ReadOnly => ErrorKind::ReadOnly,
        Engine::NotADatabase => ErrorKind::NotADatabase,
        Engine::Incompatible => ErrorKind::Incompatible,
        Engine::Corrupt => ErrorKind::Corrupt,
        Engine::Io => ErrorKind::Io,
        Engine::OutOfMemory => ErrorKind::OutOfMemory,
        Engine::TooLarge => ErrorKind::TooLarge,
        Engine::CacheFull => ErrorKind::CacheFull,
        Engine::WriterActive => ErrorKind::WriterActive,
        Engine::SnapshotTooOld => ErrorKind::SnapshotTooOld,
        Engine::TransactionFailed => ErrorKind::TransactionFailed,
        Engine::NeedsReopen => ErrorKind::NeedsReopen,
        Engine::DatabaseFull => ErrorKind::DatabaseFull,
        Engine::InvalidOption => ErrorKind::InvalidOption,
        _ => ErrorKind::Other,
    };
    SourceError::new(kind, error.to_string())
}

fn check_summary(report: &CheckReport) -> CheckSummary {
    CheckSummary {
        entries: report.entries,
        key_bytes: report.key_bytes,
        value_bytes: report.value_bytes,
        tree_height: report.tree_height,
        tree_pages: report.tree_pages,
        overflow_values: report.overflow_values,
        overflow_pages: report.overflow_pages,
        free_pages: report.free_pages,
        log_pages: report.log_pages,
        leaked_pages: report.leaked_pages,
    }
}

fn prefix_end(prefix: &[u8]) -> Bound<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xff {
            end.push(last + 1);
            return Bound::Excluded(end);
        }
    }
    Bound::Unbounded
}

fn below(key: &[u8], end: &Bound<Vec<u8>>) -> bool {
    match end {
        Bound::Excluded(end) => key < end.as_slice(),
        Bound::Included(end) => key <= end.as_slice(),
        Bound::Unbounded => true,
    }
}

type KeyBounds = (Bound<Vec<u8>>, Bound<Vec<u8>>);

fn range(prefix: &[u8], start: Start<'_>) -> Option<KeyBounds> {
    let end = prefix_end(prefix);
    let from_prefix = Bound::Included(prefix.to_vec());
    Some(match start {
        Start::First | Start::Last => (from_prefix, end),
        Start::After(key) if key >= prefix => (Bound::Excluded(key.to_vec()), end),
        Start::AtOrAfter(key) if key >= prefix => (Bound::Included(key.to_vec()), end),
        Start::After(_) | Start::AtOrAfter(_) => (from_prefix, end),
        Start::Before(key) if key <= prefix => return None,
        Start::Before(key) if below(key, &end) => (from_prefix, Bound::Excluded(key.to_vec())),
        Start::Before(_) => (from_prefix, end),
    })
}

fn clock_time(stamp: CommitStamp) -> Option<String> {
    if stamp == CommitStamp::NONE {
        return None;
    }
    let seconds = (stamp.unix_micros() / 1_000_000) % SECONDS_PER_DAY;
    Some(format!(
        "{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    ))
}

fn commit_detail(stamp: CommitStamp) -> Option<String> {
    (stamp != CommitStamp::NONE).then(|| {
        format!(
            "{} (stamp {stamp})",
            render::utc_time(stamp.unix_micros(), TimeStyle::Readable)
        )
    })
}

fn collect(scan: &mut Scan<'_>, descending: bool, limit: usize) -> eggdb::Result<Vec<KeyRow>> {
    let mut rows = Vec::with_capacity(limit.min(1024));
    while rows.len() < limit {
        let entry = if descending {
            scan.next_back_key_and_value_len()?
        } else {
            scan.next_key_and_value_len()?
        };
        let Some((key, value_bytes)) = entry else {
            break;
        };
        rows.push(KeyRow {
            key: key.to_vec(),
            value_bytes,
        });
    }
    Ok(rows)
}

fn apply(db: &Database, change: &Change, busy: WriterBusy) -> eggdb::Result<Option<CommitStamp>> {
    let mut transaction = match busy {
        WriterBusy::Fail => db.try_begin_write()?,
        WriterBusy::Wait => db.begin_write()?,
    };
    let changed = match change {
        Change::Put { key, value } => {
            transaction.put(key, value)?;
            true
        }
        Change::Delete { key } => transaction.delete(key)?,
    };
    if changed {
        transaction.commit().map(Some)
    } else {
        transaction.rollback().map(|()| None)
    }
}

impl Source for DatabaseSource {
    fn summary(&self) -> Result<Summary, SourceError> {
        let db = self.db()?;
        let stats = db.stats().map_err(|error| source_error(&error))?;
        Ok(Summary {
            name: self.name.clone(),
            file_bytes: stats.file_bytes,
            page_count: stats.page_count,
            free_pages: stats.free_pages,
            page_size: stats.page_size,
            last_commit: clock_time(stats.last_commit),
            last_commit_detail: commit_detail(stats.last_commit),
            read_only: db.is_read_only(),
            max_key_bytes: db.max_key_bytes(),
            max_value_bytes: db.max_value_bytes() as u64,
            cache_bytes: (stats.cache_pages as u64).saturating_mul(stats.page_size as u64),
            cached_pages: stats.cached_pages as u64,
            cache_hits: stats.cache_hits,
            cache_misses: stats.cache_misses,
            log_bytes: stats.log_bytes_since_checkpoint,
            write_timeout: WRITE_TIMEOUT,
        })
    }

    fn keys(
        &self,
        prefix: &[u8],
        start: Start<'_>,
        limit: usize,
    ) -> Result<Vec<KeyRow>, SourceError> {
        let Some(bounds) = range(prefix, start) else {
            return Ok(Vec::new());
        };
        let descending = matches!(start, Start::Last | Start::Before(_));
        self.db()?
            .read(|tx| collect(&mut tx.scan(bounds), descending, limit))
            .map_err(|error| source_error(&error))
    }

    fn value(&self, key: &[u8]) -> Result<Option<Vec<u8>>, SourceError> {
        self.db()?.get(key).map_err(|error| source_error(&error))
    }

    fn count(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u64,
    ) -> Result<CountStep, SourceError> {
        let start = after.map_or(Start::First, Start::After);
        let Some(bounds) = range(prefix, start) else {
            return Ok(CountStep {
                counted: 0,
                resume_after: None,
            });
        };
        self.db()?
            .read(|tx| {
                let mut scan = tx.scan(bounds);
                let mut counted = 0;
                let started = Instant::now();
                while let Some((key, _)) = scan.next_key_and_value_len()? {
                    counted += 1;
                    if counted == limit || started.elapsed() >= LONGEST_COUNT_STEP {
                        return Ok(CountStep {
                            counted,
                            resume_after: Some(key.to_vec()),
                        });
                    }
                }
                Ok(CountStep {
                    counted,
                    resume_after: None,
                })
            })
            .map_err(|error| source_error(&error))
    }

    fn write(&mut self, change: &Change, busy: WriterBusy) -> Result<String, SourceError> {
        match apply(self.db()?, change, busy) {
            Ok(Some(stamp)) => Ok(clock_time(stamp).unwrap_or_default()),
            Ok(None) => Err(SourceError::new(
                ErrorKind::Other,
                format!(
                    "{} is no longer in {}; nothing was committed. Press r to re-read",
                    render::text(change.key()),
                    self.name
                ),
            )),
            Err(error) => Err(source_error(&error)),
        }
    }

    fn start_check(&mut self) -> Result<(), SourceError> {
        let db = Arc::clone(self.db.as_ref().ok_or_else(|| {
            SourceError::new(ErrorKind::NeedsReopen, format!("{} is closed", self.name))
        })?);
        let thread = std::thread::Builder::new()
            .name("eggdb-ui-check".to_owned())
            .spawn(move || db.check())
            .map_err(|error| {
                SourceError::new(ErrorKind::Other, format!("cannot start the check: {error}"))
            })?;
        self.check = Some(thread);
        Ok(())
    }

    fn check_outcome(&mut self) -> Option<Result<CheckSummary, SourceError>> {
        if !self.check.as_ref()?.is_finished() {
            return None;
        }
        let thread = self.check.take()?;
        Some(match thread.join() {
            Ok(Ok(report)) => Ok(check_summary(&report)),
            Ok(Err(error)) => Err(source_error(&error)),
            Err(_) => Err(SourceError::new(
                ErrorKind::Other,
                "the check stopped before it finished",
            )),
        })
    }

    fn reopen(&mut self) -> Result<(), SourceError> {
        if self.check.is_some() {
            return Err(SourceError::new(
                ErrorKind::Other,
                "a check is still running; reopen once it ends",
            ));
        }
        drop(self.db.take());
        let db = Database::open_with(&self.path, self.options.clone())
            .map_err(|error| source_error(&error))?;
        self.db = Some(Arc::new(db));
        Ok(())
    }

    fn run_sql(&mut self, text: &str) -> SqlRun {
        let started = Instant::now();
        let mut run = SqlRun::default();
        let db = match self.db() {
            Ok(db) => db,
            Err(error) => {
                run.failure = Some(SqlFailure {
                    lines: vec![format!("ERROR:  {}", error.message)],
                    position: None,
                });
                return run;
            }
        };
        let store = DatabaseStore(db);
        let mut session = eggdb_sql::Session::new(&store);
        let outcome = session.execute(text, &mut |result| {
            run.tags.push(result.tag.clone());
            run.notices.extend(result.notices.iter().cloned());
            if let Some(cells) = result_cells(&result) {
                run.table = Some(SqlTable {
                    columns: cells.columns,
                    numeric: cells.numeric,
                    rows: cells.rows,
                });
            }
        });
        if let Err(error) = outcome {
            run.failure = Some(SqlFailure {
                lines: error_lines(&error, text),
                position: error.position().filter(|&position| position <= text.len()),
            });
        }
        match session.close() {
            Ok(true) => run.notices.push(
                "WARNING:  the run ended inside a transaction block (BEGIN without COMMIT); it was rolled back"
                    .to_owned(),
            ),
            Ok(false) => {}
            Err(error) if run.failure.is_none() => {
                run.failure = Some(SqlFailure {
                    lines: error_lines(&error, text),
                    position: None,
                });
            }
            Err(_) => {}
        }
        run.elapsed = started.elapsed();
        run
    }
}

#[cfg(test)]
mod tests {
    use super::{DatabaseSource, prefix_end, range};
    use eggdb::{Database, Durability, Options};
    use eggdb_ui::{Capabilities, FakeTerminal, Frame, Size, Source, Start, Ui, UiOptions};
    use std::ops::Bound;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    struct TempFile(PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn temp_path(name: &str) -> TempFile {
        let path =
            std::env::temp_dir().join(format!("eggdb-cli-ui-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        TempFile(path)
    }

    fn fill(db: &Database) {
        db.write(|tx| {
            for n in 1..=30 {
                tx.put(format!("user:{n:02}"), format!("User {n}"))?;
            }
            tx.put("doc", r#"{"a":[1,2]}"#)?;
            tx.put("big", vec![7u8; 20_000])
        })
        .unwrap_or_else(|error| panic!("write: {error}"));
    }

    fn database(name: &str) -> (TempFile, Database) {
        let file = temp_path(name);
        let db = Database::open(&file.0).unwrap_or_else(|error| panic!("open: {error}"));
        fill(&db);
        (file, db)
    }

    fn source_at(path: &Path, options: Options) -> DatabaseSource {
        let db = Database::open_with(path, options.clone())
            .unwrap_or_else(|error| panic!("open: {error}"));
        DatabaseSource::new(db, "t.db".to_owned(), options)
    }

    struct Screen {
        ui: Ui<DatabaseSource>,
        terminal: FakeTerminal,
        frame: Frame,
    }

    impl Screen {
        fn new(source: DatabaseSource) -> Self {
            let size = Size { cols: 80, rows: 24 };
            let mut screen = Self {
                ui: Ui::new(
                    source,
                    UiOptions {
                        capabilities: Capabilities::FULL,
                    },
                    size,
                ),
                terminal: FakeTerminal::new(size.cols, size.rows, Capabilities::FULL),
                frame: Frame::new(size),
            };
            screen.keys(&[]);
            screen
        }

        fn keys(&mut self, names: &[&str]) -> String {
            for name in names {
                assert!(self.terminal.press(name), "unknown key {name}");
                self.present();
            }
            self.present();
            self.terminal.screen()
        }

        fn present(&mut self) {
            self.ui
                .process(&mut self.terminal, &mut self.frame)
                .unwrap_or_else(|error| panic!("present: {error}"));
        }

        fn typed(&mut self, text: &str) -> String {
            self.terminal.type_str(text);
            self.keys(&[])
        }

        fn close(self) {
            if let Some(db) = self.ui.into_source().into_database() {
                db.close().unwrap_or_else(|error| panic!("close: {error}"));
            }
        }
    }

    #[test]
    fn prefix_ends_skip_trailing_ff_bytes() {
        assert_eq!(prefix_end(b"ab"), Bound::Excluded(b"ac".to_vec()));
        assert_eq!(prefix_end(b"a\xff"), Bound::Excluded(b"b".to_vec()));
        assert_eq!(prefix_end(b"\xff\xff"), Bound::Unbounded);
        assert_eq!(prefix_end(b""), Bound::Unbounded);
    }

    #[test]
    fn before_the_prefix_itself_is_empty() {
        assert!(range(b"user:", Start::Before(b"user:")).is_none());
        assert!(range(b"user:", Start::Before(b"a")).is_none());
    }

    #[test]
    fn the_source_scans_windows_both_ways_within_a_prefix() {
        let (file, db) = database("windows");
        let source = DatabaseSource::new(db, "t.db".to_owned(), Options::new());
        let names = |rows: Vec<eggdb_ui::KeyRow>| -> Vec<String> {
            rows.iter()
                .map(|row| String::from_utf8_lossy(&row.key).into_owned())
                .collect()
        };
        let first = source.keys(b"user:", Start::First, 2);
        assert_eq!(
            first.map(names),
            Ok(vec!["user:01".into(), "user:02".into()])
        );
        let last = source.keys(b"user:", Start::Last, 2);
        assert_eq!(
            last.map(names),
            Ok(vec!["user:30".into(), "user:29".into()])
        );
        let before = source.keys(b"user:", Start::Before(b"user:03"), 5);
        assert_eq!(
            before.map(names),
            Ok(vec!["user:02".into(), "user:01".into()])
        );
        let after = source.keys(b"", Start::After(b"doc"), 1);
        assert_eq!(after.map(names), Ok(vec!["user:01".into()]));
        let step = source.count(b"user:", None, 7);
        assert_eq!(step.as_ref().map(|s| s.counted), Ok(7));
        let rest = source.count(
            b"user:",
            step.ok().and_then(|s| s.resume_after).as_deref(),
            100,
        );
        assert_eq!(rest.map(|s| (s.counted, s.resume_after)), Ok((23, None)));
        assert_eq!(
            source.summary().map(|s| (s.name, s.read_only)),
            Ok(("t.db".to_owned(), false))
        );
        drop(source);
        drop(file);
    }

    #[test]
    fn listing_and_counting_keys_never_reads_a_large_value() {
        let (_file, db) = database("large");
        let large = 6 << 20;
        db.write(|tx| {
            tx.put("big:a", vec![1u8; large])?;
            tx.put("big:b", vec![2u8; large])?;
            tx.put("big:c", "small")
        })
        .unwrap_or_else(|error| panic!("write: {error}"));
        let source = DatabaseSource::new(db, "t.db".to_owned(), Options::new());
        let lookups = |source: &DatabaseSource| {
            source
                .summary()
                .map(|summary| summary.cache_hits + summary.cache_misses)
                .unwrap_or_else(|error| panic!("stats: {error:?}"))
        };
        let before = lookups(&source);
        let first = source.keys(b"big:", Start::First, 10);
        let last = source.keys(b"big:", Start::Last, 10);
        let counted = source.count(b"", None, 1000);
        let used = lookups(&source) - before;
        let sizes = |rows: Result<Vec<eggdb_ui::KeyRow>, eggdb_ui::SourceError>| -> Vec<u64> {
            rows.map(|rows| rows.iter().map(|row| row.value_bytes).collect())
                .unwrap_or_default()
        };
        assert_eq!(sizes(first), [large as u64, large as u64, 5]);
        assert_eq!(sizes(last), [5, large as u64, large as u64]);
        assert_eq!(counted.map(|step| step.counted), Ok(35));
        assert!(used < 64, "{used} page lookups");
    }

    #[test]
    fn the_ui_browses_a_real_database() {
        let (_file, db) = database("browse");
        let mut screen = Screen::new(DatabaseSource::new(db, "t.db".to_owned(), Options::new()));
        let shown = screen.keys(&[]);
        assert!(shown.contains("32 keys"), "{shown}");
        assert!(shown.contains("> big"), "{shown}");
        assert!(shown.contains("hex (auto)"), "{shown}");
        assert!(screen.keys(&["Down"]).contains("json (auto)"));
        let shown = screen.keys(&["End"]);
        assert!(shown.contains("> user:30"), "{shown}");
        assert!(shown.contains("13–32 of 32"), "{shown}");
        assert!(screen.typed("/user:2").contains("1–10 of 10"));
    }

    #[test]
    fn a_confirmed_edit_persists_after_reopening_and_a_cancelled_one_does_not() {
        let (file, db) = database("persist");
        db.close().unwrap_or_else(|error| panic!("close: {error}"));
        let mut screen = Screen::new(source_at(&file.0, Options::new()));
        screen.keys(&["Down", "e", "End", "Backspace", "Backspace"]);
        screen.typed(",3]}");
        let shown = screen.keys(&["Enter", "Enter"]);
        assert!(shown.contains("Committed doc (13 B) at "), "{shown}");
        screen.keys(&["Down", "e"]);
        screen.typed("cancelled ");
        screen.keys(&["Enter", "Esc", "Esc", "y"]);
        screen.keys(&["a"]);
        screen.typed("new\\x00key");
        screen.keys(&["Enter"]);
        screen.typed("\\xff\\x00 added");
        let shown = screen.keys(&["Enter", "Enter"]);
        assert!(shown.contains("> new\\x00key"), "{shown}");
        screen.keys(&["/"]);
        screen.typed("user:30");
        let shown = screen.keys(&["Enter", "d", "d"]);
        assert!(shown.contains("Deleted user:30 (7 B)"), "{shown}");
        screen.close();
        let reopened = Database::open(&file.0).unwrap_or_else(|error| panic!("open: {error}"));
        let get = |key: &[u8]| {
            reopened
                .get(key)
                .unwrap_or_else(|error| panic!("get: {error}"))
        };
        assert_eq!(get(b"doc"), Some(br#"{"a":[1,2,3]}"#.to_vec()));
        assert_eq!(get(b"user:01"), Some(b"User 1".to_vec()));
        assert_eq!(get(b"new\x00key"), Some(b"\xff\x00 added".to_vec()));
        assert_eq!(get(b"user:30"), None);
        let report = reopened
            .check()
            .unwrap_or_else(|error| panic!("check: {error}"));
        assert_eq!(report.entries, 32);
    }

    #[test]
    fn read_only_never_writes_the_file() {
        let (file, db) = database("read-only");
        db.close().unwrap_or_else(|error| panic!("close: {error}"));
        let before = std::fs::read(&file.0).unwrap_or_else(|error| panic!("read: {error}"));
        let mut screen = Screen::new(source_at(&file.0, Options::new().read_only(true)));
        assert!(screen.keys(&[]).contains(" RO"));
        for keys in [&["e"][..], &["a"], &["d"], &["Tab", "e"], &["Tab", "d"]] {
            let shown = screen.keys(keys);
            assert!(
                shown.contains("Opened read-only; start without --read-only to edit"),
                "{keys:?}: {shown}"
            );
        }
        screen.typed("vj/user:0");
        let shown = screen.keys(&["Enter", "c"]);
        assert!(shown.contains("Check  OK"), "{shown}");
        screen.keys(&["Enter", "r", "q"]);
        assert!(screen.ui.should_quit());
        screen.close();
        let after = std::fs::read(&file.0).unwrap_or_else(|error| panic!("read: {error}"));
        assert!(before == after, "a read-only session changed the file");
    }

    #[test]
    fn a_writer_on_another_thread_is_reported_and_waited_for() {
        let (_file, db) = database("busy");
        let source = DatabaseSource::new(db, "t.db".to_owned(), Options::new());
        let shared = source
            .db
            .as_ref()
            .map(Arc::clone)
            .unwrap_or_else(|| panic!("no database"));
        let (holding, held) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let writer = std::thread::spawn(move || {
            let transaction = shared
                .begin_write()
                .unwrap_or_else(|error| panic!("begin: {error}"));
            let _ = holding.send(());
            let _ = released.recv();
            transaction
                .rollback()
                .unwrap_or_else(|error| panic!("rollback: {error}"));
        });
        let _ = held.recv();
        let mut screen = Screen::new(source);
        screen.keys(&["Down", "e", "End"]);
        screen.typed(" ");
        let shown = screen.keys(&["Enter", "Enter"]);
        assert!(shown.contains("Writer busy"), "{shown}");
        assert!(shown.contains("Error (WriterActive):"), "{shown}");
        let held_for = Duration::from_millis(100);
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(held_for);
            let _ = release.send(());
        });
        let waited = Instant::now();
        let shown = screen.keys(&["r"]);
        assert!(waited.elapsed() >= held_for, "{:?}", waited.elapsed());
        assert!(shown.contains("Committed doc (12 B)"), "{shown}");
        let _ = releaser.join();
        let _ = writer.join();
        screen.close();
    }

    #[test]
    fn the_check_runs_on_a_worker_and_reports_damage() {
        let file = temp_path("damage");
        let db = Database::open(&file.0).unwrap_or_else(|error| panic!("open: {error}"));
        db.write(|tx| {
            for n in 0..3000u32 {
                tx.put(format!("key:{n:05}"), format!("value number {n}"))?;
            }
            Ok(())
        })
        .unwrap_or_else(|error| panic!("write: {error}"));
        let mut screen = Screen::new(DatabaseSource::new(db, "t.db".to_owned(), Options::new()));
        let shown = screen.keys(&["c"]);
        assert!(shown.contains("Check  OK"), "{shown}");
        assert!(shown.contains("3,000 keys"), "{shown}");
        screen.keys(&["Enter"]);
        screen.close();
        let mut bytes = std::fs::read(&file.0).unwrap_or_else(|error| panic!("read: {error}"));
        let middle = bytes.len() / 2;
        for byte in &mut bytes[middle..middle + 64] {
            *byte ^= 0x5a;
        }
        std::fs::write(&file.0, &bytes).unwrap_or_else(|error| panic!("write: {error}"));
        let mut screen = Screen::new(source_at(&file.0, Options::new().read_only(true)));
        let shown = screen.keys(&["c"]);
        assert!(shown.contains("Check  FAILED"), "{shown}");
        assert!(shown.contains("Error (Corrupt):"), "{shown}");
        screen.close();
    }

    #[test]
    fn random_edits_through_the_ui_agree_with_a_scan_of_the_file() {
        let file = temp_path("model");
        let options = Options::new().durability(Durability::Off);
        let mut screen = Screen::new(source_at(&file.0, options));
        let mut model = std::collections::BTreeMap::new();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut random = |below: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % below
        };
        for _ in 0..200 {
            let key = format!("k{}", random(25)).into_bytes();
            let value: Vec<u8> = (0..random(16))
                .map(|_| random(256) as u8)
                .filter(|&byte| byte != b'\n')
                .collect();
            let spelled: String = value.iter().map(|byte| format!("\\x{byte:02x}")).collect();
            let existing = model.contains_key(&key);
            let key_text = String::from_utf8_lossy(&key).into_owned();
            match random(10) {
                0..5 => {
                    screen.keys(&["a"]);
                    screen.typed(&key_text);
                    screen.keys(&["Enter"]);
                    screen.typed(&spelled);
                    screen.keys(&["Enter", "Enter"]);
                    model.insert(key, value);
                }
                _ if existing => {
                    screen.keys(&["/"]);
                    screen.typed(&key_text);
                    screen.keys(&["Enter", "d", "d", "/", "Esc"]);
                    model.remove(&key);
                }
                _ => continue,
            }
        }
        screen.close();
        let reopened = Database::open(&file.0).unwrap_or_else(|error| panic!("open: {error}"));
        let scanned: std::collections::BTreeMap<Vec<u8>, Vec<u8>> = reopened
            .read(|tx| tx.scan(..).collect())
            .unwrap_or_else(|error| panic!("scan: {error}"));
        assert!(scanned == model, "the file differs from the model");
    }

    #[test]
    fn the_query_screen_runs_sql_and_shows_the_result_grid() {
        let (file, db) = database("query");
        let mut screen = Screen::new(DatabaseSource::new(db, "t.db".to_owned(), Options::new()));
        let opened = screen.keys(&["s"]);
        assert!(opened.contains(" SQL  ln 1 col 1"), "{opened}");
        assert!(opened.contains(" Result"), "{opened}");
        screen.typed("CREATE TABLE items (id integer PRIMARY KEY, name text);");
        screen.keys(&["Enter"]);
        screen.typed("INSERT INTO items VALUES (1, 'egg'), (2, NULL);");
        let written = screen.keys(&["F5"]);
        assert!(written.contains("Result  no rows to show"), "{written}");
        assert!(written.contains("CREATE TABLE · INSERT 0 2"), "{written}");
        screen.keys(&["Ctrl-A", "Ctrl-K", "Up", "Ctrl-A", "Ctrl-K"]);
        screen.typed("SELECT id, name FROM items ORDER BY id;");
        let shown = screen.keys(&["Ctrl-R"]);
        assert!(shown.contains("Result  2 rows · "), "{shown}");
        assert!(shown.contains("rows 1–2/2 · cols 1–2/2"), "{shown}");
        assert!(shown.contains(" id | name"), "{shown}");
        assert!(shown.contains("  1 | egg"), "{shown}");
        assert!(shown.contains("(2 rows)"), "{shown}");
        let failed = {
            screen.keys(&["Ctrl-A", "Ctrl-K"]);
            screen.typed("SELECT nope FROM items;");
            screen.keys(&["F5"])
        };
        assert!(failed.contains("Result  ERROR"), "{failed}");
        assert!(
            failed.contains("ERROR:  column \"nope\" does not exist"),
            "{failed}"
        );
        assert!(failed.contains("SQLSTATE:  42703"), "{failed}");
        let recalled = screen.keys(&["Ctrl-P"]);
        assert!(
            recalled.contains("SELECT id, name FROM items ORDER BY id;"),
            "{recalled}"
        );
        let left_open = {
            screen.keys(&["Ctrl-A", "Ctrl-K"]);
            screen.typed("BEGIN; INSERT INTO items VALUES (3, 'milk');");
            screen.keys(&["F5"])
        };
        assert!(
            left_open.contains("BEGIN · INSERT 0 1 · WARNING:"),
            "{left_open}"
        );
        let back = screen.keys(&["Esc"]);
        assert!(back.contains("s sql"), "{back}");
        let again = screen.keys(&["s"]);
        assert!(again.contains("BEGIN; INSERT INTO items"), "{again}");
        screen.close();
        let db = Database::open(&file.0).unwrap_or_else(|error| panic!("open: {error}"));
        let store = super::DatabaseStore(&db);
        let mut session = eggdb_sql::Session::new(&store);
        let mut rows = 0;
        session
            .execute("SELECT * FROM items", &mut |result| {
                rows = result.rows.len()
            })
            .unwrap_or_else(|error| panic!("select: {error}"));
        assert_eq!(rows, 2);
    }

    #[test]
    fn the_query_grid_scrolls_rows_and_columns() {
        let (_file, db) = database("grid");
        let mut screen = Screen::new(DatabaseSource::new(db, "t.db".to_owned(), Options::new()));
        screen.keys(&["s"]);
        screen.typed("CREATE TABLE wide (a integer PRIMARY KEY, b text, c text, d text, e text);");
        screen.keys(&["F5", "Ctrl-A", "Ctrl-K"]);
        let long = "x".repeat(40);
        let values: Vec<String> = (1..=40)
            .map(|n| format!("({n}, '{long}', '{long}', '{long}', 'end')"))
            .collect();
        screen.typed(&format!("INSERT INTO wide VALUES {};", values.join(", ")));
        screen.keys(&["F5", "Ctrl-A", "Ctrl-K"]);
        screen.typed("SELECT * FROM wide ORDER BY a;");
        let first = screen.keys(&["F5"]);
        assert!(first.contains("rows 1–13/40 · cols 1–2/5"), "{first}");
        let moved = screen.keys(&["Tab", "Down", "PgDn", "Right", "Right"]);
        assert!(moved.contains("rows 15–27/40 · cols 3–3/5"), "{moved}");
        let end = screen.keys(&["End", "Ctrl-End"]);
        assert!(end.contains("rows 28–40/40 · cols 5–5/5"), "{end}");
        assert!(end.contains("\n end\n"), "{end}");
        let top = screen.keys(&["Home", "Ctrl-Home"]);
        assert!(top.contains("rows 1–13/40 · cols 1–2/5"), "{top}");
        screen.close();
    }
}
