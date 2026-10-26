use crate::width::is_unprintable;
use std::fmt::Write;

pub(crate) fn escaped(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            match character {
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\\' => out.push_str("\\\\"),
                c if c.is_control() => {
                    let mut buffer = [0u8; 4];
                    for byte in c.encode_utf8(&mut buffer).bytes() {
                        let _ = write!(out, "\\x{byte:02x}");
                    }
                }
                c if is_unprintable(c) => {
                    let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
                }
                c => out.push(c),
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(out, "\\x{byte:02x}");
        }
    }
    out
}

pub(crate) fn unprintables_escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if is_unprintable(character) {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(character));
        } else {
            out.push(character);
        }
    }
    out
}

pub(crate) fn unescaped(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'\\' {
            match bytes.get(at + 1) {
                Some(b'x') => {
                    if let Some(byte) = bytes.get(at + 2..at + 4).and_then(hex_value)
                        && let Ok(byte) = u8::try_from(byte)
                    {
                        out.push(byte);
                        at += 4;
                        continue;
                    }
                }
                Some(b'u') => {
                    if let Some((character, used)) = braced_character(&bytes[at + 2..]) {
                        let mut buffer = [0u8; 4];
                        out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                        at += 2 + used;
                        continue;
                    }
                }
                Some(b'\\') => {
                    out.push(b'\\');
                    at += 2;
                    continue;
                }
                Some(b't') => {
                    out.push(b'\t');
                    at += 2;
                    continue;
                }
                Some(b'n') => {
                    out.push(b'\n');
                    at += 2;
                    continue;
                }
                Some(b'r') => {
                    out.push(b'\r');
                    at += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    out
}

const LONGEST_CODE_POINT_DIGITS: usize = 6;

fn hex_value(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let text = std::str::from_utf8(digits).ok()?;
    u32::from_str_radix(text, 16).ok()
}

fn braced_character(bytes: &[u8]) -> Option<(char, usize)> {
    if bytes.first() != Some(&b'{') {
        return None;
    }
    let close = bytes
        .iter()
        .take(LONGEST_CODE_POINT_DIGITS + 2)
        .position(|&b| b == b'}')?;
    let digits = bytes.get(1..close)?;
    if digits.len() > LONGEST_CODE_POINT_DIGITS {
        return None;
    }
    let character = char::from_u32(hex_value(digits)?)?;
    Some((character, close + 1))
}

pub(crate) fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

const UNITS: [&str; 6] = ["K", "M", "G", "T", "P", "E"];

pub(crate) fn short_size(bytes: u64) -> String {
    if bytes < 1024 {
        return bytes.to_string();
    }
    let (whole, unit) = scaled(bytes);
    if whole < 100.0 {
        format!("{whole:.1} {unit}")
    } else {
        format!("{whole:.0} {unit}")
    }
}

pub(crate) fn file_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let (whole, unit) = scaled(bytes);
    format!("{whole:.1} {unit}iB")
}

fn scaled(bytes: u64) -> (f64, &'static str) {
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if (99.95..100.0).contains(&value) {
        value = 100.0;
    }
    (value, UNITS[unit])
}

pub(crate) fn value_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        short_size(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::{count, escaped, file_size, short_size, unescaped, unprintables_escaped};

    #[test]
    fn printable_text_is_shown_as_it_is() {
        assert_eq!(escaped("user:1 é 日".as_bytes()), "user:1 é 日");
    }

    #[test]
    fn controls_backslashes_and_invalid_bytes_are_escaped() {
        assert_eq!(escaped(b"a\tb\nc\rd\\e"), "a\\tb\\nc\\rd\\\\e");
        assert_eq!(escaped(b"\x00\x00\x12user"), "\\x00\\x00\\x12user");
        assert_eq!(escaped(b"\xff\xfe"), "\\xff\\xfe");
    }

    #[test]
    fn unescaping_reverses_escaping() {
        for bytes in [
            &b"\x00\x00\x12user"[..],
            b"a\tb\\c",
            b"\xff",
            "日本".as_bytes(),
        ] {
            assert_eq!(unescaped(&escaped(bytes)), bytes);
        }
        assert_eq!(unescaped("\\xZ1"), b"\\xZ1");
    }

    #[test]
    fn bidi_overrides_isolates_and_invisible_formats_are_shown_as_code_points() {
        assert_eq!(
            escaped("user\u{202e}gnp.exe".as_bytes()),
            "user\\u{202e}gnp.exe"
        );
        assert_eq!(
            escaped("a\u{2067}b\u{2069}\u{200b}\u{feff}\u{ad}".as_bytes()),
            "a\\u{2067}b\\u{2069}\\u{200b}\\u{feff}\\u{ad}"
        );
        assert_eq!(escaped("👩\u{200d}💻".as_bytes()), "👩\u{200d}💻");
        for text in [
            "user\u{202e}gnp.exe",
            "\u{2066}x\u{2069}",
            "\\u{202e} typed",
        ] {
            assert_eq!(unescaped(&escaped(text.as_bytes())), text.as_bytes());
        }
    }

    #[test]
    fn json_text_keeps_its_backslashes_and_shows_format_characters() {
        assert_eq!(
            unprintables_escaped("\"a\\\"b\u{202e}\u{85}\""),
            "\"a\\\"b\\u{202e}\\u{85}\""
        );
    }

    #[test]
    fn code_point_escapes_need_braces_and_a_valid_scalar() {
        assert_eq!(unescaped("\\u{202e}"), "\u{202e}".as_bytes());
        assert_eq!(unescaped("\\u{41}b"), b"Ab");
        assert_eq!(unescaped("\\u{d800}"), b"\\u{d800}");
        assert_eq!(unescaped("\\u{1100000}"), b"\\u{1100000}");
        assert_eq!(unescaped("\\u{}"), b"\\u{}");
        assert_eq!(unescaped("\\u{+41}"), b"\\u{+41}");
        assert_eq!(unescaped("\\u202e"), b"\\u202e");
        assert_eq!(unescaped("\\u{41"), b"\\u{41");
        assert_eq!(unescaped("\\u"), b"\\u");
    }

    #[test]
    fn a_byte_escape_takes_two_hex_digits_and_no_sign() {
        assert_eq!(unescaped("\\x+f"), b"\\x+f");
        assert_eq!(unescaped("\\x-1"), b"\\x-1");
        assert_eq!(unescaped("\\x0F"), b"\x0f");
        assert_eq!(unescaped("\\xf"), b"\\xf");
    }

    #[test]
    fn counts_have_thousands_separators() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1204), "1,204");
        assert_eq!(count(1_234_567), "1,234,567");
    }

    #[test]
    fn sizes_use_powers_of_1024() {
        assert_eq!(short_size(58), "58");
        assert_eq!(short_size(1229), "1.2 K");
        assert_eq!(short_size(14 * 1024 + 600), "14.6 K");
        assert_eq!(short_size(200 * 1024), "200 K");
        assert_eq!(short_size(1 << 30), "1.0 G");
        assert_eq!(file_size(1_258_291), "1.2 MiB");
        assert_eq!(file_size(512), "512 B");
    }
}
