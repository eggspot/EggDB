//! The object browser and the inspector on a real file (`docs/ux/workspace.md` 4 and its
//! checklist): the tree lists every model the file has in order with its counts, filters,
//! answers its keys, opens a table in a new query tab, and the inspector docks from 1440 px
//! with the table's columns, indexes, DDL and sample rows.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eggdb::Database;
use eggdb::sql::Dialect;
use eggdb_gui::connection::EngineConnector;
use eggdb_gui::inspector;
use eggdb_gui::main_window::{
    self, Faces, MainWindow, NODE_DDL, NODE_FILTER, NODE_FIRST_BLOCK, NODE_INSPECTOR, NODE_OBJECTS, WindowFocus,
    accessibility_tree,
};
use eggdb_gui::object_browser::{Group, TreeItem};
use eggdb_gui::png::encode_png;
use eggdb_gui::workspace::{Screen, Workspace};
use eggdb_gui::{BoxFace, Theme, ThemeKind};
use eggdb_platform::window::{AccessNode, AccessRole, Key, KeyPress, Modifiers};

const BOX_FACES: Faces<'static> = Faces {
    ui: &BoxFace,
    ui_bold: &BoxFace,
    mono: &BoxFace,
};
const LIMIT: Duration = Duration::from_secs(60);

fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "eggdb-gui-object-browser-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&directory);
    directory
}

fn make_shop(path: &Path) {
    let _ = std::fs::remove_file(path);
    let database = Database::open(path).expect("the file opens");
    {
        let store = database.sql_store();
        let mut session = store.session(Dialect::PostgreSql);
        session
            .execute(
                "CREATE TABLE customers (id integer PRIMARY KEY, name text NOT NULL);
                 CREATE TABLE orders (id integer PRIMARY KEY, customer_id integer, total numeric);
                 CREATE TABLE products (id integer PRIMARY KEY, name text, price numeric);
                 CREATE INDEX orders_customer_id_idx ON orders (customer_id);
                 CREATE UNIQUE INDEX products_name_key ON products (name);
                 CREATE INDEX customers_name_idx ON customers (name);
                 CREATE VIEW big_orders AS SELECT id, total FROM orders WHERE total > 100;
                 CREATE SEQUENCE invoice_numbers;
                 CREATE SEQUENCE ticket_numbers;
                 INSERT INTO customers VALUES (1, 'Ada'), (2, 'Grace');
                 INSERT INTO orders VALUES (1, 1, 50), (2, 1, 150), (3, 2, 250);",
                &mut |_| {},
            )
            .expect("the schema is made");
        let _ = session.close();
    }
    database
        .write(|transaction| {
            transaction.put("greeting", "hello")?;
            transaction.create_collection("sessions")?;
            let mut sessions = transaction.collection("sessions")?;
            sessions.put("s1", "alice")?;
            Ok(())
        })
        .expect("the keys are written");
}

fn opened(directory: &Path) -> Workspace {
    let path = directory.join("shop.eggdb");
    make_shop(&path);
    let mut workspace = Workspace::new(Arc::new(EngineConnector), None, directory.to_path_buf(), false);
    workspace.open_typed(&path.display().to_string(), Instant::now());
    workspace.wait_for_open(LIMIT);
    assert!(matches!(workspace.screen(), Screen::Main(_)), "the file opened");
    workspace
}

fn window(workspace: &mut Workspace) -> &mut MainWindow {
    workspace.main_window_mut().expect("the main window shows")
}

fn press(workspace: &mut Workspace, key: Key) {
    let _ = workspace.key(KeyPress::plain(key), Instant::now());
}

fn chord(workspace: &mut Workspace, key: Key, shift: bool, primary: bool) {
    let press = KeyPress {
        key,
        modifiers: Modifiers {
            shift,
            control: primary,
            ..Modifiers::default()
        },
    };
    let _ = workspace.key(press, Instant::now());
}

fn labels(window: &MainWindow) -> Vec<String> {
    window.tree.rows().into_iter().map(|row| row.label).collect()
}

fn node(nodes: &[AccessNode], id: u32) -> &AccessNode {
    nodes.iter().find(|node| node.id == id).unwrap_or_else(|| panic!("node {id} is in the tree"))
}

