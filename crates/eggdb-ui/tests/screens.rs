//! The scripted fake-terminal scenarios of docs/ux/ui.md.

use eggdb_ui::{
    Capabilities, Change, CheckSummary, Colors, CountStep, ErrorKind, Event, FakeTerminal, Frame,
    KeyRow, Size, Source, SourceError, Start, Summary, Terminal, Ui, UiOptions, WriterBusy,
};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Hooks {
    writer_held: Cell<bool>,
    check_held: Cell<bool>,
    damage: RefCell<Option<String>>,
    next_write_error: RefCell<Option<SourceError>>,
    needs_reopen: Cell<bool>,
    writes: Cell<u32>,
    reopens: Cell<u32>,
}

struct MapSource {
    entries: BTreeMap<Vec<u8>, Vec<u8>>,
    read_only: bool,
    largest_value: usize,
    hooks: Rc<Hooks>,
    check_started: bool,
}

fn after_prefix(prefix: &[u8]) -> Bound<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xff {
            end.push(last + 1);
            return Bound::Excluded(end);
        }
    }
    Bound::Unbounded
}

impl MapSource {
    fn matching(&self, prefix: &[u8]) -> impl DoubleEndedIterator<Item = (&Vec<u8>, &Vec<u8>)> {
        self.entries
            .range((Bound::Included(prefix.to_vec()), after_prefix(prefix)))
    }
}

impl Source for MapSource {
    fn summary(&self) -> Result<Summary, SourceError> {
        Ok(Summary {
            name: "app.db".to_owned(),
            file_bytes: 1_258_291,
            page_count: 312,
            free_pages: 18,
            page_size: 4096,
            last_commit: Some("09:14:03Z".to_owned()),
            last_commit_detail: Some(
                "2026-10-26 09:14:03.512000 UTC (stamp 1793005243512000.0001)".to_owned(),
            ),
            read_only: self.read_only,
            max_key_bytes: 512,
            max_value_bytes: 1 << 30,
            cache_bytes: 16 << 20,
            cached_pages: 211,
            cache_hits: 982,
            cache_misses: 18,
            log_bytes: 24 << 10,
            write_timeout: Duration::from_secs(5),
        })
    }

    fn keys(
        &self,
        prefix: &[u8],
        start: Start<'_>,
        limit: usize,
    ) -> Result<Vec<KeyRow>, SourceError> {
        let row = |(key, value): (&Vec<u8>, &Vec<u8>)| KeyRow {
            key: key.clone(),
            value_bytes: value.len() as u64,
        };
        Ok(match start {
            Start::First => self.matching(prefix).take(limit).map(row).collect(),
            Start::Last => self.matching(prefix).rev().take(limit).map(row).collect(),
            Start::After(key) => self
                .matching(prefix)
                .filter(|(k, _)| k.as_slice() > key)
                .take(limit)
                .map(row)
                .collect(),
            Start::AtOrAfter(key) => self
                .matching(prefix)
                .filter(|(k, _)| k.as_slice() >= key)
                .take(limit)
                .map(row)
                .collect(),
            Start::Before(key) => self
                .matching(prefix)
                .rev()
                .filter(|(k, _)| k.as_slice() < key)
                .take(limit)
                .map(row)
                .collect(),
        })
    }

    fn value(&self, key: &[u8]) -> Result<Option<Vec<u8>>, SourceError> {
        Ok(self.entries.get(key).cloned())
    }

    fn count(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u64,
    ) -> Result<CountStep, SourceError> {
        let mut counted = 0;
        let mut last = None;
        for (key, _) in self
            .matching(prefix)
            .filter(|(k, _)| after.is_none_or(|after| k.as_slice() > after))
        {
            if counted == limit {
                return Ok(CountStep {
                    counted,
                    resume_after: last,
                });
            }
            counted += 1;
            last = Some(key.clone());
        }
        Ok(CountStep {
            counted,
            resume_after: None,
        })
    }

    fn write(&mut self, change: &Change, busy: WriterBusy) -> Result<String, SourceError> {
        self.hooks.writes.set(self.hooks.writes.get() + 1);
        if self.read_only {
            return Err(SourceError::new(
                ErrorKind::ReadOnly,
                "the database was opened read-only",
            ));
        }
        if self.hooks.writer_held.get() {
            let message = match busy {
                WriterBusy::Fail => {
                    "another write transaction is open; one writer at a time: commit or roll it back first"
                }
                WriterBusy::Wait => {
                    "another write transaction is still open after waiting 5s; one writer at a time: commit or roll it back first, or wait longer with `Options::write_timeout`"
                }
            };
            return Err(SourceError::new(ErrorKind::WriterActive, message));
        }
        if self.hooks.needs_reopen.get() {
            return Err(SourceError::new(
                ErrorKind::NeedsReopen,
                "an earlier write failed, so the database refuses changes until it is opened again",
            ));
        }
        if let Some(error) = self.hooks.next_write_error.borrow_mut().take() {
            return Err(error);
        }
        match change {
            Change::Put { value, .. } if value.len() > self.largest_value => Err(SourceError::new(
                ErrorKind::TooLarge,
                format!(
                    "a value of {} bytes needs {} pages, more than the {} frames of the page cache, which holds every page a write transaction changes until its commit; open the database with a larger cache",
                    value.len(),
                    value.len() / 4000 + 2,
                    self.largest_value / 4096
                ),
            )),
            Change::Put { key, value } => {
                self.entries.insert(key.clone(), value.clone());
                Ok("09:14:04Z".to_owned())
            }
            Change::Delete { key } => match self.entries.remove(key) {
                Some(_) => Ok("09:14:05Z".to_owned()),
                None => Err(SourceError::new(ErrorKind::Other, "no such key")),
            },
        }
    }

    fn start_check(&mut self) -> Result<(), SourceError> {
        self.check_started = true;
        Ok(())
    }

