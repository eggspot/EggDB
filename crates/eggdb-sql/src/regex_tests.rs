use crate::regex::{CaseSensitivity, MAX_REPEAT, Regex, STATEMENT_STEPS, StepBudget};

fn matches(text: &str, pattern: &str) -> bool {
    Regex::compile(pattern, CaseSensitivity::Sensitive)
        .and_then(|regex| regex.is_match(text, &mut StepBudget::new(STATEMENT_STEPS)))
        .unwrap_or_else(|error| panic!("{pattern}: {error}"))
}

fn matches_folded(text: &str, pattern: &str) -> bool {
    Regex::compile(pattern, CaseSensitivity::Insensitive)
        .and_then(|regex| regex.is_match(text, &mut StepBudget::new(STATEMENT_STEPS)))
        .unwrap_or_else(|error| panic!("{pattern}: {error}"))
}

fn refusal(pattern: &str) -> (String, String) {
    let error = Regex::compile(pattern, CaseSensitivity::Sensitive).expect_err(pattern);
    (error.state().code().to_owned(), error.message().to_owned())
}

#[test]
fn the_manuals_pattern_matching_examples_hold() {
    let cases = [
        ("abcd", "abcd", true),
        ("abcd", "a.c", true),
        ("abcd", "^a", true),
        ("abcd", "(b|x)", true),
        ("abcd", "^c", false),
        ("abc", "b", true),
        ("abc", "^a.*c$", true),
        ("abc", "^b", false),
        ("abc", "(a)(b)c", true),
        ("thomas", ".*thomas.*", true),
        ("thomas", ".*vadim.*", false),
        ("", "", true),
        ("a", "()", true),
        ("abc", "a|", true),
        ("abc", "|b", true),
    ];
    for (text, pattern, expected) in cases {
        assert_eq!(matches(text, pattern), expected, "{text} ~ {pattern}");
    }
}

#[test]
fn brackets_classes_escapes_and_bounds_follow_the_are_syntax() {
    let cases = [
        ("a", "[]a]", true),
        ("]", "[]a]", true),
        ("-", "[a-]", true),
        ("b", "[^a]", true),
        ("a", "[^a]", false),
        ("^", "[\\^]", true),
        ("\\", "[\\\\]", true),
        ("a", "[[:alpha:]]", true),
        ("5", "[[:digit:]x]", true),
        ("x", "[\\d]", false),
        ("7", "[\\d]", true),
        ("a", "[[.a.]]", true),
        ("a", "[[=a=]]", true),
        ("q", "[a-z]", true),
        ("Q", "[a-z]", false),
        ("a b", "a\\sb", true),
        ("a_1", "^\\w+$", true),
        ("12", "^\\d+$", true),
        ("ab", "^\\D+$", true),
        ("a1", "^\\W", false),
        ("a", "\\S", true),
        ("a.b", "a\\.b", true),
        ("axb", "a\\.b", false),
        ("a+b", "a\\+b", true),
        ("*", "\\*", true),
        ("x", "\\x78", true),
        ("x", "\\u0078", true),
        ("a\tb", "a\\tb", true),
        ("a\nb", "a\\nb", true),
        ("a{", "a{", true),
        ("a{x", "a{x", true),
        ("a{,2}", "a{,2}", true),
        ("aa", "^a{2}$", true),
        ("aaa", "^a{2}$", false),
        ("aaa", "^a{2,}$", true),
        ("aa", "^a{1,3}$", true),
        ("aaaa", "^a{1,3}$", false),
        ("x", "x{0}", true),
        ("", "^x{0}$", true),
        ("aaa", "^a*?$", true),
        ("aaa", "^a+?a$", true),
        ("ab", "^a??b$", true),
        ("aaa", "^a{2,3}?$", true),
        ("ab", "a(?:b)", true),
        ("ab", "(?#comment)ab", true),
        ("a\nb", "a.b", true),
        ("a\nb", "^b", false),
        ("a\nb", "a$", false),
        ("ab", "***:ab", true),
        ("ab", "a\\Bb", false),
        ("a\\b", "a\\Bb", true),
        ("é", "[[:alpha:]]", false),
        ("é", "\\w", false),
    ];
    for (text, pattern, expected) in cases {
        assert_eq!(matches(text, pattern), expected, "{text} ~ {pattern}");
    }
}

#[test]
fn case_insensitive_matching_folds_ascii_letters_as_the_c_locale_does() {
    let cases = [
        ("AbC", "abc", true),
        ("ABC", "[a-c]+$", true),
        ("abc", "^[A-C]+$", true),
        ("A", "[^a]", false),
        ("é", "É", false),
        ("x", "X|Y", true),
    ];
    for (text, pattern, expected) in cases {
        assert_eq!(
            matches_folded(text, pattern),
            expected,
            "{text} ~* {pattern}"
        );
    }
}

