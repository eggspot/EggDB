//! The pointer, the caret and resizing on a real file (`docs/ux/workspace.md` 6 and its
//! checklist): hover and pressed fills on the toolbar, a release outside cancelling, a click
//! in the editor placing the caret, the caret's blink, the wheel on the grid and a resize that
//! keeps the grid where it was.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eggdb_gui::connection::EngineConnector;
use eggdb_gui::editor::{Editor, Position};
use eggdb_gui::grid::Cell;
use eggdb_gui::main_window::{
    self, Faces, MainWindow, NODE_EDITOR, NODE_GRID, NODE_RUN_ALL, accessibility_tree,
};
use eggdb_gui::pointer::CaretLook;
use eggdb_gui::workspace::{Screen, Workspace};
use eggdb_gui::{BoxFace, Canvas, Theme, ThemeKind, Typeface};
use eggdb_platform::window::{AccessRect, AccessRole, Key, KeyPress, Modifiers, PointerButton};

const BOX_FACES: Faces<'static> = Faces {
    ui: &BoxFace,
    ui_bold: &BoxFace,
    mono: &BoxFace,
};
const WINDOW: (u32, u32) = (1280, 800);
const LIMIT: Duration = Duration::from_secs(60);
const SAMPLE: &str = "SELECT id, name FROM customers ORDER BY id;";

fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "eggdb-gui-pointer-{}-{name}",
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

fn window(workspace: &mut Workspace) -> &mut MainWindow {
    workspace.main_window_mut().expect("the main window shows")
}

fn frame(workspace: &mut Workspace) -> (Canvas, Theme) {
    let theme = Theme::of(ThemeKind::Light);
    (main_window::draw(window(workspace), theme, BOX_FACES), theme)
}

fn bounds_of(workspace: &mut Workspace, id: u32) -> AccessRect {
    accessibility_tree(window(workspace))
        .into_iter()
        .find(|node| node.id == id)
        .map(|node| node.bounds)
        .expect("the node is in the tree")
}

fn press(workspace: &mut Workspace, (x, y): (f32, f32), now: Instant) {
    let _ = workspace.pointer_pressed(
        x,
        y,
        PointerButton::Primary,
        Modifiers::default(),
        BOX_FACES,
        WINDOW,
        now,
    );
}

fn release(workspace: &mut Workspace, (x, y): (f32, f32), now: Instant) {
    let _ = workspace.pointer_released(x, y, PointerButton::Primary, BOX_FACES, WINDOW, now);
}

fn pixel(canvas: &Canvas, x: f32, y: f32) -> Option<eggdb_gui::Rgba> {
    canvas.pixel(x as usize, y as usize)
}

