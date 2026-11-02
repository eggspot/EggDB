//! The query tab and the results grid on a real file (`docs/ux/workspace.md` 2, 3 and their
//! checklist): runs from the keyboard off the window's thread, the summary strings, inline
//! errors, the virtualized grid's frame and accessibility tree, and copies to the clipboard.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eggdb_gui::connection::EngineConnector;
use eggdb_gui::editor::Editor;
use eggdb_gui::main_window::{
    self, Faces, MainWindow, NODE_BANNER, NODE_ERROR_STRIP, NODE_FIRST_CELL, NODE_GRID,
    NODE_STATUS_LINE, accessibility_tree,
};
use eggdb_gui::png::encode_png;
use eggdb_gui::query_tab::{ResultView, TabFocus};
use eggdb_gui::workspace::{Screen, Workspace};
use eggdb_gui::{BoxFace, Canvas, Theme, ThemeKind};
use eggdb_platform::window::{AccessRole, Key, KeyPress, Modifiers};

const BOX_FACES: Faces<'static> = Faces {
    ui: &BoxFace,
    ui_bold: &BoxFace,
    mono: &BoxFace,
};
const LIMIT: Duration = Duration::from_secs(60);

fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "eggdb-gui-query-tab-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&directory);
    directory
}

fn opened(directory: &Path) -> Workspace {
    let path = directory.join("shop.eggdb");
    let _ = std::fs::remove_file(&path);
    let mut workspace = Workspace::new(
        Arc::new(EngineConnector),
        None,
        directory.to_path_buf(),
        false,
    );
    workspace.open_typed(&path.display().to_string(), Instant::now());
    workspace.wait_for_open(LIMIT);
    assert!(matches!(workspace.screen(), Screen::Main(_)), "the file opened");
    workspace
}

fn run(workspace: &mut Workspace, sql: &str, all: bool) {
    workspace.run_text(sql, all, Instant::now());
    workspace.wait_for_run(LIMIT);
    assert!(!workspace.is_running());
}

fn window(workspace: &mut Workspace) -> &mut MainWindow {
    workspace.main_window_mut().expect("the main window shows")
}

fn chord(key: Key, shift: bool, primary: bool) -> KeyPress {
    KeyPress {
        key,
        modifiers: Modifiers {
            shift,
            control: primary,
            ..Modifiers::default()
        },
    }
}

fn key(workspace: &mut Workspace, press: KeyPress) {
    let _ = workspace.key(press, Instant::now());
}

fn frame(workspace: &mut Workspace) -> (Canvas, Theme) {
    let theme = Theme::of(ThemeKind::Light);
    (main_window::draw(window(workspace), theme, BOX_FACES), theme)
}

fn with_customers(directory: &Path) -> Workspace {
    let mut workspace = opened(directory);
    run(
        &mut workspace,
        "CREATE TABLE customers (id integer, name text);\nINSERT INTO customers VALUES (1, 'Ada'), (2, 'O''Hara\tGrace'), (3, NULL);",
        true,
    );
    workspace
}

#[test]
fn ctrl_enter_runs_the_statement_under_the_caret_and_shift_runs_them_all() {
    let directory = scratch("run");
    let mut workspace = with_customers(&directory);
    window(&mut workspace).tab.editor =
        Editor::new("SELECT 1;\nSELECT id, name FROM customers ORDER BY id;");
    key(&mut workspace, chord(Key::Enter, false, true));
    assert!(workspace.is_running() || window(&mut workspace).tab.grid.is_some());
    workspace.wait_for_run(LIMIT);
    let tab = &window(&mut workspace).tab;
    let grid = tab.grid.as_ref().expect("the grid shows the second statement's rows");
    assert_eq!(grid.columns, ["id", "name"]);
    assert_eq!(grid.rows.len(), 3);
    assert!(tab.summary.starts_with("3 rows · "), "{}", tab.summary);
    assert!(tab.status_tags.starts_with("SELECT 3 · "), "{}", tab.status_tags);
    let (canvas, theme) = frame(&mut workspace);
    assert_eq!(canvas.pixel(1000, 446 + 24 + 4), Some(theme.zebra), "the second row");
    assert_eq!(canvas.pixel(1000, 446 + 72 + 4), Some(theme.bg0), "no fourth row");
    let null_ink = (360..600)
        .filter(|&x| (446 + 48..446 + 72).any(|y| canvas.pixel(x, y) == Some(theme.text_disabled)))
        .count();
    assert!(null_ink > 0, "the third row's NULL is drawn in textDisabled");
    let tree = accessibility_tree(window(&mut workspace));
    let grid_node = tree.iter().find(|node| node.id == NODE_GRID).expect("the grid node");
    assert!(grid_node.value.as_deref().is_some_and(|value| value.starts_with("3 rows, 2 columns")));
    let cell = tree
        .iter()
        .find(|node| node.id == NODE_FIRST_CELL + 5)
        .expect("row 3, column 2");
    assert_eq!(cell.value.as_deref(), Some("null"));
    let status = tree.iter().find(|node| node.id == NODE_STATUS_LINE).unwrap();
    assert!(status.name.starts_with("3 rows, "), "{}", status.name);
    key(&mut workspace, chord(Key::Enter, true, true));
    workspace.wait_for_run(LIMIT);
    let summary = window(&mut workspace).tab.summary.clone();
    assert!(summary.starts_with("2 statements · 3 rows · "), "{summary}");
}

