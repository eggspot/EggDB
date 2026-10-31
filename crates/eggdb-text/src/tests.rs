use crate::{Form, case_fold, compare_accent_insensitive, compare_case_insensitive, is_normalized, normalize, primary_key};
use std::cmp::Ordering;

#[test]
fn the_four_forms_match_known_strings() {
    assert_eq!(normalize("e\u{301}", Form::Nfc), "é");
    assert_eq!(normalize("é", Form::Nfd), "e\u{301}");
    assert_eq!(normalize("ﬁ", Form::Nfkc), "fi");
    assert_eq!(normalize("ﬁ", Form::Nfc), "ﬁ");
    assert_eq!(normalize("한", Form::Nfd), "\u{1112}\u{1161}\u{11AB}");
    assert_eq!(normalize("\u{1112}\u{1161}\u{11AB}", Form::Nfc), "한");
    assert_eq!(normalize("a\u{323}\u{302}", Form::Nfc), "ậ");
    assert_eq!(normalize("a\u{302}\u{323}", Form::Nfc), "ậ");
    assert_eq!(normalize("\u{212B}", Form::Nfc), "Å");
    assert!(is_normalized("Å", Form::Nfc) && !is_normalized("A\u{30A}", Form::Nfc));
    assert_eq!(Form::named("nfkd"), Some(Form::Nfkd));
}

#[test]
fn case_folding_and_comparisons_ignore_what_they_say() {
    assert_eq!(case_fold("Straße ΣΑΣ"), "strasse σασ");
    assert_eq!(primary_key("Résumé"), "resume");
    assert_eq!(primary_key("résumé"), primary_key("RESUME"));
    assert_eq!(compare_case_insensitive("apple", "Banana"), Ordering::Less);
    assert_eq!(compare_case_insensitive("Apple", "apple"), Ordering::Less);
    assert_eq!(compare_accent_insensitive("café", "cafe"), Ordering::Greater);
    assert_eq!(compare_accent_insensitive("same", "same"), Ordering::Equal);
}

#[test]
fn the_unicode_normalization_conformance_file_passes_when_given() {
    let Ok(path) = std::env::var("EGGDB_UNICODE_NORMALIZATION_TEST") else {
        return;
    };
    let text = std::fs::read_to_string(path).unwrap();
    let mut checked = 0;
    for line in text.lines() {
        let data = line.split('#').next().unwrap_or_default().trim();
        if data.is_empty() || data.starts_with('@') {
            continue;
        }
        let columns: Vec<String> = data
            .split(';')
            .take(5)
            .map(|column| column.split_whitespace().filter_map(|code| u32::from_str_radix(code, 16).ok().and_then(char::from_u32)).collect())
            .collect();
        let [source, nfc, nfd, nfkc, nfkd] = columns.as_slice() else {
            continue;
        };
        assert_eq!(&normalize(source, Form::Nfc), nfc, "NFC of {data}");
        assert_eq!(&normalize(source, Form::Nfd), nfd, "NFD of {data}");
        assert_eq!(&normalize(source, Form::Nfkc), nfkc, "NFKC of {data}");
        assert_eq!(&normalize(source, Form::Nfkd), nfkd, "NFKD of {data}");
        checked += 1;
    }
    assert!(checked > 10_000);
}
