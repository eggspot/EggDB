//! The plan view on a real file and on a stub plan (`docs/ux/workspace.md` 5 and its
//! checklist): `Ctrl+E` and `Ctrl+Shift+E` explain the statement under the caret, the tree is
//! drawn centred at the top of the results body with its links, bars and heat borders, the
//! keys walk and select nodes, and the accessibility tree names every node.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eggdb_gui::connection::EngineConnector;
use eggdb_gui::dialect::QueryDialect;
use eggdb_gui::editor::Editor;
use eggdb_gui::main_window::{
    self, Faces, MainWindow, NODE_FIRST_PLAN_NODE, NODE_PLAN, NODE_RESULTS_SEGMENT,
    accessibility_tree,
};
use eggdb_gui::plan_view::{Heat, NODE_HEIGHT, Plan, PlanView, TooltipCause};
use eggdb_gui::query_tab::{PLAN_PANEL, ResultView, TabFocus};
use eggdb_gui::workspace::{Screen, Workspace};
use eggdb_gui::{BoxFace, Canvas, Rect, Theme, ThemeKind};
use eggdb_platform::window::{Key, KeyPress, Modifiers, PointerButton};

const BOX_FACES: Faces<'static> = Faces {
    ui: &BoxFace,
    ui_bold: &BoxFace,
    mono: &BoxFace,
};
const LIMIT: Duration = Duration::from_secs(60);
const WINDOW: (u32, u32) = (1280, 800);
const PLAN_AREA: Rect = Rect::new(260.0, 418.0, PLAN_PANEL.0, PLAN_PANEL.1);
const STUB_STATEMENT: &str = "SELECT * FROM orders o JOIN customers c ON c.id = o.customer_id";
const STUB_PLAN: &str = "Hash Join  (cost=0.00..100.00 rows=10 width=8)
  ->  Seq Scan on orders o  (cost=0.00..20.00 rows=10 width=4)
  ->  Seq Scan on customers c  (cost=0.00..62.00 rows=4 width=4)";

fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("eggdb-gui-plan-view-{}-{name}", std::process::id()));
    let _ = std::fs::create_dir_all(&directory);
    directory
}