#[test]
fn the_grid_moves_selects_and_copies_tsv_and_inserts() {
    let directory = scratch("copy");
    let mut workspace = with_customers(&directory);
    run(&mut workspace, "SELECT id, name FROM customers ORDER BY id", false);
    key(&mut workspace, chord(Key::Character('3'), false, true));
    assert_eq!(window(&mut workspace).tab.focus, TabFocus::Grid);
    key(&mut workspace, chord(Key::Right, true, false));
    key(&mut workspace, chord(Key::Down, true, false));
    let tree = accessibility_tree(window(&mut workspace));
    let grid = tree.iter().find(|node| node.id == NODE_GRID).unwrap();
    assert!(grid.value.as_deref().is_some_and(|value| value.ends_with("2 rows by 2 columns selected")));
    let focused = tree.iter().find(|node| node.state.focused).expect("the current cell");
    assert_eq!(focused.value.as_deref(), Some("O'Hara\u{2409}Grace"));
    key(&mut workspace, chord(Key::Character('c'), false, true));
    assert_eq!(
        workspace.clipboard().text().as_deref(),
        Some("1\tAda\n2\tO'Hara\\tGrace\n")
    );
    assert_eq!(window(&mut workspace).tab.notice.as_deref(), Some("Copied 4 cells"));
    key(&mut workspace, chord(Key::Character('C'), true, true));
    assert_eq!(
        workspace.clipboard().text().as_deref(),
        Some("id\tname\n1\tAda\n2\tO'Hara\\tGrace\n")
    );
    key(&mut workspace, chord(Key::Character('I'), true, true));
    assert_eq!(
        workspace.clipboard().text().as_deref(),
        Some("INSERT INTO customers (id, name) VALUES (1, 'Ada');\nINSERT INTO customers (id, name) VALUES (2, 'O''Hara\tGrace');\n")
    );
    let (canvas, theme) = frame(&mut workspace);
    assert_eq!(canvas.pixel(315, 446 + 12), Some(theme.selection), "a selected cell");
    assert_eq!(canvas.pixel(270, 446 + 12), Some(theme.bg2), "a selected row's gutter");
    key(&mut workspace, chord(Key::Escape, false, false));
    key(&mut workspace, chord(Key::Tab, false, false));
    assert_ne!(window(&mut workspace).tab.focus, TabFocus::Grid, "Tab leaves the grid");
    workspace.tick(Instant::now() + Duration::from_secs(3));
    assert_eq!(window(&mut workspace).tab.notice, None, "the toast goes after 2 s");
}

