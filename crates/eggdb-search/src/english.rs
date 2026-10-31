const STOP_WORDS: &[&str] = &[
    "a", "about", "above", "after", "again", "against", "all", "am", "an", "and", "any", "are",
    "as", "at", "be", "because", "been", "before", "being", "below", "between", "both", "but",
    "by", "can", "did", "do", "does", "doing", "don", "down", "during", "each", "few", "for",
    "from", "further", "had", "has", "have", "having", "he", "her", "here", "hers", "herself",
    "him", "himself", "his", "how", "i", "if", "in", "into", "is", "it", "its", "itself", "just",
    "me", "more", "most", "my", "myself", "no", "nor", "not", "now", "of", "off", "on", "once",
    "only", "or", "other", "our", "ours", "ourselves", "out", "over", "own", "s", "same", "she",
    "should", "so", "some", "such", "t", "than", "that", "the", "their", "theirs", "them",
    "themselves", "then", "there", "these", "they", "this", "those", "through", "to", "too",
    "under", "until", "up", "very", "was", "we", "were", "what", "when", "where", "which",
    "while", "who", "whom", "why", "will", "with", "you", "your", "yours", "yourself",
    "yourselves",
];

/// Whether `word` (lower case) is an English stop word, left out of the index.
pub fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.binary_search(&word).is_ok()
}

fn is_vowel(byte: u8) -> bool {
    matches!(byte, b'a' | b'e' | b'i' | b'o' | b'u' | b'y')
}

fn is_double(word: &[u8]) -> bool {
    word.len() >= 2
        && word[word.len() - 1] == word[word.len() - 2]
        && matches!(word[word.len() - 1], b'b' | b'd' | b'f' | b'g' | b'm' | b'n' | b'p' | b'r' | b't')
}

fn region_after_consonant(word: &[u8], from: usize) -> usize {
    let mut at = from;
    while at + 1 < word.len() {
        if is_vowel(word[at]) && !is_vowel(word[at + 1]) {
            return at + 2;
        }
        at += 1;
    }
    word.len()
}

fn ends_short_syllable(word: &[u8]) -> bool {
    let n = word.len();
    if n == 2 {
        return is_vowel(word[0]) && !is_vowel(word[1]);
    }
    n >= 3
        && !is_vowel(word[n - 3])
        && is_vowel(word[n - 2])
        && !is_vowel(word[n - 1])
        && !matches!(word[n - 1], b'w' | b'x' | b'Y')
}

struct Stem {
    word: Vec<u8>,
    r1: usize,
    r2: usize,
}

impl Stem {
    fn ends(&self, suffix: &str) -> bool {
        self.word.ends_with(suffix.as_bytes())
    }

    fn in_r1(&self, suffix: &str) -> bool {
        self.ends(suffix) && self.word.len() - suffix.len() >= self.r1
    }

    fn in_r2(&self, suffix: &str) -> bool {
        self.ends(suffix) && self.word.len() - suffix.len() >= self.r2
    }

    fn replace(&mut self, suffix: &str, with: &str) {
        let keep = self.word.len() - suffix.len();
        self.word.truncate(keep);
        self.word.extend_from_slice(with.as_bytes());
    }

    fn has_vowel_before(&self, suffix: &str) -> bool {
        self.word[..self.word.len() - suffix.len()].iter().any(|byte| is_vowel(*byte))
    }

    fn is_short(&self) -> bool {
        self.r1 >= self.word.len() && ends_short_syllable(&self.word)
    }

    fn longest<'a>(&self, suffixes: &[&'a str]) -> Option<&'a str> {
        suffixes
            .iter()
            .filter(|suffix| self.ends(suffix))
            .max_by_key(|suffix| suffix.len())
            .copied()
    }
}

const EXCEPTIONS: &[(&str, &str)] = &[
    ("skis", "ski"),
    ("skies", "sky"),
    ("dying", "die"),
    ("lying", "lie"),
    ("tying", "tie"),
    ("idly", "idl"),
    ("gently", "gentl"),
    ("ugly", "ugli"),
    ("early", "earli"),
    ("only", "onli"),
    ("singly", "singl"),
    ("sky", "sky"),
    ("news", "news"),
    ("howe", "howe"),
    ("atlas", "atlas"),
    ("cosmos", "cosmos"),
    ("bias", "bias"),
    ("andes", "andes"),
];

const AFTER_STEP_1A: &[&str] = &[
    "inning", "outing", "canning", "herring", "earring", "proceed", "exceed", "succeed",
];

