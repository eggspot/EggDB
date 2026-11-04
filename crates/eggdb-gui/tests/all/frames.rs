//! The main window's first frame drawn offscreen with the system's fonts, when it has them:
//! the frame keeps the design's regions, its text lands inside them, and the PNG is written to
//! `target/gui/` for people to look at.

use eggdb_gui::main_window::{HEIGHT, MainWindow, WIDTH, draw};
use eggdb_gui::{SystemFaces, Theme, ThemeKind, png::encode_png};

#[test]
fn the_first_frame_draws_with_real_fonts_and_is_saved_as_png() {
    let Some(faces) = SystemFaces::find() else {
        return;
    };
    let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/gui");
    let _ = std::fs::create_dir_all(&output);
    for (kind, name) in [
        (ThemeKind::Light, "main_light.png"),
        (ThemeKind::Dark, "main_dark.png"),
    ] {
        let theme = Theme::of(kind);
        let frame = draw(&MainWindow::sample(), theme, faces.faces());
        assert_eq!((frame.width(), frame.height()), (WIDTH, HEIGHT));
        let inked = (330..640)
            .filter(|&x| (88..108).any(|y| frame.pixel(x, y) != Some(theme.bg1)))
            .count();
        assert!(inked > 100, "the editor line has text");
        let grid_text = (400..520)
            .filter(|&x| (448..468).any(|y| frame.pixel(x, y) != Some(theme.bg0)))
            .count();
        assert!(grid_text > 30, "the first row has its name");
        let _ = std::fs::write(output.join(name), encode_png(&frame));
    }
}

#[test]
fn the_connect_card_draws_with_real_fonts_in_every_theme_and_is_saved_as_png() {
    use eggdb_gui::connect_screen::{self, ConnectCard, DEFAULT_HEIGHT, DEFAULT_WIDTH};
    let Some(faces) = SystemFaces::find() else {
        return;
    };
    let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/gui");
    let _ = std::fs::create_dir_all(&output);
    let card = ConnectCard::first_start();
    for (kind, scale, name) in [
        (ThemeKind::Light, 1.0, "connect_light.png"),
        (ThemeKind::Dark, 1.0, "connect_dark.png"),
        (ThemeKind::HighContrast, 1.0, "connect_high_contrast.png"),
        (ThemeKind::Light, 2.0, "connect_light_2x.png"),
    ] {
        let theme = Theme::of(kind);
        let size = (DEFAULT_WIDTH, DEFAULT_HEIGHT);
        let frame = connect_screen::draw(&card, theme, faces.faces(), size, scale);
        let device = |logical: f32| (logical * scale) as usize;
        assert_eq!(
            (frame.width(), frame.height()),
            (device(1280.0), device(800.0))
        );
        let title = (device(408.0)..device(480.0))
            .filter(|&x| {
                (device(164.0)..device(190.0)).any(|y| frame.pixel(x, y) != Some(theme.bg0))
            })
            .count();
        assert!(title > 10, "{name}: the title has text");
        let _ = std::fs::write(output.join(name), encode_png(&frame));
    }
}