#[test]
fn the_tree_lists_every_model_in_order_with_counts_and_filters() {
    let directory = scratch("groups");
    let mut workspace = opened(&directory);
    let shown = window(&mut workspace).tree.rows();
    let pairs: Vec<(String, Option<String>)> = shown.iter().map(|row| (row.label.clone(), row.detail.clone())).collect();
    let expected: Vec<(String, Option<String>)> = [
        ("shop", None),
        ("Tables", Some("3")),
        ("customers", None),
        ("orders", None),
        ("products", None),
        ("Views", Some("1")),
        ("Indexes", Some("3")),
        ("Sequences", Some("2")),
        ("Keys", Some("2")),
    ]
    .into_iter()
    .map(|(label, detail)| (label.to_owned(), detail.map(str::to_owned)))
    .collect();
    assert_eq!(pairs, expected);
    let theme = Theme::of(ThemeKind::Light);
    chord(&mut workspace, Key::Character('1'), false, true);
    chord(&mut workspace, Key::Tab, true, false);
    assert_eq!(window(&mut workspace).focus, WindowFocus::Filter);
    for letter in "ord".chars() {
        press(&mut workspace, Key::Character(letter));
    }
    let rows = window(&mut workspace).tree.rows();
    assert_eq!(rows[1].label, "Tables");
    assert_eq!(rows[1].detail.as_deref(), Some("1 of 3"));
    assert!(labels(window(&mut workspace)).contains(&"orders".to_owned()));
    assert!(!labels(window(&mut workspace)).contains(&"customers".to_owned()));
    let filtered = main_window::draw(window(&mut workspace), theme, BOX_FACES);
    assert_eq!(filtered.pixel(6, 50), Some(theme.focus_ring), "the filter has the ring");
    let tree = accessibility_tree(window(&mut workspace));
    assert_eq!(node(&tree, NODE_FILTER).value.as_deref(), Some("ord"));
    assert!(node(&tree, NODE_FILTER).state.focused);
    press(&mut workspace, Key::Escape);
    assert_eq!(window(&mut workspace).focus, WindowFocus::Tree);
    assert_eq!(window(&mut workspace).tree.filter, "");
}

#[test]
fn keys_move_expand_type_ahead_and_open_a_table_in_a_new_tab() {
    let directory = scratch("keys");
    let mut workspace = opened(&directory);
    chord(&mut workspace, Key::Character('1'), false, true);
    press(&mut workspace, Key::Down);
    press(&mut workspace, Key::Left);
    assert_eq!(labels(window(&mut workspace))[2], "Views", "Left collapsed Tables");
    press(&mut workspace, Key::Right);
    assert_eq!(labels(window(&mut workspace))[2], "customers", "Right expanded it");
    press(&mut workspace, Key::Down);
    press(&mut workspace, Key::Down);
    assert_eq!(window(&mut workspace).tree.current, TreeItem::Table(1));
    let tree = accessibility_tree(window(&mut workspace));
    let rows: Vec<&AccessNode> = tree.iter().filter(|node| node.parent == Some(NODE_OBJECTS)).collect();
    let orders = rows.iter().find(|node| node.name == "orders").expect("orders is a tree row");
    assert!(orders.state.focused);
    assert_eq!(orders.value.as_deref(), Some("level 3, 2 of 3, collapsed"));
    assert_eq!(rows[0].value.as_deref(), Some("read-write, level 1, 1 of 1, expanded"));
    press(&mut workspace, Key::Enter);
    workspace.wait_for_run(LIMIT);
    let shown = window(&mut workspace);
    assert_eq!(shown.tab.title, "Query 2");
    assert_eq!(shown.background_tabs.len(), 1);
    assert_eq!(shown.tab.editor.text(), "SELECT * FROM orders LIMIT 100;");
    assert_eq!(shown.tab.grid.as_ref().map(|grid| grid.rows.len()), Some(3));
    chord(&mut workspace, Key::Enter, true, false);
    assert_eq!(window(&mut workspace).tab.editor.text(), "SELECT * FROM orders LIMIT 100;orders");
    press(&mut workspace, Key::Character('p'));
    press(&mut workspace, Key::Character('r'));
    assert_eq!(window(&mut workspace).tree.current, TreeItem::Table(2), "`pr` moves to products");
    press(&mut workspace, Key::End);
    assert_eq!(window(&mut workspace).tree.current, TreeItem::Group(Group::Keys));
    press(&mut workspace, Key::Tab);
    assert_eq!(window(&mut workspace).focus, WindowFocus::Tab);
}

#[test]
fn ddl_in_the_query_tab_refreshes_the_tree() {
    let directory = scratch("refresh");
    let mut workspace = opened(&directory);
    workspace.run_text("CREATE TABLE zebras (id integer)", false, Instant::now());
    workspace.wait_for_run(LIMIT);
    let shown = labels(window(&mut workspace));
    assert!(shown.contains(&"zebras".to_owned()), "{shown:?}");
    assert_eq!(window(&mut workspace).tree.rows()[1].detail.as_deref(), Some("4"));
}

