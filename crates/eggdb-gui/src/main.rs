//! `eggdb-gui`: EggDB's desktop client (D15, D256), a binary of its own so the server's
//! size never carries the GUI. It shows the first screen's connect card in a native window, or
//! renders it offscreen to a PNG file for scripts and tests. `--open` opens a database file
//! the way typing its path and pressing `Enter` does, so a PNG shows the outcome: the main
//! window on it, or the card with the failure's lines.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eggdb_gui::clipboard::SystemClipboard;
use eggdb_gui::connect_screen::{
    self, DEFAULT_HEIGHT, DEFAULT_WIDTH, MIN_HEIGHT, MIN_WIDTH, TITLE,
};
use eggdb_gui::connection::{EngineConnector, home_directory};
use eggdb_gui::layout::Layout;
use eggdb_gui::main_window::{self, Faces};
use eggdb_gui::png::encode_png;
use eggdb_gui::pointer::clamp_to_minimum;
use eggdb_gui::settings::{SETTINGS_VARIABLE, settings_path};
use eggdb_gui::workspace::{Screen, Workspace, WorkspaceCommand};
use eggdb_gui::{BoxFace, Canvas, SystemFaces, Theme, ThemeKind};
use eggdb_platform::window::{
    AccessNode, Window, WindowEvent, WindowOptions, has_file_dialog, open_window,
};

const USAGE: &str = "Usage: eggdb-gui [--window] [--png <file>] [--open <path>] [--no-create] [--read-only] [--no-file-dialog] [--run <sql>] [--explain <sql>] [--theme light|dark|high-contrast] [--layout <layout>] [--reduced-motion] [--scale <n>] [--size <width>x<height>] [--box-text] [--frames <n>]

Shows EggDB's connect screen.
  --window          open it in a native window (the default without --png)
  --png <file>      render it offscreen and write it to <file> as PNG
  --open <path>     open the database file at <path> as the connect card's Open does
  --no-create       uncheck \"Create the file if it does not exist\" (checked by default)
  --read-only       check \"Open read-only\"
  --no-file-dialog  lay the card out as on a system without an open-file dialog (with --png)
  --run <sql>       after --open, put <sql> in the editor and run it as Ctrl/Cmd+Enter does (with --png)
  --explain <sql>   after --open and --run, put <sql> in the editor and explain it as Ctrl/Cmd+E does (with --png)
  --theme <theme>   light (the default), dark or high-contrast
  --layout <layout> lay the main window out as eggdb, ssms, pgadmin, dbeaver, datagrip,
                    mysql-workbench, mongodb-compass or redis-insight for this run, without
                    keeping it in the settings
  --reduced-motion  reduce motion whatever the system's setting says (fills change at once,
                    the caret holds still)
  --scale <n>       device pixels per logical pixel for --png (default 1)
  --size <w>x<h>    the logical size for --png (default 1280x800)
  --box-text        draw text as boxes, the same pixels on every OS (for pixel tests)
  --frames <n>      close the window by itself after presenting <n> frames (for scripted runs)
";

const SCRIPTED_FRAME_INTERVAL: Duration = Duration::from_millis(50);
const OPENING_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SCRIPTED_OPEN_LIMIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
struct Options {
    png: Option<OsString>,
    window: bool,
    theme: ThemeKind,
    scale: f32,
    size: (u32, u32),
    box_text: bool,
    frames: Option<u32>,
    open: Option<OsString>,
    run: Option<OsString>,
    explain: Option<OsString>,
    create_if_missing: bool,
    read_only: bool,
    file_dialog: bool,
    layout: Option<Layout>,
    reduced_motion: bool,
}

fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once('x')?;
    let (width, height) = (width.parse().ok()?, height.parse().ok()?);
    ((1..=16384).contains(&width) && (1..=16384).contains(&height)).then_some((width, height))
}

