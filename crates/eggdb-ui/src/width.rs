const WIDE: [(u32, u32); 18] = [
    (0x1100, 0x115F),
    (0x231A, 0x231B),
    (0x2E80, 0x303E),
    (0x3041, 0x33FF),
    (0x3400, 0x4DBF),
    (0x4E00, 0x9FFF),
    (0xA000, 0xA4CF),
    (0xA960, 0xA97F),
    (0xAC00, 0xD7A3),
    (0xF900, 0xFAFF),
    (0xFE10, 0xFE19),
    (0xFE30, 0xFE6F),
    (0xFF00, 0xFF60),
    (0xFFE0, 0xFFE6),
    (0x1F300, 0x1F64F),
    (0x1F900, 0x1F9FF),
    (0x20000, 0x2FFFD),
    (0x30000, 0x3FFFD),
];

const COMBINING: [(u32, u32); 16] = [
    (0x0300, 0x036F),
    (0x0483, 0x0489),
    (0x0591, 0x05BD),
    (0x0610, 0x061A),
    (0x064B, 0x065F),
    (0x0E31, 0x0E31),
    (0x0E34, 0x0E3A),
    (0x0E47, 0x0E4E),
    (0x1AB0, 0x1AFF),
    (0x1DC0, 0x1DFF),
    (0x200B, 0x200F),
    (0x20D0, 0x20FF),
    (0x302A, 0x302F),
    (0x3099, 0x309A),
    (0xFE00, 0xFE0F),
    (0xFE20, 0xFE2F),
];

fn in_ranges(ranges: &[(u32, u32)], code: u32) -> bool {
    ranges
        .binary_search_by(|&(first, last)| {
            if last < code {
                std::cmp::Ordering::Less
            } else if first > code {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

pub(crate) fn char_width(character: char) -> usize {
    let code = u32::from(character);
    if code < 0x300 {
        return 1;
    }
    if in_ranges(&COMBINING, code) {
        0
    } else if in_ranges(&WIDE, code) {
        2
    } else {
        1
    }
}

pub(crate) fn text_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

#[cfg(test)]
mod tests {
    use super::{COMBINING, WIDE, char_width, text_width};

    #[test]
    fn the_tables_are_sorted_and_disjoint() {
        for table in [&WIDE[..], &COMBINING[..]] {
            for pair in table.windows(2) {
                assert!(pair[0].0 <= pair[0].1 && pair[0].1 < pair[1].0);
            }
        }
    }

    #[test]
    fn ascii_is_one_cell_wide() {
        assert_eq!(text_width("user:1"), 6);
    }

    #[test]
    fn east_asian_wide_characters_take_two_cells() {
        assert_eq!(char_width('漢'), 2);
        assert_eq!(char_width('한'), 2);
        assert_eq!(char_width('Ａ'), 2);
        assert_eq!(text_width("日本"), 4);
    }

    #[test]
    fn combining_marks_take_no_cell() {
        assert_eq!(char_width('\u{301}'), 0);
        assert_eq!(text_width("e\u{301}"), 1);
    }

    #[test]
    fn other_scripts_and_symbols_take_one_cell() {
        assert_eq!(char_width('é'), 1);
        assert_eq!(char_width('Ж'), 1);
        assert_eq!(char_width('…'), 1);
        assert_eq!(char_width('│'), 1);
    }
}
