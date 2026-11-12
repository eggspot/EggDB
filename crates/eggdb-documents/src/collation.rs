use crate::error::{DocumentError, DocumentResult};
use crate::filter::{matches, parse_filter};
use crate::sort::{SortSpec, compare_by};
use crate::store::Store;
use eggdb_bson::{Bson, Document};

const LEVEL_SEPARATOR: char = '\u{0}';
const MOST_DEPTH: usize = 100;
const COMPARISONS: [&str; 6] = ["$eq", "$ne", "$gt", "$gte", "$lt", "$lte"];
const LISTS: [&str; 3] = ["$in", "$nin", "$all"];

/// A MongoDB collation (`{locale, strength, numericOrdering}`): how strings compare in a
/// query, a sort and `distinct`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collation {
    strength: u8,
    numeric: bool,
}

fn base_letter(letter: char) -> char {
    let code = letter as u32;
    let pick = |upper: bool, base: char| if upper { base.to_ascii_uppercase() } else { base };
    match code {
        0xC0..=0xC5 => 'A',
        0xC7 => 'C',
        0xC8..=0xCB => 'E',
        0xCC..=0xCF => 'I',
        0xD1 => 'N',
        0xD2..=0xD6 | 0xD8 => 'O',
        0xD9..=0xDC => 'U',
        0xDD => 'Y',
        0xE0..=0xE5 => 'a',
        0xE7 => 'c',
        0xE8..=0xEB => 'e',
        0xEC..=0xEF => 'i',
        0xF1 => 'n',
        0xF2..=0xF6 | 0xF8 => 'o',
        0xF9..=0xFC => 'u',
        0xFD | 0xFF => 'y',
        0x100..=0x105 => pick(code % 2 == 0, 'a'),
        0x106..=0x10D => pick(code % 2 == 0, 'c'),
        0x10E..=0x111 => pick(code % 2 == 0, 'd'),
        0x112..=0x11B => pick(code % 2 == 0, 'e'),
        0x11C..=0x123 => pick(code % 2 == 0, 'g'),
        0x124..=0x127 => pick(code % 2 == 0, 'h'),
        0x128..=0x131 => pick(code % 2 == 0, 'i'),
        0x134..=0x135 => pick(code % 2 == 0, 'j'),
        0x136..=0x137 => pick(code % 2 == 0, 'k'),
        0x139..=0x142 => pick(code % 2 == 1, 'l'),
        0x143..=0x148 => pick(code % 2 == 1, 'n'),
        0x14C..=0x151 => pick(code % 2 == 0, 'o'),
        0x154..=0x159 => pick(code % 2 == 0, 'r'),
        0x15A..=0x161 => pick(code % 2 == 0, 's'),
        0x162..=0x167 => pick(code % 2 == 0, 't'),
        0x168..=0x173 => pick(code % 2 == 0, 'u'),
        0x174..=0x175 => pick(code % 2 == 0, 'w'),
        0x176..=0x177 => pick(code % 2 == 0, 'y'),
        0x178 => 'Y',
        0x179..=0x17E => pick(code % 2 == 1, 'z'),
        0x17F => 's',
        0x1A0..=0x1A1 => pick(code % 2 == 0, 'o'),
        0x1AF..=0x1B0 => pick(code % 2 == 1, 'u'),
        0x1EA0..=0x1EB7 => pick(code % 2 == 0, 'a'),
        0x1EB8..=0x1EC7 => pick(code % 2 == 0, 'e'),
        0x1EC8..=0x1ECB => pick(code % 2 == 0, 'i'),
        0x1ECC..=0x1EE3 => pick(code % 2 == 0, 'o'),
        0x1EE4..=0x1EF1 => pick(code % 2 == 0, 'u'),
        0x1EF2..=0x1EF9 => pick(code % 2 == 0, 'y'),
        _ => letter,
    }
}

fn swapped_case(text: &str) -> String {
    text.chars()
        .flat_map(|letter| {
            if letter.is_uppercase() {
                letter.to_lowercase().collect::<Vec<_>>()
            } else {
                letter.to_uppercase().collect::<Vec<_>>()
            }
        })
        .collect()
}

fn numbered(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut digits = String::new();
    let flush = |digits: &mut String, out: &mut String| {
        if digits.is_empty() {
            return;
        }
        let trimmed = digits.trim_start_matches('0');
        let significant = if trimmed.is_empty() { "0" } else { trimmed };
        out.push('\u{1}');
        out.push_str(&format!("{:04}", significant.len().min(9999)));
        out.push_str(significant);
        digits.clear();
    };
    for letter in text.chars() {
        if letter.is_ascii_digit() {
            digits.push(letter);
        } else {
            flush(&mut digits, &mut out);
            out.push(letter);
        }
    }
    flush(&mut digits, &mut out);
    out
}

impl Collation {
    /// The collation a command's `collation` document asks for; `None` for the `simple`
    /// locale, which compares binary as without one.
    pub fn parse(given: &Document) -> DocumentResult<Option<Self>> {
        let locale = given.get_str("locale").ok_or_else(|| DocumentError::bad_value("collation needs a locale"))?;
        if locale == "simple" {
            return Ok(None);
        }
        let strength = match given.get("strength") {
            None => 3,
            Some(value) => value
                .as_i64()
                .filter(|strength| (1..=5).contains(strength))
                .ok_or_else(|| DocumentError::bad_value("collation strength must be 1 to 5"))? as u8,
        };
        Ok(Some(Self {
            strength,
            numeric: matches!(given.get("numericOrdering"), Some(Bson::Boolean(true))),
        }))
    }