#[test]
fn an_error_shows_the_squiggle_the_strip_and_the_engines_lines() {
    let directory = scratch("error");
    let mut workspace = opened(&directory);
    run(&mut workspace, "SELECT * FROM users", false);
    let tab = window(&mut workspace).tab.clone();
    let error = tab.error.clone().expect("the error shows inline");
    assert_eq!(
        error.strip_text(),
        "Error 42P01: relation \"users\" does not exist · Ln 1, Col 15"
    );
    assert_eq!(error.range, 14..19);
    assert_eq!((tab.summary.as_str(), tab.view), ("Error", ResultView::Messages));
    assert!(tab.messages.iter().any(|line| line == "SQLSTATE:  42P01"), "{:?}", tab.messages);
    assert!(tab.messages.iter().any(|line| line.starts_with("ERROR:  relation")));
    let (canvas, theme) = frame(&mut workspace);
    assert_eq!(canvas.pixel(1270, 370), Some(theme.bg1), "the strip");
    let squiggle = (432..472)
        .filter(|&x| (104..109).any(|y| canvas.pixel(x, y) == Some(theme.error)))
        .count();
    assert!(squiggle > 10, "the squiggle is under users");
    let tree = accessibility_tree(window(&mut workspace));
    let strip = tree.iter().find(|node| node.id == NODE_ERROR_STRIP).expect("the strip node");
    assert_eq!(strip.role, AccessRole::Status);
    key(&mut workspace, chord(Key::Escape, false, false));
    let (canvas, theme) = frame(&mut workspace);
    assert_eq!(canvas.pixel(1270, 370), Some(theme.bg0), "Esc hides the strip");
}

#[test]
fn a_result_past_the_fetch_limit_shows_the_banner() {
    let directory = scratch("banner");
    let mut workspace = opened(&directory);
    run(&mut workspace, "SELECT * FROM generate_series(1, 100001)", false);
    let summary = window(&mut workspace).tab.summary.clone();
    assert!(summary.starts_with("100,000 rows shown of 100,001 · "), "{summary}");
    let (canvas, theme) = frame(&mut workspace);
    assert_eq!(canvas.pixel(1100, 420), Some(theme.bg1), "the banner");
    assert_eq!(canvas.pixel(1100, 447), Some(theme.bg1), "the grid header moved to y 446");
    let tree = accessibility_tree(window(&mut workspace));
    let banner = tree.iter().find(|node| node.id == NODE_BANNER).expect("the banner node");
    assert_eq!(banner.role, AccessRole::Status);
    assert!(banner.name.starts_with("Showing the first 100,000 rows of 100,001."));
    let drawn = tree
        .iter()
        .filter(|node| node.parent == Some(NODE_GRID) && node.role == AccessRole::Group)
        .count();
    assert_eq!(drawn, 12, "only the rows in view are in the tree");
    key(&mut workspace, chord(Key::Character('3'), false, true));
    key(&mut workspace, chord(Key::End, false, true));
    let grid = window(&mut workspace).tab.grid.clone().unwrap();
    assert_eq!(grid.current.row, 99_999);
    assert!(grid.first_row > 99_980);
}

#[test]
fn a_running_statement_never_blocks_the_window_and_esc_stops_it() {
    let directory = scratch("stop");
    let mut workspace = opened(&directory);
    let started = Instant::now();
    workspace.run_text("SELECT pg_sleep(20)", false, started);
    assert!(started.elapsed() < Duration::from_secs(2), "the run went off the window's thread");
    assert!(workspace.is_running());
    let (canvas, theme) = frame(&mut workspace);
    assert_eq!(canvas.pixel(240, 20), Some(theme.bg1), "Run is disabled");
    key(&mut workspace, chord(Key::Enter, false, true));
    assert_eq!(
        window(&mut workspace).tab.notice.as_deref(),
        Some("Running… press Esc to stop")
    );
    key(&mut workspace, chord(Key::Escape, false, false));
    assert_eq!(window(&mut workspace).tab.summary, "Stopping…");
    workspace.wait_for_run(LIMIT);
    assert!(started.elapsed() < Duration::from_secs(15), "the engine stopped it");
    assert_eq!(window(&mut workspace).tab.summary, "Stopped · 0 rows (partial)");
}

#[test]
fn the_binary_runs_a_statement_into_the_grid() {
    let directory = scratch("binary");
    let output = directory.join("frame.png");
    let status = Command::new(env!("CARGO_BIN_EXE_eggdb-gui"))
        .arg("--png")
        .arg(&output)
        .args(["--box-text", "--open", "shop.eggdb", "--run", "SELECT 1 AS one"])
        .current_dir(&directory)
        .status()
        .expect("eggdb-gui runs");
    assert!(status.success());
    let frame = std::fs::read(&output).expect("eggdb-gui wrote the PNG");
    let empty = MainWindow::opened("shop", "shop.eggdb", true, Vec::new());
    let library = main_window::draw(&empty, Theme::of(ThemeKind::Light), BOX_FACES);
    assert_ne!(frame, encode_png(&library), "the run's grid and summary are drawn");
}