#[test]
fn malformed_patterns_are_2201b_with_postgresqls_reason() {
    let cases = [
        ("(", "parentheses () not balanced"),
        ("a)", "parentheses () not balanced"),
        ("a{2,1}", "invalid repetition count(s)"),
        ("a{256}", "invalid repetition count(s)"),
        ("a{1", "braces {} not balanced"),
        ("a{1,2", "braces {} not balanced"),
        ("[a", "brackets [] not balanced"),
        ("[z-a]", "invalid character range"),
        ("[a-[:digit:]]", "invalid character range"),
        ("[[:foo:]]", "invalid character class"),
        ("a\\", "invalid escape \\ sequence"),
        ("\\q", "invalid escape \\ sequence"),
        ("*a", "quantifier operand invalid"),
        ("a**", "quantifier operand invalid"),
        ("a{1,2}{3}", "quantifier operand invalid"),
        ("(?", "quantifier operand invalid"),
        ("a|*", "quantifier operand invalid"),
        ("^*", "quantifier operand invalid"),
        ("$*", "quantifier operand invalid"),
        ("a(*)", "quantifier operand invalid"),
    ];
    for (pattern, reason) in cases {
        let (state, message) = refusal(pattern);
        assert_eq!(state, "2201B", "{pattern}");
        assert_eq!(
            message,
            format!("invalid regular expression: {reason}"),
            "{pattern}"
        );
    }
    assert!(Regex::compile(&format!("a{{{MAX_REPEAT}}}"), CaseSensitivity::Sensitive).is_ok());
}

#[test]
fn constructs_not_run_yet_are_0a000() {
    for pattern in [
        "(a)\\1", "(?=a)", "(?!a)", "(?<=a)", "(?<!a)", "\\ma", "a\\M", "\\ya", "\\Ya", "(?i)A",
        "***=a", "[[.ab.]]",
    ] {
        assert_eq!(refusal(pattern).0, "0A000", "{pattern}");
    }
}

#[test]
fn nested_stars_run_in_linear_time() {
    let text = "a".repeat(100_000);
    let started = std::time::Instant::now();
    assert!(!matches(&text, "(a*)*b"));
    assert!(!matches(&text, "(a|a)*b"));
    assert!(!matches(&text, "(a+)+b"));
    assert!(matches(&text, "(a*)*$"));
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}

#[test]
fn deeply_nested_groups_are_refused_without_exhausting_the_stack() {
    let pattern = format!("{}a{}", "(".repeat(10_000), ")".repeat(10_000));
    let (state, message) = refusal(&pattern);
    assert_eq!(state, "2201B");
    assert!(message.contains("too complex"), "{message}");
    let pattern = format!("{}a{}", "(".repeat(200), ")".repeat(200));
    assert!(matches("a", &pattern));
}

#[test]
fn oversized_programs_and_patterns_are_refused_before_they_are_built() {
    assert_eq!(refusal("((((a{255}){255}){255}){255})").0, "2201B");
    assert_eq!(refusal(&"a".repeat(70_000)).0, "2201B");
}

#[test]
fn the_step_budget_ends_a_runaway_match() {
    let regex = Regex::compile("(a|b|c|d)*e", CaseSensitivity::Sensitive).unwrap();
    let text = "abcd".repeat(10_000);
    let error = regex
        .is_match(&text, &mut StepBudget::new(10_000))
        .expect_err("budget");
    assert_eq!(error.state().code(), "54000");
    assert_eq!(
        error.message(),
        "regular expression is too complex for this statement"
    );
    assert!(error.hint().is_some());
}

struct Seeded(u64);

impl Seeded {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

#[test]
fn seeded_random_patterns_and_texts_never_panic() {
    const PIECES: [&str; 40] = [
        "a",
        "b",
        ".",
        "*",
        "+",
        "?",
        "|",
        "(",
        ")",
        "(?:",
        "[",
        "]",
        "^",
        "$",
        "{",
        "}",
        "{2}",
        "{1,3}",
        "{,",
        "\\",
        "\\d",
        "\\w",
        "\\s",
        "\\1",
        "[:alpha:]",
        "[[:digit:]]",
        "-",
        "z-a",
        "\\x41",
        "\\u00e9",
        "é",
        "(?#",
        "(?=",
        "***=",
        "***:",
        "\\m",
        "[^",
        "a{255}",
        "\\B",
        "0",
    ];
    let mut random = Seeded(0x51ED_2701_B0B5_CAFE);
    for _ in 0..20_000 {
        let mut pattern = String::new();
        for _ in 0..random.below(12) {
            pattern.push_str(PIECES[random.below(PIECES.len() as u64) as usize]);
        }
        let mut text = String::new();
        for _ in 0..random.below(24) {
            text.push(['a', 'b', 'é', '\n', ' ', '1', '{'][random.below(7) as usize]);
        }
        for case in [CaseSensitivity::Sensitive, CaseSensitivity::Insensitive] {
            if let Ok(regex) = Regex::compile(&pattern, case) {
                let _ = regex.is_match(&text, &mut StepBudget::new(1_000_000));
            }
        }
    }
    let mut bytes = Seeded(0xBAD_5EED);
    for _ in 0..5_000 {
        let raw: Vec<u8> = (0..bytes.below(32))
            .map(|_| bytes.below(256) as u8)
            .collect();
        let pattern = String::from_utf8_lossy(&raw);
        if let Ok(regex) = Regex::compile(&pattern, CaseSensitivity::Sensitive) {
            let _ = regex.is_match(&pattern, &mut StepBudget::new(1_000_000));
        }
    }
}