    fn check_outcome(&mut self) -> Option<Result<CheckSummary, SourceError>> {
        if !self.check_started || self.hooks.check_held.get() {
            return None;
        }
        self.check_started = false;
        if let Some(damage) = self.hooks.damage.borrow().clone() {
            return Some(Err(SourceError::new(ErrorKind::Corrupt, damage)));
        }
        Some(Ok(CheckSummary {
            entries: self.entries.len() as u64,
            key_bytes: self.entries.keys().map(|key| key.len() as u64).sum(),
            value_bytes: self.entries.values().map(|value| value.len() as u64).sum(),
            tree_height: 2,
            tree_pages: 9,
            overflow_values: 1,
            overflow_pages: 4,
            free_pages: 18,
            log_pages: 30,
            leaked_pages: 0,
        }))
    }

    fn reopen(&mut self) -> Result<(), SourceError> {
        self.hooks.reopens.set(self.hooks.reopens.get() + 1);
        self.hooks.needs_reopen.set(false);
        Ok(())
    }
}

fn fixture() -> MapSource {
    let mut entries = BTreeMap::new();
    for n in 1..=20 {
        entries.insert(
            format!("user:{n}").into_bytes(),
            format!("User {n}\nuser{n}@example.com\nborn 18{n:02}-12-10\n").into_bytes(),
        );
    }
    entries.insert(
        b"doc:json".to_vec(),
        br#"{"name":"Ada Lovelace","born":1815,"tags":["math","poetry"],"active":true,"notes":null}"#
            .to_vec(),
    );
    entries.insert(b"bin".to_vec(), (0u8..40).collect());
    entries.insert(b"empty".to_vec(), Vec::new());
    let mut large = String::new();
    let mut line = 0;
    while large.len() < 14 * 1024 {
        line += 1;
        large.push_str(&format!(
            "line {line:04}: the quick brown fox jumps over the lazy dog, again and again and again\n"
        ));
    }
    entries.insert(b"large".to_vec(), large.into_bytes());
    MapSource {
        entries,
        read_only: false,
        largest_value: 1 << 30,
        hooks: Rc::new(Hooks::default()),
        check_started: false,
    }
}

struct Scenario {
    terminal: FakeTerminal,
    ui: Ui<MapSource>,
    frame: Frame,
}

impl Scenario {
    fn new(cols: u16, rows: u16) -> Self {
        Self::with(cols, rows, Capabilities::FULL, fixture())
    }

    fn with(cols: u16, rows: u16, capabilities: Capabilities, source: MapSource) -> Self {
        let terminal = FakeTerminal::new(cols, rows, capabilities);
        let ui = Ui::new(source, UiOptions { capabilities }, terminal.size());
        let mut scenario = Self {
            terminal,
            ui,
            frame: Frame::new(Size { cols, rows }),
        };
        scenario.step();
        scenario
    }

    fn step(&mut self) {
        if let Err(error) = self.ui.process(&mut self.terminal, &mut self.frame) {
            panic!("presenting failed: {error}");
        }
    }

    fn press(&mut self, names: &[&str]) {
        for name in names {
            assert!(self.terminal.press(name), "unknown key name {name}");
        }
        self.step();
    }

    fn type_str(&mut self, text: &str) {
        self.terminal.type_str(text);
        self.step();
    }

    fn paste(&mut self, text: &str) {
        self.terminal.push(Event::Paste(text.to_owned()));
        self.step();
    }

    fn hooks(&self) -> Rc<Hooks> {
        Rc::clone(&self.ui.source().hooks)
    }

    fn entries(&self) -> &BTreeMap<Vec<u8>, Vec<u8>> {
        &self.ui.source().entries
    }

    fn handle_without_waiting(&mut self, name: &str) -> String {
        let mut key = FakeTerminal::new(1, 1, Capabilities::FULL);
        assert!(key.press(name), "unknown key name {name}");
        while let Ok(Some(event)) = key.next_event(Duration::ZERO) {
            self.ui.handle(event);
        }
        self.ui.draw(&mut self.frame);
        frame_screen(&self.frame)
    }

    fn screen(&self) -> String {
        self.terminal.screen()
    }

    fn row(&self, row: usize) -> String {
        self.screen()
            .lines()
            .nth(row)
            .unwrap_or_default()
            .to_owned()
    }

    fn matches(&self, name: &str) {
        golden(name, &self.screen());
    }
}

fn frame_screen(frame: &Frame) -> String {
    (0..frame.size.rows)
        .map(|row| frame.row_text(row) + "\n")
        .collect()
}

fn golden(name: &str, screen: &str) {
    let screen = screen.replace(env!("CARGO_PKG_VERSION"), "X.Y.Z");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/screens")
        .join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_SCREENS").is_some() {
        if let Err(error) = std::fs::write(&path, screen) {
            panic!("writing {}: {error}", path.display());
        }
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    if expected != screen.as_str() {
        let mut report = format!(
            "screen {name} differs from {} (UPDATE_SCREENS=1 rewrites it)\n",
            path.display()
        );
        let width = expected.lines().map(str::len).max().unwrap_or(0).max(10);
        let left: Vec<&str> = expected.lines().collect();
        let right: Vec<&str> = screen.lines().collect();
        for index in 0..left.len().max(right.len()) {
            let a = left.get(index).copied().unwrap_or("");
            let b = right.get(index).copied().unwrap_or("");
            let mark = if a == b { ' ' } else { '!' };
            report.push_str(&format!("{mark} {a:<width$} | {b}\n"));
        }
        panic!("{report}");
    }
}

#[test]
fn scenario_01_opens_at_80_by_24_on_the_first_key() {
    let s = Scenario::new(80, 24);
    s.matches("01_open_80x24");
    assert!(s.row(2).starts_with("> bin"));
}

