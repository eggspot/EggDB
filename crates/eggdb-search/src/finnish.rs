const STOP_WORDS: &[&str] = &[
    "olla", "olen", "olet", "on", "olemme", "olette", "ovat", "ole", "oli", "olisi", "olisit",
    "olisin", "olisimme", "olisitte", "olisivat", "olit", "olin", "olimme", "olitte", "olivat",
    "ollut", "olleet", "en", "et", "ei", "emme", "ette", "eivät", "minä", "minun", "minut",
    "minua", "minussa", "minusta", "minuun", "minulla", "minulta", "minulle", "sinä", "sinun",
    "sinut", "sinua", "sinussa", "sinusta", "sinuun", "sinulla", "sinulta", "sinulle", "hän",
    "hänen", "hänet", "häntä", "hänessä", "hänestä", "häneen", "hänellä", "häneltä", "hänelle",
    "me", "meidän", "meidät", "meitä", "meissä", "meistä", "meihin", "meillä", "meiltä", "meille",
    "te", "teidän", "teidät", "teitä", "teissä", "teistä", "teihin", "teillä", "teiltä", "teille",
    "he", "heidän", "heidät", "heitä", "heissä", "heistä", "heihin", "heillä", "heiltä",
    "heille", "tämä", "tämän", "tätä", "tässä", "tästä", "tähän", "tällä", "tältä", "tälle",
    "tänä", "täksi", "tuo", "tuon", "tuota", "tuossa", "tuosta", "tuohon", "tuolla", "tuolta",
    "tuolle", "tuona", "tuoksi", "se", "sen", "sitä", "siinä", "siitä", "siihen", "sillä",
    "siltä", "sille", "siksi", "nämä", "näiden", "näitä", "näissä", "näistä", "näihin",
    "näillä", "näiltä", "näille", "näinä", "näiksi", "nuo", "noiden", "noita", "noissa",
    "noista", "noihin", "noilla", "noilta", "noille", "noina", "noiksi", "ne", "niiden", "niitä",
    "niissä", "niistä", "niihin", "niillä", "niiltä", "niille", "niinä", "niiksi", "kuka",
    "kenen", "kenet", "ketä", "kenessä", "kenestä", "keneen", "kenellä", "keneltä", "kenelle",
    "kenenä", "keneksi", "ketkä", "keiden", "keitä", "keissä", "keistä", "keihin", "keillä",
    "keiltä", "keille", "keinä", "keiksi", "mikä", "minkä", "mitä", "missä", "mistä", "mihin",
    "millä", "miltä", "mille", "miksi", "mitkä", "joka", "jonka", "jota", "jossa", "josta",
    "johon", "jolla", "jolta", "jolle", "jona", "joksi", "jotka", "joiden", "joita", "joissa",
    "joista", "joihin", "joilla", "joilta", "joille", "joina", "joiksi", "että", "ja", "jos",
    "koska", "kuin", "mutta", "niin", "sekä", "tai", "vaan", "vai", "vaikka", "kanssa",
    "mukaan", "noin", "poikki", "yli", "kun", "nyt", "itse",
];

/// Whether `word` (lower case) is on Snowball's Finnish stop word list.
pub fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

fn is_v1(letter: char) -> bool {
    matches!(letter, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'ä' | 'ö')
}

fn is_v2(letter: char) -> bool {
    matches!(letter, 'a' | 'e' | 'i' | 'o' | 'u' | 'ä' | 'ö')
}

fn region_after(letters: &[char], from: usize) -> usize {
    let mut at = from;
    while at < letters.len() && !is_v1(letters[at]) {
        at += 1;
    }
    while at < letters.len() && is_v1(letters[at]) {
        at += 1;
    }
    (at + 1).min(letters.len())
}

fn ends_with(letters: &[char], suffix: &str) -> Option<usize> {
    let suffix: Vec<char> = suffix.chars().collect();
    letters.ends_with(&suffix).then(|| letters.len() - suffix.len())
}

fn ends_with_any(letters: &[char], suffixes: &[&str]) -> bool {
    suffixes.iter().any(|suffix| ends_with(letters, suffix).is_some())
}

fn long_vowel(letters: &[char]) -> bool {
    let length = letters.len();
    length >= 2 && letters[length - 1] == letters[length - 2] && is_v1(letters[length - 1]) && letters[length - 1] != 'y'
}

fn vowel_then_i(letters: &[char]) -> bool {
    let length = letters.len();
    length >= 2 && letters[length - 1] == 'i' && is_v2(letters[length - 2])
}

fn consonant_then_vowel(letters: &[char]) -> bool {
    let length = letters.len();
    length >= 2 && is_v1(letters[length - 1]) && !is_v1(letters[length - 2])
}

