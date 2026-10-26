use eggdb::{CommitStamp, Database, Scan};
use eggdb_ui::{CountStep, KeyRow, Source, Start, Summary};
use std::ops::Bound;
use std::time::{Duration, Instant};

const SECONDS_PER_DAY: u64 = 86_400;
const LONGEST_COUNT_STEP: Duration = Duration::from_millis(20);

pub(crate) struct DatabaseSource {
    db: Database,
    name: String,
}

impl DatabaseSource {
    pub(crate) fn new(db: Database, name: String) -> Self {
        Self { db, name }
    }

    pub(crate) fn into_database(self) -> Database {
        self.db
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

impl Source for DatabaseSource {
    fn summary(&self) -> Result<Summary, String> {
        let stats = self.db.stats().map_err(|error| error.to_string())?;
        Ok(Summary {
            name: self.name.clone(),
            file_bytes: stats.file_bytes,
            page_count: stats.page_count,
            page_size: stats.page_size,
            last_commit: clock_time(stats.last_commit),
            read_only: self.db.is_read_only(),
            max_key_bytes: self.db.max_key_bytes(),
            max_value_bytes: self.db.max_value_bytes() as u64,
        })
    }

    fn keys(&self, prefix: &[u8], start: Start<'_>, limit: usize) -> Result<Vec<KeyRow>, String> {
        let Some(bounds) = range(prefix, start) else {
            return Ok(Vec::new());
        };
        let descending = matches!(start, Start::Last | Start::Before(_));
        self.db
            .read(|tx| collect(&mut tx.scan(bounds), descending, limit))
            .map_err(|error| error.to_string())
    }

    fn value(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.db.get(key).map_err(|error| error.to_string())
    }

    fn count(&self, prefix: &[u8], after: Option<&[u8]>, limit: u64) -> Result<CountStep, String> {
        let start = after.map_or(Start::First, Start::After);
        let Some(bounds) = range(prefix, start) else {
            return Ok(CountStep {
                counted: 0,
                resume_after: None,
            });
        };
        self.db
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
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{DatabaseSource, prefix_end, range};
    use eggdb::Database;
    use eggdb_ui::{Capabilities, FakeTerminal, Frame, Size, Source, Start, Ui, UiOptions};
    use std::ops::Bound;
    use std::path::PathBuf;

    struct TempFile(PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn database(name: &str) -> (TempFile, Database) {
        let path =
            std::env::temp_dir().join(format!("eggdb-cli-ui-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Database::open(&path).unwrap_or_else(|error| panic!("open: {error}"));
        db.write(|tx| {
            for n in 1..=30 {
                tx.put(format!("user:{n:02}"), format!("User {n}"))?;
            }
            tx.put("doc", r#"{"a":[1,2]}"#)?;
            tx.put("big", vec![7u8; 20_000])
        })
        .unwrap_or_else(|error| panic!("write: {error}"));
        (TempFile(path), db)
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
        let (_file, db) = database("windows");
        let source = DatabaseSource::new(db, "t.db".to_owned());
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
        let source = DatabaseSource::new(db, "t.db".to_owned());
        let lookups = |source: &DatabaseSource| {
            let stats = source
                .db
                .stats()
                .unwrap_or_else(|error| panic!("stats: {error}"));
            stats.cache_hits + stats.cache_misses
        };
        let before = lookups(&source);
        let first = source.keys(b"big:", Start::First, 10);
        let last = source.keys(b"big:", Start::Last, 10);
        let counted = source.count(b"", None, 1000);
        let used = lookups(&source) - before;
        let sizes = |rows: Result<Vec<eggdb_ui::KeyRow>, String>| -> Vec<u64> {
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
        let source = DatabaseSource::new(db, "t.db".to_owned());
        let mut terminal = FakeTerminal::new(80, 24, Capabilities::FULL);
        let mut ui = Ui::new(
            source,
            UiOptions {
                capabilities: Capabilities::FULL,
            },
            Size { cols: 80, rows: 24 },
        );
        let mut frame = Frame::new(Size { cols: 80, rows: 24 });
        let step = |ui: &mut Ui<DatabaseSource>, terminal: &mut FakeTerminal, frame: &mut Frame| {
            ui.process(terminal, frame)
                .unwrap_or_else(|error| panic!("present: {error}"));
        };
        step(&mut ui, &mut terminal, &mut frame);
        let screen = terminal.screen();
        assert!(screen.contains("32 keys"), "{screen}");
        assert!(screen.contains("> big"), "{screen}");
        assert!(screen.contains("hex (auto)"), "{screen}");
        terminal.press("Down");
        step(&mut ui, &mut terminal, &mut frame);
        assert!(terminal.screen().contains("json (auto)"));
        terminal.press("End");
        step(&mut ui, &mut terminal, &mut frame);
        let screen = terminal.screen();
        assert!(screen.contains("> user:30"), "{screen}");
        assert!(screen.contains("13–32 of 32"), "{screen}");
        terminal.type_str("/user:2");
        step(&mut ui, &mut terminal, &mut frame);
        assert!(terminal.screen().contains("1–10 of 10"));
    }
}