#[test]
fn scenario_02_read_only_refuses_every_change_and_never_writes() {
    let mut source = fixture();
    source.read_only = true;
    let mut s = Scenario::with(80, 24, Capabilities::FULL, source);
    assert!(s.row(0).ends_with("RO"));
    assert!(
        s.row(22).contains("Opened app.db read-only"),
        "{}",
        s.row(22)
    );
    for key in ["e", "a", "d"] {
        s.press(&[key]);
        assert_eq!(
            s.row(22),
            " Opened read-only; start without --read-only to edit"
        );
    }
    s.press(&["Tab", "e"]);
    assert!(s.row(22).contains("read-only"), "{}", s.row(22));
    s.press(&["Down"]);
    assert!(s.row(22).is_empty());
    s.press(&["Tab", "c"]);
    assert!(s.screen().contains("Check  OK"), "{}", s.screen());
    s.press(&["Enter"]);
    assert_eq!(s.hooks().writes.get(), 0);
    assert_eq!(s.entries(), &fixture().entries);
    let mono = Capabilities {
        colors: Colors::None,
        attributes: true,
        unicode: true,
    };
    let mut source = fixture();
    source.read_only = true;
    let s = Scenario::with(80, 24, mono, source);
    assert!(!s.row(23).contains("e edit"), "{}", s.row(23));
    assert!(s.row(23).contains("c check"), "{}", s.row(23));
}

#[test]
fn scenario_03_navigation_moves_the_window_by_the_model() {
    let mut s = Scenario::new(80, 24);
    let downs = vec!["Down"; 25];
    s.press(&downs);
    s.matches("03_after_25_downs");
    assert!(s.row(1).contains("5–24 of 24"), "{}", s.row(1));
    s.press(&["Home"]);
    assert!(s.row(2).starts_with("> bin"));
    s.press(&["PgDn"]);
    assert!(s.row(1).contains("5–24 of 24"), "{}", s.row(1));
    s.press(&["PgUp"]);
    assert!(s.row(1).contains("1–20 of 24"), "{}", s.row(1));
    s.press(&["End"]);
    assert!(s.row(21).starts_with("> user:9"), "{}", s.row(21));
    s.press(&["g"]);
    assert!(s.row(2).starts_with("> bin"));
    s.press(&["G", "k"]);
    assert!(s.row(20).starts_with("> user:8"), "{}", s.row(20));
}

#[test]
fn scenario_04_the_position_is_honest_before_the_count_finishes() {
    let s = Scenario::new(80, 24);
    assert!(s.row(1).contains("1–20 of 24"));
    assert!(s.row(0).contains("24 keys"));
    let capabilities = Capabilities::FULL;
    let mut ui = Ui::new(
        fixture(),
        UiOptions { capabilities },
        Size { cols: 80, rows: 24 },
    );
    let mut frame = Frame::new(Size { cols: 80, rows: 24 });
    ui.handle(Event::Key(eggdb_ui::KeyEvent::plain(
        eggdb_ui::KeyCode::End,
    )));
    ui.draw(&mut frame);
    assert!(
        frame.row_text(1).contains("?–? of 0+"),
        "{}",
        frame.row_text(1)
    );
    assert!(frame.row_text(0).contains("0+ keys"));
    ui.run_until_idle();
    ui.draw(&mut frame);
    assert!(
        frame.row_text(1).contains("5–24 of 24"),
        "{}",
        frame.row_text(1)
    );
}

#[test]
fn scenario_05_the_prefix_filter_narrows_and_restores() {
    let mut s = Scenario::new(80, 24);
    s.press(&["/"]);
    s.type_str("user:1");
    s.matches("05_filter_user_1");
    assert!(s.row(1).contains("1–11 of 11"), "{}", s.row(1));
    s.press(&["Esc"]);
    assert!(s.row(1).contains("1–20 of 24"), "{}", s.row(1));
    s.press(&["/"]);
    s.type_str("zzz");
    s.press(&["Enter"]);
    assert!(
        s.row(2).contains("no keys start with \"zzz\""),
        "{}",
        s.row(2)
    );
    assert!(s.row(1).contains(" Keys /zzz"));
    s.press(&["/"]);
    s.type_str("\\x62");
    s.press(&["Enter"]);
    assert!(s.row(2).starts_with("> bin"), "{}", s.row(2));
    assert!(s.row(1).contains("1–1 of 1"), "{}", s.row(1));
    s.press(&["/", "Esc"]);
    assert!(s.row(1).contains("of 24"), "{}", s.row(1));
}

#[test]
fn scenario_06_values_open_in_the_mode_their_bytes_suggest() {
    let mut s = Scenario::new(80, 24);
    s.matches("06_binary_hex_80");
    s.press(&["Down"]);
    s.matches("06_json_pretty");
    s.press(&["Down"]);
    assert!(s.row(4).contains("(empty)"), "{}", s.row(4));
    let wide = Scenario::new(120, 24);
    wide.matches("06_binary_hex_120");
    s.press(&["Down", "Down"]);
    assert!(s.row(1).contains("text (auto)"), "{}", s.row(1));
}

#[test]
fn scenario_07_v_cycles_modes_and_json_on_text_explains_why_not() {
    let mut s = Scenario::new(80, 24);
    s.press(&["End"]);
    s.press(&["v"]);
    assert!(s.row(1).contains("Value  text ·"), "{}", s.row(1));
    s.press(&["v"]);
    assert!(
        s.row(22).contains("Not JSON: expected a value at byte 0"),
        "{}",
        s.row(22)
    );
    assert!(s.row(1).contains("Value  text ·"), "{}", s.row(1));
    s.press(&["v"]);
    assert!(s.row(1).contains("Value  hex ·"), "{}", s.row(1));
    s.press(&["v"]);
    assert!(s.row(1).contains("text (auto)"), "{}", s.row(1));
}

