const STOP_WORDS: &[&str] = &[
    "de", "la", "que", "el", "en", "y", "a", "los", "del", "se", "las", "por", "un", "para", "con",
    "no", "una", "su", "al", "lo", "como", "más", "pero", "sus", "le", "ya", "o", "este", "sí",
    "porque", "esta", "entre", "cuando", "muy", "sin", "sobre", "también", "me", "hasta", "hay",
    "donde", "quien", "desde", "todo", "nos", "durante", "todos", "uno", "les", "ni", "contra",
    "otros", "ese", "eso", "ante", "ellos", "e", "esto", "mí", "antes", "algunos", "qué", "unos",
    "yo", "otro", "otras", "otra", "él", "tanto", "esa", "estos", "mucho", "quienes", "nada",
    "muchos", "cual", "poco", "ella", "estar", "estas", "algunas", "algo", "nosotros", "mi",
    "mis", "tú", "te", "ti", "tu", "tus", "ellas", "nosotras", "vosotros", "vosotras", "os",
    "mío", "mía", "míos", "mías", "tuyo", "tuya", "tuyos", "tuyas", "suyo", "suya", "suyos",
    "suyas", "nuestro", "nuestra", "nuestros", "nuestras", "vuestro", "vuestra", "vuestros",
    "vuestras", "esos", "esas", "estoy", "estás", "está", "estamos", "estáis", "están", "esté",
    "estés", "estemos", "estéis", "estén", "estaba", "estabas", "estábamos", "estabais",
    "estaban", "estuve", "estuvo", "estuvimos", "estuvieron", "he", "has", "ha", "hemos",
    "habéis", "han", "haya", "había", "habían", "hube", "hubo", "soy", "eres", "es", "somos",
    "sois", "son", "sea", "era", "eras", "éramos", "erais", "eran", "fui", "fue", "fuimos",
    "fueron", "tengo", "tienes", "tiene", "tenemos", "tenéis", "tienen", "tenía", "tenían",
    "tuve", "tuvo",
];
const PRONOUNS: [&str; 13] = [
    "me", "se", "sa", "sela", "selo", "selas", "selos", "la", "le", "lo", "las", "les", "nos",
];
const STEP_TWO_B_DELETE: &[&str] = &[
    "arían", "arías", "arán", "arás", "aríais", "aría", "aréis", "aríamos", "aremos", "ará", "aré",
    "erían", "erías", "erán", "erás", "eríais", "ería", "eréis", "eríamos", "eremos", "erá", "eré",
    "irían", "irías", "irán", "irás", "iríais", "iría", "iréis", "iríamos", "iremos", "irá", "iré",
    "aba", "ada", "ida", "ía", "ara", "iera", "ad", "ed", "id", "ase", "iese", "aste", "iste",
    "an", "aban", "ían", "aran", "ieran", "asen", "iesen", "aron", "ieron", "ado", "ido", "ando",
    "iendo", "ió", "ar", "er", "ir", "as", "abas", "adas", "idas", "ías", "aras", "ieras", "ases",
    "ieses", "ís", "áis", "abais", "íais", "arais", "ierais", "aseis", "ieseis", "asteis",
];
const STEP_TWO_B_DELETE_MORE: &[&str] = &[
    "isteis", "ados", "idos", "amos", "ábamos", "íamos", "imos", "áramos", "iéramos", "iésemos",
    "ásemos",
];

/// Whether `word` (lower case) is on Snowball's Spanish stop word list.
pub fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

