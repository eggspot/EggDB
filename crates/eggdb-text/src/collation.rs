//! The Unicode Collation Algorithm (UTS #10) over the Default Unicode Collation Element Table
//! (D1345): `ducet.bin`, written by `scripts/unicode/generate_collation.py` from Unicode's
//! `allkeys.txt`, holds the collation elements of single code points and of contractions; Han
//! ideographs and the other unlisted code points get the implicit weights UTS #10 computes. A
//! sort key holds the primary weights, then the secondary, then the tertiary, each level ended
//! by a zero, with variable elements not ignorable, as CLDR's root collation orders text.

use crate::{Form, normalize};
use std::cmp::Ordering;

static TABLE: &[u8] = include_bytes!("ducet.bin");
const MAGIC: &[u8] = b"EGGUCA16";

/// How many levels a comparison looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Strength {
    /// Base letters only: accents and case are ignored.
    Primary,
    /// Base letters and accents: case is ignored.
    Secondary,
    /// Base letters, accents and case.
    Tertiary,
}

struct Table {
    singles: &'static [u8],
    contractions: &'static [u8],
    sequences: &'static [u8],
    elements: &'static [u8],
    implicit: &'static [u8],
}

fn word(bytes: &[u8], index: usize) -> Option<u32> {
    let at = index.checked_mul(4)?;
    bytes.get(at..at + 4).map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn table() -> Option<Table> {
    let rest = TABLE.strip_prefix(MAGIC)?;
    let count = |index: usize| word(rest, index).map(|count| count as usize);
    let (singles, contractions, sequences, elements, implicit) = (count(0)?, count(1)?, count(2)?, count(3)?, count(4)?);
    let mut at = 20;
    let mut take = |words: usize| {
        let slice = rest.get(at..at + words * 4);
        at += words * 4;
        slice
    };
    Some(Table {
        singles: take(singles * 2)?,
        contractions: take(contractions * 3)?,
        sequences: take(sequences)?,
        elements: take(elements)?,
        implicit: take(implicit * 3)?,
    })
}

#[derive(Debug, Clone, Copy)]
struct Element {
    primary: u16,
    secondary: u16,
    tertiary: u8,
}

fn unpacked(packed: u32) -> Element {
    Element {
        primary: ((packed >> 15) & 0xFFFF) as u16,
        secondary: ((packed >> 5) & 0x1FF) as u16,
        tertiary: (packed & 0x1F) as u8,
    }
}

impl Table {
    fn elements_of(&self, entry: u32, out: &mut Vec<Element>) {
        let start = (entry >> 5) as usize;
        for index in start..start + (entry & 0x1F) as usize {
            if let Some(packed) = word(self.elements, index) {
                out.push(unpacked(packed));
            }
        }
    }

    fn single(&self, code: u32) -> Option<u32> {
        let rows = self.singles.len() / 8;
        let (mut low, mut high) = (0, rows);
        while low < high {
            let middle = (low + high) / 2;
            let found = word(self.singles, middle * 2)?;
            match found.cmp(&code) {
                Ordering::Equal => return word(self.singles, middle * 2 + 1),
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
            }
        }
        None
    }

    fn longest_contraction(&self, codes: &[u32]) -> Option<(usize, u32)> {
        let first = *codes.first()?;
        let rows = self.contractions.len() / 12;
        let first_of = |row: usize| word(self.contractions, row * 3).and_then(|start| word(self.sequences, start as usize));
        let (mut low, mut high) = (0, rows);
        while low < high {
            let middle = (low + high) / 2;
            if first_of(middle)? < first {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let mut best: Option<(usize, u32)> = None;
        let mut row = low;
        while row < rows && first_of(row)? == first {
            let start = word(self.contractions, row * 3)? as usize;
            let length = word(self.contractions, row * 3 + 1)? as usize;
            let entry = word(self.contractions, row * 3 + 2)?;
            let matches = length <= codes.len()
                && (0..length).all(|offset| word(self.sequences, start + offset) == Some(codes[offset]));
            if matches && best.is_none_or(|(longest, _)| length > longest) {
                best = Some((length, entry));
            }
            row += 1;
        }
        best
    }

    fn implicit_base(&self, code: u32) -> Option<(u32, u32)> {
        for row in 0..self.implicit.len() / 12 {
            let low = word(self.implicit, row * 3)?;
            let high = word(self.implicit, row * 3 + 1)?;
            if (low..=high).contains(&code) {
                return Some((word(self.implicit, row * 3 + 2)?, code - low));
            }
        }
        None
    }
}

fn is_core_han(code: u32) -> bool {
    (0x4E00..=0x9FFF).contains(&code)
        || matches!(code, 0xFA0E | 0xFA0F | 0xFA11 | 0xFA13 | 0xFA14 | 0xFA1F | 0xFA21 | 0xFA23 | 0xFA24 | 0xFA27..=0xFA29)
}

fn is_other_han(code: u32) -> bool {
    (0x3400..=0x4DBF).contains(&code)
        || (0x20000..=0x2A6DF).contains(&code)
        || (0x2A700..=0x2EE5F).contains(&code)
        || (0x30000..=0x323AF).contains(&code)
}

fn implicit(table: &Table, code: u32, out: &mut Vec<Element>) {
    let (first, second) = match table.implicit_base(code) {
        Some((base, offset)) => (base, offset | 0x8000),
        None => {
            let base = if is_core_han(code) {
                0xFB40
            } else if is_other_han(code) {
                0xFB80
            } else {
                0xFBC0
            };
            (base + (code >> 15), (code & 0x7FFF) | 0x8000)
        }
    };
    out.push(Element { primary: first as u16, secondary: 0x20, tertiary: 0x02 });
    out.push(Element { primary: second as u16, secondary: 0, tertiary: 0 });
}

fn elements(text: &str) -> Vec<Element> {
    let codes: Vec<u32> = normalize(text, Form::Nfd).chars().map(u32::from).collect();
    let mut out = Vec::with_capacity(codes.len() * 2);
    let Some(table) = table() else {
        for &code in &codes {
            out.push(Element { primary: (code >> 16) as u16 | 0x8000, secondary: 0x20, tertiary: 0x02 });
            out.push(Element { primary: code as u16, secondary: 0, tertiary: 0 });
        }
        return out;
    };
    let mut at = 0;
    while at < codes.len() {
        if let Some((length, entry)) = table.longest_contraction(&codes[at..]) {
            table.elements_of(entry, &mut out);
            at += length;
            continue;
        }
        match table.single(codes[at]) {
            Some(entry) => table.elements_of(entry, &mut out),
            None => implicit(&table, codes[at], &mut out),
        }
        at += 1;
    }
    out
}

/// The sort key of `text` at `strength`: comparing keys bytewise orders texts as the Unicode
/// Collation Algorithm does.
pub fn sort_key(text: &str, strength: Strength) -> Vec<u8> {
    let found = elements(text);
    let mut key = Vec::with_capacity(found.len() * 6);
    for element in &found {
        if element.primary != 0 {
            key.extend_from_slice(&element.primary.to_be_bytes());
        }
    }
    if strength >= Strength::Secondary {
        key.extend_from_slice(&[0, 0]);
        for element in &found {
            if element.secondary != 0 {
                key.extend_from_slice(&element.secondary.to_be_bytes());
            }
        }
    }
    if strength >= Strength::Tertiary {
        key.extend_from_slice(&[0, 0]);
        for element in &found {
            if element.tertiary != 0 {
                key.push(element.tertiary);
            }
        }
    }
    key
}

/// Orders `left` and `right` by the Unicode Collation Algorithm at `strength`, texts equal at
/// that strength by their code points, so only identical texts compare equal.
pub fn compare_collated(left: &str, right: &str, strength: Strength) -> Ordering {
    sort_key(left, strength).cmp(&sort_key(right, strength)).then_with(|| left.cmp(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(words: &[&str]) -> Vec<String> {
        let mut words: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
        words.sort_by(|left, right| compare_collated(left, right, Strength::Tertiary));
        words
    }

    #[test]
    fn root_collation_orders_letters_accents_and_case_as_uts10_does() {
        assert_eq!(sorted(&["b", "A", "a", "ä", "B", "Z", "z"]), ["a", "A", "ä", "b", "B", "z", "Z"]);
        assert_eq!(sorted(&["côte", "cote", "coté", "côté"]), ["cote", "coté", "côte", "côté"]);
        assert_eq!(sorted(&["10", "9", "a", " "]), [" ", "10", "9", "a"]);
        assert_eq!(sort_key("Résumé", Strength::Primary), sort_key("resume", Strength::Primary));
        assert_ne!(sort_key("Résumé", Strength::Secondary), sort_key("resume", Strength::Secondary));
        assert_eq!(sort_key("ABC", Strength::Secondary), sort_key("abc", Strength::Secondary));
        assert_eq!(compare_collated("abc", "abc", Strength::Tertiary), Ordering::Equal);
        assert!(compare_collated("中", "国", Strength::Tertiary) != Ordering::Equal);
        assert!(table().is_some());
    }
}
