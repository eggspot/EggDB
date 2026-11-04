//! The pure parts of the Win32 window backend, compiled and tested on every operating system:
//! translating virtual-key codes and `WM_CHAR` UTF-16 units into the GUI's keys, reading the
//! modifier state, packing the words of a message's parameters, turning a monitor's DPI into a
//! scale, and converting an RGBA frame into the BGRA rows a 32-bit device-independent bitmap
//! holds, the filter list and path buffers of the open-file dialog, the pointer's position and
//! wheel steps in mouse messages, clipboard text as `CF_UNICODETEXT`, and the drop effects and
//! drag-over state of an OLE `IDropTarget` ([`FileDragTarget`]). The values come from
//! Microsoft's public Win32 and COM documentation.

use std::path::PathBuf;

use super::{FileFilter, Key, KeyPress, Modifiers, WindowEvent};

/// The DPI Windows treats as a scale of 1.
pub const BASE_DPI: u32 = 96;

/// `VK_BACK`, Backspace.
pub const VK_BACK: usize = 0x08;
/// `VK_TAB`.
pub const VK_TAB: usize = 0x09;
/// `VK_RETURN`, Enter.
pub const VK_RETURN: usize = 0x0D;
/// `VK_SHIFT`.
pub const VK_SHIFT: i32 = 0x10;
/// `VK_CONTROL`.
pub const VK_CONTROL: i32 = 0x11;
/// `VK_MENU`, Alt.
pub const VK_MENU: i32 = 0x12;
/// `VK_ESCAPE`.
pub const VK_ESCAPE: usize = 0x1B;
/// `VK_SPACE`.
pub const VK_SPACE: usize = 0x20;
/// `VK_PRIOR` (Page Up).
pub const VK_PRIOR: usize = 0x21;
/// `VK_NEXT` (Page Down).
pub const VK_NEXT: usize = 0x22;
/// `VK_END`.
pub const VK_END: usize = 0x23;
/// `VK_HOME`.
pub const VK_HOME: usize = 0x24;
/// `VK_LEFT`.
pub const VK_LEFT: usize = 0x25;
/// `VK_UP`.
pub const VK_UP: usize = 0x26;
/// `VK_RIGHT`.
pub const VK_RIGHT: usize = 0x27;
/// `VK_DOWN`.
pub const VK_DOWN: usize = 0x28;
/// `VK_DELETE`.
pub const VK_DELETE: usize = 0x2E;
/// `VK_F2`.
pub const VK_F2: usize = 0x71;
/// `VK_F4`.
pub const VK_F4: usize = 0x73;
/// `VK_F5`.
pub const VK_F5: usize = 0x74;
/// `VK_F10`.
pub const VK_F10: usize = 0x79;
/// `VK_APPS` (the menu key).
pub const VK_APPS: usize = 0x5D;

/// The modifiers from `GetKeyState` results for Shift, Control and Alt: a key is held when
/// the high bit of its state is set.
pub fn modifiers_from_key_states(shift: i16, control: i16, alt: i16) -> Modifiers {
    Modifiers {
        shift: shift < 0,
        control: control < 0,
        alt: alt < 0,
        command: false,
    }
}

/// The key a `WM_KEYDOWN` or `WM_SYSKEYDOWN` virtual-key code stands for. Keys that type text
/// arrive through `WM_CHAR` instead ([`key_press_from_character`]), except letters and digits
/// with Control held (and not Alt), whose `WM_CHAR` is a control code: those are answered
/// here as the letter or digit, upper case with Shift.
pub fn key_from_virtual_key(virtual_key: usize, modifiers: Modifiers) -> Option<Key> {
    Some(match virtual_key {
        VK_TAB => Key::Tab,
        VK_RETURN => Key::Enter,
        VK_ESCAPE => Key::Escape,
        VK_SPACE => Key::Space,
        VK_BACK => Key::Backspace,
        VK_DELETE => Key::Delete,
        VK_LEFT => Key::Left,
        VK_RIGHT => Key::Right,
        VK_UP => Key::Up,
        VK_DOWN => Key::Down,
        VK_HOME => Key::Home,
        VK_END => Key::End,
        VK_F4 => Key::F4,
        VK_F2 => Key::F2,
        VK_F5 => Key::F5,
        VK_F10 => Key::F10,
        VK_APPS => Key::ContextMenu,
        VK_PRIOR => Key::PageUp,
        VK_NEXT => Key::PageDown,
        0x30..=0x39 | 0x41..=0x5A if modifiers.control && !modifiers.alt => {
            let code = u8::try_from(virtual_key).ok()?;
            let character = char::from(code);
            Key::Character(if modifiers.shift {
                character
            } else {
                character.to_ascii_lowercase()
            })
        }
        _ => return None,
    })
}