#[test]
fn scenario_08_scrolling_a_large_value() {
    let mut s = Scenario::new(80, 24);
    s.press(&["Down", "Down", "Down", "Enter"]);
    assert!(s.row(1).contains("line 1–18/"), "{}", s.row(1));
    s.press(&["End"]);
    s.matches("08_large_text_end");
    s.press(&["Right", "Right"]);
    assert!(s.row(4).starts_with(&format!("{}│", " ".repeat(33))) || s.row(4).contains("fox"));
    s.press(&["w"]);
    assert!(s.row(1).contains("line 158–"), "{}", s.row(1));
    s.press(&["End"]);
    s.matches("08_large_text_wrapped");
    assert!(s.row(1).contains("–175/175"), "{}", s.row(1));
    assert!(s.row(21).contains("again"), "{}", s.row(21));
    s.press(&["v", "v", "v"]);
    s.press(&["Home"]);
    assert!(s.row(1).contains("Value  hex ·"), "{}", s.row(1));
    assert!(s.row(1).contains("bytes 0–143 of"), "{}", s.row(1));
}

#[test]
fn scenario_09_a_value_above_64_mib_waits_for_enter() {
    let mut source = fixture();
    source
        .entries
        .insert(b"aaa:huge".to_vec(), vec![b'x'; (64 << 20) + 1]);
    let mut s = Scenario::with(80, 24, Capabilities::FULL, source);
    assert!(s.row(4).contains("press Enter to load"), "{}", s.row(4));
    s.press(&["Enter", "Enter"]);
    assert!(s.row(4).contains("xxxx"), "{}", s.row(4));
    assert!(!s.screen().contains("press Enter to load"));
}

#[test]
fn a_single_20000_character_line_wraps_and_scrolls_to_its_end() {
    let mut source = fixture();
    let mut line: String = (0..2000).map(|n| format!("{:09} ", n)).collect();
    line.push_str("THE END");
    source
        .entries
        .insert(b"aaa:long".to_vec(), line.into_bytes());
    let mut s = Scenario::with(80, 24, Capabilities::FULL, source);
    s.press(&["Enter", "w"]);
    assert!(s.row(4).contains("000000000 000000001"), "{}", s.row(4));
    s.press(&["End"]);
    s.matches("wrap_long_line_end");
    assert!(s.row(21).contains("THE END"), "{}", s.screen());
    s.press(&["Home"]);
    for _ in 0..30 {
        s.press(&["PgDn"]);
    }
    assert!(s.row(21).contains("THE END"), "{}", s.screen());
    s.press(&["Up"]);
    assert!(!s.screen().contains("THE END"), "{}", s.screen());
    s.press(&["Down"]);
    assert!(s.row(21).contains("THE END"), "{}", s.screen());
}

#[test]
fn values_above_4_mib_are_never_pretty_printed() {
    let mut source = fixture();
    let mut big = String::from("[");
    while big.len() < (4 << 20) + 16 {
        big.push_str("1,");
    }
    big.push_str("1]");
    source
        .entries
        .insert(b"aaa:array".to_vec(), big.into_bytes());
    let mut s = Scenario::with(80, 24, Capabilities::FULL, source);
    assert!(s.row(1).contains("text (auto)"), "{}", s.row(1));
    assert!(
        s.row(22).contains("JSON view takes values up to 4 MiB"),
        "{}",
        s.row(22)
    );
    s.press(&["v", "v"]);
    assert!(s.row(1).contains("Value  text"), "{}", s.row(1));
    assert!(
        s.row(22).contains("JSON view takes values up to 4 MiB"),
        "{}",
        s.row(22)
    );
    s.press(&["Enter", "Right", "Right", "End"]);
    assert!(s.row(4).contains(",1,1,1"), "{}", s.row(4));
}

#[test]
fn scenario_17_quit_keys_and_interrupt_end_the_ui() {
    for keys in [&["q"][..], &["Ctrl-C"][..]] {
        let mut s = Scenario::new(80, 24);
        assert!(!s.ui.should_quit());
        s.press(keys);
        assert!(s.ui.should_quit());
    }
    let mut s = Scenario::new(80, 24);
    s.terminal.push(Event::Interrupt);
    s.step();
    assert!(s.ui.should_quit());
}

#[test]
fn scenario_19_help_shows_the_keys_and_the_database_limits() {
    let mut s = Scenario::new(80, 24);
    s.press(&["?"]);
    s.matches("19_help");
    s.press(&["Esc"]);
    assert!(s.row(0).starts_with(" EggDB  app.db"));
}

#[test]
fn scenario_20_resizing_keeps_the_selection() {
    let mut s = Scenario::new(80, 24);
    s.press(&["Down", "Down", "Down", "Down", "Down"]);
    s.terminal.resize(120, 40);
    s.step();
    assert!(s.row(7).starts_with("> user:10 "), "{}", s.row(7));
    s.terminal.resize(60, 20);
    s.step();
    s.matches("20_single_pane_60x20");
    s.press(&["Tab"]);
    assert!(s.row(1).contains("Value"), "{}", s.row(1));
    s.terminal.resize(30, 8);
    s.step();
    s.matches("20_too_small");
    s.terminal.resize(80, 24);
    s.step();
    assert!(s.screen().contains("> user:10 "), "{}", s.screen());
    s.press(&["Down"]);
    s.terminal.resize(80, 10);
    s.step();
    assert!(s.screen().contains("> user:10"), "{}", s.screen());
}

