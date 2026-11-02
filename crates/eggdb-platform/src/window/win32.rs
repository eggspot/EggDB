//! The pure parts of the Win32 window backend, compiled and tested on every operating system:
//! translating virtual-key codes and `WM_CHAR` UTF-16 units into the GUI's keys, reading the
//! modifier state, packing the words of a message's parameters, turning a monitor's DPI into a
//! scale, and converting an RGBA frame into the BGRA rows a 32-bit device-independent bitmap
//! holds. The values come from Microsoft's public Win32 documentation.

use super::{Key, KeyPress, Modifiers};

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
/// `VK_F4`.
pub const VK_F4: usize = 0x73;

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
    for pixel in rgba.chunks_exact(4) {
        if let [red, green, blue, alpha] = *pixel {
            bgra.extend_from_slice(&[blue, green, red, alpha]);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    const HELD: i16 = i16::MIN;

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
}
