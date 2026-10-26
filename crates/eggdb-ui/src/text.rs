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
                c => out.push(c),
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(out, "\\x{byte:02x}");
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
                    let digits = bytes.get(at + 2..at + 4).and_then(|pair| {
                        std::str::from_utf8(pair)
                            .ok()
                            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                    });
                    if let Some(byte) = digits {
                        out.push(byte);
                        at += 4;
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
    use super::{count, escaped, file_size, short_size, unescaped};

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
