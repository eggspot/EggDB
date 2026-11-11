const STOP_WORDS: &[&str] = &[
    "aber", "alle", "allem", "allen", "aller", "alles", "als", "also", "am", "an", "ander",
    "andere", "anderem", "anderen", "anderer", "anderes", "anderm", "andern", "anderr", "anders",
    "auch", "auf", "aus", "bei", "bin", "bis", "bist", "da", "damit", "dann", "der", "den", "des",
    "dem", "die", "das", "dass", "daß", "derselbe", "derselben", "denselben", "desselben",
    "demselben", "dieselbe", "dieselben", "dasselbe", "dazu", "dein", "deine", "deinem", "deinen",
    "deiner", "deines", "denn", "derer", "dessen", "dich", "dir", "du", "dies", "diese", "diesem",
    "diesen", "dieser", "dieses", "doch", "dort", "durch", "ein", "eine", "einem", "einen",
    "einer", "eines", "einig", "einige", "einigem", "einigen", "einiger", "einiges", "einmal",
    "er", "ihn", "ihm", "es", "etwas", "euer", "eure", "eurem", "euren", "eurer", "eures", "für",
    "gegen", "gewesen", "hab", "habe", "haben", "hat", "hatte", "hatten", "hier", "hin",
    "hinter", "ich", "mich", "mir", "ihr", "ihre", "ihrem", "ihren", "ihrer", "ihres", "euch",
    "im", "in", "indem", "ins", "ist", "jede", "jedem", "jeden", "jeder", "jedes", "jene",
    "jenem", "jenen", "jener", "jenes", "jetzt", "kann", "kein", "keine", "keinem", "keinen",
    "keiner", "keines", "können", "könnte", "machen", "man", "manche", "manchem", "manchen",
    "mancher", "manches", "mein", "meine", "meinem", "meinen", "meiner", "meines", "mit", "muss",
    "musste", "nach", "nicht", "nichts", "noch", "nun", "nur", "ob", "oder", "ohne", "sehr",
    "sein", "seine", "seinem", "seinen", "seiner", "seines", "selbst", "sich", "sie", "ihnen",
    "sind", "so", "solche", "solchem", "solchen", "solcher", "solches", "soll", "sollte",
    "sondern", "sonst", "über", "um", "und", "uns", "unsere", "unserem", "unseren", "unser",
    "unseres", "unter", "viel", "vom", "von", "vor", "während", "war", "waren", "warst", "was",
    "weg", "weil", "weiter", "welche", "welchem", "welchen", "welcher", "welches", "wenn",
    "werde", "werden", "wie", "wieder", "will", "wir", "wird", "wirst", "wo", "wollen", "wollte",
    "würde", "würden", "zu", "zum", "zur", "zwar", "zwischen",
];
const S_ENDINGS: [char; 11] = ['b', 'd', 'f', 'g', 'h', 'k', 'l', 'm', 'n', 'r', 't'];
const ST_ENDINGS: [char; 10] = ['b', 'd', 'f', 'g', 'h', 'k', 'l', 'm', 'n', 't'];
const UPPER_U: char = 'U';
const UPPER_Y: char = 'Y';

/// Whether `word` (lower case) is on Snowball's German stop word list.
pub fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

