const STOP_WORDS: &[&str] = &[
    "a", "ahogy", "ahol", "aki", "akik", "akkor", "alatt", "által", "általában", "amely",
    "amelyek", "amelyekben", "amelyeket", "amelyet", "amelynek", "ami", "amit", "amolyan", "amíg",
    "amikor", "át", "abban", "ahhoz", "annak", "arra", "arról", "az", "azok", "azon", "azt",
    "azzal", "azért", "aztán", "azután", "azonban", "bár", "be", "belül", "benne", "csak", "de",
    "e", "eddig", "egész", "egy", "egyes", "egyetlen", "egyéb", "egyik", "egyre", "ekkor", "el",
    "elég", "ellen", "elő", "először", "előtt", "első", "én", "éppen", "ebben", "ehhez",
    "emilyen", "ennek", "erre", "ez", "ezt", "ezek", "ezen", "ezzel", "ezért", "és", "fel",
    "felé", "hanem", "hiszen", "hogy", "hogyan", "igen", "így", "illetve", "ilyen", "ilyenkor",
    "ismét", "itt", "jó", "jól", "jobban", "kell", "kellett", "keresztül", "ki", "kívül",
    "között", "közül", "legalább", "lehet", "lehetett", "legyen", "lenne", "lenni", "lesz",
    "lett", "maga", "magát", "majd", "már", "más", "másik", "meg", "még", "mellett", "mert",
    "mely", "melyek", "mi", "mit", "míg", "miért", "milyen", "mikor", "minden", "mindent",
    "mindenki", "mindig", "mint", "mintha", "mivel", "most", "nagy", "nagyobb", "nagyon", "ne",
    "néha", "nekem", "neki", "nem", "néhány", "nélkül", "nincs", "olyan", "ott", "össze", "ő",
    "ők", "őket", "pedig", "persze", "rá", "s", "saját", "sem", "semmi", "sok", "sokat",
    "sokkal", "számára", "szemben", "szerint", "szinte", "talán", "tehát", "teljes", "tovább",
    "továbbá", "több", "úgy", "ugyanis", "új", "újabb", "újra", "után", "utána", "utolsó",
    "vagy", "vagyis", "valaki", "valami", "valamint", "való", "vagyok", "van", "vannak", "volt",
    "voltam", "voltak", "voltunk", "vissza", "vele", "viszont", "volna",
];
const DIGRAPHS: &[&str] = &["dzs", "cs", "dz", "gy", "ly", "ny", "sz", "ty", "zs"];
const DOUBLES: &[&str] = &[
    "bb", "cc", "ccs", "dd", "ff", "gg", "ggy", "jj", "kk", "ll", "lly", "mm", "nn", "nny", "pp",
    "rr", "ssz", "ss", "tt", "tty", "vv", "zz", "zzs",
];

/// Whether `word` (lower case) is on Snowball's Hungarian stop word list.
pub fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

fn is_vowel(letter: char) -> bool {
    matches!(
        letter,
        'a' | 'á' | 'e' | 'é' | 'i' | 'í' | 'o' | 'ó' | 'ö' | 'ő' | 'u' | 'ú' | 'ü' | 'ű'
    )
}

fn starts_with(letters: &[char], at: usize, prefix: &str) -> bool {
    let prefix: Vec<char> = prefix.chars().collect();
    letters.get(at..).is_some_and(|rest| rest.starts_with(&prefix))
}

fn region_r1(letters: &[char]) -> usize {
    let length = letters.len();
    let Some(&first) = letters.first() else {
        return 0;
    };
    if is_vowel(first) {
        let Some(consonant) = letters.iter().position(|&letter| !is_vowel(letter)) else {
            return length;
        };
        let digraph = DIGRAPHS
            .iter()
            .find(|digraph| starts_with(letters, consonant, digraph))
            .map_or(1, |digraph| digraph.chars().count());
        (consonant + digraph).min(length)
    } else {
        letters
            .iter()
            .position(|&letter| is_vowel(letter))
            .map_or(length, |vowel| vowel + 1)
    }
}

fn ends_with(letters: &[char], suffix: &str) -> Option<usize> {
    let suffix: Vec<char> = suffix.chars().collect();
    letters.ends_with(&suffix).then(|| letters.len() - suffix.len())
}

