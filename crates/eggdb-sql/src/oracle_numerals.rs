const ROMAN_DIGITS: [(u32, &str); 13] = [
    (1000, "M"),
    (900, "CM"),
    (500, "D"),
    (400, "CD"),
    (100, "C"),
    (90, "XC"),
    (50, "L"),
    (40, "XL"),
    (10, "X"),
    (9, "IX"),
    (5, "V"),
    (4, "IV"),
    (1, "I"),
];

pub(crate) const MAX_ROMAN: u32 = 3999;

const ONES: [&str; 20] = [
    "ZERO",
    "ONE",
    "TWO",
    "THREE",
    "FOUR",
    "FIVE",
    "SIX",
    "SEVEN",
    "EIGHT",
    "NINE",
    "TEN",
    "ELEVEN",
    "TWELVE",
    "THIRTEEN",
    "FOURTEEN",
    "FIFTEEN",
    "SIXTEEN",
    "SEVENTEEN",
    "EIGHTEEN",
    "NINETEEN",
];

const TENS: [&str; 10] = [
    "", "", "TWENTY", "THIRTY", "FORTY", "FIFTY", "SIXTY", "SEVENTY", "EIGHTY", "NINETY",
];

const SCALES: [(u64, &str); 3] = [
    (1_000_000_000, "BILLION"),
    (1_000_000, "MILLION"),
    (1000, "THOUSAND"),
];

pub(crate) const MAX_SPELLED_BYTES: usize = 160;

pub(crate) fn roman(value: u32) -> Option<String> {
    if !(1..=MAX_ROMAN).contains(&value) {
        return None;
    }
    let mut rest = value;
    let mut out = String::new();
    for (unit, letters) in ROMAN_DIGITS {
        while rest >= unit {
            out.push_str(letters);
            rest -= unit;
        }
    }
    Some(out)
}

pub(crate) fn roman_value(text: &str) -> Option<u32> {
    let upper = text.to_ascii_uppercase();
    let mut rest = upper.as_str();
    let mut total = 0u32;
    let mut last_unit = u32::MAX;
    for (unit, letters) in ROMAN_DIGITS {
        let mut repeats = 0;
        while let Some(after) = rest.strip_prefix(letters) {
            if unit > last_unit || repeats == 3 || (repeats == 1 && letters.len() == 2) {
                return None;
            }
            total += unit;
            rest = after;
            repeats += 1;
        }
        if repeats > 0 {
            last_unit = unit;
        }
    }
    (rest.is_empty() && total > 0 && roman(total).as_deref() == Some(upper.as_str()))
        .then_some(total)
}

fn below_thousand(value: u64, out: &mut Vec<String>) {
    let hundreds = value / 100;
    let rest = value % 100;
    if hundreds > 0 {
        out.push(ONES[hundreds as usize].to_owned());
        out.push("HUNDRED".to_owned());
    }
    if rest == 0 {
        return;
    }
    if rest < 20 {
        out.push(ONES[rest as usize].to_owned());
    } else if rest % 10 == 0 {
        out.push(TENS[(rest / 10) as usize].to_owned());
    } else {
        out.push(format!(
            "{}-{}",
            TENS[(rest / 10) as usize],
            ONES[(rest % 10) as usize]
        ));
    }
}

pub(crate) fn spelled(value: u64) -> String {
    if value == 0 {
        return ONES[0].to_owned();
    }
    let mut words = Vec::new();
    let mut rest = value;
    for (unit, name) in SCALES {
        if rest >= unit {
            let count = rest / unit;
            if count >= 1000 {
                words.push(spelled(count));
            } else {
                below_thousand(count, &mut words);
            }
            words.push(name.to_owned());
            rest %= unit;
        }
    }
    below_thousand(rest, &mut words);
    words.join(" ")
}

fn ordinal_word(word: &str) -> String {
    match word {
        "ONE" => "FIRST".to_owned(),
        "TWO" => "SECOND".to_owned(),
        "THREE" => "THIRD".to_owned(),
        "FIVE" => "FIFTH".to_owned(),
        "EIGHT" => "EIGHTH".to_owned(),
        "NINE" => "NINTH".to_owned(),
        "TWELVE" => "TWELFTH".to_owned(),
        other => match other.strip_suffix('Y') {
            Some(stem) => format!("{stem}IETH"),
            None => format!("{other}TH"),
        },
    }
}