fn is_vowel(letter: char) -> bool {
    matches!(letter, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'ä' | 'ö' | 'ü')
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

fn ends_with(letters: &[char], suffix: &str) -> Option<usize> {
    let suffix: Vec<char> = suffix.chars().collect();
    letters
        .ends_with(&suffix)
        .then(|| letters.len() - suffix.len())
}

fn longest_suffix(letters: &[char], suffixes: &[&'static str]) -> Option<(&'static str, usize)> {
    suffixes
        .iter()
        .filter_map(|suffix| ends_with(letters, suffix).map(|start| (*suffix, start)))
        .max_by_key(|(suffix, _)| suffix.chars().count())
}

fn marked(word: &str) -> Vec<char> {
    let mut letters: Vec<char> = word.replace('ß', "ss").chars().collect();
    for at in 1..letters.len().saturating_sub(1) {
        let between_vowels = is_vowel(letters[at - 1]) && is_vowel(letters[at + 1]);
        if between_vowels && letters[at] == 'u' {
            letters[at] = UPPER_U;
        } else if between_vowels && letters[at] == 'y' {
            letters[at] = UPPER_Y;
        }
    }
    letters
}

fn step_one(letters: &mut Vec<char>, r1: usize) {
    let Some((suffix, start)) = longest_suffix(letters, &["em", "ern", "er", "e", "en", "es", "s"])
    else {
        return;
    };
    if start < r1 {
        return;
    }
    match suffix {
        "s" => {
            if start > 0 && S_ENDINGS.contains(&letters[start - 1]) {
                letters.truncate(start);
            }
        }
        "e" | "en" | "es" => {
            letters.truncate(start);
            if ends_with(letters, "niss").is_some() {
                letters.pop();
            }
        }
        _ => letters.truncate(start),
    }
}

fn step_two(letters: &mut Vec<char>, r1: usize) {
    let Some((suffix, start)) = longest_suffix(letters, &["en", "er", "est", "st"]) else {
        return;
    };
    if start < r1 {
        return;
    }
    if suffix == "st" {
        if start >= 4 && ST_ENDINGS.contains(&letters[start - 1]) {
            letters.truncate(start);
        }
    } else {
        letters.truncate(start);
    }
}

fn preceded_by_e(letters: &[char], start: usize) -> bool {
    start > 0 && letters[start - 1] == 'e'
}

fn step_three(letters: &mut Vec<char>, r1: usize, r2: usize) {
    let suffixes = ["end", "ung", "ig", "ik", "isch", "lich", "heit", "keit"];
    let Some((suffix, start)) = longest_suffix(letters, &suffixes) else {
        return;
    };
    if start < r2 {
        return;
    }
    match suffix {
        "end" | "ung" => {
            letters.truncate(start);
            if let Some(at) = ends_with(letters, "ig")
                && at >= r2
                && !preceded_by_e(letters, at)
            {
                letters.truncate(at);
            }
        }
        "ig" | "ik" | "isch" => {
            if !preceded_by_e(letters, start) {
                letters.truncate(start);
            }
        }
        "lich" | "heit" => {
            letters.truncate(start);
            if let Some(at) = ends_with(letters, "er").or_else(|| ends_with(letters, "en"))
                && at >= r1
            {
                letters.truncate(at);
            }
        }
        _ => {
            letters.truncate(start);
            if let Some(at) = ends_with(letters, "lich").or_else(|| ends_with(letters, "ig"))
                && at >= r2
            {
                letters.truncate(at);
            }
        }
    }
}

/// The stem of `word` (lower case) by Snowball's German stemmer: `ß` read as `ss`, the standard
/// suffix steps over the regions R1 and R2, and umlauts dropped.
pub fn stem(word: &str) -> String {
    let mut letters = marked(word);
    let r1 = region_after(&letters, 0).max(3).min(letters.len());
    let r2 = region_after(&letters, r1).max(r1);
    step_one(&mut letters, r1);
    step_two(&mut letters, r1);
    step_three(&mut letters, r1, r2);
    letters
        .into_iter()
        .map(|letter| match letter {
            UPPER_U | 'ü' => 'u',
            UPPER_Y => 'y',
            'ä' => 'a',
            'ö' => 'o',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_drop_inflections_and_umlauts() {
        for (word, expected) in [
            ("häuser", "haus"),
            ("laufen", "lauf"),
            ("glücklich", "glucklich"),
            ("schönheit", "schonheit"),
            ("straße", "strass"),
            ("kinder", "kind"),
        ] {
            assert_eq!(stem(word), expected, "{word}");
        }
        assert!(is_stop_word("und"));
        assert!(!is_stop_word("haus"));
    }
}
