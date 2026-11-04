use std::ops::Range;

struct Anchor {
    folded: Range<usize>,
    original: Range<usize>,
}

pub(super) struct FoldedText {
    folded: String,
    anchors: Vec<Anchor>,
}

impl FoldedText {
    pub(super) fn new(original: &str) -> Self {
        if original.is_ascii() {
            return Self {
                folded: original.to_ascii_lowercase(),
                anchors: Vec::new(),
            };
        }
        let mut folded = String::with_capacity(original.len());
        let mut anchors = Vec::new();
        for (start, character) in original.char_indices() {
            let folded_start = folded.len();
            eggdb_text::case_fold_into(character, &mut folded);
            let segment = folded.get(folded_start..).unwrap_or_default();
            let keeps_shape =
                segment.len() == character.len_utf8() && segment.chars().nth(1).is_none();
            if !keeps_shape {
                anchors.push(Anchor {
                    folded: folded_start..folded.len(),
                    original: start..start + character.len_utf8(),
                });
            }
        }
        Self { folded, anchors }
    }

    fn original_at(&self, at: usize) -> Option<usize> {
        if !self.folded.is_char_boundary(at) {
            return None;
        }
        let after = self
            .anchors
            .partition_point(|anchor| anchor.folded.start <= at);
        let Some(anchor) = after.checked_sub(1).and_then(|last| self.anchors.get(last)) else {
            return Some(at);
        };
        if at == anchor.folded.start {
            Some(anchor.original.start)
        } else if at < anchor.folded.end {
            None
        } else {
            Some(anchor.original.end + (at - anchor.folded.end))
        }
    }

    pub(super) fn matches(&self, needle: &str, limit: usize) -> Vec<Range<usize>> {
        let needle = crate::citext::fold(needle);
        let mut found = Vec::new();
        if needle.is_empty() || limit == 0 {
            return found;
        }
        let mut free_from = 0;
        each_occurrence(self.folded.as_bytes(), needle.as_bytes(), |start| {
            let end = start + needle.len();
            if start < free_from {
                return true;
            }
            if let (Some(from), Some(to)) = (self.original_at(start), self.original_at(end)) {
                found.push(from..to);
                free_from = end;
            }
            found.len() < limit
        });
        found
    }
}

fn failure_table(needle: &[u8]) -> Vec<usize> {
    let mut table = vec![0; needle.len()];
    let mut matched = 0;
    for (at, byte) in needle.iter().enumerate().skip(1) {
        while matched > 0 && needle.get(matched) != Some(byte) {
            matched = table.get(matched - 1).copied().unwrap_or(0);
        }
        if needle.get(matched) == Some(byte) {
            matched += 1;
        }
        if let Some(slot) = table.get_mut(at) {
            *slot = matched;
        }
    }
    table
}

fn each_occurrence(haystack: &[u8], needle: &[u8], mut found: impl FnMut(usize) -> bool) {
    if needle.is_empty() {
        return;
    }
    let table = failure_table(needle);
    let mut matched = 0;
    for (at, byte) in haystack.iter().enumerate() {
        while matched > 0 && needle.get(matched) != Some(byte) {
            matched = table.get(matched - 1).copied().unwrap_or(0);
        }
        if needle.get(matched) == Some(byte) {
            matched += 1;
        }
        if matched == needle.len() {
            if !found(at + 1 - needle.len()) {
                return;
            }
            matched = table.get(matched - 1).copied().unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FoldedText;

    fn slices(text: &str, needle: &str) -> Vec<String> {
        let folded = FoldedText::new(text);
        folded
            .matches(needle, usize::MAX)
            .into_iter()
            .map(|range| text.get(range).unwrap_or_default().to_owned())
            .collect()
    }

    #[test]
    fn matches_return_the_original_text_of_every_case_variant() {
        assert_eq!(slices("CAFÉ café Café", "é"), ["É", "é", "é"]);
        assert_eq!(slices("Straße STRASSE", "ss"), ["ß", "SS"]);
        assert_eq!(slices("\u{212A}elvin KELVIN", "k"), ["\u{212A}", "K"]);
        assert_eq!(slices("ΣΑΣ σας", "σ"), ["Σ", "Σ", "σ", "ς"]);
        assert_eq!(slices("abc", ""), Vec::<String>::new());
    }

    #[test]
    fn a_match_inside_one_character_fold_is_skipped() {
        assert_eq!(slices("ß", "s"), Vec::<String>::new());
        assert_eq!(slices("sßs", "ss"), ["ß"]);
        assert_eq!(slices("aßa", "sa"), Vec::<String>::new());
    }

    #[test]
    fn matches_do_not_overlap_and_go_left_to_right() {
        assert_eq!(slices("aaaa", "AA"), ["aa", "aa"]);
        assert_eq!(slices("aAaAa", "aa"), ["aA", "aA"]);
    }

    #[test]
    fn misaligned_candidates_everywhere_stay_linear() {
        let text = "ß".repeat(200_000);
        let needle = format!("s{}", "ß".repeat(100_000));
        let started = std::time::Instant::now();
        assert!(
            FoldedText::new(&text)
                .matches(&needle, usize::MAX)
                .is_empty()
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
    }
}
