const STOP_WORDS: &[&str] = &[
    "au", "aux", "avec", "ce", "ces", "dans", "de", "des", "du", "elle", "en", "et", "eux", "il",
    "je", "la", "le", "leur", "lui", "ma", "mais", "me", "même", "mes", "moi", "mon", "ne", "nos",
    "notre", "nous", "on", "ou", "par", "pas", "pour", "qu", "que", "qui", "sa", "se", "ses",
    "son", "sur", "ta", "te", "tes", "toi", "ton", "tu", "un", "une", "vos", "votre", "vous", "c",
    "d", "j", "l", "à", "m", "n", "s", "t", "y", "été", "étée", "étées", "étés", "étant", "suis",
    "es", "est", "sommes", "êtes", "sont", "serai", "sera", "serons", "seront", "serais", "serait",
    "étais", "était", "étions", "étiez", "étaient", "fus", "fut", "fûmes", "furent", "sois",
    "soit", "soyons", "soient", "ayant", "eu", "eue", "eues", "eus", "ai", "as", "avons", "avez",
    "ont", "aurai", "aura", "aurons", "auront", "aurais", "aurait", "avais", "avait", "avions",
    "aviez", "avaient", "eut", "eûmes", "eurent", "aie", "aies", "ait", "ayons", "aient",
];
const STEP_TWO_A: &[&str] = &[
    "îmes", "ît", "îtes", "i", "ie", "ies", "ir", "ira", "irai", "iraIent", "irais", "irait",
    "iras", "irent", "irez", "iriez", "irions", "irons", "iront", "is", "issaIent", "issais",
    "issait", "issant", "issante", "issantes", "issants", "isse", "issent", "isses", "issez",
    "issiez", "issions", "issons", "it",
];
const STEP_TWO_B_PLAIN: &[&str] = &[
    "é", "ée", "ées", "és", "èrent", "er", "era", "erai", "eraIent", "erais", "erait", "eras",
    "erez", "eriez", "erions", "erons", "eront", "ez", "iez",
];
const STEP_TWO_B_AFTER_E: &[&str] = &[
    "âmes", "ât", "âtes", "a", "ai", "aIent", "ais", "ait", "ant", "ante", "antes", "ants", "as",
    "asse", "assent", "asses", "assiez", "assions",
];
const STANDARD_SUFFIXES: &[&str] = &[
    "ance", "iqUe", "isme", "able", "iste", "eux", "ances", "iqUes", "ismes", "ables", "istes",
    "atrice", "ateur", "ation", "atrices", "ateurs", "ations", "logie", "logies", "usion",
    "ution", "usions", "utions", "ence", "ences", "ement", "ements", "ité", "ités", "if", "ive",
    "ifs", "ives", "eaux", "aux", "euse", "euses", "issement", "issements", "amment", "emment",
    "ment", "ments",
];

/// Whether `word` (lower case) is on Snowball's French stop word list.
pub fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

fn is_vowel(letter: char) -> bool {
    matches!(
        letter,
        'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'â' | 'à' | 'ë' | 'é' | 'ê' | 'è' | 'ï' | 'î' | 'ô' | 'û'
            | 'ù'
    )
}

fn region_after(letters: &[char], from: usize) -> usize {
    let mut at = from;
    while at < letters.len() && !is_vowel(letters[at]) {
        at += 1;
    }
    while at < letters.len() && is_vowel(letters[at]) {
        at += 1;
    }
    (at + 1).min(letters.len())
}