fn with_customers(directory: &Path) -> Workspace {
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
    assert!(
        matches!(workspace.screen(), Screen::Main(_)),
        "the file opened"
    );
    workspace.run_text(
        "CREATE TABLE customers (id integer PRIMARY KEY, name text);\nINSERT INTO customers VALUES (1, 'Ada'), (2, 'Grace'), (3, 'Edsger');",
        true,
        Instant::now(),
    );
    workspace.wait_for_run(LIMIT);
    workspace
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

fn draw(window: &MainWindow) -> (Canvas, Theme) {
    let theme = Theme::of(ThemeKind::Light);
    (main_window::draw(window, theme, BOX_FACES), theme)
}

fn pixel(canvas: &Canvas, x: f32, y: f32) -> Option<eggdb_gui::Rgba> {
    canvas.pixel(x.floor() as usize, y.floor() as usize)
}

fn stub_window() -> MainWindow {
    let mut window = MainWindow::sample();
    window.tab.editor = Editor::new(STUB_STATEMENT);
    let plan =
        Plan::from_postgresql_text(&STUB_PLAN.lines().map(str::to_owned).collect::<Vec<_>>())
            .expect("the stub reads");
    window.tab.plan = Some(PlanView::new(
        plan,
        0..STUB_STATEMENT.len(),
        STUB_STATEMENT.to_owned(),
    ));
    window.tab.view = ResultView::Plan;
    window
}

#[test]
fn ctrl_e_shows_the_index_scan_centred_at_the_top_without_a_cost_row() {
    let directory = scratch("explain");
    let mut workspace = with_customers(&directory);
    window(&mut workspace).tab.editor = Editor::new("SELECT * FROM customers WHERE id = 2");
    let _ = workspace.key(chord(Key::Character('e'), false, true), Instant::now());
    workspace.wait_for_run(LIMIT);
    let tab = &window(&mut workspace).tab;
    assert_eq!(tab.view, ResultView::Plan, "{:?}", tab.messages);
    let plan = tab.plan.as_ref().expect("the plan shows");
    assert_eq!(plan.plan.nodes.len(), 1);
    assert_eq!(
        plan.plan.nodes[0].operation,
        "Index Scan using customers_pkey on customers"
    );
    assert!(!plan.plan.analyze);
    assert_eq!(
        plan.plan.facts(0),
        "",
        "the engine gives no costs, so no cost row"
    );
    let rect = plan.node_rect(0, PLAN_AREA).unwrap();
    assert!(
        (rect.x + rect.w / 2.0 - (260.0 + PLAN_PANEL.0 / 2.0)).abs() <= 1.0,
        "centred"
    );
    assert_eq!(rect.y, 434.0, "at the top of the results body");
    let (canvas, theme) = draw(window(&mut workspace));
    assert_eq!(
        pixel(&canvas, rect.x + rect.w / 2.0, rect.y),
        Some(theme.border_control)
    );
    assert_eq!(
        pixel(&canvas, rect.x + rect.w - 20.0, rect.y + 35.0),
        Some(theme.bg2),
        "an empty bar"
    );
    let tree = accessibility_tree(window(&mut workspace));
    let segment = tree
        .iter()
        .find(|node| node.id == NODE_RESULTS_SEGMENT + 2)
        .expect("the Plan segment");
    assert_eq!(segment.name, "Plan");
    assert!(segment.state.selected);
    let node = tree
        .iter()
        .find(|node| node.id == NODE_FIRST_PLAN_NODE)
        .expect("the plan's node");
    assert_eq!(node.parent, Some(NODE_PLAN));
    assert_eq!(node.name, "Index Scan using customers_pkey on customers");
}

#[test]
fn ctrl_shift_e_measures_the_statement_and_rolls_a_write_back() {
    let directory = scratch("analyze");
    let mut workspace = with_customers(&directory);
    window(&mut workspace).tab.editor = Editor::new("SELECT * FROM customers");
    let _ = workspace.key(chord(Key::Character('E'), true, true), Instant::now());
    workspace.wait_for_run(LIMIT);
    let tab = &window(&mut workspace).tab;
    let plan = tab.plan.as_ref().expect("the analyzed plan shows");
    assert!(plan.plan.analyze);
    assert!(plan.plan.execution_ms.is_some());
    let facts = plan.plan.facts(0);
    assert!(facts.contains(" ms · rows 3"), "{facts}");
    workspace.explain_text(
        "UPDATE customers SET name = 'changed'",
        true,
        Instant::now(),
    );
    workspace.wait_for_run(LIMIT);
    let tab = &window(&mut workspace).tab;
    assert_eq!(tab.view, ResultView::Plan, "{:?}", tab.messages);
    assert_eq!(tab.messages.first().map(String::as_str), Some("BEGIN"));
    assert_eq!(tab.messages.last().map(String::as_str), Some("ROLLBACK"));
    workspace.run_text(
        "SELECT name FROM customers WHERE id = 1",
        false,
        Instant::now(),
    );
    workspace.wait_for_run(LIMIT);
    let grid = window(&mut workspace).tab.grid.clone().expect("the rows");
    assert_eq!(
        grid.rows[0][0].as_deref(),
        Some("Ada"),
        "the UPDATE was rolled back"
    );
}

#[test]
fn oracle_explains_through_dbms_xplan_and_keeps_no_plan_table_row() {
    let directory = scratch("oracle");
    let mut workspace = with_customers(&directory);
    window(&mut workspace).tab.set_dialect(QueryDialect::Oracle);
    workspace.explain_text("SELECT * FROM customers", false, Instant::now());
    workspace.wait_for_run(LIMIT);
    let tab = &window(&mut workspace).tab;
    assert_eq!(tab.view, ResultView::Plan, "{:?}", tab.messages);
    let plan = tab.plan.as_ref().expect("the plan shows");
    assert_eq!(plan.plan.nodes[0].operation, "SELECT STATEMENT");
    assert_eq!(plan.plan.nodes[1].operation, "TABLE ACCESS FULL CUSTOMERS");
    assert_eq!(plan.plan.nodes[1].relation.as_deref(), Some("CUSTOMERS"));
    assert!(plan.plan.text[0].starts_with("Plan hash value: "));
    workspace.run_text(
        "SELECT count(*) FROM plan_table",
        false,
        Instant::now(),
    );
    workspace.wait_for_run(LIMIT);
    let tab = &window(&mut workspace).tab;
    let kept = tab
        .grid
        .as_ref()
        .and_then(|grid| grid.rows.first())
        .and_then(|row| row.first().cloned().flatten());
    assert!(
        tab.error.is_some() || kept.as_deref() == Some("0"),
        "the plan's rows were rolled back: {kept:?}"
    );
    window(&mut workspace).tab.set_dialect(QueryDialect::Oracle);
    workspace.explain_text("SELECT * FROM customers", true, Instant::now());
    assert!(!workspace.is_running());
    let tab = &window(&mut workspace).tab;
    assert_eq!(tab.view, ResultView::Messages);
    assert!(
        tab.messages[0].contains("DBMS_XPLAN.DISPLAY_CURSOR"),
        "{:?}",
        tab.messages
    );
}

#[test]
fn sql_server_explains_with_showplan_all_and_measures_with_statistics_profile() {
    let directory = scratch("sqlserver");
    let mut workspace = with_customers(&directory);
    window(&mut workspace).tab.set_dialect(QueryDialect::SqlServer);
    workspace.explain_text("SELECT * FROM customers WHERE id = 2", false, Instant::now());
    workspace.wait_for_run(LIMIT);
    let tab = &window(&mut workspace).tab;
    assert_eq!(tab.view, ResultView::Plan, "{:?}", tab.messages);
    let plan = tab.plan.as_ref().expect("the plan shows");
    assert!(!plan.plan.analyze);
    assert_eq!(plan.plan.nodes[0].operation, "SELECT");
    assert!(
        plan.plan.nodes[1].operation.starts_with("Clustered Index Seek [customers]"),
        "{:?}",
        plan.plan.nodes
    );
    assert_eq!(plan.plan.nodes[1].relation.as_deref(), Some("customers"));
    workspace.explain_text(
        "UPDATE customers SET name = 'changed' WHERE id = 1",
        true,
        Instant::now(),
    );
    workspace.wait_for_run(LIMIT);
    let tab = &window(&mut workspace).tab;
    assert_eq!(tab.view, ResultView::Plan, "{:?}", tab.messages);
    let plan = tab.plan.as_ref().expect("the profile shows");
    assert!(plan.plan.analyze);
    assert_eq!(plan.plan.actual_rows_total(0), Some(1.0));
    workspace.run_text(
        "SELECT name FROM customers WHERE id = 1",
        false,
        Instant::now(),
    );
    workspace.wait_for_run(LIMIT);
    let grid = window(&mut workspace).tab.grid.clone().expect("the rows");
    assert_eq!(
        grid.rows[0][0].as_deref(),
        Some("Ada"),
        "the UPDATE was rolled back and showplan is off again"
    );
}

#[test]
fn hovering_a_node_fills_it_shows_its_tooltip_after_600_ms_and_a_click_selects_it() {
    let directory = scratch("hover");
    let mut workspace = with_customers(&directory);
    let stub = stub_window().tab.plan.clone().expect("the stub plan");
    {
        let window = window(&mut workspace);
        window.tab.editor = Editor::new(STUB_STATEMENT);
        window.tab.plan = Some(stub);
        window.tab.view = ResultView::Plan;
    }
    let bounds = accessibility_tree(window(&mut workspace))
        .into_iter()
        .find(|node| node.id == NODE_FIRST_PLAN_NODE + 2)
        .map(|node| node.bounds)
        .expect("the node is in the tree");
    let centre = (bounds.x + bounds.w / 2.0, bounds.y + bounds.h / 2.0);
    let start = Instant::now();
    assert!(workspace.pointer_moved(centre.0, centre.1, BOX_FACES, WINDOW));
    assert_eq!(window(&mut workspace).hovered_plan_node(), Some(2));
    let (canvas, theme) = draw(window(&mut workspace));
    assert_eq!(
        pixel(&canvas, centre.0, bounds.y + 3.0),
        Some(theme.bg2),
        "the hovered node fills bg2"
    );
    let _ = workspace.tick(start);
    assert_eq!(window(&mut workspace).tab.plan.as_ref().unwrap().tooltip, None);
    let wait = workspace.until_plan_tooltip(start).expect("a tooltip is due");
    assert!(wait <= Duration::from_millis(600), "{wait:?}");
    assert!(workspace.tick(start + Duration::from_millis(600)));
    let plan = window(&mut workspace).tab.plan.clone().unwrap();
    assert_eq!(plan.tooltip, Some((2, TooltipCause::Pointer)));
    assert_eq!(plan.tooltip_lines(2)[0], "Seq Scan on customers c");
    assert_eq!(workspace.until_plan_tooltip(start + Duration::from_millis(600)), None);
    let (canvas, theme) = draw(window(&mut workspace));
    let tip_top = bounds.y + NODE_HEIGHT + 4.0;
    assert_eq!(
        pixel(&canvas, bounds.x + 10.0, tip_top),
        Some(theme.border),
        "the tooltip's border sits 4 px under the node"
    );
    assert_eq!(
        pixel(&canvas, bounds.x + 3.0, tip_top + 20.0),
        Some(theme.bg0),
        "the tooltip draws as a popover"
    );
    let _ = workspace.pointer_pressed(
        centre.0,
        centre.1,
        PointerButton::Primary,
        Modifiers::default(),
        BOX_FACES,
        WINDOW,
        start + Duration::from_millis(700),
    );
    let plan = window(&mut workspace).tab.plan.clone().unwrap();
    assert_eq!((plan.current, plan.selected), (2, Some(2)));
    let at = STUB_STATEMENT.find("customers").unwrap();
    assert_eq!(plan.highlight, Some(at..at + "customers".len()));
    assert!(workspace.pointer_moved(1.0, 1.0, BOX_FACES, WINDOW));
    assert!(workspace.tick(start + Duration::from_millis(800)));
    assert_eq!(window(&mut workspace).tab.plan.as_ref().unwrap().tooltip, None);
}

#[test]
fn ctrl_shift_space_shows_the_current_nodes_tooltip_at_once_and_esc_hides_it() {
    let mut window = stub_window();
    let _ = window.key(chord(Key::Character('3'), false, true), Instant::now());
    let _ = window.key(chord(Key::Down, false, false), Instant::now());
    let _ = window.key(chord(Key::Space, true, true), Instant::now());
    let plan = window.tab.plan.as_ref().unwrap();
    assert_eq!(plan.tooltip, Some((1, TooltipCause::Keyboard)));
    let lines = plan.tooltip_lines(1);
    assert_eq!(lines[0], "Seq Scan on orders o");
    assert!(lines[1].starts_with("cost 0.00..20.00 · rows 10"), "{lines:?}");
    let _ = window.key(chord(Key::Escape, false, false), Instant::now());
    let plan = window.tab.plan.as_ref().unwrap();
    assert_eq!((plan.tooltip, plan.current), (None, 1));
    let _ = window.key(chord(Key::Space, true, true), Instant::now());
    let _ = window.key(chord(Key::Right, false, false), Instant::now());
    assert_eq!(window.tab.plan.as_ref().unwrap().tooltip, None);
}

#[test]
fn the_stub_plan_draws_links_bars_and_heat_borders() {
    let window = stub_window();
    let plan = window.tab.plan.as_ref().unwrap();
    assert_eq!(
        (plan.plan.heat(0), plan.plan.heat(1), plan.plan.heat(2)),
        (Heat::Plain, Heat::Warm, Heat::Hot)
    );
    let rects: Vec<Rect> = (0..3)
        .map(|index| plan.node_rect(index, PLAN_AREA).unwrap())
        .collect();
    assert_eq!(rects[1].y - rects[0].y, 56.0);
    assert!(rects[2].x - (rects[1].x + rects[1].w) >= 24.0);
    let (canvas, theme) = draw(&window);
    let top_middle =
        |rect: &Rect, below: f32| pixel(&canvas, rect.x + rect.w / 2.0, rect.y + below);
    assert_eq!(
        top_middle(&rects[0], 0.0),
        Some(theme.border_control),
        "the root is plain"
    );
    assert_eq!(
        top_middle(&rects[0], 1.0),
        Some(theme.bg0),
        "with a 1 px border"
    );
    assert_eq!(
        top_middle(&rects[1], 1.0),
        Some(theme.warning),
        "20 % is warm, 2 px"
    );
    assert_eq!(
        top_middle(&rects[2], 1.0),
        Some(theme.error),
        "62 % is hot, 2 px"
    );
    let bar = |rect: &Rect, along: f32| {
        pixel(
            &canvas,
            rect.x + 12.0 + (rect.w - 24.0) * along,
            rect.y + 35.0,
        )
    };
    assert_eq!(
        bar(&rects[2], 0.5),
        Some(theme.accent),
        "62 % of the bar is filled"
    );
    assert_eq!(bar(&rects[2], 0.8), Some(theme.bg2));
    assert_eq!(bar(&rects[1], 0.1), Some(theme.accent));
    assert_eq!(bar(&rects[1], 0.3), Some(theme.bg2));
    let link_x = (rects[0].x + rects[0].w / 2.0).floor();
    let link_inked = (link_x as usize - 1..=link_x as usize + 1)
        .any(|x| canvas.pixel(x, (rects[0].y + NODE_HEIGHT + 4.0) as usize) != Some(theme.bg0));
    assert!(link_inked, "the link leaves the root's bottom centre");
    let error_ink = ((rects[2].x + 12.0) as usize..(rects[2].x + rects[2].w - 12.0) as usize)
        .filter(|&x| canvas.pixel(x, (rects[2].y + 28.0) as usize) == Some(theme.error))
        .count();
    assert!(error_ink > 2, "` · 62 %` is drawn in error");
}

#[test]
fn down_right_enter_selects_the_hot_child_and_highlights_its_table() {
    let mut window = stub_window();
    let _ = window.key(chord(Key::Character('3'), false, true), Instant::now());
    assert_eq!(window.tab.focus, TabFocus::Plan);
    for key in [Key::Down, Key::Right, Key::Enter] {
        let _ = window.key(chord(key, false, false), Instant::now());
    }
    let plan = window.tab.plan.as_ref().unwrap();
    assert_eq!((plan.current, plan.selected), (2, Some(2)));
    let at = STUB_STATEMENT.find("customers").unwrap();
    assert_eq!(plan.highlight, Some(at..at + "customers".len()));
    let tree = accessibility_tree(&window);
    let node = tree
        .iter()
        .find(|node| node.id == NODE_FIRST_PLAN_NODE + 2)
        .unwrap();
    assert_eq!(node.name, "Seq Scan on customers c");
    let description = node.description.clone().unwrap_or_default();
    assert!(description.contains("62 percent, hot"), "{description}");
    assert_eq!(node.role, eggdb_platform::window::AccessRole::TreeItem);
    assert!(node.tree_position.is_some());
    assert!(node.state.focused && node.state.selected);
    let rect = plan.node_rect(2, PLAN_AREA).unwrap();
    let (canvas, theme) = draw(&window);
    assert_eq!(
        pixel(&canvas, rect.x + rect.w / 2.0, rect.y + 3.0),
        Some(theme.selection)
    );
    assert_eq!(
        pixel(&canvas, rect.x + rect.w / 2.0, rect.y - 1.0),
        Some(theme.focus_ring)
    );
    let highlighted = (320..1000).any(|x| canvas.pixel(x, 89) == Some(theme.selection));
    assert!(highlighted, "the editor highlights the table's name");
    let _ = window.key(chord(Key::Escape, false, false), Instant::now());
    let plan = window.tab.plan.as_ref().unwrap();
    assert_eq!((plan.selected, plan.highlight.clone()), (None, None));
    let _ = window.key(
        KeyPress {
            key: Key::Character('t'),
            modifiers: Modifiers {
                alt: true,
                ..Modifiers::default()
            },
        },
        Instant::now(),
    );
    assert!(window.tab.plan.as_ref().is_some_and(|plan| plan.text_shown));
    assert!(
        !accessibility_tree(&window)
            .iter()
            .any(|node| node.id == NODE_FIRST_PLAN_NODE)
    );
}
