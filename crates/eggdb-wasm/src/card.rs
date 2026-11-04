use std::cell::{Cell, RefCell};
use std::time::Instant;

use eggdb_gui::browser::{
    CardPointer, PointerAnswer, accessibility_json, action_code, key_from_dom, modifiers_from_bits,
    pointer_button_from_dom, theme_from_code,
};
use eggdb_gui::connect_screen::{self, ConnectCard, DEFAULT_HEIGHT, DEFAULT_WIDTH, frame_size};
use eggdb_gui::main_window::Faces;
use eggdb_gui::pointer;
use eggdb_gui::{BoxFace, Canvas, Theme, TrueTypeFont};
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
    static POINTER: RefCell<CardPointer> = RefCell::new(CardPointer::default());
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

fn fitted_scale(size: (u32, u32), scale: f32) -> f32 {
    let scale = clamp_scale(scale);
    let logical_pixels = f64::from(size.0) * f64::from(size.1);
    let device_pixels = logical_pixels * f64::from(scale) * f64::from(scale);
    let most = Canvas::MAX_PIXELS as f64;
    if device_pixels <= most {
        return scale;
    }
    let exceeds = |scale: f32| {
        let (width, height) = frame_size(size, scale);
        width as f64 * height as f64 > most
    };
    let mut fitted = ((most / logical_pixels).sqrt() as f32).min(scale);
    while exceeds(fitted) {
        fitted = fitted.next_down();
    }
    fitted
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
        with_faces(|faces| {
            connect_screen::draw(&card, theme, faces, size, fitted_scale(size, scale))
        })
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
/// 2 browse, 3 cancel, 4 show the layout picker, 5 load the sample dataset, 6 quit, 7 stop
/// opening, 8 copy the selection with [`eggdb_wasm_card_copied_text`], 9 paste with
/// [`eggdb_wasm_card_paste`]), or [`KEY_IGNORED`] when the card does not use the key.
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

/// Answers the text `Ctrl/Cmd+C` copies, the focused input's selection (never the
/// password's), and returns 1; returns 0 with nothing to copy.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_copied_text() -> u32 {
    match CARD.with(|card| card.borrow().copied_text()) {
        Some(text) => {
            super::answer(text);
            1
        }
        None => 0,
    }
}

/// Pastes the UTF-8 text in the input into the focused input, made one line. Returns 1 when
/// an input took it, 2 when it is over 1 MB and refused (the status line says so), and 0 when
/// no input has the focus or the text is not UTF-8.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_paste() -> u32 {
    let Ok(raw) = super::input_text() else {
        return 0;
    };
    CARD.with(|card| {
        let mut card = card.borrow_mut();
        match pointer::paste_text(&raw) {
            Ok(text) => u32::from(card.paste(&text)),
            Err(refused) => {
                card.status = Some(refused.to_owned());
                2
            }
        }
    })
}

fn with_pointer<T>(run: impl FnOnce(&mut CardPointer, &mut ConnectCard, (u32, u32)) -> T) -> T {
    let size = SIZE.with(Cell::get);
    POINTER.with(|pointer| {
        CARD.with(|card| run(&mut pointer.borrow_mut(), &mut card.borrow_mut(), size))
    })
}

fn pointer_code(answer: PointerAnswer) -> u32 {
    if answer.used() {
        action_code(answer.action)
    } else {
        KEY_IGNORED
    }
}

/// Answers a DOM `pointerdown` at (`x`, `y`) CSS pixels from the canvas's top-left corner with
/// DOM button number `button` (0 main, 1 auxiliary, 2 secondary) and `modifiers` as for
/// [`eggdb_wasm_card_key`]. Returns the action code as [`eggdb_wasm_card_key`] does, 0 when only
/// the frame changed, or [`KEY_IGNORED`] when the card does not use the press (so the browser
/// keeps its own behaviour, such as its context menu).
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_pointer_down(x: f32, y: f32, button: u32, modifiers: u32) -> u32 {
    let Some(button) = pointer_button_from_dom(button) else {
        return KEY_IGNORED;
    };
    let modifiers = modifiers_from_bits(modifiers);
    let answer = with_pointer(|pointer, card, size| {
        with_faces(|faces| {
            pointer.pressed(card, faces, size, (x, y), button, modifiers, Instant::now())
        })
    });
    pointer_code(answer)
}

/// Answers a DOM `pointermove` at (`x`, `y`): the hover, and a drag's selection in an input.
/// Returns 1 when the frame changed, 0 otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_pointer_move(x: f32, y: f32) -> u32 {
    let changed = with_pointer(|pointer, card, size| {
        with_faces(|faces| pointer.moved(card, faces, size, (x, y)))
    });
    u32::from(changed)
}

/// Answers a DOM `pointerup` at (`x`, `y`) with DOM button number `button`: a press that
/// started on a button, link, checkbox or tab and ends on it acts. Returns what
/// [`eggdb_wasm_card_pointer_down`] returns.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_pointer_up(x: f32, y: f32, button: u32) -> u32 {
    let Some(button) = pointer_button_from_dom(button) else {
        return KEY_IGNORED;
    };
    let answer = with_pointer(|pointer, card, size| {
        with_faces(|faces| pointer.released(card, faces, size, (x, y), button))
    });
    pointer_code(answer)
}

/// Answers the browser's own `dblclick` at (`x`, `y`): the word under it in an input when the
/// card's own click count missed it. Returns 1 when the frame changed, 0 otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_double_click(x: f32, y: f32) -> u32 {
    let changed = with_pointer(|pointer, card, size| {
        with_faces(|faces| pointer.double_clicked(card, faces, size, (x, y)))
    });
    u32::from(changed)
}

