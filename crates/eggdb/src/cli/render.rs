use std::fmt::Write;

const KIB: u64 = 1024;
const MIB: u64 = KIB * 1024;
const GIB: u64 = MIB * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineBreaks {
    Refused,
    Allowed,
}

pub(crate) fn as_plain_text(bytes: &[u8], line_breaks: LineBreaks) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let allowed = |c: char| {
        line_breaks == LineBreaks::Allowed && matches!(c, '\n' | '\t' | '\r') || !c.is_control()
    };
    text.chars().all(allowed).then_some(text)
}

pub(crate) fn text(bytes: &[u8]) -> String {
    text_with(bytes, LineBreaks::Refused)
}

pub(crate) fn text_with(bytes: &[u8], line_breaks: LineBreaks) -> String {
    match as_plain_text(bytes, line_breaks) {
        Some(text) => text.to_owned(),
        None => format!("0x{}", hex(bytes)),
    }
}

pub(crate) fn text_cut(bytes: &[u8], longest: usize) -> Option<String> {
    if bytes.len() <= longest {
        return None;
    }
    let shown = match as_plain_text(bytes, LineBreaks::Refused) {
        Some(text) => {
            let mut end = longest;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text[..end].to_owned()
        }
        None => format!("0x{}", hex(&bytes[..longest])),
    };
    Some(shown)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    push_hex(&mut out, bytes);
    out
}

pub(crate) fn push_hex(out: &mut String, bytes: &[u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0xf)]));
    }
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

pub(crate) fn size(bytes: u64) -> String {
    let (unit, name) = match bytes {
        0..KIB => return format!("{bytes} B"),
        KIB..MIB => (KIB, "KiB"),
        MIB..GIB => (MIB, "MiB"),
        _ => (GIB, "GiB"),
    };
    let tenths = (u128::from(bytes) * 10 + u128::from(unit) / 2) / u128::from(unit);
    format!("{}.{} {name}", tenths / 10, tenths % 10)
}

pub(crate) fn page_size(bytes: usize) -> String {
    format!("{} KiB", bytes / 1024)
}

pub(crate) fn pages(n: u64) -> String {
    match n {
        1 => "1 page".to_owned(),
        _ => format!("{} pages", count(n)),
    }
}

