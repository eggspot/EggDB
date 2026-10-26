//! The scripted fake-terminal scenarios of docs/ux/ui.md that apply to read-only browsing.

use eggdb_ui::{
    Capabilities, Colors, CountStep, Event, FakeTerminal, Frame, KeyRow, Size, Source, Start,
    Summary, Terminal, Ui, UiOptions,
};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::PathBuf;

struct MapSource {
    entries: BTreeMap<Vec<u8>, Vec<u8>>,
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
    fn summary(&self) -> Result<Summary, String> {
        Ok(Summary {
            name: "app.db".to_owned(),
            file_bytes: 1_258_291,
            page_count: 312,
            page_size: 4096,
            last_commit: Some("09:14:03Z".to_owned()),
            read_only: true,
            max_key_bytes: 512,
            max_value_bytes: 1 << 30,
        })
    }

    fn keys(&self, prefix: &[u8], start: Start<'_>, limit: usize) -> Result<Vec<KeyRow>, String> {
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

    fn value(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        Ok(self.entries.get(key).cloned())
    }

    fn count(&self, prefix: &[u8], after: Option<&[u8]>, limit: u64) -> Result<CountStep, String> {
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
    MapSource { entries }
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
fn scenario_02_change_keys_explain_that_the_ui_is_read_only() {
    let mut s = Scenario::new(80, 24);
    assert!(s.row(0).ends_with("RO"));
    s.press(&["e"]);
    assert!(s.row(22).contains("read-only"), "{}", s.row(22));
    s.press(&["Down"]);
    assert!(s.row(22).is_empty());
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