/// The English stem of `word` (lower case ASCII letters), by the Snowball English ("Porter2")
/// algorithm as its published description gives it; other words come back unchanged.
pub fn stem(word: &str) -> String {
    if word.len() <= 2 || !word.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'\'') {
        return word.to_owned();
    }
    if let Some((_, stemmed)) = EXCEPTIONS.iter().find(|(from, _)| *from == word) {
        return (*stemmed).to_owned();
    }
    let mut bytes = word.trim_start_matches('\'').as_bytes().to_vec();
    if bytes.first() == Some(&b'y') {
        bytes[0] = b'Y';
    }
    for at in 1..bytes.len() {
        if bytes[at] == b'y' && is_vowel(bytes[at - 1]) {
            bytes[at] = b'Y';
        }
    }
    let r1 = ["gener", "commun", "arsen"]
        .iter()
        .find(|prefix| bytes.starts_with(prefix.as_bytes()))
        .map_or_else(|| region_after_consonant(&bytes, 0), |prefix| prefix.len());
    let r2 = region_after_consonant(&bytes, r1);
    let mut stem = Stem { word: bytes, r1, r2 };
    if let Some(suffix) = stem.longest(&["'s'", "'s", "'"]) {
        stem.replace(suffix, "");
    }
    match stem.longest(&["sses", "ied", "ies", "us", "ss", "s"]) {
        Some("sses") => stem.replace("sses", "ss"),
        Some(suffix @ ("ied" | "ies")) => {
            let with = if stem.word.len() > suffix.len() + 1 { "i" } else { "ie" };
            stem.replace(suffix, with);
        }
        Some("s") => {
            let before = &stem.word[..stem.word.len() - 1];
            if before.len() >= 2 && before[..before.len() - 1].iter().any(|byte| is_vowel(*byte)) {
                stem.replace("s", "");
            }
        }
        _ => {}
    }
    let after_1a = String::from_utf8_lossy(&stem.word).into_owned();
    if AFTER_STEP_1A.contains(&after_1a.as_str()) {
        return after_1a;
    }
    match stem.longest(&["eedly", "eed", "ingly", "edly", "ing", "ed"]) {
        Some(suffix @ ("eed" | "eedly")) => {
            if stem.in_r1(suffix) {
                stem.replace(suffix, "ee");
            }
        }
        Some(suffix) => {
            if stem.has_vowel_before(suffix) {
                stem.replace(suffix, "");
                if stem.ends("at") || stem.ends("bl") || stem.ends("iz") {
                    stem.word.push(b'e');
                } else if is_double(&stem.word) {
                    stem.word.pop();
                } else if stem.is_short() {
                    stem.word.push(b'e');
                }
            }
        }
        None => {}
    }
    let n = stem.word.len();
    if n > 2 && matches!(stem.word[n - 1], b'y' | b'Y') && !is_vowel(stem.word[n - 2]) {
        stem.word[n - 1] = b'i';
    }
    const STEP_2: &[(&str, &str)] = &[
        ("ational", "ate"), ("tional", "tion"), ("enci", "ence"), ("anci", "ance"), ("abli", "able"),
        ("entli", "ent"), ("ization", "ize"), ("izer", "ize"), ("ation", "ate"), ("ator", "ate"),
        ("alism", "al"), ("aliti", "al"), ("alli", "al"), ("fulness", "ful"), ("ousli", "ous"),
        ("ousness", "ous"), ("iveness", "ive"), ("iviti", "ive"), ("biliti", "ble"), ("bli", "ble"),
        ("ogi", "og"), ("fulli", "ful"), ("lessli", "less"), ("li", ""),
    ];
    let step_2: Vec<&str> = STEP_2.iter().map(|(suffix, _)| *suffix).collect();
    if let Some(suffix) = stem.longest(&step_2)
        && stem.in_r1(suffix)
    {
        let with = STEP_2.iter().find(|(from, _)| *from == suffix).map_or("", |(_, to)| *to);
        let before = stem.word.len() - suffix.len();
        let allowed = match suffix {
            "ogi" => before > 0 && stem.word[before - 1] == b'l',
            "li" => before > 0 && matches!(stem.word[before - 1], b'c' | b'd' | b'e' | b'g' | b'h' | b'k' | b'm' | b'n' | b'r' | b't'),
            _ => true,
        };
        if allowed {
            stem.replace(suffix, with);
        }
    }
    const STEP_3: &[(&str, &str)] = &[
        ("ational", "ate"), ("tional", "tion"), ("alize", "al"), ("icate", "ic"), ("iciti", "ic"),
        ("ical", "ic"), ("ful", ""), ("ness", ""), ("ative", ""),
    ];
    let step_3: Vec<&str> = STEP_3.iter().map(|(suffix, _)| *suffix).collect();
    if let Some(suffix) = stem.longest(&step_3)
        && stem.in_r1(suffix)
        && (suffix != "ative" || stem.in_r2(suffix))
    {
        let with = STEP_3.iter().find(|(from, _)| *from == suffix).map_or("", |(_, to)| *to);
        stem.replace(suffix, with);
    }
    const STEP_4: &[&str] = &[
        "al", "ance", "ence", "er", "ic", "able", "ible", "ant", "ement", "ment", "ent", "ism",
        "ate", "iti", "ous", "ive", "ize", "ion",
    ];
    if let Some(suffix) = stem.longest(STEP_4)
        && stem.in_r2(suffix)
    {
        let before = stem.word.len() - suffix.len();
        if suffix != "ion" || (before > 0 && matches!(stem.word[before - 1], b's' | b't')) {
            stem.replace(suffix, "");
        }
    }
    if stem.ends("e") {
        let before = &stem.word[..stem.word.len() - 1];
        if stem.in_r2("e") || (stem.in_r1("e") && !ends_short_syllable(before)) {
            stem.replace("e", "");
        }
    } else if stem.ends("l") && stem.in_r2("l") && stem.word.len() >= 2 && stem.word[stem.word.len() - 2] == b'l' {
        stem.replace("l", "");
    }
    String::from_utf8_lossy(&stem.word).replace('Y', "y")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_follow_the_published_examples() {
        for (word, expected) in [
            ("cats", "cat"),
            ("rats", "rat"),
            ("running", "run"),
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("ties", "tie"),
            ("hopping", "hop"),
            ("hoped", "hope"),
            ("agreed", "agre"),
            ("generalization", "general"),
            ("national", "nation"),
            ("happiness", "happi"),
            ("dying", "die"),
            ("news", "news"),
            ("gas", "gas"),
            ("kiwis", "kiwi"),
            ("connection", "connect"),
            ("relational", "relat"),
            ("controlling", "control"),
            ("database", "databas"),
        ] {
            assert_eq!(stem(word), expected, "{word}");
        }
        assert!(STOP_WORDS.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(is_stop_word("the") && !is_stop_word("cat"));
    }
}