/// Whether the window answers a `WM_SYSKEYDOWN` or `WM_SYSKEYUP` for `virtual_key` itself
/// rather than passing it to `DefWindowProcW`: `F10` and `Shift+F10`, which the default turns
/// into `SC_KEYMENU` and the window menu's keyboard mode, so the next `Down` or `Enter` would
/// reach the system menu instead of the GUI. Keys held with Alt, such as `Alt+F4`, keep the
/// default.
pub fn system_key_is_answered(virtual_key: usize, modifiers: Modifiers) -> bool {
    virtual_key == VK_F10 && !modifiers.alt
}

/// The key press a `WM_CHAR` character makes, or `None` when the key was already answered by
/// [`key_from_virtual_key`]: control codes, the space bar, and anything typed with Control
/// but not Alt. Control and Alt together are AltGr, whose character the keyboard layout has
/// already produced, so they are not reported as held.
pub fn key_press_from_character(character: char, modifiers: Modifiers) -> Option<KeyPress> {
    let alt_gr = modifiers.control && modifiers.alt;
    if character.is_control() || character == ' ' || (modifiers.control && !alt_gr) {
        return None;
    }
    let modifiers = if alt_gr {
        Modifiers {
            control: false,
            alt: false,
            ..modifiers
        }
    } else {
        modifiers
    };
    Some(KeyPress {
        key: Key::Character(character),
        modifiers,
    })
}

/// Joins the UTF-16 code units `WM_CHAR` delivers one per message into characters: a high
/// surrogate waits for the low surrogate that follows it, and an unpaired surrogate is
/// dropped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Utf16Characters {
    high_surrogate: Option<u16>,
}

impl Utf16Characters {
    /// Takes the next code unit and returns the character it completes, if any.
    pub fn push(&mut self, unit: u16) -> Option<char> {
        match unit {
            0xD800..=0xDBFF => {
                self.high_surrogate = Some(unit);
                None
            }
            0xDC00..=0xDFFF => {
                let high = self.high_surrogate.take()?;
                let value =
                    0x1_0000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
                char::from_u32(value)
            }
            _ => {
                self.high_surrogate = None;
                char::from_u32(u32::from(unit))
            }
        }
    }
}

/// The low 16 bits of a message parameter (`LOWORD`).
pub fn low_word(value: usize) -> u16 {
    (value & 0xFFFF) as u16
}

/// Bits 16 to 31 of a message parameter (`HIWORD`).
pub fn high_word(value: usize) -> u16 {
    ((value >> 16) & 0xFFFF) as u16
}

/// The client area's width and height from `WM_SIZE`'s `lParam`.
pub fn client_size_from_lparam(lparam: isize) -> (u32, u32) {
    let bits = lparam as usize;
    (u32::from(low_word(bits)), u32::from(high_word(bits)))
}

/// The pointer's position in a mouse message's `lparam` (`GET_X_LPARAM`, `GET_Y_LPARAM`):
/// signed, since a captured pointer outside the window has negative coordinates.
pub fn point_from_lparam(lparam: isize) -> (i32, i32) {
    let bits = lparam as usize;
    (
        i32::from(low_word(bits) as i16),
        i32::from(high_word(bits) as i16),
    )
}

/// The wheel's travel in a `WM_MOUSEWHEEL` or `WM_MOUSEHWHEEL` `wparam`, in notches of
/// `WHEEL_DELTA` (120): positive away from the user (up) or to the right.
pub fn wheel_notches(wparam: usize) -> f32 {
    f32::from(high_word(wparam) as i16) / WHEEL_DELTA
}

const WHEEL_DELTA: f32 = 120.0;

/// `text` as `CF_UNICODETEXT` wants it: UTF-16 with `\r\n` line breaks and a closing NUL.
pub fn clipboard_units(text: &str) -> Vec<u16> {
    let mut units = Vec::with_capacity(text.len() + 1);
    let mut previous = '\0';
    for character in text.chars() {
        if character == '\n' && previous != '\r' {
            units.push(u16::from(b'\r'));
        }
        if character != '\0' {
            let mut buffer = [0u16; 2];
            units.extend_from_slice(character.encode_utf16(&mut buffer));
        }
        previous = character;
    }
    units.push(0);
    units
}

