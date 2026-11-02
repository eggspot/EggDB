//! The connect card acting for real (`docs/ux/workspace.md` 1 and its checklist): the
//! `eggdb-gui` binary opens a typed path with `--open` and its PNG equals the library's frame
//! of the outcome, the main window on the file or the card with the failure's lines; the
//! card's layout without an open-file dialog, its drag-over ring and its accessibility tree.

use std::path::{Path, PathBuf};
use std::process::Command;

use eggdb_gui::connect_screen::{
    self, CardError, ConnectCard, DEFAULT_HEIGHT, DEFAULT_WIDTH, Focus, NODE_BROWSE, NODE_ERROR,
    NODE_FILE, NODE_STATUS_LINE, TextField, accessibility_tree,
};
use eggdb_gui::main_window::{self, Faces, MainWindow};
use eggdb_gui::png::encode_png;
use eggdb_gui::{BoxFace, Theme, ThemeKind};
use eggdb_platform::window::{AccessNode, AccessRole};

const BOX_FACES: Faces<'static> = Faces {
    ui: &BoxFace,
    ui_bold: &BoxFace,
    mono: &BoxFace,
};
const SIZE: (u32, u32) = (DEFAULT_WIDTH, DEFAULT_HEIGHT);
const ORIGIN: (usize, usize) = (360, 140);

fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "eggdb-gui-workspace-frames-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&directory);
    directory
}

fn binary_png(directory: &Path, arguments: &[&str]) -> Vec<u8> {
    let output = directory.join("frame.png");
    let status = Command::new(env!("CARGO_BIN_EXE_eggdb-gui"))
        .arg("--png")
        .arg(&output)
        .arg("--box-text")
        .args(arguments)
        .current_dir(directory)
        .status()
        .expect("eggdb-gui runs");
    assert!(status.success(), "eggdb-gui {arguments:?} failed");
    std::fs::read(&output).expect("eggdb-gui wrote the PNG")
}

fn card_png(card: &ConnectCard) -> Vec<u8> {
    encode_png(&connect_screen::draw(
        card,
        Theme::of(ThemeKind::Light),
        BOX_FACES,
        SIZE,
        1.0,
    ))
}

fn node(nodes: &[AccessNode], id: u32) -> &AccessNode {
    nodes
        .iter()
        .find(|node| node.id == id)
        .unwrap_or_else(|| panic!("node {id} is in the tree"))
}

#[test]
fn a_typed_path_opens_the_main_window_on_the_file() {
    let directory = scratch("open");
    let path = directory.join("shop.eggdb");
    let _ = std::fs::remove_file(&path);
    let frame = binary_png(&directory, &["--open", "shop.eggdb"]);
    assert!(path.exists(), "Create is checked by default, so the file was created");
    let expected = MainWindow {
        database: "shop".to_owned(),
        location: "shop.eggdb".to_owned(),
        writable: true,
        tables: Vec::new(),
        selected_table: None,
        sql: String::new(),
        columns: Vec::new(),
        rows: Vec::new(),
        elapsed: String::new(),
        dialect: "PostgreSQL".to_owned(),
    };
    let library = main_window::draw(&expected, Theme::of(ThemeKind::Light), BOX_FACES);
    assert_eq!(frame, encode_png(&library), "the main window on shop.eggdb");
    let reopened = binary_png(&directory, &["--open", "shop.eggdb", "--no-create", "--read-only"]);
    let read_only = MainWindow {
        writable: false,
        ..expected
    };
    let library = main_window::draw(&read_only, Theme::of(ThemeKind::Light), BOX_FACES);
    assert_eq!(reopened, encode_png(&library), "the main window says read-only");
    let _ = std::fs::remove_file(&path);
}

fn missing_file_card(path: &str) -> ConnectCard {
    let mut file = TextField::selected(path);
    file.select_all();
    ConnectCard {
        file,
        create_if_missing: false,
        error: Some(CardError {
            line: format!("Error: cannot open {path}: no such file"),
            fix: Some(format!(
                "Try: check \"Create the file if it does not exist\", or eggdb create {path}"
            )),
        }),
        focus: Focus::FileInput,
        ..ConnectCard::first_start()
    }
}