#[test]
fn the_inspector_docks_from_1440_px_and_shows_the_selected_table() {
    let directory = scratch("inspector");
    let mut workspace = opened(&directory);
    let theme = Theme::of(ThemeKind::Light);
    assert!(!window(&mut workspace).inspector.is_open(), "closed at 1280");
    window(&mut workspace).set_width(1440);
    assert!(window(&mut workspace).inspector.docked);
    chord(&mut workspace, Key::Character('1'), false, true);
    press(&mut workspace, Key::Down);
    press(&mut workspace, Key::Down);
    press(&mut workspace, Key::Down);
    press(&mut workspace, Key::Space);
    let shown = window(&mut workspace);
    assert_eq!(shown.tree.selected, Some(TreeItem::Table(1)));
    let sample = shown.inspector.sample.clone().expect("the sample was fetched");
    assert_eq!((sample.of.as_str(), sample.row_count, sample.rows.rows.len()), ("orders", Some(3), 3));
    let description = inspector::describe(&shown.tree, TreeItem::Table(1), Some(&sample));
    assert_eq!(description.subtitle, "Table · 3 rows");
    let headings: Vec<String> = description.blocks().into_iter().map(|(_, heading)| heading).collect();
    assert_eq!(headings, ["Columns (3)", "Indexes (1)", "DDL", "Sample rows"]);
    assert!(description.ddl.as_deref().is_some_and(|ddl| ddl.starts_with("CREATE TABLE")));
    let docked = main_window::draw(shown, theme, BOX_FACES);
    assert_eq!(docked.width(), 1440);
    assert_eq!(docked.pixel(1160, 400), Some(theme.border), "the inspector's left border");
    assert_eq!(docked.pixel(1300, 60), Some(theme.bg1), "the inspector's background");
    assert_eq!(docked.pixel(1100, 250), Some(theme.bg0), "the editor ends before it");
    let tree = accessibility_tree(shown);
    let panel = node(&tree, NODE_INSPECTOR);
    assert_eq!((panel.bounds.x, panel.bounds.y, panel.bounds.width, panel.bounds.height), (1160.0, 44.0, 280.0, 732.0));
    assert_eq!(node(&tree, NODE_FIRST_BLOCK).name, "Columns (3)");
    assert_eq!(node(&tree, NODE_FIRST_BLOCK).value.as_deref(), Some("expanded"));
    assert!(node(&tree, NODE_DDL).state.disabled, "the DDL is read-only");
    assert_eq!(node(&tree, NODE_DDL).role, AccessRole::TextField);
    window(&mut workspace).set_width(1280);
    assert!(!window(&mut workspace).inspector.is_open(), "closed again at 1280");
    let closed = main_window::draw(window(&mut workspace), theme, BOX_FACES);
    chord(&mut workspace, Key::Character('i'), false, true);
    assert!(window(&mut workspace).inspector.overlay);
    let overlay = main_window::draw(window(&mut workspace), theme, BOX_FACES);
    assert_eq!(overlay.width(), 1280);
    assert_eq!(overlay.pixel(1000, 400), Some(theme.border), "the overlay's left border");
    assert_eq!(overlay.pixel(1100, 250), Some(theme.bg1), "the overlay covers the editor");
    assert_ne!(overlay.pixel(995, 250), closed.pixel(995, 250), "the shadow darkens the editor");
    press(&mut workspace, Key::Escape);
    assert!(!window(&mut workspace).inspector.is_open(), "Esc closes the overlay");
}

#[test]
fn the_binary_draws_the_docked_inspector_at_1440() {
    let directory = scratch("binary");
    let path = directory.join("shop.eggdb");
    make_shop(&path);
    let output = directory.join("frame.png");
    let status = Command::new(env!("CARGO_BIN_EXE_eggdb-gui"))
        .arg("--png")
        .arg(&output)
        .args(["--box-text", "--open", "shop.eggdb", "--size", "1440x800"])
        .current_dir(&directory)
        .status()
        .expect("eggdb-gui runs");
    assert!(status.success());
    let frame = std::fs::read(&output).expect("eggdb-gui wrote the PNG");
    let mut workspace = Workspace::new(Arc::new(EngineConnector), None, directory.clone(), false);
    workspace.open_typed("shop.eggdb", Instant::now());
    workspace.wait_for_open(LIMIT);
    window(&mut workspace).set_width(1440);
    let library = main_window::draw(window(&mut workspace), Theme::of(ThemeKind::Light), BOX_FACES);
    assert_eq!(frame, encode_png(&library), "the binary's frame is the library's");
}