fn by_length(suffixes: &[&'static str]) -> Vec<&'static str> {
    let mut sorted = suffixes.to_vec();
    sorted.sort_by_key(|suffix| std::cmp::Reverse(suffix.chars().count()));
    sorted
}

fn remove_first_accepted(
    letters: &mut Vec<char>,
    region: usize,
    suffixes: &[&'static str],
    accepts: impl Fn(&'static str, &[char]) -> bool,
) -> Option<&'static str> {
    for suffix in by_length(suffixes) {
        if let Some(start) = ends_with(letters, suffix)
            && start >= region
            && accepts(suffix, &letters[..start])
        {
            letters.truncate(start);
            return Some(suffix);
        }
    }
    None
}

fn particles(letters: &mut Vec<char>, r1: usize, r2: usize) {
    let suffixes = ["kin", "kaan", "kään", "ko", "kö", "han", "hän", "pa", "pä", "sti"];
    let Some(suffix) = by_length(&suffixes).into_iter().find(|suffix| {
        ends_with(letters, suffix).is_some_and(|start| start >= r1)
    }) else {
        return;
    };
    let start = letters.len() - suffix.chars().count();
    let before = &letters[..start];
    let removable = if suffix == "sti" {
        start >= r2
    } else {
        before.last().is_some_and(|&last| is_v1(last) || last == 'n' || last == 't')
    };
    if removable {
        letters.truncate(start);
    }
}

fn possessives(letters: &mut Vec<char>, r1: usize) {
    let suffixes = ["si", "ni", "nsa", "nsä", "mme", "nne", "an", "än", "en"];
    let removed = remove_first_accepted(letters, r1, &suffixes, |suffix, before| match suffix {
        "si" => before.last() != Some(&'k'),
        "an" => ends_with_any(before, &["ta", "ssa", "sta", "lla", "lta", "na"]),
        "än" => ends_with_any(before, &["tä", "ssä", "stä", "llä", "ltä", "nä"]),
        "en" => ends_with_any(before, &["lle", "ine"]),
        _ => true,
    });
    if removed == Some("ni") && ends_with(letters, "kse").is_some() {
        letters.pop();
        letters.push('i');
    }
}

fn cases(letters: &mut Vec<char>, r1: usize) -> bool {
    let suffixes = [
        "han", "hen", "hin", "hon", "hän", "hön", "siin", "seen", "den", "tten", "a", "ä", "tta",
        "ttä", "ta", "tä", "ssa", "ssä", "sta", "stä", "lla", "llä", "lta", "ltä", "lle", "na",
        "nä", "ksi", "ine", "n",
    ];
    let removed = remove_first_accepted(letters, r1, &suffixes, |suffix, before| match suffix {
        "han" | "hen" | "hin" | "hon" | "hän" | "hön" => {
            let vowel = suffix.chars().nth(1);
            before.last().copied() == vowel
        }
        "siin" | "den" | "tten" => vowel_then_i(before),
        "seen" => long_vowel(before),
        "a" | "ä" => consonant_then_vowel(before),
        "tta" | "ttä" => before.last() == Some(&'e'),
        _ => true,
    });
    if removed == Some("n") && (long_vowel(letters) || ends_with(letters, "ie").is_some()) {
        letters.pop();
    }
    removed.is_some()
}

fn other_endings(letters: &mut Vec<char>, r2: usize) {
    let suffixes = [
        "mpi", "mpa", "mpä", "mmi", "mma", "mmä", "impi", "impa", "impä", "immi", "imma", "immä",
        "eja", "ejä",
    ];
    remove_first_accepted(letters, r2, &suffixes, |suffix, before| {
        !(suffix.starts_with('m') && ends_with(before, "po").is_some())
    });
}

fn plurals(letters: &mut Vec<char>, r1: usize, r2: usize, ending_removed: bool) {
    let length = letters.len();
    if ending_removed {
        if length > r1 && matches!(letters[length - 1], 'i' | 'j') {
            letters.pop();
        }
        return;
    }
    if length >= 2 && length - 1 >= r1 && letters[length - 1] == 't' && is_v1(letters[length - 2]) {
        letters.pop();
        if let Some(start) = ends_with(letters, "imma")
            && start >= r2
        {
            letters.truncate(start);
        } else if let Some(start) = ends_with(letters, "mma")
            && start >= r2
            && ends_with(&letters[..start], "po").is_none()
        {
            letters.truncate(start);
        }
    }
}

fn tidy(letters: &mut Vec<char>, r1: usize) {
    let in_region = |letters: &[char], count: usize| letters.len() >= r1 + count;
    if in_region(letters, 2) && long_vowel(letters) {
        letters.pop();
    }
    let length = letters.len();
    if in_region(letters, 2)
        && matches!(letters[length - 1], 'a' | 'ä' | 'e' | 'i')
        && !is_v1(letters[length - 2])
    {
        letters.pop();
    }
    if in_region(letters, 2) && ends_with_any(letters, &["oj", "uj"]) {
        letters.pop();
    }
    if in_region(letters, 2) && ends_with(letters, "jo").is_some() {
        letters.pop();
    }
    let Some(last_consonant) = letters.iter().rposition(|&letter| !is_v1(letter)) else {
        return;
    };
    if last_consonant >= 1 && letters[last_consonant - 1] == letters[last_consonant] {
        letters.remove(last_consonant);
    }
}

/// The stem of `word` (lower case) by Snowball's Finnish stemmer: particles, possessives,
/// case endings, the other endings and plural markers removed over R1 and R2, then long
/// vowels, final vowels after a consonant and double consonants tidied.
pub fn stem(word: &str) -> String {
    let mut letters: Vec<char> = word.chars().collect();
    let r1 = region_after(&letters, 0);
    let r2 = region_after(&letters, r1).max(r1);
    particles(&mut letters, r1, r2);
    possessives(&mut letters, r1);
    let ending_removed = cases(&mut letters, r1);
    other_endings(&mut letters, r2);
    plurals(&mut letters, r1, r2, ending_removed);
    tidy(&mut letters, r1);
    letters.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_drop_case_endings_and_tidy() {
        for (word, expected) in [
            ("taloissa", "talo"),
            ("kirjassa", "kirj"),
            ("aarteisiin", "aart"),
        ] {
            assert_eq!(stem(word), expected, "{word}");
        }
        assert!(is_stop_word("ja"));
        assert!(!is_stop_word("talo"));
    }
}
