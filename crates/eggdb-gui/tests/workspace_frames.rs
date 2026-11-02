//! The connect card acting for real (`docs/ux/workspace.md` 1 and its checklist): the
//! `eggdb-gui` binary opens a typed path with `--open` and its PNG equals the library's frame
//! of the outcome, the main window on the file or the card with the failure's lines; the
//! card's layout without an open-file dialog, its drag-over ring and its accessibility tree.

use std::path::{Path, PathBuf};
use std::process::Command;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eggdb_gui::connect_screen::{
    self, CardError, ConnectCard, DEFAULT_HEIGHT, DEFAULT_WIDTH, Focus, NODE_BROWSE,
    NODE_CONNECT_TO_A_SERVER, NODE_DATABASE, NODE_ERROR, NODE_FILE, NODE_HOST, NODE_OPEN,
    NODE_PASSWORD, NODE_PORT, NODE_PROTOCOL, NODE_STATUS_LINE, NODE_TLS_WARNING, NODE_URL,
    NODE_USE_TLS, NODE_USER, Segment, TLS_WARNING, TextField, accessibility_tree,
};
use eggdb_gui::connection::EngineConnector;
use eggdb_gui::main_window::{self, Faces, MainWindow};
use eggdb_gui::png::encode_png;
use eggdb_gui::workspace::{Screen, Workspace};
use eggdb_gui::{BoxFace, Theme, ThemeKind};
use eggdb_pgwire::{ScramVerifier, ServerConfig};
use eggdb_platform::window::{AccessNode, AccessRole, Key, KeyPress, Modifiers};

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
    let expected = MainWindow::opened("shop", "shop.eggdb", true, Vec::new());
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

const SERVER_USER: &str = "ada";
const SERVER_PASSWORD: &str = "correct horse";