/// Answers a DOM `pointerleave`: nothing is hovered. Returns 1 when the frame changed.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_pointer_leave() -> u32 {
    u32::from(with_pointer(|pointer, card, _| pointer.left(card)))
}

/// Puts the card back in its first-start state.
#[unsafe(no_mangle)]
pub extern "C" fn eggdb_wasm_card_reset() {
    CARD.with(|card| *card.borrow_mut() = ConnectCard::first_start());
    POINTER.with(|pointer| *pointer.borrow_mut() = CardPointer::default());
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_gui::connect_screen::{
        NODE_BROWSE, NODE_FILE, NODE_OPEN, NODE_READ_ONLY, NODE_WINDOW,
    };

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
    fn the_largest_size_at_the_largest_scale_fits_the_pixel_limit() {
        let largest = clamp_size(u32::MAX, u32::MAX);
        let scale = fitted_scale(largest, MAX_SCALE);
        let (width, height) = frame_size(largest, scale);
        assert!(
            width
                .checked_mul(height)
                .is_some_and(|count| count <= Canvas::MAX_PIXELS)
        );
        assert!(width >= 8000, "{width}");
        assert_eq!(fitted_scale((1280, 800), 2.0), 2.0);
        assert_eq!(fitted_scale((1280, 800), f32::NAN), 1.0);
        let tall = (1, MAX_SIDE);
        let (width, height) = frame_size(tall, fitted_scale(tall, MAX_SCALE));
        assert!(width * height <= Canvas::MAX_PIXELS);
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
        assert_eq!(key("F6", 0), KEY_IGNORED);
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

    fn bounds(id: u32) -> [f32; 4] {
        let tree = tree();
        let at = tree.find(&format!("{{\"id\":{id},")).unwrap();
        let number = |name: &str| -> f32 {
            let key = format!("\"{name}\":");
            let start = at + tree[at..].find(&key).unwrap() + key.len();
            let end = start + tree[start..].find(',').unwrap();
            tree[start..end].parse().unwrap()
        };
        [number("x"), number("y"), number("width"), number("height")]
    }

    fn centre(id: u32) -> (f32, f32) {
        let [x, y, width, height] = bounds(id);
        (x + width / 2.0, y + height / 2.0)
    }

    #[test]
    fn the_host_sends_pointer_presses_moves_and_releases() {
        eggdb_wasm_card_reset();
        eggdb_wasm_card_render(1280, 800, 1.0, 0);
        let (x, y) = centre(NODE_READ_ONLY);
        assert_eq!(eggdb_wasm_card_pointer_move(x, y), 1);
        assert_eq!(eggdb_wasm_card_pointer_move(x, y), 0);
        assert_eq!(eggdb_wasm_card_pointer_down(x, y, 2, 0), KEY_IGNORED);
        assert_eq!(eggdb_wasm_card_pointer_down(x, y, 4, 0), KEY_IGNORED);
        assert_eq!(eggdb_wasm_card_pointer_down(x, y, 0, 0), 0);
        assert_eq!(eggdb_wasm_card_pointer_up(x, y, 0), 0);
        assert!(tree().contains("\"name\":\"Open read-only\",\"value\":null,\"placeholder\":null,\"focused\":true,\"checked\":true"));
        assert_eq!(eggdb_wasm_card_pointer_up(x, y, 0), KEY_IGNORED);
        assert_eq!(eggdb_wasm_card_pointer_leave(), 1);
        assert_eq!(eggdb_wasm_card_pointer_leave(), 0);
        let (x, y) = centre(NODE_BROWSE);
        assert_eq!(eggdb_wasm_card_pointer_down(x, y, 0, 0), 0);
        assert_eq!(eggdb_wasm_card_pointer_up(x, y, 0), 2);
        let [left, top, _, height] = bounds(NODE_FILE);
        let (x, y) = (left + 12.0, top + height / 2.0);
        assert_eq!(eggdb_wasm_card_pointer_down(x, y, 0, 0), 0);
        assert_eq!(eggdb_wasm_card_pointer_up(x, y, 0), 0);
        assert_eq!(eggdb_wasm_card_double_click(x, y), 1);
        assert_eq!(eggdb_wasm_card_copied_text(), 1);
        assert_eq!(eggdb_wasm_card_double_click(x, y), 0);
        eggdb_wasm_card_reset();
    }

    #[test]
    fn a_font_that_is_not_truetype_is_refused() {
        write_input(b"not a font");
        assert_eq!(eggdb_wasm_card_font(0), 0);
        assert_eq!(eggdb_wasm_card_font(5), 0);
    }

    #[test]
    fn the_host_copies_and_pastes_through_the_card() {
        eggdb_wasm_card_reset();
        eggdb_wasm_card_render(1280, 800, 1.0, 0);
        assert_eq!(key("c", 2), 8);
        assert_eq!(eggdb_wasm_card_copied_text(), 1);
        assert_eq!(output(), b"~/eggdb/quickstart.eggdb");
        assert_eq!(key("v", 2), 9);
        write_input(b"shop\n.eggdb");
        assert_eq!(eggdb_wasm_card_paste(), 1);
        assert!(tree().contains("\"value\":\"shop .eggdb\""));
        assert_eq!(eggdb_wasm_card_copied_text(), 0);
        write_input(&vec![b'x'; pointer::MAX_PASTE_BYTES + 1]);
        assert_eq!(eggdb_wasm_card_paste(), 2);
        assert!(tree().contains(pointer::PASTE_TOO_LARGE));
        write_input(&[0xff]);
        assert_eq!(eggdb_wasm_card_paste(), 0);
        eggdb_wasm_card_reset();
    }
}