pub(crate) fn entries(n: u64) -> String {
    match n {
        0 => "no entries".to_owned(),
        1 => "1 entry".to_owned(),
        _ => format!("{} entries", count(n)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimeStyle {
    Readable,
    Rfc3339,
}

pub(crate) fn utc_time(unix_micros: u64, style: TimeStyle) -> String {
    let seconds = unix_micros / 1_000_000;
    let micros = unix_micros % 1_000_000;
    let (year, month, day) = civil_from_days(seconds / 86_400);
    let of_day = seconds % 86_400;
    let (hour, minute, second) = (of_day / 3600, of_day / 60 % 60, of_day % 60);
    let date = format!("{year:04}-{month:02}-{day:02}");
    let time = format!("{hour:02}:{minute:02}:{second:02}.{micros:06}");
    match style {
        TimeStyle::Readable => format!("{date} {time} UTC"),
        TimeStyle::Rfc3339 => format!("{date}T{time}Z"),
    }
}

fn civil_from_days(days_since_epoch: u64) -> (u64, u64, u64) {
    let shifted = days_since_epoch + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[derive(Debug, Default)]
pub(crate) struct JsonObject {
    out: String,
}

impl JsonObject {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn name(&mut self, name: &str) {
        self.out.push(if self.out.is_empty() { '{' } else { ',' });
        push_json_string(&mut self.out, name);
        self.out.push(':');
    }

    pub(crate) fn string(mut self, name: &str, value: &str) -> Self {
        self.name(name);
        push_json_string(&mut self.out, value);
        self
    }

    pub(crate) fn optional_string(mut self, name: &str, value: Option<&str>) -> Self {
        match value {
            Some(value) => self.string(name, value),
            None => {
                self.name(name);
                self.out.push_str("null");
                self
            }
        }
    }

    pub(crate) fn string_if_any(self, name: &str, value: Option<&str>) -> Self {
        match value {
            Some(value) => self.string(name, value),
            None => self,
        }
    }

    pub(crate) fn number(mut self, name: &str, value: u64) -> Self {
        self.name(name);
        let _ = write!(self.out, "{value}");
        self
    }

    pub(crate) fn boolean(mut self, name: &str, value: bool) -> Self {
        self.name(name);
        self.out.push_str(if value { "true" } else { "false" });
        self
    }

    pub(crate) fn bytes(mut self, name: &str, value: &[u8]) -> Self {
        match std::str::from_utf8(value) {
            Ok(text) => self.string(name, text),
            Err(_) => {
                self.name(&format!("{name}_hex"));
                self.out.push('"');
                push_hex(&mut self.out, value);
                self.out.push('"');
                self
            }
        }
    }

    pub(crate) fn raw(mut self, name: &str, json: &str) -> Self {
        self.name(name);
        self.out.push_str(json);
        self
    }

    pub(crate) fn object(mut self) -> String {
        if self.out.is_empty() {
            self.out.push('{');
        }
        self.out.push('}');
        self.out
    }

    pub(crate) fn line(mut self) -> String {
        if self.out.is_empty() {
            self.out.push('{');
        }
        self.out.push_str("}\n");
        self.out
    }
}

pub(crate) fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    push_json_string(&mut out, text);
    out
}

fn push_json_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c <= '\u{1f}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::{
        JsonObject, LineBreaks, TimeStyle, as_plain_text, count, entries, pages, size, text,
        text_cut, utc_time,
    };

    #[test]
    fn printable_utf8_is_text_and_the_rest_is_hex() {
        assert_eq!(text(b"Ada"), "Ada");
        assert_eq!(text("Zoë 😀".as_bytes()), "Zoë 😀");
        assert_eq!(text(&[0x00, 0xff, 0x10]), "0x00ff10");
        assert_eq!(text(b"a\tb"), "0x610962");
        assert_eq!(text(b"del\x7f"), "0x64656c7f");
        assert_eq!(text("csi\u{9b}".as_bytes()), "0x637369c29b");
    }

    #[test]
    fn get_keeps_line_breaks_as_text() {
        assert_eq!(
            as_plain_text(b"line\nnext\ttab\r", LineBreaks::Allowed),
            Some("line\nnext\ttab\r")
        );
        assert_eq!(as_plain_text(b"bell\x07", LineBreaks::Allowed), None);
    }

    #[test]
    fn long_values_are_cut_on_a_character_boundary() {
        assert_eq!(text_cut(b"short", 128), None);
        assert_eq!(text_cut(b"abcdef", 4), Some("abcd".to_owned()));
        assert_eq!(text_cut("aéé".as_bytes(), 4), Some("aé".to_owned()));
        assert_eq!(text_cut(&[0xff; 6], 2), Some("0xffff".to_owned()));
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(12_480), "12,480");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(count(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn entry_counts_read_as_words() {
        assert_eq!(entries(0), "no entries");
        assert_eq!(entries(1), "1 entry");
        assert_eq!(entries(12_345), "12,345 entries");
        assert_eq!(pages(0), "0 pages");
        assert_eq!(pages(1), "1 page");
        assert_eq!(pages(4_096), "4,096 pages");
    }

    #[test]
    fn sizes_have_one_decimal() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(2_097_152), "2.0 MiB");
        assert_eq!(size(202_957), "198.2 KiB");
        assert_eq!(size(1_677_722), "1.6 MiB");
        assert_eq!(size(u64::MAX), "17179869184.0 GiB");
    }

    #[test]
    fn times_are_utc_with_microseconds() {
        assert_eq!(
            utc_time(1_761_470_047_123_456, TimeStyle::Readable),
            "2025-10-26 09:14:07.123456 UTC"
        );
        assert_eq!(
            utc_time(1_767_225_600_000_000, TimeStyle::Rfc3339),
            "2026-01-01T00:00:00.000000Z"
        );
        assert_eq!(
            utc_time(951_782_400_000_001, TimeStyle::Rfc3339),
            "2000-02-29T00:00:00.000001Z"
        );
        assert_eq!(
            utc_time(0, TimeStyle::Rfc3339),
            "1970-01-01T00:00:00.000000Z"
        );
    }

    #[test]
    fn json_strings_escape_what_json_requires() {
        let line = JsonObject::new()
            .string("key", "a\"b\\c\nd\u{1}é")
            .bytes("value", &[0xff, 0x00])
            .number("n", 7)
            .boolean("ok", true)
            .optional_string("none", None)
            .line();
        assert_eq!(
            line,
            "{\"key\":\"a\\\"b\\\\c\\nd\\u0001é\",\"value_hex\":\"ff00\",\"n\":7,\"ok\":true,\"none\":null}\n"
        );
    }
}