#[test]
fn a_missing_file_with_create_unchecked_shows_the_two_error_lines() {
    let directory = scratch("missing");
    let path = directory.join("absent.eggdb");
    let _ = std::fs::remove_file(&path);
    let shown = path.display().to_string();
    let frame = binary_png(&directory, &["--open", &shown, "--no-create"]);
    assert!(!path.exists());
    let card = missing_file_card(&shown);
    assert_eq!(frame, card_png(&card), "the card with the failure's lines");
    let theme = Theme::of(ThemeKind::Light);
    let canvas = connect_screen::draw(&card, theme, BOX_FACES, SIZE, 1.0);
    let (cx, cy) = ORIGIN;
    assert_eq!(canvas.pixel(cx + 200, cy + 142 + 36), Some(theme.error), "error border");
    let line_ink = |top: usize, color| {
        (cx + 24..cx + 300)
            .filter(|&x| (top..top + 18).any(|y| canvas.pixel(x, y) == Some(color)))
            .count()
    };
    assert!(line_ink(cy + 100, theme.error) > 50, "the Error line");
    assert!(line_ink(cy + 118, theme.text_primary) > 50, "the Try line");
    let nodes = accessibility_tree(&card, BOX_FACES, SIZE);
    let error = node(&nodes, NODE_ERROR);
    assert_eq!(error.role, AccessRole::Status);
    assert!(error.name.starts_with("Error: cannot open "));
    assert!(error.name.contains("\nTry: check \"Create the file if it does not exist\""));
    let file = node(&nodes, NODE_FILE);
    assert!(file.state.focused);
    assert_eq!(file.value.as_deref(), Some(shown.as_str()));
    assert_eq!(card.file.selection(), Some((0, shown.chars().count())));
}

#[test]
fn without_an_open_file_dialog_browse_is_absent_and_the_input_is_512_wide() {
    let directory = scratch("no-dialog");
    let card = ConnectCard {
        file_dialog: false,
        ..ConnectCard::first_start()
    };
    let frame = binary_png(&directory, &["--no-file-dialog"]);
    assert_eq!(frame, card_png(&card));
    let nodes = accessibility_tree(&card, BOX_FACES, SIZE);
    assert!(nodes.iter().all(|node| node.id != NODE_BROWSE));
    assert_eq!(node(&nodes, NODE_FILE).bounds.width, 512.0);
    let with_dialog = accessibility_tree(&ConnectCard::first_start(), BOX_FACES, SIZE);
    let browse = node(&with_dialog, NODE_BROWSE);
    assert_eq!((browse.bounds.x, browse.bounds.y), (360.0 + 452.0, 140.0 + 142.0));
    assert_eq!(
        eggdb_platform::window::has_file_dialog(),
        cfg!(any(target_os = "macos", windows))
    );
}

#[test]
fn a_drag_over_the_window_draws_the_accent_border_and_drop_to_open() {
    let theme = Theme::of(ThemeKind::Light);
    let card = ConnectCard {
        drag_over: true,
        ..ConnectCard::first_start()
    };
    let canvas = connect_screen::draw(&card, theme, BOX_FACES, SIZE, 1.0);
    let (cx, cy) = ORIGIN;
    assert_eq!(canvas.pixel(cx + 280, cy), Some(theme.accent), "outer border");
    assert_eq!(canvas.pixel(cx + 280, cy + 1), Some(theme.accent), "2 px wide");
    let hint = (cx + 400..cx + 536)
        .filter(|&x| (cy + 28..cy + 46).any(|y| canvas.pixel(x, y) == Some(theme.accent)))
        .count();
    assert!(hint > 30, "Drop to open is drawn");
    let plain = connect_screen::draw(&ConnectCard::first_start(), theme, BOX_FACES, SIZE, 1.0);
    assert_ne!(plain.pixel(cx + 280, cy + 1), Some(theme.accent));
}

#[test]
fn while_an_open_shows_progress_every_control_is_disabled_but_cancel() {
    let mut card = ConnectCard::first_start();
    card.begin_opening();
    card.progress = Some("Opening quickstart.eggdb…".to_owned());
    let nodes = accessibility_tree(&card, BOX_FACES, SIZE);
    let status = node(&nodes, NODE_STATUS_LINE);
    assert_eq!(status.role, AccessRole::Status);
    assert_eq!(status.name, "Opening quickstart.eggdb…");
    for control in nodes.iter().filter(|node| {
        matches!(
            node.role,
            AccessRole::TextField | AccessRole::CheckBox | AccessRole::Link | AccessRole::Tab
        )
    }) {
        assert!(control.state.disabled, "{} is disabled", control.name);
    }
    let cancel = nodes
        .iter()
        .find(|node| node.name == "Cancel")
        .expect("Cancel is in the tree");
    assert!(!cancel.state.disabled, "Cancel stops the open");
}