#[test]
fn hovering_and_pressing_run_all_fill_it_and_a_release_outside_runs_nothing() {
    let directory = scratch("hover");
    let mut workspace = opened(&directory);
    window(&mut workspace).tab.editor = Editor::new(SAMPLE);
    let run_all = bounds_of(&mut workspace, NODE_RUN_ALL);
    let centre = (run_all.x + run_all.width / 2.0, run_all.y + run_all.height / 2.0);
    let padding = (run_all.x + 4.0, centre.1);
    let top_edge = (centre.0, run_all.y);
    let (canvas, theme) = frame(&mut workspace);
    assert_eq!(pixel(&canvas, padding.0, padding.1), Some(theme.bg0), "at rest");
    assert!(workspace.pointer_moved(centre.0, centre.1, BOX_FACES, WINDOW));
    let (canvas, _) = frame(&mut workspace);
    assert_eq!(pixel(&canvas, padding.0, padding.1), Some(theme.bg2), "hover fills bg2");
    assert_eq!(pixel(&canvas, top_edge.0, top_edge.1), Some(theme.border));
    let now = Instant::now();
    press(&mut workspace, centre, now);
    let (canvas, _) = frame(&mut workspace);
    assert_eq!(pixel(&canvas, padding.0, padding.1), Some(theme.bg2), "pressed fills bg2");
    assert_eq!(pixel(&canvas, top_edge.0, top_edge.1), Some(theme.border_strong), "with borderStrong");
    let outside = (700.0, 600.0);
    workspace.pointer_moved(outside.0, outside.1, BOX_FACES, WINDOW);
    release(&mut workspace, outside, now);
    assert!(!workspace.is_running(), "a release outside the button runs nothing");
    assert!(!window(&mut workspace).tab.running);
    let (canvas, _) = frame(&mut workspace);
    assert_eq!(pixel(&canvas, padding.0, padding.1), Some(theme.bg0), "the fill goes with the pointer");
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_click_in_the_editor_puts_the_caret_at_the_clicked_column() {
    let directory = scratch("editor-click");
    let mut workspace = opened(&directory);
    window(&mut workspace).tab.editor = Editor::new(SAMPLE);
    let editor = bounds_of(&mut workspace, NODE_EDITOR);
    assert!(editor.x <= 600.0 && editor.y <= 90.0, "(600, 90) is in the editor");
    let now = Instant::now();
    press(&mut workspace, (600.0, 90.0), now);
    release(&mut workspace, (600.0, 90.0), now);
    let advance = BoxFace.advance('x', 13.0);
    let column = ((600.0 - 320.0) / advance + 0.5).floor() as usize;
    let shown = window(&mut workspace);
    assert_eq!(shown.tab.editor.caret(), Position { line: 0, column: column.min(SAMPLE.len()) });
    assert!(shown.pointer.focus_by_pointer);
    let caret_x = 320.0 + advance * column as f32 + 1.0;
    let (canvas, theme) = frame(&mut workspace);
    assert_eq!(pixel(&canvas, caret_x, 98.0), Some(theme.accent), "the caret is at the click");
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn the_caret_is_solid_then_blinks_and_holds_still_under_reduced_motion_and_greys_inactive() {
    let directory = scratch("blink");
    let mut workspace = opened(&directory);
    window(&mut workspace).tab.editor = Editor::new(SAMPLE);
    let caret_x = 320.0 + BoxFace.advance('x', 13.0) * SAMPLE.len() as f32 + 1.0;
    let start = Instant::now();
    workspace.start_caret_blink(start);
    workspace.key(KeyPress::plain(Key::End), start);
    let mut shown = Vec::new();
    for ms in [0, 400, 1_100, 1_700] {
        workspace.tick(start + Duration::from_millis(ms));
        let (canvas, theme) = frame(&mut workspace);
        shown.push(pixel(&canvas, caret_x, 98.0) == Some(theme.accent));
    }
    assert_eq!(shown, [true, true, false, true]);
    workspace.set_reduced_motion(true);
    for ms in [0, 400, 1_100, 1_700] {
        workspace.tick(start + Duration::from_millis(ms));
        assert_eq!(window(&mut workspace).caret, CaretLook::On, "{ms} ms under reduced motion");
    }
    workspace.set_window_active(false, start + Duration::from_millis(2_000));
    let (canvas, theme) = frame(&mut workspace);
    assert_eq!(pixel(&canvas, caret_x, 98.0), Some(theme.text_disabled), "an inactive window's caret");
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn the_wheel_scrolls_the_grid_a_click_sets_its_cell_and_a_resize_keeps_both() {
    let directory = scratch("wheel");
    let mut workspace = opened(&directory);
    workspace.run_text("SELECT * FROM generate_series(1, 100);", false, Instant::now());
    workspace.wait_for_run(LIMIT);
    let grid = bounds_of(&mut workspace, NODE_GRID);
    let centre = (grid.x + grid.width / 2.0, grid.y + grid.height / 2.0);
    assert!(workspace.wheel(centre.0, centre.1, 2.0, 0.0, Modifiers::default(), BOX_FACES, WINDOW));
    assert_eq!(window(&mut workspace).tab.grid.as_ref().map(|grid| grid.first_row), Some(6));
    let target = accessibility_tree(window(&mut workspace))
        .into_iter()
        .find(|node| node.role == AccessRole::Cell && node.cell.is_some_and(|cell| cell.row == 8))
        .expect("row 8 shows");
    let at = target.cell.expect("a cell");
    let point = (target.bounds.x + 4.0, target.bounds.y + target.bounds.height / 2.0);
    let now = Instant::now();
    press(&mut workspace, point, now);
    release(&mut workspace, point, now);
    let expected = Cell { row: at.row, column: at.column };
    assert_eq!(window(&mut workspace).tab.grid.as_ref().map(|grid| grid.current), Some(expected));
    window(&mut workspace).set_width(1440);
    let shown = window(&mut workspace);
    assert!(shown.inspector.docked, "the inspector docks at 1440");
    let grid = shown.tab.grid.as_ref().expect("the grid");
    assert_eq!((grid.first_row, grid.current), (6, expected), "a resize keeps the grid where it was");
    let _ = std::fs::remove_dir_all(&directory);
}