struct TestServer {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn start_server() -> TestServer {
    let listener = eggdb_pgwire::listen_on_loopback("127.0.0.1:0".parse().expect("an address"))
        .expect("a loopback listener");
    let port = listener.local_addr().expect("its address").port();
    let stop = Arc::new(AtomicBool::new(false));
    let config = ServerConfig {
        user: SERVER_USER.to_owned(),
        verifier: ScramVerifier::derive(SERVER_PASSWORD.as_bytes(), b"0123456789abcdef", 4096),
        database: "shop".to_owned(),
        max_connections: 8,
        authentication_timeout: Duration::from_secs(5),
        idle_timeout: None,
        memory_budget: Arc::default(),
        auth_rules: None,
        tls: None,
    };
    let thread_stop = Arc::clone(&stop);
    std::thread::spawn(move || {
        let database =
            eggdb::Database::in_memory_with(eggdb::Options::new()).expect("a database in memory");
        let store = database.sql_store();
        let stopped = move || thread_stop.load(Ordering::Acquire);
        let _ = eggdb_pgwire::serve(&store, &listener, &config, &stopped, &|_: &str| {});
    });
    TestServer { port, stop }
}

fn shift_tab() -> KeyPress {
    KeyPress {
        key: Key::Tab,
        modifiers: Modifiers {
            shift: true,
            ..KeyPress::plain(Key::Tab).modifiers
        },
    }
}

fn press(workspace: &mut Workspace, key: Key) {
    workspace.key(KeyPress::plain(key), Instant::now());
}

fn type_text(workspace: &mut Workspace, text: &str) {
    for character in text.chars() {
        press(workspace, Key::Character(character));
    }
}

fn server_workspace() -> Workspace {
    let mut workspace = Workspace::new(
        Arc::new(EngineConnector),
        None,
        std::env::temp_dir(),
        true,
    );
    workspace.key(shift_tab(), Instant::now());
    press(&mut workspace, Key::Right);
    press(&mut workspace, Key::Tab);
    workspace
}

fn connect(workspace: &mut Workspace, url: &str, password: &str) {
    type_text(workspace, url);
    for _ in 0..6 {
        press(workspace, Key::Tab);
    }
    assert_eq!(
        workspace.card_mut().map(|card| card.focus),
        Some(Focus::Password)
    );
    type_text(workspace, password);
    press(workspace, Key::Enter);
    workspace.wait_for_open(Duration::from_secs(15));
}

fn shown_card(workspace: &mut Workspace) -> ConnectCard {
    workspace.card_mut().cloned().expect("the card is still shown")
}

#[test]
fn the_server_segment_fills_its_fields_from_the_typed_url() {
    let mut workspace = server_workspace();
    type_text(&mut workspace, "postgresql://ada@db:5433/shop?sslmode=disable");
    let card = shown_card(&mut workspace);
    assert_eq!(card.segment, Segment::Server);
    assert_eq!(card.focus, Focus::Url);
    let nodes = accessibility_tree(&card, BOX_FACES, SIZE);
    let value = |id| node(&nodes, id).value.clone().unwrap_or_default();
    assert_eq!(value(NODE_HOST), "db");
    assert_eq!(value(NODE_PORT), "5433");
    assert_eq!(value(NODE_DATABASE), "shop");
    assert_eq!(value(NODE_USER), "ada");
    assert_eq!(value(NODE_PROTOCOL), "PostgreSQL protocol");
    assert!(node(&nodes, NODE_URL).state.focused);
    assert!(node(&nodes, NODE_CONNECT_TO_A_SERVER).state.selected);
    assert_eq!(node(&nodes, NODE_USE_TLS).state.checked, Some(false));
    assert_eq!(node(&nodes, NODE_TLS_WARNING).name, TLS_WARNING);
    assert_eq!(node(&nodes, NODE_OPEN).name, "Connect");
    let url = node(&nodes, NODE_URL);
    assert_eq!((url.bounds.x, url.bounds.y, url.bounds.width), (384.0, 282.0, 512.0));
    let theme = Theme::of(ThemeKind::Light);
    let canvas = connect_screen::draw(&card, theme, BOX_FACES, SIZE, 1.0);
    let (cx, cy) = ORIGIN;
    assert_eq!(canvas.pixel(cx + 290, cy + 80), Some(theme.bg2), "the server segment is active");
    assert_eq!(canvas.pixel(cx + 40, cy + 80), Some(theme.bg0), "the file segment is not");
    assert_eq!(canvas.pixel(cx + 300, cy + 142), Some(theme.accent), "the focused URL input");
    let warning = (cx + 24..cx + 356)
        .filter(|&x| (cy + 406..cy + 424).any(|y| canvas.pixel(x, y) == Some(theme.warning)))
        .count();
    assert!(warning > 50, "the TLS warning is drawn");
    let mut checked = card.clone();
    checked.server.toggle_tls();
    let nodes = accessibility_tree(&checked, BOX_FACES, SIZE);
    assert!(nodes.iter().all(|node| node.id != NODE_TLS_WARNING));
    assert_eq!(checked.server.url.value(), "postgresql://ada@db:5433/shop");
}

#[test]
fn connecting_to_a_running_server_opens_the_main_window_on_its_database() {
    let server = start_server();
    let mut workspace = server_workspace();
    let url = format!("postgresql://{SERVER_USER}@127.0.0.1:{}/shop?sslmode=disable", server.port);
    connect(&mut workspace, &url, SERVER_PASSWORD);
    let place = format!("127.0.0.1:{}", server.port);
    match workspace.screen() {
        Screen::Main(window) => {
            assert_eq!(window.database, "shop");
            assert_eq!(window.location, place);
            assert!(window.tab.editor.text().is_empty());
        }
        Screen::Connect(card) => panic!("still on the card: {:?}", card.error),
    }
    let connection = workspace.connection_mut().expect("the connection is held");
    assert!(connection.is_server());
    assert_eq!(connection.status_text(), format!("Connected · {place} · no TLS"));
    assert_eq!(connection.toast(), "Connected to 127.0.0.1");
    let tables = connection.run("SELECT 1 AS one").expect("the session answers");
    assert_eq!(tables.len(), 1);
}

#[test]
fn a_refused_port_shows_the_error_lines_and_focuses_host() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let port = listener.local_addr().expect("its address").port();
    drop(listener);
    let mut workspace = server_workspace();
    connect(
        &mut workspace,
        &format!("postgresql://ada@127.0.0.1:{port}/shop?sslmode=disable"),
        "",
    );
    let card = shown_card(&mut workspace);
    let error = card.error.clone().expect("the error lines");
    assert_eq!(
        error.line,
        format!("Error: cannot connect to 127.0.0.1:{port}: connection refused")
    );
    assert_eq!(
        error.fix.as_deref(),
        Some("Try: check that the server is running and the port is open")
    );
    assert_eq!(card.focus, Focus::Host);
    assert_eq!(card.server.host.selection(), Some((0, "127.0.0.1".len())));
    let theme = Theme::of(ThemeKind::Light);
    let canvas = connect_screen::draw(&card, theme, BOX_FACES, SIZE, 1.0);
    let (cx, cy) = ORIGIN;
    assert_eq!(
        canvas.pixel(cx + 300, cy + 208 + 36),
        Some(theme.error),
        "the Host input has the error border, moved down by the two lines"
    );
    assert_ne!(canvas.pixel(cx + 100, cy + 142 + 36), Some(theme.error), "the URL does not");
    let nodes = accessibility_tree(&card, BOX_FACES, SIZE);
    assert!(node(&nodes, NODE_HOST).state.focused);
    assert!(node(&nodes, NODE_ERROR).name.contains("connection refused"));
}