fn longest(letters: &[char], r1: usize, suffixes: &[&'static str]) -> Option<(&'static str, usize)> {
    suffixes
        .iter()
        .filter_map(|suffix| ends_with(letters, suffix).map(|start| (*suffix, start)))
        .filter(|(_, start)| *start >= r1)
        .max_by_key(|(suffix, _)| suffix.chars().count())
}

fn ends_with_double(letters: &[char]) -> bool {
    DOUBLES.iter().any(|double| ends_with(letters, double).is_some())
}

fn undouble(letters: &mut Vec<char>) {
    let length = letters.len();
    if length >= 2 {
        letters.remove(length - 2);
    }
}

fn replace_tail(letters: &mut Vec<char>, start: usize, with: &str) {
    letters.truncate(start);
    letters.extend(with.chars());
}

fn remove_doubled_ending(letters: &mut Vec<char>, r1: usize, suffixes: &[&'static str]) {
    if let Some((_, start)) = longest(letters, r1, suffixes)
        && ends_with_double(&letters[..start])
    {
        letters.truncate(start);
        undouble(letters);
    }
}

fn remove_with_vowel_ending(letters: &mut Vec<char>, r1: usize, deleted: &[&'static str], to_a: &[&'static str], to_e: &[&'static str]) {
    let all: Vec<&'static str> = deleted.iter().chain(to_a).chain(to_e).copied().collect();
    let Some((suffix, start)) = longest(letters, r1, &all) else {
        return;
    };
    if to_a.contains(&suffix) {
        replace_tail(letters, start, "a");
    } else if to_e.contains(&suffix) {
        replace_tail(letters, start, "e");
    } else {
        letters.truncate(start);
    }
}

fn frequent_case(letters: &mut Vec<char>, r1: usize) {
    let suffixes = [
        "ban", "ben", "ba", "be", "ra", "re", "nak", "nek", "val", "vel", "tól", "től", "ról",
        "ről", "ból", "ből", "hoz", "hez", "höz", "nál", "nél", "ig", "at", "et", "ot", "öt",
        "ért", "képp", "képpen", "kor", "ul", "ül", "vá", "vé", "onként", "enként", "anként",
        "ként", "en", "on", "an", "ön", "n", "t",
    ];
    let Some((_, start)) = longest(letters, r1, &suffixes) else {
        return;
    };
    letters.truncate(start);
    if let Some((suffix, start)) = longest(letters, r1, &["á", "é"]) {
        replace_tail(letters, start, if suffix == "á" { "a" } else { "e" });
    }
}

/// The stem of `word` (lower case) by Snowball's Hungarian stemmer: in R1 (after the first
/// consonant or digraph of a word starting with a vowel, else after the first vowel), the
/// instrumental, case, factive, owned, owner and plural suffixes removed in turn, a final
/// `á` or `é` left before them read as `a` or `e`.
pub fn stem(word: &str) -> String {
    let mut letters: Vec<char> = word.chars().collect();
    let r1 = region_r1(&letters);
    remove_doubled_ending(&mut letters, r1, &["al", "el"]);
    frequent_case(&mut letters, r1);
    remove_with_vowel_ending(&mut letters, r1, &[], &["án", "ánként"], &["én"]);
    remove_with_vowel_ending(&mut letters, r1, &["astul", "estül", "stul", "stül"], &["ástul"], &["éstül"]);
    remove_doubled_ending(&mut letters, r1, &["á", "é"]);
    remove_with_vowel_ending(
        &mut letters,
        r1,
        &["oké", "öké", "aké", "eké", "ké", "éi", "é"],
        &["áké", "áéi"],
        &["éké", "ééi", "éé"],
    );
    remove_with_vowel_ending(
        &mut letters,
        r1,
        &[
            "ünk", "unk", "nk", "juk", "jük", "uk", "ük", "em", "om", "am", "m", "od", "ed", "ad",
            "öd", "d", "ja", "je", "a", "e", "o",
        ],
        &["ánk", "ájuk", "ám", "ád", "á"],
        &["énk", "éjük", "ém", "éd", "é"],
    );
    remove_with_vowel_ending(
        &mut letters,
        r1,
        &[
            "jaim", "jeim", "aim", "eim", "im", "jaid", "jeid", "aid", "eid", "id", "jai", "jei",
            "ai", "ei", "i", "jaink", "jeink", "eink", "aink", "ink", "jaitok", "jeitek", "aitok",
            "eitek", "itek", "jeik", "jaik", "aik", "eik", "ik",
        ],
        &["áim", "áid", "ái", "áink", "áitok", "áik"],
        &["éim", "éid", "éi", "éink", "éitek", "éik"],
    );
    remove_with_vowel_ending(&mut letters, r1, &["ök", "ok", "ek", "ak", "k"], &["ák"], &["ék"]);
    letters.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_drop_case_and_plural_suffixes() {
        for (word, expected) in [("házban", "ház"), ("almák", "alma"), ("embereknek", "ember")] {
            assert_eq!(stem(word), expected, "{word}");
        }
        assert!(is_stop_word("és"));
        assert!(!is_stop_word("ház"));
    }
}