/// The text of `CF_UNICODETEXT` units, up to the first NUL, with unpaired surrogates replaced.
pub fn text_from_clipboard_units(units: &[u16]) -> String {
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    String::from_utf16_lossy(units.get(..end).unwrap_or(&[]))
}

/// Device pixels per logical pixel for a DPI: the DPI over 96, and 1 for a DPI of 0 (what
/// `GetDpiForWindow` answers for an invalid window).
pub fn scale_from_dpi(dpi: u32) -> f32 {
    if dpi == 0 {
        1.0
    } else {
        dpi as f32 / BASE_DPI as f32
    }
}

/// A logical length in device pixels at `scale`, at least 1 and at most what a Win32
/// coordinate holds.
pub fn device_pixels(logical: u32, scale: f32) -> i32 {
    let scaled = (logical as f64 * f64::from(scale)).round();
    if scaled.is_finite() && scaled >= 1.0 {
        scaled.min(f64::from(i32::MAX)) as i32
    } else {
        1
    }
}

/// Converts `rgba` (red, green, blue, alpha per pixel) into `bgra` (blue, green, red,
/// alpha), the byte order of a 32-bit `BI_RGB` bitmap, reusing `bgra`'s allocation. A
/// trailing partial pixel is dropped.
pub fn bgra_from_rgba(rgba: &[u8], bgra: &mut Vec<u8>) {
    bgra.clear();
    bgra.reserve(rgba.len());
    for &[red, green, blue, alpha] in rgba.as_chunks::<4>().0 {
        bgra.extend_from_slice(&[blue, green, red, alpha]);
    }
}

/// `text` as a NUL-terminated UTF-16 string for the `W` functions, with any NUL inside it
/// dropped so the string is not cut short.
pub fn wide_string(text: &str) -> Vec<u16> {
    text.encode_utf16()
        .filter(|&unit| unit != 0)
        .chain(std::iter::once(0))
        .collect()
}

/// The `lpstrFilter` of an `OPENFILENAMEW`: each filter's name and its patterns joined by
/// `;`, each string NUL-terminated, the list ended by a second NUL. NULs inside a name or a
/// pattern are dropped; an empty list is a lone pair of NULs.
pub fn open_file_filter(filters: &[FileFilter]) -> Vec<u16> {
    let mut wide = Vec::new();
    for filter in filters {
        let patterns = filter.patterns.join(";");
        for text in [filter.name.as_str(), patterns.as_str()] {
            wide.extend(text.encode_utf16().filter(|&unit| unit != 0));
            wide.push(0);
        }
    }
    if wide.is_empty() {
        wide.push(0);
    }
    wide.push(0);
    wide
}

/// The path the dialog wrote into `buffer`: the UTF-16 units before the first NUL, or `None`
/// when the buffer holds no path or the units are not valid UTF-16.
pub fn path_from_wide_buffer(buffer: &[u16]) -> Option<PathBuf> {
    let length = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    let units = buffer.get(..length)?;
    if units.is_empty() {
        return None;
    }
    String::from_utf16(units).ok().map(PathBuf::from)
}

/// `DROPEFFECT_NONE`: the drop target takes nothing.
pub const DROPEFFECT_NONE: u32 = 0;
/// `DROPEFFECT_COPY`: the drop target copies the data (opens the file, leaving it in place).
pub const DROPEFFECT_COPY: u32 = 1;

/// The drop effect a target answers with for a drag it `accepts`, among the effects the
/// source `allowed`: a copy when the source allows one, otherwise none.
pub fn drop_effect(accepts: bool, allowed: u32) -> u32 {
    if accepts && allowed & DROPEFFECT_COPY != 0 {
        DROPEFFECT_COPY
    } else {
        DROPEFFECT_NONE
    }
}

/// The drag-over state of the window's OLE drop target: whether the drag over it carries
/// files, which the `IDropTarget` methods (`DragEnter`, `DragOver`, `DragLeave`, `Drop`) turn
/// into the drop effect and the window's drag events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileDragTarget {
    accepting: bool,
}