fn parse_options(mut args: impl Iterator<Item = OsString>) -> Result<Option<Options>, String> {
    let mut options = Options {
        png: None,
        window: false,
        theme: ThemeKind::Light,
        scale: 1.0,
        size: (DEFAULT_WIDTH, DEFAULT_HEIGHT),
        box_text: false,
        frames: None,
        open: None,
        run: None,
        explain: None,
        create_if_missing: true,
        read_only: false,
        file_dialog: true,
        layout: None,
        reduced_motion: false,
    };
    while let Some(arg) = args.next() {
        let name = arg
            .to_str()
            .ok_or_else(|| format!("{} is not valid UTF-8", arg.to_string_lossy()))?
            .to_owned();
        let mut value = |what: &str| args.next().ok_or_else(|| format!("{name} needs {what}"));
        match name.as_str() {
            "-h" | "--help" => return Ok(None),
            "--window" => options.window = true,
            "--box-text" => options.box_text = true,
            "--no-create" => options.create_if_missing = false,
            "--read-only" => options.read_only = true,
            "--reduced-motion" => options.reduced_motion = true,
            "--no-file-dialog" => options.file_dialog = false,
            "--open" => options.open = Some(value("a path")?),
            "--run" => options.run = Some(value("a statement")?),
            "--explain" => options.explain = Some(value("a statement")?),
            "--frames" => {
                let frames = value("a number")?;
                options.frames = Some(
                    frames
                        .to_str()
                        .and_then(|text| text.parse::<u32>().ok())
                        .filter(|frames| *frames >= 1)
                        .ok_or_else(|| {
                            format!(
                                "--frames {} is not a whole number of at least 1",
                                frames.to_string_lossy()
                            )
                        })?,
                );
            }
            "--png" => options.png = Some(value("a file")?),
            "--layout" => {
                let layout = value("a layout")?;
                options.layout = Some(layout.to_str().and_then(Layout::from_key).ok_or_else(|| {
                    format!(
                        "--layout {} is not eggdb, ssms, pgadmin, dbeaver, datagrip, mysql-workbench, mongodb-compass or redis-insight",
                        layout.to_string_lossy()
                    )
                })?);
            }
            "--theme" => {
                let theme = value("a theme")?;
                options.theme = theme.to_str().and_then(ThemeKind::parse).ok_or_else(|| {
                    format!(
                        "--theme {} is not light, dark or high-contrast",
                        theme.to_string_lossy()
                    )
                })?;
            }
            "--scale" => {
                let scale = value("a number")?;
                options.scale = scale
                    .to_str()
                    .and_then(|text| text.parse::<f32>().ok())
                    .filter(|scale| scale.is_finite() && (0.5..=8.0).contains(scale))
                    .ok_or_else(|| {
                        format!(
                            "--scale {} is not a number from 0.5 to 8",
                            scale.to_string_lossy()
                        )
                    })?;
            }
            "--size" => {
                let size = value("<width>x<height>")?;
                options.size = size.to_str().and_then(parse_size).ok_or_else(|| {
                    format!(
                        "--size {} is not <width>x<height> from 1 to 16384",
                        size.to_string_lossy()
                    )
                })?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if options.png.is_none() {
        options.window = true;
    }
    Ok(Some(options))
}

const BOX_FACES: Faces<'static> = Faces {
    ui: &BoxFace,
    ui_bold: &BoxFace,
    mono: &BoxFace,
};

fn with_faces<T>(box_text: bool, run: impl FnOnce(Faces<'_>) -> T) -> T {
    if box_text {
        return run(BOX_FACES);
    }
    match SystemFaces::find() {
        Some(system) => run(system.faces()),
        None => {
            eprintln!("eggdb-gui: no usable system font was found; text is drawn as boxes");
            run(BOX_FACES)
        }
    }
}

fn workspace(options: &Options, file_dialog: bool, keep_settings: bool) -> Workspace {
    let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut workspace = Workspace::new(
        Arc::new(EngineConnector),
        home_directory(),
        current,
        file_dialog,
    );
    if let Some(card) = workspace.card_mut() {
        card.create_if_missing = options.create_if_missing;
        card.read_only = options.read_only;
    }
    if keep_settings {
        workspace.use_settings(settings_path());
    }
    if let Some(layout) = options.layout {
        workspace.show_layout(layout);
    }
    workspace.set_reduced_motion(options.reduced_motion);
    workspace
}

fn render(
    workspace: &Workspace,
    theme: Theme,
    faces: Faces<'_>,
    size: (u32, u32),
    scale: f32,
) -> Canvas {
    match workspace.screen() {
        Screen::Connect(card) => connect_screen::draw(card, theme, faces, size, scale),
        Screen::Main(window) => main_window::draw(window, theme, faces),
    }
}

fn write_png(options: &Options, path: &OsString, faces: Faces<'_>) -> Result<(), String> {
    let explicit_settings = std::env::var_os(SETTINGS_VARIABLE).is_some();
    let mut workspace = workspace(options, options.file_dialog, explicit_settings);
    if let Some(open) = &options.open {
        workspace.open_typed(&open.to_string_lossy(), Instant::now());
        workspace.wait_for_open(SCRIPTED_OPEN_LIMIT);
    }
    if let Some(window) = workspace.main_window_mut() {
        window.set_width(options.size.0 as usize);
    }
    if let Some(sql) = &options.run {
        workspace.run_text(&sql.to_string_lossy(), false, Instant::now());
        workspace.wait_for_run(SCRIPTED_OPEN_LIMIT);
    }
    if let Some(sql) = &options.explain {
        workspace.explain_text(&sql.to_string_lossy(), false, Instant::now());
        workspace.wait_for_run(SCRIPTED_OPEN_LIMIT);
    }
    let frame = render(
        &workspace,
        Theme::of(options.theme),
        faces,
        options.size,
        options.scale,
    );
    std::fs::write(path, encode_png(&frame))
        .map_err(|error| format!("cannot write {}: {error}", path.to_string_lossy()))
}

fn logical_size(device: (u32, u32), scale: f32) -> (u32, u32) {
    let logical = |pixels: u32| ((pixels as f32 / scale).round() as u32).max(1);
    (logical(device.0), logical(device.1))
}

fn relayout(workspace: &mut Workspace, logical: (u32, u32)) {
    let (width, _) = clamp_to_minimum(logical);
    if let Some(window) = workspace.main_window_mut()
        && window.width != (width as usize).max(MIN_WIDTH as usize)
    {
        window.set_width(width as usize);
    }
}

fn screen_tree(workspace: &Workspace, faces: Faces<'_>, logical: (u32, u32)) -> Vec<AccessNode> {
    match workspace.screen() {
        Screen::Connect(card) => connect_screen::accessibility_tree(card, faces, logical),
        Screen::Main(main) => main_window::accessibility_tree(main),
    }
}

fn run_window(options: &Options, faces: Faces<'_>) -> Result<(), String> {
    let window: Rc<RefCell<Box<dyn Window>>> = Rc::new(RefCell::new(
        open_window(&WindowOptions {
            title: TITLE.to_owned(),
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            min_width: MIN_WIDTH,
            min_height: MIN_HEIGHT,
        })
        .map_err(|error| error.to_string())?,
    ));
    let theme = Theme::of(options.theme);
    let mut workspace = workspace(options, has_file_dialog(), true);
    workspace.set_clipboard(Box::new(SystemClipboard::new(Rc::clone(&window))));
    workspace.start_caret_blink(Instant::now());
    let system_reduced_motion = window.borrow_mut().reduced_motion();
    workspace.set_reduced_motion(options.reduced_motion || system_reduced_motion);
    if let Some(open) = &options.open {
        workspace.open_typed(&open.to_string_lossy(), Instant::now());
    }
    let mut scale = window.borrow().scale();
    let mut size = window.borrow().size();
    let mut dirty = true;
    let mut presented: u32 = 0;
    let scripted = options.frames.map(|_| SCRIPTED_FRAME_INTERVAL);
    loop {
        if workspace.tick(Instant::now()) {
            dirty = true;
        }
        let logical = logical_size(size, scale);
        if dirty {
            relayout(&mut workspace, logical);
            let frame = render(&workspace, theme, faces, logical, scale);
            let (width, height) = (frame.width() as u32, frame.height() as u32);
            window
                .borrow_mut()
                .present(width, height, &frame.rgba_bytes())
                .map_err(|error| error.to_string())?;
            let tree = screen_tree(&workspace, faces, logical);
            window.borrow_mut().set_accessibility_tree(&tree);
            dirty = false;
            presented = presented.saturating_add(1);
            if options.frames.is_some_and(|limit| presented >= limit) {
                return Ok(());
            }
        }
        let polling = workspace.is_opening() || workspace.is_running();
        let timeout = [
            scripted,
            polling.then_some(OPENING_POLL_INTERVAL),
            workspace.until_caret_changes(Instant::now()),
            workspace.until_fade_changes(),
            workspace.until_plan_tooltip(Instant::now()),
        ]
        .into_iter()
        .flatten()
        .min();
        let event = window
            .borrow_mut()
            .next_event(timeout)
            .map_err(|error| error.to_string())?;
        let now = Instant::now();
        let at = move |x: f32, y: f32| (x / scale, y / scale);
        let command = match event {
            Some(WindowEvent::AccessRequested(request)) => {
                let tree = screen_tree(&workspace, faces, logical);
                workspace.access(&request, &tree, now)
            }
            Some(WindowEvent::Key(press)) => workspace.key(press, now),
            Some(WindowEvent::PointerPressed {
                x,
                y,
                button,
                modifiers,
            }) => {
                let (x, y) = at(x, y);
                workspace.pointer_pressed(x, y, button, modifiers, faces, logical, now)
            }
            Some(WindowEvent::PointerReleased { x, y, button, .. }) => {
                let (x, y) = at(x, y);
                workspace.pointer_released(x, y, button, faces, logical, now)
            }
            Some(WindowEvent::PointerMoved { x, y }) => {
                let (x, y) = at(x, y);
                dirty |= workspace.pointer_moved(x, y, faces, logical);
                continue;
            }
            Some(WindowEvent::Wheel {
                x,
                y,
                lines_down,
                lines_right,
                modifiers,
            }) => {
                let (x, y) = at(x, y);
                dirty |= workspace.wheel(x, y, lines_down, lines_right, modifiers, faces, logical);
                continue;
            }
            Some(WindowEvent::PointerLeft) => {
                dirty |= workspace.pointer_left();
                continue;
            }
            Some(WindowEvent::FileDragEntered(paths)) => {
                workspace.drag_entered(&paths);
                dirty = true;
                continue;
            }
            Some(WindowEvent::FileDragLeft) => {
                workspace.drag_left();
                dirty = true;
                continue;
            }
            Some(WindowEvent::FilesDropped(paths)) => {
                workspace.drag_left();
                workspace.dropped(&paths, now);
                dirty = true;
                continue;
            }
            Some(WindowEvent::Resized { width, height }) => {
                size = (width, height);
                dirty = true;
                continue;
            }
            Some(WindowEvent::ScaleChanged(new_scale)) => {
                scale = new_scale;
                dirty = true;
                continue;
            }
            Some(WindowEvent::Redraw) => {
                dirty = true;
                continue;
            }
            Some(WindowEvent::CloseRequested) => return Ok(()),
            Some(WindowEvent::Focused(active)) => {
                if active {
                    let system_reduced_motion = window.borrow_mut().reduced_motion();
                    workspace.set_reduced_motion(options.reduced_motion || system_reduced_motion);
                }
                dirty |= workspace.set_window_active(active, now);
                continue;
            }
            None => {
                dirty |= scripted.is_some();
                continue;
            }
        };
        match command {
            WorkspaceCommand::Quit => return Ok(()),
            WorkspaceCommand::ChooseFile(dialog) => {
                let chosen = window
                    .borrow_mut()
                    .choose_file_to_open(&dialog)
                    .unwrap_or_else(|error| {
                        eprintln!("eggdb-gui: {error}");
                        None
                    });
                workspace.file_chosen(chosen.as_deref());
            }
            WorkspaceCommand::None => {}
        }
        dirty = true;
    }
}

fn main() -> ExitCode {
    let options = match parse_options(std::env::args_os().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("eggdb-gui: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = with_faces(options.box_text, |faces| {
        if let Some(path) = &options.png {
            write_png(&options, path, faces)?;
        }
        if options.window {
            run_window(&options, faces)?;
        }
        Ok::<(), String>(())
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("eggdb-gui: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Options>, String> {
        parse_options(args.iter().map(OsString::from))
    }

    #[test]
    fn options_parse_and_bad_ones_are_errors() {
        let window = parse(&[]).unwrap().unwrap();
        assert!(window.window && window.png.is_none());
        assert!(!window.box_text && window.frames.is_none());
        assert!(window.create_if_missing && !window.read_only && window.file_dialog);
        let opening = parse(&[
            "--png",
            "a.png",
            "--open",
            "shop.eggdb",
            "--no-create",
            "--read-only",
            "--no-file-dialog",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(opening.open, Some(OsString::from("shop.eggdb")));
        assert_eq!(opening.layout, None);
        let laid_out = parse(&["--png", "a.png", "--layout", "MySQL-Workbench"])
            .unwrap()
            .unwrap();
        assert_eq!(laid_out.layout, Some(Layout::Workbench));
        assert!(parse(&["--layout", "toad"]).is_err());
        assert!(parse(&["--layout"]).is_err());
        assert!(!laid_out.reduced_motion);
        let still = parse(&["--png", "a.png", "--reduced-motion"])
            .unwrap()
            .unwrap();
        assert!(still.reduced_motion);
        assert!(!opening.create_if_missing && opening.read_only && !opening.file_dialog);
        let scripted = parse(&["--window", "--frames", "3", "--box-text"])
            .unwrap()
            .unwrap();
        assert!(scripted.window && scripted.box_text);
        assert_eq!(scripted.frames, Some(3));
        let png = parse(&[
            "--png",
            "card.png",
            "--theme",
            "high-contrast",
            "--scale",
            "2",
            "--size",
            "900x600",
        ])
        .unwrap()
        .unwrap();
        assert!(!png.window);
        assert_eq!(png.png, Some(OsString::from("card.png")));
        assert_eq!(png.theme, ThemeKind::HighContrast);
        assert_eq!(png.scale, 2.0);
        assert_eq!(png.size, (900, 600));
        assert_eq!(parse(&["--help"]), Ok(None));
        for bad in [
            &["--theme", "sepia"][..],
            &["--scale", "0"],
            &["--scale", "NaN"],
            &["--size", "0x10"],
            &["--size", "big"],
            &["--png"],
            &["--frames", "0"],
            &["--frames", "-1"],
            &["--frames"],
            &["--open"],
            &["--frobnicate"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(logical_size((2560, 1600), 2.0), (1280, 800));
    }
}