fn region_rv(letters: &[char]) -> usize {
    let starts = |prefix: &str| letters.iter().collect::<String>().starts_with(prefix);
    if letters.len() >= 2 && is_vowel(letters[0]) && is_vowel(letters[1]) {
        return 3.min(letters.len());
    }
    if starts("par") || starts("col") || starts("tap") {
        return 3;
    }
    (1..letters.len())
        .find(|&at| is_vowel(letters[at]))
        .map_or(letters.len(), |at| at + 1)
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

fn prepared(word: &str) -> Vec<char> {
    let mut letters: Vec<char> = word.chars().collect();
    for at in 0..letters.len() {
        let before = at.checked_sub(1).map(|index| letters[index]);
        let after = letters.get(at + 1).copied();
        let vowel_before = before.is_some_and(is_vowel);
        let vowel_after = after.is_some_and(is_vowel);
        match letters[at] {
            'u' if before == Some('q') => letters[at] = 'U',
            'u' if vowel_before && vowel_after => letters[at] = 'U',
            'i' if vowel_before && vowel_after => letters[at] = 'I',
            'y' if vowel_before || vowel_after => letters[at] = 'Y',
            _ => {}
        }
    }
    letters
}

struct Regions {
    rv: usize,
    r1: usize,
    r2: usize,
}

enum Standard {
    Nothing,
    Removed,
    ReachesVerbs,
}

fn standard_suffix(letters: &mut Vec<char>, regions: &Regions) -> Standard {
    let Regions { rv, r1, r2 } = *regions;
    let Some((suffix, start)) = longest(letters, STANDARD_SUFFIXES) else {
        return Standard::Nothing;
    };
    let in_r2 = |at: usize| at >= r2;
    let ends = |letters: &[char], ending: &str| ends_with(letters, ending);
    match suffix {
        "ance" | "iqUe" | "isme" | "able" | "iste" | "eux" | "ances" | "iqUes" | "ismes"
        | "ables" | "istes" => {
            if !in_r2(start) {
                return Standard::Nothing;
            }
            letters.truncate(start);
        }
        "atrice" | "ateur" | "ation" | "atrices" | "ateurs" | "ations" => {
            if !in_r2(start) {
                return Standard::Nothing;
            }
            letters.truncate(start);
            if let Some(at) = ends(letters, "ic") {
                if in_r2(at) {
                    letters.truncate(at);
                } else {
                    replace_tail(letters, at, "iqU");
                }
            }
        }
        "logie" | "logies" => {
            if !in_r2(start) {
                return Standard::Nothing;
            }
            replace_tail(letters, start, "log");
        }
        "usion" | "ution" | "usions" | "utions" => {
            if !in_r2(start) {
                return Standard::Nothing;
            }
            replace_tail(letters, start, "u");
        }
        "ence" | "ences" => {
            if !in_r2(start) {
                return Standard::Nothing;
            }
            replace_tail(letters, start, "ent");
        }
        "ement" | "ements" => {
            if start < rv {
                return Standard::Nothing;
            }
            letters.truncate(start);
            if let Some(at) = ends(letters, "iv")
                && in_r2(at)
            {
                letters.truncate(at);
                if let Some(at) = ends(letters, "at")
                    && in_r2(at)
                {
                    letters.truncate(at);
                }
            } else if let Some(at) = ends(letters, "eus") {
                if in_r2(at) {
                    letters.truncate(at);
                } else if at >= r1 {
                    replace_tail(letters, at, "eux");
                }
            } else if let Some(at) = ends(letters, "abl").or_else(|| ends(letters, "iqU")) {
                if in_r2(at) {
                    letters.truncate(at);
                }
            } else if let Some(at) = ends(letters, "ièr").or_else(|| ends(letters, "Ièr"))
                && at >= rv
            {
                replace_tail(letters, at, "i");
            }
        }
        "ité" | "ités" => {
            if !in_r2(start) {
                return Standard::Nothing;
            }
            letters.truncate(start);
            if let Some(at) = ends(letters, "abil") {
                if in_r2(at) {
                    letters.truncate(at);
                } else {
                    replace_tail(letters, at, "abl");
                }
            } else if let Some(at) = ends(letters, "ic") {
                if in_r2(at) {
                    letters.truncate(at);
                } else {
                    replace_tail(letters, at, "iqU");
                }
            } else if let Some(at) = ends(letters, "iv")
                && in_r2(at)
            {
                letters.truncate(at);
            }
        }
        "if" | "ive" | "ifs" | "ives" => {
            if !in_r2(start) {
                return Standard::Nothing;
            }
            letters.truncate(start);
            if let Some(at) = ends(letters, "at")
                && in_r2(at)
            {
                letters.truncate(at);
                if let Some(at) = ends(letters, "ic") {
                    if in_r2(at) {
                        letters.truncate(at);
                    } else {
                        replace_tail(letters, at, "iqU");
                    }
                }
            }
        }
        "eaux" => replace_tail(letters, start, "eau"),
        "aux" => {
            if start < r1 {
                return Standard::Nothing;
            }
            replace_tail(letters, start, "al");
        }
        "euse" | "euses" => {
            if in_r2(start) {
                letters.truncate(start);
            } else if start >= r1 {
                replace_tail(letters, start, "eux");
            } else {
                return Standard::Nothing;
            }
        }
        "issement" | "issements" => {
            if start < r1 || start == 0 || is_vowel(letters[start - 1]) {
                return Standard::Nothing;
            }
            letters.truncate(start);
        }
        "amment" => {
            if start < rv {
                return Standard::ReachesVerbs;
            }
            replace_tail(letters, start, "ant");
            return Standard::ReachesVerbs;
        }
        "emment" => {
            if start < rv {
                return Standard::ReachesVerbs;
            }
            replace_tail(letters, start, "ent");
            return Standard::ReachesVerbs;
        }
        _ => {
            if start > rv && is_vowel(letters[start - 1]) {
                letters.truncate(start);
            }
            return Standard::ReachesVerbs;
        }
    }
    Standard::Removed
}

fn i_verb_suffix(letters: &mut Vec<char>, rv: usize) -> bool {
    if let Some((_, start)) = longest(letters, STEP_TWO_A)
        && start > rv
        && !is_vowel(letters[start - 1])
    {
        letters.truncate(start);
        return true;
    }
    false
}

fn other_verb_suffix(letters: &mut Vec<char>, rv: usize, r2: usize) -> bool {
    let mut suffixes: Vec<&'static str> = vec!["ions"];
    suffixes.extend_from_slice(STEP_TWO_B_PLAIN);
    suffixes.extend_from_slice(STEP_TWO_B_AFTER_E);
    let Some((suffix, start)) = longest(letters, &suffixes) else {
        return false;
    };
    if start < rv || (suffix == "ions" && start < r2) {
        return false;
    }
    letters.truncate(start);
    if STEP_TWO_B_AFTER_E.contains(&suffix)
        && let Some(at) = ends_with(letters, "e")
        && at >= rv
    {
        letters.truncate(at);
    }
    true
}

fn residual_suffix(letters: &mut Vec<char>, rv: usize, r2: usize) {
    if let Some(at) = ends_with(letters, "s")
        && at > 0
        && !matches!(letters[at - 1], 'a' | 'i' | 'o' | 'u' | 'è' | 's')
    {
        letters.pop();
    }
    let Some((suffix, start)) = longest(letters, &["ion", "ier", "ière", "Ier", "Ière", "e", "ë"])
    else {
        return;
    };
    if start < rv {
        return;
    }
    match suffix {
        "ion" => {
            if start >= r2 && start > 0 && matches!(letters[start - 1], 's' | 't') {
                letters.truncate(start);
            }
        }
        "ier" | "ière" | "Ier" | "Ière" => replace_tail(letters, start, "i"),
        "e" => letters.truncate(start),
        _ => {
            if ends_with(&letters[..start], "gu").is_some() {
                letters.truncate(start);
            }
        }
    }
}

fn undouble(letters: &mut Vec<char>) {
    if longest(letters, &["enn", "onn", "ett", "ell", "eill"]).is_some() {
        letters.pop();
    }
}

fn unaccent(letters: &mut [char]) {
    let mut at = letters.len();
    while at > 0 && !is_vowel(letters[at - 1]) {
        at -= 1;
    }
    if at < letters.len() && at > 0 && matches!(letters[at - 1], 'é' | 'è') {
        letters[at - 1] = 'e';
    }
}

/// The stem of `word` (lower case) by Snowball's French stemmer: the standard, verb and residual
/// suffixes removed over RV, R1 and R2, doubled consonants and a final accent undone.
pub fn stem(word: &str) -> String {
    let mut letters = prepared(word);
    let regions = Regions {
        rv: region_rv(&letters),
        r1: region_after(&letters, 0),
        r2: 0,
    };
    let regions = Regions {
        r2: region_after(&letters, regions.r1).max(regions.r1),
        ..regions
    };
    let before = letters.clone();
    let changed = match standard_suffix(&mut letters, &regions) {
        Standard::Removed => true,
        outcome => {
            let verbs = i_verb_suffix(&mut letters, regions.rv)
                || other_verb_suffix(&mut letters, regions.rv, regions.r2);
            verbs || matches!(outcome, Standard::ReachesVerbs) && letters != before
        }
    };
    if changed {
        match letters.last() {
            Some('Y') => {
                letters.pop();
                letters.push('i');
            }
            Some('ç') => {
                letters.pop();
                letters.push('c');
            }
            _ => {}
        }
    } else {
        residual_suffix(&mut letters, regions.rv, regions.r2);
    }
    undouble(&mut letters);
    unaccent(&mut letters);
    letters
        .into_iter()
        .map(|letter| match letter {
            'U' => 'u',
            'I' => 'i',
            'Y' => 'y',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_drop_suffixes_and_double_consonants() {
        for (word, expected) in [
            ("continuellement", "continuel"),
            ("pommes", "pomm"),
            ("nationale", "national"),
        ] {
            assert_eq!(stem(word), expected, "{word}");
        }
        assert!(is_stop_word("dans"));
        assert!(!is_stop_word("maison"));
    }
}
