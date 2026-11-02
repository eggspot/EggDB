use std::cell::{Cell, RefCell};

use eggdb_gui::browser::{
    accessibility_json, action_code, key_from_dom, modifiers_from_bits, theme_from_code,
};
use eggdb_gui::connect_screen::{self, ConnectCard, DEFAULT_HEIGHT, DEFAULT_WIDTH};
use eggdb_gui::main_window::Faces;
use eggdb_gui::{BoxFace, Theme, TrueTypeFont};
use eggdb_platform::window::KeyPress;

use super::{INPUT, OUTPUT};

/// What [`eggdb_wasm_card_key`] returns for a key the card does not use, so the host leaves
/// the browser's own handling of it alone.
pub const KEY_IGNORED: u32 = u32::MAX;

const MAX_SIDE: u32 = 16384;
const MIN_SCALE: f32 = 0.5;
const MAX_SCALE: f32 = 8.0;

#[derive(Default)]
struct CardFonts {
    ui: Option<TrueTypeFont>,
    bold: Option<TrueTypeFont>,
}

thread_local! {
    static CARD: RefCell<ConnectCard> = RefCell::new(ConnectCard::first_start());
    static FONTS: RefCell<CardFonts> = RefCell::new(CardFonts::default());
    static SIZE: Cell<(u32, u32)> = const { Cell::new((DEFAULT_WIDTH, DEFAULT_HEIGHT)) };
}

fn with_faces<T>(run: impl FnOnce(Faces<'_>) -> T) -> T {
    FONTS.with(|fonts| {
        let fonts = fonts.borrow();
        match (&fonts.ui, &fonts.bold) {
            (Some(ui), Some(bold)) => run(Faces {
                ui,
                ui_bold: bold,
                mono: ui,
            }),
            (Some(ui), None) => run(Faces {
                ui,
                ui_bold: ui,
                mono: ui,
            }),
            _ => run(Faces {
                ui: &BoxFace,
                ui_bold: &BoxFace,
                mono: &BoxFace,
            }),
        }
    })
}

fn clamp_size(width: u32, height: u32) -> (u32, u32) {
    (width.clamp(1, MAX_SIDE), height.clamp(1, MAX_SIDE))
}

fn clamp_scale(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.clamp(MIN_SCALE, MAX_SCALE)
    } else {
        1.0
    }
}

/// Renders the connect card for a `width` × `height` CSS-pixel canvas at `scale` device
/// pixels per CSS pixel (`devicePixelRatio`) in theme `theme` (0 light, 1 dark, 2 high
/// contrast). The answer is the frame's RGBA bytes, row by row, ready for `ImageData`; the
/// return value is the frame's width in device pixels (its height is the answer's length
/// divided by four times the width).
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_render(width: u32, height: u32, scale: f32, theme: u32) -> u32 {
    let size = clamp_size(width, height);
    SIZE.with(|cell| cell.set(size));
    let theme = Theme::of(theme_from_code(theme));
    let frame = CARD.with(|card| {
        let card = card.borrow();
        with_faces(|faces| connect_screen::draw(&card, theme, faces, size, clamp_scale(scale)))
    });
    let frame_width = u32::try_from(frame.width()).unwrap_or(0);
    OUTPUT.with(|output| *output.borrow_mut() = frame.rgba_bytes());
    frame_width
}

fn input_key() -> Option<eggdb_platform::window::Key> {
    INPUT.with(|input| {
        let input = input.borrow();
        std::str::from_utf8(&input).ok().and_then(key_from_dom)
    })
}

/// Answers the key whose DOM `KeyboardEvent.key` value is in the input, with `modifiers` an OR
/// of 1 Shift, 2 Control, 4 Alt and 8 Meta. Returns the action for the host (0 none, 1 open,
/// 2 browse, 3 cancel, 4 show the layout picker, 5 load the sample dataset, 6 quit), or
/// [`KEY_IGNORED`] when the card does not use the key.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_key(modifiers: u32) -> u32 {
    let Some(key) = input_key() else {
        return KEY_IGNORED;
    };
    let press = KeyPress {
        key,
        modifiers: modifiers_from_bits(modifiers),
    };
    CARD.with(|card| {
        let current = card.borrow().clone();
        let (next, action) = connect_screen::handle_key(current, press);
        *card.borrow_mut() = next;
        action_code(action)
    })
}

/// Moves the card's focus to accessibility node `node`, as when the ARIA mirror's element for
/// it takes the DOM focus; returns 1 when the node can take the focus, 0 otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_focus(node: u32) -> u32 {
    CARD.with(|card| u32::from(card.borrow_mut().focus_node(node)))
}

