const SWEDISH_STOP_WORDS: &[&str] = &[
    "och", "det", "att", "i", "en", "jag", "hon", "som", "han", "på", "den", "med", "var", "sig",
    "för", "så", "till", "är", "men", "ett", "om", "hade", "de", "av", "icke", "mig", "du",
    "henne", "då", "sin", "nu", "har", "inte", "hans", "honom", "skulle", "hennes", "där", "min",
    "man", "ej", "vid", "kunde", "något", "från", "ut", "när", "efter", "upp", "vi", "dem", "vara",
    "vad", "över", "än", "dig", "kan", "sina", "här", "ha", "mot", "alla", "under", "någon",
    "eller", "allt", "mycket", "sedan", "ju", "denna", "själv", "detta", "åt", "utan", "varit",
    "hur", "ingen", "mitt", "ni", "bli", "blev", "oss", "din", "dessa", "några", "deras", "blir",
    "mina", "samma", "vilken", "er", "sådan", "vår", "blivit", "dess", "inom", "mellan", "sådant",
    "varför", "varje", "vilka", "ditt", "vem", "vilket", "sitta", "sådana", "vart", "dina", "vars",
    "vårt", "våra", "ert", "era", "vilkas",
];
const NORWEGIAN_STOP_WORDS: &[&str] = &[
    "og", "i", "jeg", "det", "at", "en", "et", "den", "til", "er", "som", "på", "de", "med", "han",
    "av", "ikke", "ikkje", "der", "så", "var", "meg", "seg", "men", "ett", "har", "om", "vi",
    "min", "mitt", "ha", "hadde", "hun", "nå", "over", "da", "ved", "fra", "du", "ut", "sin",
    "dem", "oss", "opp", "man", "kan", "hans", "hvor", "eller", "hva", "skal", "selv", "sjøl",
    "her", "alle", "vil", "bli", "ble", "blei", "blitt", "kunne", "inn", "når", "være", "kom",
    "noen", "noe", "ville", "dere", "deres", "kun", "ja", "etter", "ned", "skulle", "denne",
    "for", "deg", "si", "sine", "sitt", "mot", "å", "meget", "hvorfor", "dette", "disse", "uten",
    "hvordan", "ingen", "din", "ditt", "blir", "samme", "hvilken", "hvilke", "sånn", "inni",
    "mellom", "vår", "hver", "hvem", "vors", "hvis", "både", "bare", "enn", "fordi", "før", "mange",
    "også", "slik", "vært", "båe", "begge", "siden",
];
const DANISH_STOP_WORDS: &[&str] = &[
    "og", "i", "jeg", "det", "at", "en", "den", "til", "er", "som", "på", "de", "med", "han",
    "af", "for", "ikke", "der", "var", "mig", "sig", "men", "et", "har", "om", "vi", "min",
    "havde", "ham", "hun", "nu", "over", "da", "fra", "du", "ud", "sin", "dem", "os", "op", "man",
    "hans", "hvor", "eller", "hvad", "skal", "selv", "her", "alle", "vil", "blev", "kunne", "ind",
    "når", "være", "dog", "noget", "ville", "jo", "deres", "efter", "ned", "skulle", "denne",
    "end", "dette", "mit", "også", "under", "have", "dig", "anden", "hende", "mine", "alt",
    "meget", "sit", "sine", "vor", "mod", "disse", "hvis", "din", "nogle", "hos", "blive",
    "mange", "ad", "bliver", "hendes", "været", "thi", "jer", "sådan",
];
const SWEDISH_SUFFIXES: &[&str] = &[
    "a", "arna", "erna", "heterna", "orna", "ad", "e", "ade", "ande", "arne", "are", "aste", "en",
    "anden", "aren", "heten", "ern", "ar", "er", "heter", "or", "as", "arnas", "ernas", "ornas",
    "es", "ades", "andes", "ens", "arens", "hetens", "erns", "at", "andet", "het", "ast", "s",
];
const NORWEGIAN_SUFFIXES: &[&str] = &[
    "a", "e", "ede", "ande", "ende", "ane", "ene", "hetene", "en", "heten", "ar", "er", "heter",
    "as", "es", "edes", "endes", "enes", "hetenes", "ens", "hetens", "ers", "ets", "et", "het",
    "ast", "s", "erte", "ert",
];
const DANISH_SUFFIXES: &[&str] = &[
    "hed", "ethed", "ered", "e", "erede", "ende", "erende", "ene", "erne", "ere", "en", "heden",
    "eren", "er", "heder", "erer", "heds", "es", "endes", "erendes", "enes", "ernes", "eres",
    "ens", "hedens", "erens", "ers", "ets", "erets", "et", "eret", "s",
];

/// The three Scandinavian languages Snowball stems with the same shape of algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Language {
    Swedish,
    Norwegian,
    Danish,
}

impl Language {
    fn is_vowel(self, letter: char) -> bool {
        match self {
            Self::Swedish => matches!(letter, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'ä' | 'å' | 'ö'),
            Self::Norwegian | Self::Danish => {
                matches!(letter, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'æ' | 'å' | 'ø')
            }
        }
    }