    /// The comparison key of `text`: equal keys are equal strings under the collation, and
    /// keys order as the collation orders their strings.
    pub fn key(&self, text: &str) -> String {
        let text = if self.numeric { numbered(text) } else { text.to_owned() };
        let lower = text.to_lowercase();
        let mut key: String = lower.chars().map(|letter| base_letter(letter).to_ascii_lowercase()).collect();
        if self.strength >= 2 {
            key.push(LEVEL_SEPARATOR);
            key.push_str(&lower);
        }
        if self.strength >= 3 {
            key.push(LEVEL_SEPARATOR);
            key.push_str(&swapped_case(&text));
        }
        key
    }

    /// `value` with every string replaced by its key, in arrays and embedded documents too.
    pub fn collated(&self, value: &Bson) -> Bson {
        self.collated_at(value, 0)
    }

    fn collated_at(&self, value: &Bson, depth: usize) -> Bson {
        if depth > MOST_DEPTH {
            return value.clone();
        }
        match value {
            Bson::String(text) => Bson::String(self.key(text)),
            Bson::Array(items) => Bson::Array(items.iter().map(|item| self.collated_at(item, depth + 1)).collect()),
            Bson::Document(document) => Bson::Document(self.collated_document(document, depth + 1)),
            other => other.clone(),
        }
    }

    fn collated_document(&self, document: &Document, depth: usize) -> Document {
        document.iter().map(|(name, value)| (name.clone(), self.collated_at(value, depth))).collect()
    }

    fn collated_condition(&self, condition: &Bson, depth: usize) -> Bson {
        let Bson::Document(operators) = condition else {
            return self.collated_at(condition, depth);
        };
        if !operators.iter().next().is_some_and(|(name, _)| name.starts_with('$')) {
            return self.collated_at(condition, depth);
        }
        Bson::Document(
            operators
                .iter()
                .map(|(operator, argument)| {
                    let changed = match operator.as_str() {
                        name if COMPARISONS.contains(&name) => self.collated_at(argument, depth + 1),
                        name if LISTS.contains(&name) => self.collated_at(argument, depth + 1),
                        "$not" => self.collated_condition(argument, depth + 1),
                        "$elemMatch" => match argument {
                            Bson::Document(inner) => Bson::Document(self.collated_filter_at(inner, depth + 1)),
                            other => other.clone(),
                        },
                        _ => argument.clone(),
                    };
                    (operator.clone(), changed)
                })
                .collect(),
        )
    }

    fn collated_filter_at(&self, filter: &Document, depth: usize) -> Document {
        filter
            .iter()
            .map(|(name, value)| {
                let changed = match name.as_str() {
                    "$and" | "$or" | "$nor" => match value {
                        Bson::Array(items) => Bson::Array(
                            items
                                .iter()
                                .map(|item| match item {
                                    Bson::Document(inner) => Bson::Document(self.collated_filter_at(inner, depth + 1)),
                                    other => other.clone(),
                                })
                                .collect(),
                        ),
                        other => other.clone(),
                    },
                    operator if operator.starts_with('$') => value.clone(),
                    _ => self.collated_condition(value, depth + 1),
                };
                (name.clone(), changed)
            })
            .collect()
    }

    /// The filter with the strings it compares (equality, ranges, `$in`, `$nin`, `$all`,
    /// `$elemMatch`, `$not`) replaced by their keys; `$regex`, `$type` and the like keep theirs.
    pub fn collated_filter(&self, filter: &Document) -> Document {
        self.collated_filter_at(filter, 0)
    }

    /// The documents of `database.collection` matching `filter` under the collation, in `sort`
    /// order under it.
    pub fn find(&self, store: &Store<'_>, database: &str, collection: &str, filter: &Document, sort: &SortSpec) -> DocumentResult<Vec<Document>> {
        let everything = parse_filter(&Document::new())?;
        let collated_filter = parse_filter(&self.collated_filter(filter))?;
        let mut kept: Vec<(Document, Document)> = store
            .find(database, collection, &everything, &SortSpec::default())?
            .into_iter()
            .map(|document| (self.collated_document(&document, 0), document))
            .filter(|(collated, _)| matches(&collated_filter, collated))
            .collect();
        if !sort.is_empty() {
            kept.sort_by(|left, right| compare_by(sort, &left.0, &right.0));
        }
        Ok(kept.into_iter().map(|(_, document)| document).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collation(strength: i32, numeric: bool) -> Collation {
        let mut given = Document::new();
        given.insert("locale", Bson::String("vi".to_owned()));
        given.insert("strength", Bson::Int32(strength));
        given.insert("numericOrdering", Bson::Boolean(numeric));
        Collation::parse(&given).unwrap().unwrap()
    }

    #[test]
    fn strengths_ignore_accents_then_case() {
        assert_eq!(collation(1, false).key("Phở"), collation(1, false).key("pho"));
        assert_ne!(collation(2, false).key("Phở"), collation(2, false).key("pho"));
        assert_eq!(collation(2, false).key("PHỞ"), collation(2, false).key("phở"));
        assert_ne!(collation(3, false).key("PHỞ"), collation(3, false).key("phở"));
        assert!(collation(3, false).key("apple") < collation(3, false).key("Banana"));
        assert!(collation(3, true).key("item 9") < collation(3, true).key("item 10"));
    }
}