#[test]
fn scenario_21_fallbacks_keep_the_marker_and_drop_what_cannot_show() {
    let ascii = Capabilities {
        colors: Colors::Ansi16,
        attributes: true,
        unicode: false,
    };
    let s = Scenario::with(80, 24, ascii, fixture());
    s.matches("21_ascii");
    assert!(s.screen().is_ascii());
    let mono = Capabilities {
        colors: Colors::None,
        attributes: true,
        unicode: true,
    };
    let s = Scenario::with(80, 24, mono, fixture());
    assert!(s.terminal.styled(2, 0).inverse);
    assert_eq!(s.terminal.styled(2, 0).fg, eggdb_ui::Color::Default);
    let plain = Capabilities {
        colors: Colors::None,
        attributes: false,
        unicode: false,
    };
    let s = Scenario::with(80, 24, plain, fixture());
    s.matches("21_plain");
    assert!(s.row(1).contains("[Keys]"));
    assert!(s.row(2).starts_with("> bin"));
    assert_eq!(s.terminal.styled(2, 0), eggdb_ui::Style::default());
}

#[test]
fn scenario_22_moving_down_redraws_only_what_changed() {
    let mut s = Scenario::new(80, 24);
    s.press(&["Down", "Down", "Down", "Down"]);
    let before = s.screen();
    let presented = s.terminal.frames_presented();
    s.press(&["Down"]);
    assert_eq!(s.terminal.frames_presented(), presented + 1);
    let after = s.screen();
    let list_rows_changed: Vec<usize> = before
        .lines()
        .zip(after.lines())
        .enumerate()
        .filter(|(row, (a, b))| (2..22).contains(row) && a.chars().take(32).ne(b.chars().take(32)))
        .map(|(row, _)| row)
        .collect();
    assert_eq!(list_rows_changed, [6, 7]);
    assert_eq!(before.lines().next(), after.lines().next());
    assert_eq!(before.lines().last(), after.lines().last());
    assert!(
        s.terminal.cells_changed() < 80 * 24 / 3,
        "{}",
        s.terminal.cells_changed()
    );
}

#[test]
fn the_screen_works_at_40_by_10() {
    let mut s = Scenario::new(40, 10);
    s.matches("40x10_open");
    s.press(&["Down", "PgDn", "End", "Enter", "?", "Esc", "Tab", "/"]);
    s.type_str("user");
    s.press(&["Enter"]);
    assert!(s.row(1).contains("/user"), "{}", s.row(1));
}

fn has_unprintable(screen: &str) -> bool {
    screen.chars().any(|c| {
        c.is_control() && c != '\n'
            || matches!(u32::from(c), 0x200B | 0x200E | 0x200F | 0x2028..=0x202E | 0x2060..=0x206F | 0xFEFF)
    })
}

#[test]
fn bidi_overrides_and_isolates_in_keys_and_values_show_as_code_points() {
    let mut source = fixture();
    source.entries.insert(
        "aaa:user\u{202e}gnp.exe".as_bytes().to_vec(),
        "safe\u{2067}text\u{2069} and json".as_bytes().to_vec(),
    );
    source.entries.insert(
        "aab:doc".as_bytes().to_vec(),
        "{\"name\":\"x\u{202e}y\"}".as_bytes().to_vec(),
    );
    let mut s = Scenario::with(80, 24, Capabilities::FULL, source);
    s.matches("bidi_controls");
    let screen = s.screen();
    assert!(!has_unprintable(&screen), "{screen:?}");
    assert!(
        s.row(2).contains("aaa:user\\u{202e}gnp.exe"),
        "{}",
        s.row(2)
    );
    assert!(
        screen.contains("safe\\u{2067}text\\u{2069} and json"),
        "{screen}"
    );
    s.press(&["Down"]);
    assert!(s.row(1).contains("json (auto)"), "{}", s.row(1));
    assert!(s.screen().contains("\"x\\u{202e}y\""), "{}", s.screen());
    assert!(!has_unprintable(&s.screen()));
}

#[test]
fn combining_marks_in_a_value_stay_on_their_letter() {
    let mut source = fixture();
    source
        .entries
        .insert(b"aaa:accent".to_vec(), "cafe\u{301} ok".as_bytes().to_vec());
    let s = Scenario::with(80, 24, Capabilities::FULL, source);
    assert!(s.row(4).contains("cafe\u{301} ok"), "{:?}", s.row(4));
}

#[test]
fn hex_keeps_its_first_byte_when_the_row_width_changes() {
    let mut source = fixture();
    source.entries.insert(
        b"aaa:bytes".to_vec(),
        (0..4096u32).map(|n| n as u8).collect(),
    );
    let mut s = Scenario::with(140, 24, Capabilities::FULL, source);
    s.press(&["Tab"]);
    for _ in 0..10 {
        s.press(&["Down"]);
    }
    assert!(s.row(1).contains("bytes 160–"), "{}", s.row(1));
    s.terminal.resize(80, 24);
    s.step();
    assert!(s.row(1).contains("bytes 160–"), "{}", s.row(1));
    s.terminal.resize(140, 24);
    s.step();
    assert!(s.row(1).contains("bytes 160–"), "{}", s.row(1));
}

#[test]
fn growing_the_screen_keeps_the_viewer_and_help_inside_their_text() {
    let mut s = Scenario::new(80, 24);
    s.press(&["Down", "Down", "Down", "Enter", "End"]);
    assert!(s.row(1).contains("line 158–175/175"), "{}", s.row(1));
    s.terminal.resize(80, 60);
    s.step();
    assert!(s.row(1).contains("line 122–175/175"), "{}", s.row(1));
    s.press(&["?", "End"]);
    s.terminal.resize(80, 200);
    s.step();
    assert!(s.row(0).contains(" 1–"), "{}", s.row(0));
}

#[test]
fn the_widest_terminal_does_not_overflow_the_layout() {
    let s = Scenario::new(u16::MAX, 12);
    assert!(s.row(0).starts_with(" EggDB"), "{}", s.row(0));
    assert!(s.row(2).starts_with("> bin"), "{}", s.row(2));
}

fn assert_model(s: &Scenario, model: &BTreeMap<Vec<u8>, Vec<u8>>) {
    assert!(s.entries() == model, "the database differs from the model");
}

