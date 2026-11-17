use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlagFormat {
    Character,
    Long,
    Numeric,
}

impl FlagFormat {
    fn split(self, flags: &str) -> Vec<String> {
        match self {
            Self::Character => flags.chars().map(String::from).collect(),
            Self::Long => {
                let characters: Vec<char> = flags.chars().collect();
                characters.chunks(2).map(|pair| pair.iter().collect()).collect()
            }
            Self::Numeric => flags
                .split(',')
                .map(str::trim)
                .filter(|flag| !flag.is_empty())
                .map(str::to_owned)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConditionPart {
    Any,
    OneOf(Vec<char>),
    NoneOf(Vec<char>),
}

impl ConditionPart {
    fn matches(&self, character: char) -> bool {
        match self {
            Self::Any => true,
            Self::OneOf(characters) => characters.contains(&character),
            Self::NoneOf(characters) => !characters.contains(&character),
        }
    }
}

fn condition(text: &str) -> Result<Vec<ConditionPart>, String> {
    let mut parts = Vec::new();
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        match character {
            '.' => parts.push(ConditionPart::Any),
            '[' => {
                let mut set = Vec::new();
                let mut negated = false;
                let mut closed = false;
                for (index, inner) in characters.by_ref().enumerate() {
                    match inner {
                        '^' if index == 0 => negated = true,
                        ']' => {
                            closed = true;
                            break;
                        }
                        other => set.push(other),
                    }
                }
                if !closed {
                    return Err(format!("unclosed bracket in affix condition \"{text}\""));
                }
                parts.push(if negated { ConditionPart::NoneOf(set) } else { ConditionPart::OneOf(set) });
            }
            other => parts.push(ConditionPart::OneOf(vec![other])),
        }
    }
    Ok(parts)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AffixRule {
    flag: String,
    cross_product: bool,
    strip: String,
    affix: String,
    condition: Vec<ConditionPart>,
}

impl AffixRule {
    fn condition_holds_at_end(&self, stem: &str) -> bool {
        let characters: Vec<char> = stem.chars().collect();
        characters.len() >= self.condition.len()
            && self
                .condition
                .iter()
                .rev()
                .zip(characters.iter().rev())
                .all(|(part, character)| part.matches(*character))
    }

    fn condition_holds_at_start(&self, stem: &str) -> bool {
        let characters: Vec<char> = stem.chars().collect();
        characters.len() >= self.condition.len()
            && self
                .condition
                .iter()
                .zip(characters.iter())
                .all(|(part, character)| part.matches(*character))
    }

    fn remove_suffix(&self, word: &str) -> Option<String> {
        let rest = word.strip_suffix(self.affix.as_str())?;
        let stem = format!("{rest}{}", self.strip);
        (!stem.is_empty() && self.condition_holds_at_end(&stem)).then_some(stem)
    }

    fn remove_prefix(&self, word: &str) -> Option<String> {
        let rest = word.strip_prefix(self.affix.as_str())?;
        let stem = format!("{}{rest}", self.strip);
        (!stem.is_empty() && self.condition_holds_at_start(&stem)).then_some(stem)
    }
}

/// An `ispell` dictionary: a word list with affix flags and the affix rules that build the
/// other forms of each word, in the Hunspell format (`.dic` and `.aff`) PostgreSQL's
/// `ispell` template reads. A known form is reduced to its base word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ispell {
    words: BTreeMap<String, Vec<String>>,
    suffixes: Vec<AffixRule>,
    prefixes: Vec<AffixRule>,
}

impl Ispell {
    /// Reads a dictionary (one `word/FLAGS` per line; a leading count line is skipped) and its
    /// affix rules (`FLAG`, `PFX` and `SFX` lines). Words are lower cased.
    ///
    /// # Errors
    ///
    /// A malformed `PFX` or `SFX` line, or a condition with an unclosed bracket.
    pub fn parse(dictionary: &str, affixes: &str) -> Result<Self, String> {
        let mut format = FlagFormat::Character;
        let mut suffixes = Vec::new();
        let mut prefixes = Vec::new();
        let mut cross_products: BTreeMap<(bool, String), bool> = BTreeMap::new();
        for line in affixes.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.as_slice() {
                ["FLAG", "long", ..] => format = FlagFormat::Long,
                ["FLAG", "num", ..] => format = FlagFormat::Numeric,
                [kind @ ("PFX" | "SFX"), flag, cross, count] if count.parse::<usize>().is_ok() && cross.len() == 1 => {
                    cross_products.insert((*kind == "PFX", (*flag).to_owned()), matches!(*cross, "Y" | "y"));
                }
                [kind @ ("PFX" | "SFX"), flag, strip, affix, rest @ ..] => {
                    let prefix = *kind == "PFX";
                    let cross_product = cross_products.get(&(prefix, (*flag).to_owned())).copied().ok_or_else(|| {
                        format!("affix rule for flag \"{flag}\" comes before its header line")
                    })?;
                    let zero = |text: &str| if text == "0" { String::new() } else { text.to_lowercase() };
                    let affix = affix.split('/').next().unwrap_or_default();
                    let rule = AffixRule {
                        flag: (*flag).to_owned(),
                        cross_product,
                        strip: zero(strip),
                        affix: zero(affix),
                        condition: condition(&rest.first().copied().unwrap_or(".").to_lowercase())?,
                    };
                    if prefix { prefixes.push(rule) } else { suffixes.push(rule) }
                }
                ["PFX" | "SFX", ..] => return Err(format!("malformed affix line \"{line}\"")),
                _ => {}
            }
        }
        let mut words: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (index, line) in dictionary.lines().enumerate() {
            let entry = line.split_whitespace().next().unwrap_or_default();
            if entry.is_empty() || (index == 0 && entry.parse::<usize>().is_ok()) {
                continue;
            }
            let (word, flags) = entry.split_once('/').unwrap_or((entry, ""));
            words.entry(word.to_lowercase()).or_default().extend(format.split(flags));
        }
        Ok(Self { words, suffixes, prefixes })
    }

    fn has_flag(&self, word: &str, flag: &str) -> bool {
        self.words.get(word).is_some_and(|flags| flags.iter().any(|held| held == flag))
    }

    /// The base words `word` (lower case) is a form of, in the order found: the word itself
    /// when listed, then the bases reached by removing a suffix, a suffix and a prefix, or a
    /// prefix.
    pub fn base_words(&self, word: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut add = |base: String| {
            if !found.contains(&base) {
                found.push(base);
            }
        };
        if self.words.contains_key(word) {
            add(word.to_owned());
        }
        for suffix in &self.suffixes {
            let Some(stem) = suffix.remove_suffix(word) else {
                continue;
            };
            if self.has_flag(&stem, &suffix.flag) {
                add(stem.clone());
            }
            if !suffix.cross_product {
                continue;
            }
            for prefix in self.prefixes.iter().filter(|prefix| prefix.cross_product) {
                if let Some(base) = prefix.remove_prefix(&stem)
                    && self.has_flag(&base, &suffix.flag)
                    && self.has_flag(&base, &prefix.flag)
                {
                    add(base);
                }
            }
        }
        for prefix in &self.prefixes {
            if let Some(base) = prefix.remove_prefix(word)
                && self.has_flag(&base, &prefix.flag)
            {
                add(base);
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AFFIXES: &str = "SET UTF-8\nPFX U Y 1\nPFX U 0 un .\nSFX S Y 2\nSFX S y ies [^aeiou]y\nSFX S 0 s [^y]\nSFX D N 1\nSFX D 0 ed [^e]\n";
    const WORDS: &str = "3\nbook/SU\ncity/S\nwalk/D\n";

    #[test]
    fn suffixes_prefixes_and_cross_products_reduce_to_the_base_word() {
        let ispell = Ispell::parse(WORDS, AFFIXES).unwrap();
        assert_eq!(ispell.base_words("books"), ["book"]);
        assert_eq!(ispell.base_words("cities"), ["city"]);
        assert_eq!(ispell.base_words("unbook"), ["book"]);
        assert_eq!(ispell.base_words("unbooks"), ["book"]);
        assert_eq!(ispell.base_words("walked"), ["walk"]);
        assert_eq!(ispell.base_words("walk"), ["walk"]);
        assert!(ispell.base_words("citys").is_empty());
        assert!(ispell.base_words("unwalked").is_empty());
    }

    #[test]
    fn long_and_numeric_flags_and_bad_lines() {
        let ispell = Ispell::parse("dog/AaBb\n", "FLAG long\nSFX Aa Y 1\nSFX Aa 0 s .\n").unwrap();
        assert_eq!(ispell.base_words("dogs"), ["dog"]);
        let numeric = Ispell::parse("cat/12,7\n", "FLAG num\nSFX 7 Y 1\nSFX 7 0 s .\n").unwrap();
        assert_eq!(numeric.base_words("cats"), ["cat"]);
        assert!(Ispell::parse("", "SFX S 0 s .\n").is_err());
        assert!(Ispell::parse("", "SFX S Y 1\nSFX S 0 s [ab\n").is_err());
    }
}