/// Answers the card's accessibility tree for the last rendered size as a JSON array (see
/// `eggdb_gui::browser::accessibility_json`) and returns the answer's length.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_accessibility() -> u32 {
    let size = SIZE.with(Cell::get);
    let json = CARD.with(|card| {
        let card = card.borrow();
        with_faces(|faces| {
            accessibility_json(&connect_screen::accessibility_tree(&card, faces, size))
        })
    });
    super::answer(json)
}

/// Uses the TrueType font in the input for the card's text: `role` 0 the interface face, 1 its
/// bold. Returns 1 when the font was read, 0 when it is not a usable TrueType font (the card
/// keeps drawing text as boxes until both are loaded).
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_font(role: u32) -> u32 {
    let bytes = INPUT.with(|input| input.borrow().clone());
    let Ok(font) = TrueTypeFont::parse(bytes) else {
        return 0;
    };
    FONTS.with(|fonts| {
        let mut fonts = fonts.borrow_mut();
        match role {
            0 => fonts.ui = Some(font),
            1 => fonts.bold = Some(font),
            _ => return 0,
        }
        1
    })
}

/// Puts the card back in its first-start state.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_reset() {
    CARD.with(|card| *card.borrow_mut() = ConnectCard::first_start());
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_gui::connect_screen::{NODE_BROWSE, NODE_FILE, NODE_OPEN, NODE_WINDOW};

    fn write_input(text: &[u8]) {
        let start = super::super::eggdb_wasm_input(u32::try_from(text.len()).unwrap());
        INPUT.with(|input| {
            assert_eq!(input.borrow().as_ptr(), start.cast_const());
            input.borrow_mut().copy_from_slice(text);
        });
    }

    fn output() -> Vec<u8> {
        OUTPUT.with(|output| output.borrow().clone())
    }

    fn key(name: &str, modifiers: u32) -> u32 {
        write_input(name.as_bytes());
        eggdb_wasm_card_key(modifiers)
    }

    fn tree() -> String {
        eggdb_wasm_card_accessibility();
        String::from_utf8(output()).unwrap()
    }

    #[test]
    fn a_host_renders_the_card_into_an_rgba_frame_at_its_scale() {
        eggdb_wasm_card_reset();
        let width = eggdb_wasm_card_render(1280, 800, 1.0, 0);
        assert_eq!(width, 1280);
        assert_eq!(output().len(), 1280 * 800 * 4);
        let light = output();
        let width = eggdb_wasm_card_render(640, 400, 2.0, 1);
        assert_eq!(width, 1280);
        assert_eq!(output().len(), 1280 * 800 * 4);
        assert_ne!(output(), light);
        assert_eq!(eggdb_wasm_card_render(0, 0, f32::NAN, 7), 1);
        assert_eq!(output().len(), 4);
        assert_eq!(eggdb_wasm_card_render(100, 50, 100.0, 2), 800);
    }

    #[test]
    fn keys_focus_and_the_tree_follow_the_card() {
        eggdb_wasm_card_reset();
        eggdb_wasm_card_render(1280, 800, 1.0, 0);
        assert!(tree().contains("\"id\":9,\"parent\":7,\"role\":\"textbox\",\"name\":\"File\""));
        assert!(tree().starts_with("[{\"id\":1,"));
        assert_eq!(key("Tab", 0), 0);
        assert!(tree().contains("\"role\":\"button\",\"name\":\"Browse\",\"value\":null,\"placeholder\":null,\"focused\":true"));
        assert_eq!(key("Enter", 0), 2);
        assert_eq!(key("Shift", 0), KEY_IGNORED);
        assert_eq!(key("F5", 0), KEY_IGNORED);
        write_input(&[0xff, 0xfe]);
        assert_eq!(eggdb_wasm_card_key(0), KEY_IGNORED);
        assert_eq!(eggdb_wasm_card_focus(NODE_FILE), 1);
        assert_eq!(key("x", 0), 0);
        assert!(tree().contains("\"value\":\"x\""));
        assert_eq!(eggdb_wasm_card_focus(NODE_WINDOW), 0);
        assert_eq!(eggdb_wasm_card_focus(NODE_OPEN), 1);
        assert_eq!(key(" ", 0), 1);
        assert_eq!(key("q", 2), 6);
        assert_eq!(eggdb_wasm_card_focus(NODE_BROWSE), 1);
        eggdb_wasm_card_reset();
        assert!(tree().contains("\"value\":\"~/eggdb/quickstart.eggdb\""));
    }

    #[test]
    fn a_font_that_is_not_truetype_is_refused() {
        write_input(b"not a font");
        assert_eq!(eggdb_wasm_card_font(0), 0);
        assert_eq!(eggdb_wasm_card_font(5), 0);
    }
}