#[test]
fn scenario_10_edit_a_value_on_one_line_and_commit_it() {
    let mut s = Scenario::new(80, 24);
    s.press(&["Down", "e"]);
    s.matches("10_edit_single_line");
    s.press(&["End", "Backspace"]);
    s.type_str(",\"x\":1}");
    assert!(s.row(1).contains("87 B → 93 B"), "{}", s.row(1));
    s.press(&["Enter"]);
    s.matches("10_confirm_replace");
    s.press(&["Enter"]);
    s.matches("10_committed");
    assert_eq!(s.row(22), " Committed doc:json (93 B) at 09:14:04Z");
    let mut model = fixture().entries;
    model.insert(
        b"doc:json".to_vec(),
        br#"{"name":"Ada Lovelace","born":1815,"tags":["math","poetry"],"active":true,"notes":null,"x":1}"#.to_vec(),
    );
    assert_model(&s, &model);
    assert!(s.row(3).starts_with("> doc:json"), "{}", s.row(3));
}

#[test]
fn scenario_11_edit_many_lines_with_undo_and_cancel() {
    let mut s = Scenario::new(80, 24);
    s.press(&["Down", "Down", "Down", "Down", "e"]);
    s.matches("11_edit_multi_line");
    s.type_str("Dr. ");
    assert!(s.row(4).contains("Dr. User 1"), "{}", s.row(4));
    s.press(&["Ctrl-Z", "Ctrl-Z"]);
    assert!(s.row(4).contains("│ User 1"), "{}", s.row(4));
    s.press(&["Ctrl-Y"]);
    assert!(s.row(4).contains("Dr.User 1"), "{}", s.row(4));
    s.press(&["Ctrl-S"]);
    assert!(s.screen().contains("Replace value"), "{}", s.screen());
    s.press(&["Esc"]);
    assert!(!s.screen().contains("Replace value"));
    assert!(s.row(4).contains("Dr.User 1"), "{}", s.row(4));
    s.press(&["Esc"]);
    s.matches("11_discard_question");
    s.press(&["Esc"]);
    assert!(s.row(1).contains(" Edit  text"), "{}", s.row(1));
    s.press(&["Esc", "y"]);
    assert!(s.row(1).contains(" Keys"), "{}", s.row(1));
    assert_model(&s, &fixture().entries);
    s.press(&["e", "Esc"]);
    assert!(s.row(1).contains(" Keys"), "{}", s.row(1));
    s.press(&["Up", "Up", "Up", "e", "Alt-Enter"]);
    assert!(s.row(23).contains("Ctrl-S confirm"), "{}", s.row(23));
    s.press(&["Ctrl-S", "Enter"]);
    assert_eq!(
        s.entries()
            .get(b"doc:json".as_slice())
            .map(|value| value[0]),
        Some(b'\n')
    );
}

#[test]
fn scenario_12_binary_values_are_edited_as_escapes() {
    let mut s = Scenario::new(80, 24);
    s.press(&["e"]);
    assert!(s.row(4).contains("\\x00\\x01\\x02"), "{}", s.row(4));
    s.press(&["Enter", "Enter"]);
    assert_eq!(s.hooks().writes.get(), 1);
    assert_model(&s, &fixture().entries);
    s.press(&["e", "Home"]);
    s.type_str("\\xZ");
    s.press(&["Enter"]);
    assert_eq!(
        s.row(22),
        " Bad escape at column 1: expected two hex digits after \\x"
    );
    assert!(s.row(1).ends_with("ln 1 col 1"), "{}", s.row(1));
    assert_eq!(s.hooks().writes.get(), 1);
    s.press(&["Delete", "Delete", "Delete", "Enter", "Enter"]);
    assert_eq!(s.hooks().writes.get(), 2);
    assert_model(&s, &fixture().entries);
}

#[test]
fn scenario_13_add_keys() {
    let mut s = Scenario::new(80, 24);
    s.press(&["a"]);
    s.matches("13_add_key");
    s.type_str("user:21");
    s.press(&["Enter"]);
    s.type_str("new user");
    s.press(&["Enter"]);
    s.matches("13_confirm_add");
    s.press(&["Enter"]);
    assert_eq!(s.row(22), " Committed user:21 (8 B) at 09:14:04Z");
    assert!(s.screen().contains("> user:21 "), "{}", s.screen());
    assert!(s.row(0).contains("25 keys"), "{}", s.row(0));
    let mut model = fixture().entries;
    model.insert(b"user:21".to_vec(), b"new user".to_vec());
    assert_model(&s, &model);
    s.press(&["a"]);
    s.type_str("user:2");
    s.press(&["Enter"]);
    s.type_str("x");
    s.press(&["Enter"]);
    assert!(s.screen().contains("┌ Replace value"), "{}", s.screen());
    assert!(
        s.screen()
            .contains("Value  already exists: 41 B → 1 B, text"),
        "{}",
        s.screen()
    );
    s.press(&["Esc", "Esc", "y"]);
    assert_model(&s, &model);
    s.press(&["a"]);
    s.type_str(&"k".repeat(530));
    s.press(&["Enter"]);
    assert_eq!(
        s.row(22),
        " Key is 530 B; this database's keys are at most 512 B"
    );
    s.press(&["Esc"]);
    s.press(&["/"]);
    s.type_str("user");
    s.press(&["Enter", "a"]);
    s.type_str("zzz");
    s.press(&["Enter"]);
    s.type_str("v");
    s.press(&["Enter", "Enter"]);
    assert_eq!(
        s.row(22),
        " Committed zzz (1 B) at 09:14:04Z; not shown by filter /user"
    );
    model.insert(b"zzz".to_vec(), b"v".to_vec());
    assert_model(&s, &model);
}