pub(crate) fn spelled_ordinal(value: u64) -> String {
    let cardinal = spelled(value);
    let (head, last) = match cardinal.rfind([' ', '-']) {
        Some(at) => cardinal.split_at(at + 1),
        None => ("", cardinal.as_str()),
    };
    format!("{head}{}", ordinal_word(last))
}

pub(crate) fn ordinal_suffix(value: u64) -> &'static str {
    match (value % 10, value % 100) {
        (_, 11..=13) => "TH",
        (1, _) => "ST",
        (2, _) => "ND",
        (3, _) => "RD",
        _ => "TH",
    }
}

pub(crate) fn spelled_year(year: u64) -> String {
    let (high, low) = (year / 100, year % 100);
    if high == 0 {
        return spelled(low);
    }
    match (high % 10, low) {
        (0, 0) => format!("{} THOUSAND", spelled(high / 10)),
        (_, 0) => format!("{} HUNDRED", spelled(high)),
        (0, 1..=9) => format!("{} THOUSAND {}", spelled(high / 10), spelled(low)),
        _ => format!("{} {}", spelled(high), spelled(low)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ordinal_suffix, roman, roman_value, spelled, spelled_ordinal, spelled_year,
    };

    #[test]
    fn roman_numerals_cover_one_to_3999_both_ways() {
        assert_eq!(roman(14).as_deref(), Some("XIV"));
        assert_eq!(roman(1999).as_deref(), Some("MCMXCIX"));
        assert_eq!(roman(3999).as_deref(), Some("MMMCMXCIX"));
        assert_eq!(roman(0), None);
        assert_eq!(roman(4000), None);
        for value in 1..=3999 {
            let text = roman(value).expect("roman");
            assert_eq!(roman_value(&text), Some(value), "{text}");
            assert_eq!(roman_value(&text.to_ascii_lowercase()), Some(value));
        }
        assert_eq!(roman_value("IIII"), None);
        assert_eq!(roman_value("IC"), None);
        assert_eq!(roman_value(""), None);
        assert_eq!(roman_value("XIIx"), None);
    }

    #[test]
    fn numbers_spell_as_english_words() {
        assert_eq!(spelled(0), "ZERO");
        assert_eq!(spelled(15), "FIFTEEN");
        assert_eq!(spelled(21), "TWENTY-ONE");
        assert_eq!(spelled(40), "FORTY");
        assert_eq!(spelled(335), "THREE HUNDRED THIRTY-FIVE");
        assert_eq!(
            spelled(2_451_514),
            "TWO MILLION FOUR HUNDRED FIFTY-ONE THOUSAND FIVE HUNDRED FOURTEEN"
        );
        assert_eq!(spelled_ordinal(1), "FIRST");
        assert_eq!(spelled_ordinal(12), "TWELFTH");
        assert_eq!(spelled_ordinal(20), "TWENTIETH");
        assert_eq!(spelled_ordinal(23), "TWENTY-THIRD");
        assert_eq!(spelled_ordinal(100), "ONE HUNDREDTH");
        assert_eq!(ordinal_suffix(1), "ST");
        assert_eq!(ordinal_suffix(11), "TH");
        assert_eq!(ordinal_suffix(22), "ND");
        assert_eq!(ordinal_suffix(113), "TH");
        assert!(spelled(u64::MAX).len() < 400);
    }

    #[test]
    fn years_spell_in_pairs_of_digits() {
        assert_eq!(spelled_year(1999), "NINETEEN NINETY-NINE");
        assert_eq!(spelled_year(2000), "TWO THOUSAND");
        assert_eq!(spelled_year(2008), "TWO THOUSAND EIGHT");
        assert_eq!(spelled_year(2024), "TWENTY TWENTY-FOUR");
        assert_eq!(spelled_year(1900), "NINETEEN HUNDRED");
        assert_eq!(spelled_year(5), "FIVE");
    }
}
