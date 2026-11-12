use super::*;

fn document(columns: &[(&str, &str)]) -> Document {
    Document::of(
        columns
            .iter()
            .map(|(name, text)| ((*name).to_owned(), (*text).to_owned())),
    )
}

fn hit(query: &str, columns: &[(&str, &str)]) -> bool {
    matches(&parse_query(query).expect("a query"), &document(columns))
}

#[test]
fn the_query_syntax_finds_terms_phrases_prefixes_and_boolean_combinations() {
    let row = [("title", "Egg recipes"), ("body", "Boil the eggs for nine minutes")];
    assert!(hit("eggs", &row));
    assert!(hit("EGG*", &row));
    assert!(hit("\"nine minutes\"", &row));
    assert!(!hit("\"minutes nine\"", &row));
    assert!(hit("boil AND nine", &row));
    assert!(hit("boil nine", &row));
    assert!(hit("fry OR boil", &row));
    assert!(!hit("boil NOT nine", &row));
    assert!(hit("title : recipes", &row));
    assert!(!hit("title : boil", &row));
    assert!(hit("- title : boil", &row));
    assert!(hit("{title body} : boil", &row));
    assert!(hit("NEAR(boil minutes, 4)", &row));
    assert!(!hit("NEAR(boil minutes, 1)", &row));
    assert!(hit("^egg", &row));
    assert!(!hit("^recipes", &row));
    assert!(hit("(fry OR boil) AND eggs", &row));
    assert!(parse_query("\"unclosed").is_err());
    assert!(parse_query("").is_err());
    assert!(parse_query("a AND").is_err());
}

#[test]
fn highlights_wrap_the_matched_words() {
    let query = parse_query("egg*").unwrap();
    assert_eq!(highlighted("Eggs, more eggs.", &query, "[", "]"), "[Eggs], more [eggs].");
    assert!(score(&query, &document(&[("t", "egg egg egg")])) < score(&query, &document(&[("t", "egg")])));
}

fn run(session: &mut crate::Session<'_>, sql: &str) -> String {
    let mut outputs = Vec::new();
    session
        .execute(sql, &mut |result| outputs.push(crate::tests::render(&result)))
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    outputs.pop().unwrap_or_default()
}

#[test]
fn an_fts5_table_answers_match_bm25_and_highlight() {
    let store = crate::memory_store::MemoryStore::default();
    let mut session = crate::Session::with_dialect(&store, crate::Dialect::Sqlite);
    run(&mut session, "CREATE VIRTUAL TABLE notes USING fts5(title, body, tokenize = 'unicode61')");
    run(&mut session, "INSERT INTO notes (title, body) VALUES ('eggs', 'boil eggs eggs'), ('toast', 'butter the toast'), ('omelette', 'whisk eggs')");
    let found = run(&mut session, "SELECT title FROM notes WHERE notes MATCH 'eggs' ORDER BY bm25(notes)");
    assert_eq!(found, "title\neggs\nomelette\nSELECT 2");
    let marked = run(&mut session, "SELECT highlight(notes, 1, '<b>', '</b>') AS h FROM notes WHERE notes MATCH 'toast'");
    assert!(marked.contains("butter the <b>toast</b>"), "{marked}");
}