#[test]
fn scenario_14_delete_asks_for_d_and_moves_the_selection() {
    let mut s = Scenario::new(80, 24);
    s.press(&["Down", "Down", "Down", "Down", "d"]);
    s.matches("14_confirm_delete");
    s.press(&["Enter"]);
    assert!(s.screen().contains("Delete key"), "{}", s.screen());
    assert_eq!(s.hooks().writes.get(), 0);
    s.press(&["d"]);
    assert_eq!(s.row(22), " Deleted user:1 (41 B) at 09:14:05Z");
    assert!(s.row(6).starts_with("> user:10 "), "{}", s.row(6));
    assert!(s.row(0).contains("23 keys"), "{}", s.row(0));
    s.press(&["End", "d", "d"]);
    assert!(s.screen().contains("> user:8 "), "{}", s.screen());
    let mut model = fixture().entries;
    model.remove(b"user:1".as_slice());
    model.remove(b"user:9".as_slice());
    assert_model(&s, &model);
    s.press(&["d", "Esc"]);
    assert!(!s.screen().contains("Delete key"));
    assert_model(&s, &model);
}

#[test]
fn scenario_15_a_value_larger_than_the_cache_takes_keeps_the_editor() {
    let mut source = fixture();
    source.largest_value = 1 << 20;
    let mut s = Scenario::with(80, 24, Capabilities::FULL, source);
    s.press(&["a"]);
    s.type_str("big");
    s.press(&["Enter"]);
    s.paste(&"x".repeat(1 << 20));
    s.paste(&"y".repeat(1 << 20));
    s.press(&["Enter", "Enter"]);
    s.matches("15_too_large");
    assert!(
        s.row(22)
            .starts_with(" Error (TooLarge): a value of 2097152 bytes")
    );
    assert!(s.row(22).ends_with("… (m)"), "{}", s.row(22));
    assert!(
        s.row(1).contains(" Add  text · new → 2.0 M"),
        "{}",
        s.row(1)
    );
    assert_model(&s, &fixture().entries);
    s.press(&["Esc", "y", "m"]);
    assert!(
        s.screen().contains("Try: start eggdb ui with --cache 64M"),
        "{}",
        s.screen()
    );
}

#[test]
fn scenario_16_a_busy_writer_is_waited_for_on_request() {
    let mut s = Scenario::new(80, 24);
    let hooks = s.hooks();
    hooks.writer_held.set(true);
    s.press(&["Down", "e", "End"]);
    s.type_str(" ");
    s.press(&["Enter", "Enter"]);
    s.matches("16_writer_busy");
    let waiting = s.handle_without_waiting("r");
    assert!(
        waiting.contains("Waiting up to 5 s for the other write transaction…"),
        "{waiting}"
    );
    s.step();
    assert!(s.screen().contains("is still open after"), "{}", s.screen());
    hooks.writer_held.set(false);
    s.press(&["r"]);
    assert_eq!(s.row(22), " Committed doc:json (88 B) at 09:14:04Z");
    hooks.writer_held.set(true);
    s.press(&["d", "d"]);
    assert!(s.screen().contains("Writer busy"), "{}", s.screen());
    s.press(&["Esc"]);
    assert!(s.screen().contains("Delete key"), "{}", s.screen());
    hooks.writer_held.set(false);
    s.press(&["d"]);
    assert!(s.row(22).starts_with(" Deleted doc:json"), "{}", s.row(22));
}

#[test]
fn scenario_18_the_check_screen_runs_then_reports() {
    let mut s = Scenario::new(80, 24);
    let hooks = s.hooks();
    hooks.check_held.set(true);
    let running = s.handle_without_waiting("c");
    golden("18_check_running", &running);
    assert!(!s.ui.is_idle());
    assert_eq!(s.handle_without_waiting("Esc"), running);
    hooks.check_held.set(false);
    s.step();
    s.matches("18_check_ok");
    s.press(&["Enter"]);
    assert!(s.row(1).contains(" Keys"), "{}", s.row(1));
    hooks.damage.replace(Some(
        "page 7 failed its checksum: expected 0x1234abcd, found 0x00000000".to_owned(),
    ));
    s.press(&["c"]);
    s.matches("18_check_failed");
    s.press(&["Esc"]);
    assert!(s.row(1).contains(" Keys"), "{}", s.row(1));
}

#[test]
fn scenario_23_a_long_message_opens_in_full_and_scrolls() {
    let mut s = Scenario::new(80, 24);
    let long: String = (1..=120)
        .map(|n| format!("detail {n} of a long explanation."))
        .collect::<Vec<_>>()
        .join(" ");
    s.hooks()
        .next_write_error
        .replace(Some(SourceError::new(ErrorKind::CacheFull, long)));
    s.press(&["Down", "e", "Enter", "Enter"]);
    assert!(s.row(22).ends_with("… (m)"), "{}", s.row(22));
    s.press(&["Esc", "m"]);
    s.matches("23_message");
    s.press(&["PgDn"]);
    assert!(!s.screen().contains("detail 1 of"), "{}", s.screen());
    s.press(&["End"]);
    assert!(s.screen().contains("Try: start eggdb ui"), "{}", s.screen());
    s.press(&["Esc"]);
    assert!(!s.screen().contains("┌"), "{}", s.screen());
}

