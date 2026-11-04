//! Pixel tests for the connect card: `eggdb-gui --png --box-text` renders it in every theme at
//! 1x and 2x, the PNG must equal the library's own render byte for byte, and its checksum must
//! equal the one in `tests/data/reference-frames.txt` on every OS (the rasterizer is our own
//! CPU code and the box typeface needs no font file, so the pixels are the same everywhere).
//! `EGGDB_GUI_UPDATE_REFERENCES=1 cargo test -p eggdb-gui --test all reference_frames` rewrites
//! the list from this system's frames; each frame is kept in `target/gui/reference/` to look at.

use std::path::{Path, PathBuf};
use std::process::Command;

use eggdb_gui::connect_screen::{self, ConnectCard, DEFAULT_HEIGHT, DEFAULT_WIDTH};
use eggdb_gui::main_window::Faces;
use eggdb_gui::png::encode_png;
use eggdb_gui::{BoxFace, Theme, ThemeKind};

const UPDATE_VARIABLE: &str = "EGGDB_GUI_UPDATE_REFERENCES";
const UNSET: &str = "unset";

const BOX_FACES: Faces<'static> = Faces {
    ui: &BoxFace,
    ui_bold: &BoxFace,
    mono: &BoxFace,
};

const FRAMES: [(&str, ThemeKind, &str, u32); 6] = [
    ("connect_light_1x", ThemeKind::Light, "light", 1),
    ("connect_dark_1x", ThemeKind::Dark, "dark", 1),
    (
        "connect_high_contrast_1x",
        ThemeKind::HighContrast,
        "high-contrast",
        1,
    ),
    ("connect_light_2x", ThemeKind::Light, "light", 2),
    ("connect_dark_2x", ThemeKind::Dark, "dark", 2),
    (
        "connect_high_contrast_2x",
        ThemeKind::HighContrast,
        "high-contrast",
        2,
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Reference {
    name: String,
    size: String,
    checksum: String,
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn references_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/reference-frames.txt")
}

fn output_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/gui/reference")
}

fn parse_references(text: &str) -> Vec<Reference> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split_whitespace();
            let mut field = || fields.next().unwrap_or_default().to_owned();
            Reference {
                name: field(),
                size: field(),
                checksum: field(),
            }
        })
        .collect()
}

fn format_references(references: &[Reference]) -> String {
    references
        .iter()
        .map(|reference| {
            format!(
                "{} {} {}\n",
                reference.name, reference.size, reference.checksum
            )
        })
        .collect()
}

fn render_with_binary(theme: &str, scale: u32, path: &Path) -> Vec<u8> {
    let status = Command::new(env!("CARGO_BIN_EXE_eggdb-gui"))
        .arg("--png")
        .arg(path)
        .args(["--box-text", "--theme", theme, "--scale"])
        .arg(scale.to_string())
        .status()
        .expect("eggdb-gui runs");
    assert!(status.success(), "eggdb-gui --png exits with success");
    std::fs::read(path).expect("eggdb-gui wrote the PNG")
}

fn render_with_library(kind: ThemeKind, scale: u32) -> Vec<u8> {
    let frame = connect_screen::draw(
        &ConnectCard::first_start(),
        Theme::of(kind),
        BOX_FACES,
        (DEFAULT_WIDTH, DEFAULT_HEIGHT),
        scale as f32,
    );
    encode_png(&frame)
}

fn current_frames() -> Vec<Reference> {
    let output = output_dir();
    std::fs::create_dir_all(&output).expect("target/gui/reference is writable");
    FRAMES
        .iter()
        .map(|&(name, kind, theme, scale)| {
            let png = render_with_binary(theme, scale, &output.join(format!("{name}.png")));
            assert!(
                png == render_with_library(kind, scale),
                "{name}: eggdb-gui --png wrote other bytes than connect_screen::draw"
            );
            Reference {
                name: name.to_owned(),
                size: format!("{}x{}", DEFAULT_WIDTH * scale, DEFAULT_HEIGHT * scale),
                checksum: format!("{:016x}", fnv1a_64(&png)),
            }
        })
        .collect()
}

fn mismatches(expected: &[Reference], actual: &[Reference]) -> Vec<String> {
    let mut problems: Vec<String> = actual
        .iter()
        .filter_map(|frame| {
            match expected
                .iter()
                .find(|reference| reference.name == frame.name)
            {
                None => Some(format!("{}: not in the reference list", frame.name)),
                Some(reference) if reference.checksum == UNSET => Some(format!(
                    "{}: the reference list has no checksum yet",
                    frame.name
                )),
                Some(reference) if reference != frame => Some(format!(
                    "{}: {} {} differs from the reference {} {} (see target/gui/reference/{}.png)",
                    frame.name,
                    frame.size,
                    frame.checksum,
                    reference.size,
                    reference.checksum,
                    frame.name
                )),
                Some(_) => None,
            }
        })
        .collect();
    problems.extend(
        expected
            .iter()
            .filter(|reference| actual.iter().all(|frame| frame.name != reference.name))
            .map(|reference| format!("{}: listed but never rendered", reference.name)),
    );
    problems
}

#[test]
fn the_connect_card_renders_the_reference_pixels_in_every_theme_and_scale() {
    let actual = current_frames();
    if std::env::var_os(UPDATE_VARIABLE).is_some_and(|value| value == "1") {
        assert!(
            std::env::var_os("CI").is_none(),
            "{UPDATE_VARIABLE}=1 rewrites the reference list instead of comparing it, so CI refuses it"
        );
        std::fs::write(references_path(), format_references(&actual))
            .expect("tests/data/reference-frames.txt is writable");
        return;
    }
    let text = std::fs::read_to_string(references_path()).expect("tests/data/reference-frames.txt");
    let problems = mismatches(&parse_references(&text), &actual);
    assert!(
        problems.is_empty(),
        "the connect card's pixels changed:\n{}\nIf the change is intended, run \
         {UPDATE_VARIABLE}=1 cargo test -p eggdb-gui --test all reference_frames and commit \
         crates/eggdb-gui/tests/data/reference-frames.txt",
        problems.join("\n")
    );
}

#[test]
fn the_reference_list_round_trips_and_reports_every_kind_of_mismatch() {
    let list = "a 2x2 0000000000000001\nb 2x2 unset\n\nc 4x4 0000000000000003\n";
    let references = parse_references(list);
    assert_eq!(references.len(), 3);
    assert_eq!(format_references(&references), list.replace("\n\n", "\n"));
    let frame = |name: &str, size: &str, checksum: &str| Reference {
        name: name.to_owned(),
        size: size.to_owned(),
        checksum: checksum.to_owned(),
    };
    let actual = [
        frame("a", "2x2", "0000000000000001"),
        frame("b", "2x2", "0000000000000002"),
        frame("d", "2x2", "0000000000000004"),
    ];
    let problems = mismatches(&references, &actual);
    assert_eq!(problems.len(), 3, "{problems:?}");
    assert!(problems[0].starts_with("b: the reference list has no checksum"));
    assert!(problems[1].starts_with("d: not in the reference list"));
    assert!(problems[2].starts_with("c: listed but never rendered"));
    assert!(mismatches(&references[..1], &actual[..1]).is_empty());
    assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
}