    fn stop_words(self) -> &'static [&'static str] {
        match self {
            Self::Swedish => SWEDISH_STOP_WORDS,
            Self::Norwegian => NORWEGIAN_STOP_WORDS,
            Self::Danish => DANISH_STOP_WORDS,
        }
    }

    fn valid_s_ending(self, letters: &[char], at: usize) -> bool {
        let Some(&before) = at.checked_sub(1).and_then(|index| letters.get(index)) else {
            return false;
        };
        match self {
            Self::Swedish => "bcdfghjklmnoprtvy".contains(before),
            Self::Norwegian => {
                "bcdfghjlmnoprtvyz".contains(before)
                    || (before == 'k'
                        && at
                            .checked_sub(2)
                            .and_then(|index| letters.get(index))
                            .is_some_and(|&earlier| !self.is_vowel(earlier)))
            }
            Self::Danish => "abcdfghjklmnoprtvyzå".contains(before),
        }
    }

    /// Whether `word` (lower case) is on Snowball's stop word list for this language.
    pub(crate) fn is_stop_word(self, word: &str) -> bool {
        self.stop_words().contains(&word)
    }

    fn region_one(self, letters: &[char]) -> usize {
        let mut at = 0;
        while at < letters.len() && !self.is_vowel(letters[at]) {
            at += 1;
        }
        while at < letters.len() && self.is_vowel(letters[at]) {
            at += 1;
        }
        (at + 1).max(3).min(letters.len())
    }

    fn main_suffix(self, letters: &mut Vec<char>, r1: usize) {
        let suffixes = match self {
            Self::Swedish => SWEDISH_SUFFIXES,
            Self::Norwegian => NORWEGIAN_SUFFIXES,
            Self::Danish => DANISH_SUFFIXES,
        };
        let Some((suffix, start)) = longest(letters, suffixes) else {
            return;
        };
        if start < r1 {
            return;
        }
        match suffix {
            "s" => {
                if self.valid_s_ending(letters, start) {
                    letters.truncate(start);
                }
            }
            "erte" | "ert" if self == Self::Norwegian => replace_tail(letters, start, "er"),
            _ => letters.truncate(start),
        }
    }

    fn consonant_pair(self, letters: &mut Vec<char>, r1: usize) {
        let pairs: &[&str] = match self {
            Self::Swedish => &["dd", "gd", "nn", "dt", "gt", "kt", "tt"],
            Self::Norwegian => &["dt", "vt"],
            Self::Danish => &["gd", "dt", "gt", "kt"],
        };
        if let Some((_, start)) = longest(letters, pairs)
            && start >= r1
        {
            letters.pop();
        }
    }

    fn other_suffix(self, letters: &mut Vec<char>, r1: usize) {
        match self {
            Self::Swedish => {
                let Some((suffix, start)) = longest(letters, &["lig", "ig", "els", "löst", "fullt"])
                else {
                    return;
                };
                if start < r1 {
                    return;
                }
                match suffix {
                    "löst" | "fullt" => {
                        letters.pop();
                    }
                    _ => letters.truncate(start),
                }
            }
            Self::Norwegian => {
                let suffixes = [
                    "leg", "eleg", "ig", "eig", "lig", "elig", "els", "lov", "elov", "slov",
                    "hetslov",
                ];
                if let Some((_, start)) = longest(letters, &suffixes)
                    && start >= r1
                {
                    letters.truncate(start);
                }
            }
            Self::Danish => {
                if let Some(at) = ends_with(letters, "igst") {
                    letters.truncate(at + 2);
                }
                let Some((suffix, start)) = longest(letters, &["ig", "lig", "elig", "els", "løst"])
                else {
                    return;
                };
                if start < r1 {
                    return;
                }
                if suffix == "løst" {
                    letters.pop();
                } else {
                    letters.truncate(start);
                    self.consonant_pair(letters, r1);
                }
            }
        }
    }

    fn undouble(self, letters: &mut Vec<char>, r1: usize) {
        let length = letters.len();
        if self == Self::Danish
            && length >= 2
            && length - 2 >= r1
            && letters[length - 1] == letters[length - 2]
            && !self.is_vowel(letters[length - 1])
        {
            letters.pop();
        }
    }

    /// The stem of `word` (lower case) by Snowball's stemmer for this language.
    pub(crate) fn stem(self, word: &str) -> String {
        let mut letters: Vec<char> = word.chars().collect();
        let r1 = self.region_one(&letters);
        self.main_suffix(&mut letters, r1);
        self.consonant_pair(&mut letters, r1);
        self.other_suffix(&mut letters, r1);
        self.undouble(&mut letters, r1);
        letters.into_iter().collect()
    }
}

fn ends_with(letters: &[char], suffix: &str) -> Option<usize> {
    let suffix: Vec<char> = suffix.chars().collect();
    letters
        .ends_with(&suffix)
        .then(|| letters.len() - suffix.len())
}

fn longest(letters: &[char], suffixes: &[&'static str]) -> Option<(&'static str, usize)> {
    suffixes
        .iter()
        .filter_map(|suffix| ends_with(letters, suffix).map(|start| (*suffix, start)))
        .max_by_key(|(suffix, _)| suffix.chars().count())
}

fn replace_tail(letters: &mut Vec<char>, start: usize, with: &str) {
    letters.truncate(start);
    letters.extend(with.chars());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_drop_definite_and_plural_endings() {
        assert_eq!(Language::Swedish.stem("flickorna"), "flick");
        assert_eq!(Language::Swedish.stem("kraftigt"), "kraft");
        assert_eq!(Language::Norwegian.stem("bilene"), "bil");
        assert_eq!(Language::Norwegian.stem("hoppet"), "hopp");
        assert_eq!(Language::Danish.stem("bilerne"), "bil");
        assert_eq!(Language::Danish.stem("løbende"), "løb");
        assert!(Language::Swedish.is_stop_word("och"));
        assert!(Language::Danish.is_stop_word("og"));
        assert!(!Language::Norwegian.is_stop_word("bil"));
    }
}