#[test]
fn a_failed_commit_shows_the_error_kind_and_keeps_the_editor() {
    let mut s = Scenario::new(80, 24);
    let hooks = s.hooks();
    hooks.next_write_error.replace(Some(SourceError::new(
        ErrorKind::Io,
        "cannot write page 12 of app.db: the disk is full",
    )));
    s.press(&["Down", "e", "End"]);
    s.type_str("!");
    s.press(&["Enter", "Enter"]);
    s.matches("error_modal");
    s.press(&["Esc"]);
    assert!(s.row(1).contains(" Edit  json"), "{}", s.row(1));
    hooks.needs_reopen.set(true);
    s.press(&["Enter", "Enter"]);
    assert!(s.screen().contains("Reopen needed"), "{}", s.screen());
    s.press(&["Esc"]);
    assert!(s.screen().contains("Reopen needed"), "{}", s.screen());
    s.press(&["r"]);
    assert_eq!(hooks.reopens.get(), 1);
    assert!(s.row(22).starts_with(" Reopened app.db"), "{}", s.row(22));
    assert!(s.row(1).contains(" Edit  json"), "{}", s.row(1));
    s.press(&["Enter", "Enter"]);
    assert!(
        s.row(22).starts_with(" Committed doc:json"),
        "{}",
        s.row(22)
    );
    hooks.next_write_error.replace(Some(SourceError::new(
        ErrorKind::Corrupt,
        "page 3 failed its checksum",
    )));
    s.press(&["e", "Enter", "Enter"]);
    assert!(s.screen().contains("Damaged database"), "{}", s.screen());
    s.press(&["c"]);
    assert!(s.screen().contains("Check  OK"), "{}", s.screen());
}

#[test]
fn toasts_expire_after_8_seconds_or_at_the_next_key() {
    let mut s = Scenario::new(80, 24);
    assert!(!s.row(22).is_empty());
    let now = Instant::now();
    assert!(!s.ui.tick_at(now + Duration::from_secs(5)));
    s.ui.draw(&mut s.frame);
    assert!(!s.frame.row_text(22).is_empty());
    assert!(s.ui.tick_at(now + Duration::from_secs(9)));
    s.ui.draw(&mut s.frame);
    assert!(s.frame.row_text(22).is_empty());
    s.press(&["e", "Esc", "m"]);
    assert!(
        s.screen().contains("Opened app.db read-write"),
        "{}",
        s.screen()
    );
}

#[test]
fn quitting_with_an_unsaved_edit_asks_first() {
    let mut s = Scenario::new(80, 24);
    s.press(&["Down", "e", "q"]);
    assert!(!s.ui.should_quit());
    assert!(s.row(1).contains("87 B → 88 B"), "{}", s.row(1));
    s.press(&["Ctrl-C"]);
    assert!(!s.ui.should_quit());
    assert!(s.screen().contains("┌ Quit"), "{}", s.screen());
    s.press(&["Esc"]);
    assert!(!s.ui.should_quit());
    s.press(&["Ctrl-C", "Ctrl-C"]);
    assert!(s.ui.should_quit());
    assert_model(&s, &fixture().entries);
    let mut s = Scenario::new(80, 24);
    s.press(&["e", "Ctrl-C"]);
    assert!(s.ui.should_quit());
}

#[test]
fn an_editor_survives_a_terminal_too_small_to_draw_it() {
    let mut s = Scenario::new(80, 24);
    s.press(&["Down", "e", "End"]);
    s.type_str(" kept");
    s.terminal.resize(30, 8);
    s.step();
    s.matches("20_too_small");
    s.press(&["q"]);
    assert!(!s.ui.should_quit());
    s.terminal.resize(80, 24);
    s.step();
    assert!(s.row(4).contains("} kept"), "{}", s.row(4));
    s.terminal.resize(120, 40);
    s.step();
    assert!(s.row(4).contains("} kept"), "{}", s.row(4));
}

#[test]
fn editing_works_at_40_by_10() {
    let mut s = Scenario::new(40, 10);
    s.press(&["Down", "e", "End"]);
    s.type_str("!");
    s.matches("40x10_edit");
    s.press(&["Enter"]);
    s.matches("40x10_confirm");
    s.press(&["Enter"]);
    assert!(s.row(8).contains("Committed"), "{}", s.row(8));
    s.press(&["a"]);
    s.type_str("new");
    s.press(&["Enter"]);
    s.type_str("v");
    s.press(&["Enter", "Enter", "d", "d"]);
    let mut model = fixture().entries;
    let mut json = model[b"doc:json".as_slice()].clone();
    json.push(b'!');
    model.insert(b"doc:json".to_vec(), json);
    assert_model(&s, &model);
}

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn spelled_as_escapes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\x{byte:02x}")).collect()
}

#[test]
fn scenario_24_random_edits_through_the_ui_agree_with_a_model() {
    let mut s = Scenario::new(80, 24);
    let mut model = fixture().entries;
    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    for _ in 0..500 {
        let key = format!("k{}", random.below(40)).into_bytes();
        let value: Vec<u8> = (0..random.below(24))
            .map(|_| random.below(256) as u8)
            .filter(|&byte| byte != b'\n')
            .collect();
        let existing = model.contains_key(&key);
        let filter = |s: &mut Scenario, key: &[u8]| {
            s.terminal.press("/");
            s.terminal.type_str(&String::from_utf8_lossy(key));
            s.terminal.press("Enter");
        };
        match random.below(10) {
            0..4 => {
                s.terminal.press("a");
                s.terminal.type_str(&String::from_utf8_lossy(&key));
                s.terminal.press("Enter");
                s.terminal.type_str(&spelled_as_escapes(&value));
                s.terminal.press("Enter");
                s.terminal.press("Enter");
                model.insert(key, value);
            }
            4..7 if existing => {
                filter(&mut s, &key);
                for name in ["e", "Ctrl-A", "Ctrl-K"] {
                    s.terminal.press(name);
                }
                s.terminal.type_str(&spelled_as_escapes(&value));
                s.terminal.press("Enter");
                s.terminal.press("Enter");
                s.terminal.press("/");
                s.terminal.press("Esc");
                model.insert(key, value);
            }
            _ if existing => {
                filter(&mut s, &key);
                s.terminal.press("d");
                s.terminal.press("d");
                s.terminal.press("/");
                s.terminal.press("Esc");
                model.remove(&key);
            }
            _ => continue,
        }
        s.step();
    }
    assert_model(&s, &model);
    assert!(
        s.row(0).contains(&format!("{} keys", model.len())),
        "{}",
        s.row(0)
    );
}