impl FileDragTarget {
    /// `DragEnter` with the files the drag carries (none when it carries no `CF_HDROP`) and
    /// the effects the source allows: the effect, and [`WindowEvent::FileDragEntered`] when
    /// it carries files.
    pub fn enter(&mut self, paths: Vec<PathBuf>, allowed: u32) -> (u32, Option<WindowEvent>) {
        self.accepting = !paths.is_empty();
        let event = self
            .accepting
            .then_some(WindowEvent::FileDragEntered(paths));
        (drop_effect(self.accepting, allowed), event)
    }

    /// `DragOver`: the same effect as on entering, among the effects allowed now.
    pub fn over(&self, allowed: u32) -> u32 {
        drop_effect(self.accepting, allowed)
    }

    /// `DragLeave`: [`WindowEvent::FileDragLeft`] when the drag carried files.
    pub fn leave(&mut self) -> Option<WindowEvent> {
        std::mem::take(&mut self.accepting).then_some(WindowEvent::FileDragLeft)
    }

    /// `Drop` with the files dropped: the effect and [`WindowEvent::FilesDropped`], or, when
    /// nothing usable was dropped, no effect and the end of the drag-over ring.
    pub fn drop(&mut self, paths: Vec<PathBuf>, allowed: u32) -> (u32, Option<WindowEvent>) {
        let was_accepting = std::mem::take(&mut self.accepting);
        if paths.is_empty() {
            return (
                DROPEFFECT_NONE,
                was_accepting.then_some(WindowEvent::FileDragLeft),
            );
        }
        (
            drop_effect(true, allowed),
            Some(WindowEvent::FilesDropped(paths)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELD: i16 = i16::MIN;

    #[test]
    fn a_file_drag_shows_the_ring_copies_and_drops() {
        let file = || vec![PathBuf::from("C:\\data\\shop.eggdb")];
        let mut target = FileDragTarget::default();
        assert_eq!(
            target.enter(file(), DROPEFFECT_COPY | 2),
            (DROPEFFECT_COPY, Some(WindowEvent::FileDragEntered(file())))
        );
        assert_eq!(target.over(DROPEFFECT_COPY), DROPEFFECT_COPY);
        assert_eq!(target.over(2), DROPEFFECT_NONE);
        assert_eq!(
            target.drop(file(), DROPEFFECT_COPY),
            (DROPEFFECT_COPY, Some(WindowEvent::FilesDropped(file())))
        );
        assert_eq!(target.leave(), None);
    }

    #[test]
    fn a_drag_without_files_is_refused_and_a_leave_ends_the_ring() {
        let mut target = FileDragTarget::default();
        assert_eq!(
            target.enter(Vec::new(), DROPEFFECT_COPY),
            (DROPEFFECT_NONE, None)
        );
        assert_eq!(target.over(DROPEFFECT_COPY), DROPEFFECT_NONE);
        assert_eq!(target.leave(), None);
        let file = vec![PathBuf::from("shop.eggdb")];
        target.enter(file, DROPEFFECT_COPY);
        assert_eq!(target.leave(), Some(WindowEvent::FileDragLeft));
        assert_eq!(target.leave(), None);
        target.enter(vec![PathBuf::from("shop.eggdb")], DROPEFFECT_COPY);
        assert_eq!(
            target.drop(Vec::new(), DROPEFFECT_COPY),
            (DROPEFFECT_NONE, Some(WindowEvent::FileDragLeft))
        );
    }

    #[test]
    fn the_dialog_filter_is_name_and_patterns_pairs_ending_in_two_nuls() {
        let filters = [
            FileFilter {
                name: "EggDB databases".to_owned(),
                patterns: vec!["*.eggdb".to_owned()],
            },
            FileFilter {
                name: "All files".to_owned(),
                patterns: vec!["*.*".to_owned(), "*\0x".to_owned()],
            },
        ];
        let expected: Vec<u16> = "EggDB databases\0*.eggdb\0All files\0*.*;*x\0\0"
            .encode_utf16()
            .collect();
        assert_eq!(open_file_filter(&filters), expected);
        assert_eq!(open_file_filter(&[]), vec![0, 0]);
    }

    #[test]
    fn a_dialog_buffer_reads_up_to_its_first_nul() {
        let mut buffer = vec![0u16; 16];
        for (slot, unit) in buffer.iter_mut().zip("C:\\a.eggdb".encode_utf16()) {
            *slot = unit;
        }
        assert_eq!(
            path_from_wide_buffer(&buffer),
            Some(PathBuf::from("C:\\a.eggdb"))
        );
        assert_eq!(path_from_wide_buffer(&[0; 4]), None);
        assert_eq!(path_from_wide_buffer(&[0xD800, 0]), None);
        assert_eq!(path_from_wide_buffer(&[]), None);
    }

    fn with(shift: bool, control: bool, alt: bool) -> Modifiers {
        Modifiers {
            shift,
            control,
            alt,
            command: false,
        }
    }

    #[test]
    fn the_high_bit_of_a_key_state_means_held() {
        assert_eq!(
            modifiers_from_key_states(HELD, 0, 1),
            with(true, false, false)
        );
        assert_eq!(
            modifiers_from_key_states(0, HELD | 1, HELD),
            with(false, true, true)
        );
        assert_eq!(modifiers_from_key_states(1, 1, 1), Modifiers::default());
    }

    #[test]
    fn named_virtual_keys_map_to_the_gui_keys() {
        let plain = Modifiers::default();
        let cases = [
            (VK_TAB, Key::Tab),
            (VK_RETURN, Key::Enter),
            (VK_ESCAPE, Key::Escape),
            (VK_SPACE, Key::Space),
            (VK_BACK, Key::Backspace),
            (VK_DELETE, Key::Delete),
            (VK_LEFT, Key::Left),
            (VK_RIGHT, Key::Right),
            (VK_UP, Key::Up),
            (VK_DOWN, Key::Down),
            (VK_HOME, Key::Home),
            (VK_END, Key::End),
            (VK_F4, Key::F4),
            (VK_F2, Key::F2),
            (VK_F5, Key::F5),
            (VK_F10, Key::F10),
            (VK_APPS, Key::ContextMenu),
            (VK_PRIOR, Key::PageUp),
            (VK_NEXT, Key::PageDown),
        ];
        for (virtual_key, key) in cases {
            assert_eq!(key_from_virtual_key(virtual_key, plain), Some(key));
        }
        assert_eq!(
            key_from_virtual_key(VK_TAB, with(true, false, false)),
            Some(Key::Tab)
        );
        assert_eq!(key_from_virtual_key(0x41, plain), None);
        assert_eq!(key_from_virtual_key(0x70, plain), None);
        assert_eq!(key_from_virtual_key(usize::MAX, plain), None);
    }

    #[test]
    fn f10_is_answered_by_the_window_and_alt_combinations_keep_the_default() {
        assert!(system_key_is_answered(VK_F10, Modifiers::default()));
        assert!(system_key_is_answered(VK_F10, with(true, false, false)));
        assert!(system_key_is_answered(VK_F10, with(false, true, false)));
        assert!(!system_key_is_answered(VK_F10, with(false, false, true)));
        assert!(!system_key_is_answered(VK_F4, with(false, false, true)));
        assert!(!system_key_is_answered(VK_APPS, Modifiers::default()));
        assert!(!system_key_is_answered(usize::MAX, Modifiers::default()));
    }

    #[test]
    fn control_with_a_letter_or_digit_is_answered_from_the_virtual_key() {
        assert_eq!(
            key_from_virtual_key(0x51, with(false, true, false)),
            Some(Key::Character('q'))
        );
        assert_eq!(
            key_from_virtual_key(0x51, with(true, true, false)),
            Some(Key::Character('Q'))
        );
        assert_eq!(
            key_from_virtual_key(0x37, with(true, true, false)),
            Some(Key::Character('7'))
        );
        assert_eq!(key_from_virtual_key(0x51, with(false, true, true)), None);
    }

    #[test]
    fn typed_characters_become_key_presses_once() {
        assert_eq!(
            key_press_from_character('a', Modifiers::default()),
            Some(KeyPress::plain(Key::Character('a')))
        );
        assert_eq!(
            key_press_from_character('A', with(true, false, false)),
            Some(KeyPress {
                key: Key::Character('A'),
                modifiers: with(true, false, false),
            })
        );
        assert_eq!(key_press_from_character(' ', Modifiers::default()), None);
        assert_eq!(key_press_from_character('\t', Modifiers::default()), None);
        assert_eq!(key_press_from_character('\r', Modifiers::default()), None);
        assert_eq!(
            key_press_from_character('\u{11}', with(false, true, false)),
            None
        );
        assert_eq!(
            key_press_from_character('q', with(false, true, false)),
            None
        );
    }

    #[test]
    fn alt_gr_characters_arrive_without_control_and_alt() {
        assert_eq!(
            key_press_from_character('@', with(false, true, true)),
            Some(KeyPress::plain(Key::Character('@')))
        );
        assert_eq!(
            key_press_from_character('€', with(true, true, true)),
            Some(KeyPress {
                key: Key::Character('€'),
                modifiers: with(true, false, false),
            })
        );
    }

    #[test]
    fn surrogate_pairs_join_and_unpaired_halves_are_dropped() {
        let mut characters = Utf16Characters::default();
        assert_eq!(characters.push(u16::from(b'x')), Some('x'));
        assert_eq!(characters.push(0xD83D), None);
        assert_eq!(characters.push(0xDE00), Some('😀'));
        assert_eq!(characters.push(0xDE00), None);
        assert_eq!(characters.push(0xD83D), None);
        assert_eq!(characters.push(u16::from(b'y')), Some('y'));
        assert_eq!(characters.push(0xDE00), None);
        assert_eq!(characters.push(0x00E9), Some('é'));
    }

    #[test]
    fn message_words_and_client_sizes_unpack() {
        assert_eq!(low_word(0x0012_0034), 0x34);
        assert_eq!(high_word(0x0012_0034), 0x12);
        assert_eq!(high_word(usize::MAX), 0xFFFF);
        assert_eq!(client_size_from_lparam(0x0258_0320), (800, 600));
        assert_eq!(client_size_from_lparam(-1), (0xFFFF, 0xFFFF));
    }

    #[test]
    fn dpi_turns_into_a_scale() {
        assert_eq!(scale_from_dpi(96), 1.0);
        assert_eq!(scale_from_dpi(144), 1.5);
        assert_eq!(scale_from_dpi(192), 2.0);
        assert_eq!(scale_from_dpi(0), 1.0);
    }

    #[test]
    fn logical_lengths_scale_to_device_pixels_within_range() {
        assert_eq!(device_pixels(800, 1.0), 800);
        assert_eq!(device_pixels(800, 1.5), 1200);
        assert_eq!(device_pixels(0, 2.0), 1);
        assert_eq!(device_pixels(u32::MAX, 4.0), i32::MAX);
        assert_eq!(device_pixels(10, f32::NAN), 1);
    }

    #[test]
    fn rgba_frames_become_bgra_rows() {
        let mut bgra = vec![9; 3];
        bgra_from_rgba(&[1, 2, 3, 4, 10, 20, 30, 40, 7], &mut bgra);
        assert_eq!(bgra, [3, 2, 1, 4, 30, 20, 10, 40]);
        bgra_from_rgba(&[], &mut bgra);
        assert!(bgra.is_empty());
    }

    #[test]
    fn wide_strings_end_in_one_nul() {
        assert_eq!(wide_string("Eg"), [0x45, 0x67, 0]);
        assert_eq!(wide_string("a\0b"), [0x61, 0x62, 0]);
        assert_eq!(wide_string(""), [0]);
    }

    #[test]
    fn mouse_parameters_give_signed_points_and_wheel_notches() {
        let lparam = ((0xFFF6usize << 16) | 300) as isize;
        assert_eq!(point_from_lparam(lparam), (300, -10));
        assert_eq!(wheel_notches(120 << 16), 1.0);
        assert_eq!(wheel_notches(0xFF88 << 16), -1.0);
        assert_eq!(wheel_notches(60 << 16), 0.5);
    }

    #[test]
    fn clipboard_text_round_trips_through_unicode_units() {
        let units = clipboard_units("SELECT 1;\nSELECT 'é😀';\r\n");
        assert_eq!(units.last(), Some(&0));
        assert_eq!(
            units
                .iter()
                .filter(|&&unit| unit == u16::from(b'\r'))
                .count(),
            2
        );
        assert_eq!(
            text_from_clipboard_units(&units),
            "SELECT 1;\r\nSELECT 'é😀';\r\n"
        );
        assert_eq!(text_from_clipboard_units(&[0x61, 0, 0x62]), "a");
        assert_eq!(text_from_clipboard_units(&[0xD800, 0x61]), "\u{FFFD}a");
    }
}