fn is_vowel(letter: char) -> bool {
    matches!(
        letter,
        'a' | 'e' | 'i' | 'o' | 'u' | 'á' | 'é' | 'í' | 'ó' | 'ú' | 'ü'
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
    if letters.len() < 2 {
        return letters.len();
    }
    let after = |from: usize, wanted_vowel: bool| {
        (from..letters.len())
            .find(|&at| is_vowel(letters[at]) == wanted_vowel)
            .map_or(letters.len(), |at| at + 1)
    };
    if !is_vowel(letters[1]) {
        after(2, true)
    } else if is_vowel(letters[0]) {
        after(2, false)
    } else {
        3.min(letters.len())
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

fn unaccented(letter: char) -> char {
    match letter {
        'á' => 'a',
        'é' => 'e',
        'í' => 'i',
        'ó' => 'o',
        'ú' => 'u',
        other => other,
    }
}

fn attached_pronoun(letters: &mut Vec<char>, rv: usize) {
    let Some((_, start)) = longest(letters, &PRONOUNS) else {
        return;
    };
    if start < rv {
        return;
    }
    let before = letters[..start].to_vec();
    let accented = ["iéndo", "ándo", "ár", "ér", "ír"];
    let plain = ["ando", "iendo", "ar", "er", "ir"];
    if let Some((_, at)) = longest(&before, &accented)
        && at >= rv
    {
        letters.truncate(start);
        for letter in &mut letters[at..] {
            *letter = unaccented(*letter);
        }
    } else if let Some((_, at)) = longest(&before, &plain)
        && at >= rv
    {
        letters.truncate(start);
    } else if let Some(at) = ends_with(&before, "yendo")
        && at >= rv
        && at > 0
        && before[at - 1] == 'u'
    {
        letters.truncate(start);
    }
}

fn standard_suffix(letters: &mut Vec<char>, r1: usize, r2: usize) -> bool {
    let suffixes = [
        "anza", "anzas", "ico", "ica", "icos", "icas", "ismo", "ismos", "able", "ables", "ible",
        "ibles", "ista", "istas", "oso", "osa", "osos", "osas", "amiento", "amientos", "imiento",
        "imientos", "adora", "ador", "ación", "adoras", "adores", "aciones", "ante", "antes",
        "ancia", "ancias", "logía", "logías", "ución", "uciones", "encia", "encias", "amente",
        "mente", "idad", "idades", "iva", "ivo", "ivas", "ivos",
    ];
    let Some((suffix, start)) = longest(letters, &suffixes) else {
        return false;
    };
    let in_r2 = |at: usize| at >= r2;
    let drop_if = |letters: &mut Vec<char>, ending: &str| {
        if let Some(at) = ends_with(letters, ending)
            && in_r2(at)
        {
            letters.truncate(at);
            return true;
        }
        false
    };
    match suffix {
        "adora" | "ador" | "ación" | "adoras" | "adores" | "aciones" | "ante" | "antes"
        | "ancia" | "ancias" => {
            if !in_r2(start) {
                return false;
            }
            letters.truncate(start);
            drop_if(letters, "ic");
        }
        "logía" | "logías" => {
            if !in_r2(start) {
                return false;
            }
            replace_tail(letters, start, "log");
        }
        "ución" | "uciones" => {
            if !in_r2(start) {
                return false;
            }
            replace_tail(letters, start, "u");
        }
        "encia" | "encias" => {
            if !in_r2(start) {
                return false;
            }
            replace_tail(letters, start, "ente");
        }
        "amente" => {
            if start < r1 {
                return false;
            }
            letters.truncate(start);
            if drop_if(letters, "iv") {
                drop_if(letters, "at");
            } else if !drop_if(letters, "os") && !drop_if(letters, "ic") {
                drop_if(letters, "ad");
            }
        }
        "mente" => {
            if !in_r2(start) {
                return false;
            }
            letters.truncate(start);
            if !drop_if(letters, "ante") && !drop_if(letters, "able") {
                drop_if(letters, "ible");
            }
        }
        "idad" | "idades" => {
            if !in_r2(start) {
                return false;
            }
            letters.truncate(start);
            if !drop_if(letters, "abil") && !drop_if(letters, "ic") {
                drop_if(letters, "iv");
            }
        }
        "iva" | "ivo" | "ivas" | "ivos" => {
            if !in_r2(start) {
                return false;
            }
            letters.truncate(start);
            drop_if(letters, "at");
        }
        _ => {
            if !in_r2(start) {
                return false;
            }
            letters.truncate(start);
        }
    }
    true
}

fn verb_suffix(letters: &mut Vec<char>, rv: usize) {
    let y_forms = [
        "ya", "ye", "yan", "yen", "yeron", "yendo", "yo", "yó", "yas", "yes", "yais", "yamos",
    ];
    if let Some((_, start)) = longest(letters, &y_forms)
        && start >= rv
        && start > 0
        && letters[start - 1] == 'u'
    {
        letters.truncate(start);
        return;
    }
    let mut endings: Vec<&'static str> = vec!["en", "es", "éis", "emos"];
    endings.extend_from_slice(STEP_TWO_B_DELETE);
    endings.extend_from_slice(STEP_TWO_B_DELETE_MORE);
    let Some((suffix, start)) = longest(letters, &endings) else {
        return;
    };
    if start < rv {
        return;
    }
    letters.truncate(start);
    if matches!(suffix, "en" | "es" | "éis" | "emos") && ends_with(letters, "gu").is_some() {
        letters.pop();
    }
}

fn residual_suffix(letters: &mut Vec<char>, rv: usize) {
    if let Some((_, start)) = longest(letters, &["os", "a", "o", "á", "í", "ó"])
        && start >= rv
    {
        letters.truncate(start);
        return;
    }
    if let Some((_, start)) = longest(letters, &["e", "é"])
        && start >= rv
    {
        letters.truncate(start);
        if let Some(at) = ends_with(letters, "gu")
            && at + 1 >= rv
        {
            letters.pop();
        }
    }
}

/// The stem of `word` (lower case) by Snowball's Spanish stemmer: attached pronouns, the
/// standard suffixes, verb suffixes and residual suffixes removed over RV, R1 and R2, and acute
/// accents dropped.
pub fn stem(word: &str) -> String {
    let mut letters: Vec<char> = word.chars().collect();
    let rv = region_rv(&letters);
    let r1 = region_after(&letters, 0);
    let r2 = region_after(&letters, r1).max(r1);
    attached_pronoun(&mut letters, rv);
    if !standard_suffix(&mut letters, r1, r2) {
        verb_suffix(&mut letters, rv);
    }
    residual_suffix(&mut letters, rv);
    letters.into_iter().map(unaccented).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_drop_suffixes_pronouns_and_accents() {
        for (word, expected) in [
            ("chica", "chic"),
            ("cantando", "cant"),
            ("niñas", "niñ"),
            ("rápidamente", "rapid"),
            ("diciéndole", "dic"),
        ] {
            assert_eq!(stem(word), expected, "{word}");
        }
        assert!(is_stop_word("los"));
        assert!(!is_stop_word("casa"));
    }
}
