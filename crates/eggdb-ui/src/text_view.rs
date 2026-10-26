use crate::theme::Role;
use crate::width::char_width;

pub(crate) type Segment = (String, Role);

const TAB_CELLS: usize = 4;
pub(crate) const LONGEST_WALK: usize = 1 << 20;

enum Unit {
    Char(char),
    Tab,
    Control(u32),
    Invalid(u8),
}

impl Unit {
    fn width(&self) -> usize {
        match self {
            Self::Char(c) => char_width(*c),
            Self::Tab => TAB_CELLS,
            Self::Control(code) if *code < 0x20 || *code == 0x7f => 2,
            Self::Control(code) => format!("\\u{{{code:x}}}").len(),
            Self::Invalid(_) => 4,
        }
    }

    fn push(&self, segments: &mut Vec<Segment>) {
        let (text, role) = match self {
            Self::Char(c) => (c.to_string(), Role::Plain),
            Self::Tab => (" ".repeat(TAB_CELLS), Role::Plain),
            Self::Control(code) if *code < 0x20 || *code == 0x7f => {
                let shown = char::from_u32(code ^ 0x40).unwrap_or('?');
                (format!("^{shown}"), Role::Plain)
            }
            Self::Control(code) => (format!("\\u{{{code:x}}}"), Role::Plain),
            Self::Invalid(byte) => (format!("\\x{byte:02x}"), Role::Error),
        };
        match segments.last_mut() {
            Some((last, last_role)) if *last_role == role => last.push_str(&text),
            _ => segments.push((text, role)),
        }
    }
}

fn unit_at(bytes: &[u8], at: usize) -> (usize, Unit) {
    let lead = bytes[at];
    let length = match lead {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return (1, Unit::Invalid(lead)),
    };
    let decoded = bytes
        .get(at..at + length)
        .and_then(|slice| std::str::from_utf8(slice).ok())
        .and_then(|text| text.chars().next());
    match decoded {
        Some('\t') => (1, Unit::Tab),
        Some(c) if c.is_control() => (length, Unit::Control(u32::from(c))),
        Some(c) => (length, Unit::Char(c)),
        None => (1, Unit::Invalid(lead)),
    }
}

pub(crate) fn segments(bytes: &[u8]) -> Vec<Segment> {
    prefix_segments(bytes, usize::MAX)
}

pub(crate) fn prefix_segments(bytes: &[u8], cells: usize) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut at = 0;
    let mut used = 0;
    while at < bytes.len() && used < cells {
        let (length, unit) = unit_at(bytes, at);
        used += unit.width();
        unit.push(&mut out);
        at += length;
    }
    out
}

pub(crate) fn row_end(line: &[u8], from: usize, room: usize) -> usize {
    let mut at = from;
    let mut used = 0;
    while at < line.len() {
        let (length, unit) = unit_at(line, at);
        let width = unit.width();
        if used + width > room && at > from {
            break;
        }
        used += width;
        at += length;
    }
    at
}

pub(crate) fn row_start_before(line: &[u8], offset: usize, room: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    if offset > LONGEST_WALK {
        let mut start = offset.saturating_sub(room.max(1));
        while start > 0 && line.get(start).is_some_and(|b| b & 0xc0 == 0x80) {
            start -= 1;
        }
        return start;
    }
    let mut start = 0;
    loop {
        let end = row_end(line, start, room);
        if end >= offset || end == start {
            return start;
        }
        start = end;
    }
}

#[cfg(test)]
mod tests {
    use super::{prefix_segments, row_end, row_start_before, segments};
    use crate::theme::Role;

    #[test]
    fn tabs_controls_and_invalid_bytes_are_visible() {
        assert_eq!(
            segments(b"a\tb\x01\xffc"),
            [
                ("a    b^A".to_owned(), Role::Plain),
                ("\\xff".to_owned(), Role::Error),
                ("c".to_owned(), Role::Plain),
            ]
        );
    }

    #[test]
    fn a_prefix_stops_after_the_cells_asked_for() {
        let line = vec![b'x'; 10_000_000];
        let shown = prefix_segments(&line, 30);
        assert_eq!(shown.iter().map(|(text, _)| text.len()).sum::<usize>(), 30);
    }

    #[test]
    fn rows_break_at_the_room_and_walk_back() {
        let line = b"abcdefg";
        assert_eq!(row_end(line, 0, 3), 3);
        assert_eq!(row_end(line, 3, 3), 6);
        assert_eq!(row_end(line, 6, 3), 7);
        assert_eq!(row_start_before(line, 7, 3), 6);
        assert_eq!(row_start_before(line, 6, 3), 3);
        assert_eq!(row_start_before(line, 3, 3), 0);
        assert_eq!(row_end("日本語".as_bytes(), 0, 3), 3);
    }

    #[test]
    fn walking_back_in_a_huge_line_is_bounded() {
        let line = vec![b'x'; 3 * super::LONGEST_WALK];
        let offset = 2 * super::LONGEST_WALK + 5;
        assert_eq!(row_start_before(&line, offset, 40), offset - 40);
    }
}
