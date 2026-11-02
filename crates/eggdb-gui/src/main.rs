//! `eggdb-gui`: EggDB's desktop client (D15, DNEW-I29), a binary of its own so the server's
//! size never carries the GUI. It shows the first screen's connect card in a native window, or
//! renders it offscreen to a PNG file for scripts and tests.

use std::ffi::OsString;
use std::process::ExitCode;
use std::time::Duration;

use eggdb_gui::connect_screen::{
    self, ConnectAction, ConnectCard, DEFAULT_HEIGHT, DEFAULT_WIDTH, MIN_HEIGHT, MIN_WIDTH, TITLE,
};
use eggdb_gui::main_window::Faces;
use eggdb_gui::png::encode_png;
use eggdb_gui::{BoxFace, SystemFaces, Theme, ThemeKind};
use eggdb_platform::window::{WindowEvent, WindowOptions, open_window};

const USAGE: &str = "Usage: eggdb-gui [--window] [--png <file>] [--theme light|dark|high-contrast] [--scale <n>] [--size <width>x<height>] [--box-text] [--frames <n>]

Shows EggDB's connect screen.
  --window          open it in a native window (the default without --png)
  --png <file>      render it offscreen and write it to <file> as PNG
  --theme <theme>   light (the default), dark or high-contrast
  --scale <n>       device pixels per logical pixel for --png (default 1)
  --size <w>x<h>    the logical size for --png (default 1280x800)
  --box-text        draw text as boxes, the same pixels on every OS (for pixel tests)
  --frames <n>      close the window by itself after presenting <n> frames (for scripted runs)
";

const SCRIPTED_FRAME_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq)]
struct Options {
    png: Option<OsString>,
    window: bool,
    theme: ThemeKind,
    scale: f32,
    size: (u32, u32),
    box_text: bool,
    frames: Option<u32>,
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

fn write_png(options: &Options, path: &OsString, faces: Faces<'_>) -> Result<(), String> {
    let card = ConnectCard::first_start();
    let frame = connect_screen::draw(
        &card,
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

fn run_window(options: &Options, faces: Faces<'_>) -> Result<(), String> {
    let mut window = open_window(&WindowOptions {
        title: TITLE.to_owned(),
        width: DEFAULT_WIDTH,
        height: DEFAULT_HEIGHT,
        min_width: MIN_WIDTH,
        min_height: MIN_HEIGHT,
    })
    .map_err(|error| error.to_string())?;
    let theme = Theme::of(options.theme);
    let mut card = ConnectCard::first_start();
    let mut scale = window.scale();
    let mut size = window.size();
    let mut dirty = true;
    let mut presented: u32 = 0;
    let timeout = options.frames.map(|_| SCRIPTED_FRAME_INTERVAL);
    loop {
        if dirty {
            let logical = logical_size(size, scale);
            let frame = connect_screen::draw(&card, theme, faces, logical, scale);
            let (width, height) = (frame.width() as u32, frame.height() as u32);
            window
                .present(width, height, &frame.rgba_bytes())
                .map_err(|error| error.to_string())?;
            window
                .set_accessibility_tree(&connect_screen::accessibility_tree(&card, faces, logical));
            dirty = false;
            presented = presented.saturating_add(1);
            if options.frames.is_some_and(|limit| presented >= limit) {
                return Ok(());
            }
        }
        match window
            .next_event(timeout)
            .map_err(|error| error.to_string())?
        {
            Some(WindowEvent::Key(press)) => {
                let (next, action) = connect_screen::handle_key(card, press);
                card = next;
                if action == ConnectAction::Quit {
                    return Ok(());
                }
                dirty = true;
            }
            Some(WindowEvent::Resized { width, height }) => {
                size = (width, height);
                dirty = true;
            }
            Some(WindowEvent::ScaleChanged(new_scale)) => {
                scale = new_scale;
                dirty = true;
            }
            Some(WindowEvent::Redraw) => dirty = true,
            Some(WindowEvent::CloseRequested) => return Ok(()),
            Some(WindowEvent::Focused(_)) => {}
            None => dirty = timeout.is_some(),
        }
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
            &["--frobnicate"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(logical_size((2560, 1600), 2.0), (1280, 800));
    }
}