#[test]
fn tls_a_wrong_password_and_a_missing_database_say_the_engine_lines() {
    let server = start_server();
    let base = format!("postgresql://{SERVER_USER}@127.0.0.1:{}", server.port);
    let mut tls = server_workspace();
    connect(&mut tls, &format!("{base}/shop"), SERVER_PASSWORD);
    let card = shown_card(&mut tls);
    let error = card.error.expect("the TLS failure");
    assert_eq!(
        error.line,
        format!("Error: cannot connect to 127.0.0.1:{}: the server does not offer TLS", server.port)
    );
    assert_eq!(
        error.fix.as_deref(),
        Some("Try: uncheck \"Use TLS\" only on a network you trust")
    );
    let mut wrong = server_workspace();
    connect(&mut wrong, &format!("{base}/shop?sslmode=disable"), "not it");
    let card = shown_card(&mut wrong);
    let error = card.error.clone().expect("the authentication failure");
    assert_eq!(
        error.line,
        format!("Error 28P01: password authentication failed for user \"{SERVER_USER}\"")
    );
    assert_eq!(error.fix.as_deref(), Some("Try: check the user name and password"));
    assert_eq!(card.focus, Focus::Password);
    let nodes = accessibility_tree(&card, BOX_FACES, SIZE);
    assert!(
        node(&nodes, NODE_PASSWORD).value.as_deref().is_some_and(|value| !value.contains("not it")),
        "the password is never in the tree"
    );
    let mut missing = server_workspace();
    connect(&mut missing, &format!("{base}/nowhere?sslmode=disable"), SERVER_PASSWORD);
    let card = shown_card(&mut missing);
    let error = card.error.clone().expect("the missing database");
    assert!(
        error.line.starts_with("Error 3D000: database \"nowhere\" does not exist"),
        "{}",
        error.line
    );
    assert_eq!(
        error.fix.as_deref(),
        Some("Try: check the database name, or leave it empty for the default")
    );
    assert_eq!(card.focus, Focus::Database);
}

#[test]
fn a_url_that_does_not_parse_fails_at_once_and_focuses_the_url() {
    let mut workspace = server_workspace();
    type_text(&mut workspace, "postgresql://db:port/shop");
    press(&mut workspace, Key::Enter);
    assert!(!workspace.is_opening(), "nothing is started");
    let card = shown_card(&mut workspace);
    let error = card.error.clone().expect("the URL failure");
    assert_eq!(error.line, "Error: the URL is not a database URL");
    assert_eq!(error.fix.as_deref(), Some("Try: postgresql://user@host:5432/database"));
    assert_eq!(card.focus, Focus::Url);
    assert_eq!(card.server.url.selection(), Some((0, "postgresql://db:port/shop".len())));
}
