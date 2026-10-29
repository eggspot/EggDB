//! Paging (D145): `eggdb row list` and `eggdb table list` read a page at a time, and the cursor
//! at the end of a cut page stays right while a writer commits between pages: no item that
//! stays is repeated or skipped, items added after the cursor are read, items added before it
//! are not, and removed ones never appear.

mod common;

use common::TempPath;
use eggdb::sql::Dialect;
use eggdb::{Database, Options};
use eggdb_sql::json::{Json, Node, parse_document};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const ROWS: i64 = 10_000;
const PAGE: usize = 1_000;

fn eggdb(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[track_caller]
fn succeeded(output: &Output) -> String {
    assert!(output.status.success(), "{}", text(&output.stderr));
    text(&output.stdout)
}

fn write(path: &Path, sql: &str) {
    let db = Database::open_with(path, Options::new().create_if_missing(true)).unwrap();
    let store = db.sql_store();
    let mut session = store.session(Dialect::PostgreSql);
    session.execute(sql, &mut |_| {}).unwrap();
    let _ = session.close();
    db.close().unwrap();
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(member, _)| member == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn scalar(node: Option<&Node>) -> String {
    match node.map(|node| &node.value) {
        Some(Json::String(text) | Json::Number(text)) => text.clone(),
        Some(Json::Boolean(value)) => value.to_string(),
        other => panic!("not a scalar: {other:?}"),
    }
}

struct Page {
    records: Vec<Node>,
    next_cursor: Option<String>,
}

fn page_of(stdout: &str, limit: usize) -> Page {
    let mut records: Vec<Node> = stdout
        .lines()
        .map(|line| parse_document(line).unwrap_or_else(|error| panic!("{line}: {error:?}")))
        .collect();
    let next_cursor = if records.len() > limit {
        let trailer = records.pop().unwrap();
        let Json::Object(members) = &trailer.value else {
            panic!("{trailer:?}");
        };
        let names: Vec<&str> = members.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["truncated", "next_cursor"]);
        assert_eq!(scalar(member(&trailer, "truncated")), "true");
        Some(scalar(member(&trailer, "next_cursor")))
    } else {
        None
    };
    assert!(records.len() <= limit);
    Page {
        records,
        next_cursor,
    }
}

fn values_list(ids: impl Iterator<Item = i64>) -> String {
    ids.map(|id| format!("({id}, 'note {id}')"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[test]
fn a_ten_thousand_row_table_pages_without_repeats_or_gaps_while_a_writer_commits() {
    let file = TempPath::new("paging-rows");
    let path = file.path().to_str().unwrap().to_owned();
    let mut present: BTreeSet<i64> = (1..=ROWS).map(|n| n * 4).collect();
    write(
        file.path(),
        &format!(
            "CREATE TABLE orders (id integer PRIMARY KEY, note text); INSERT INTO orders VALUES {}",
            values_list(present.iter().copied())
        ),
    );
    let first_rows = present.clone();
    let mut seen: Vec<i64> = Vec::new();
    let mut added_after: BTreeSet<i64> = BTreeSet::new();
    let mut added_before: BTreeSet<i64> = BTreeSet::new();
    let mut removed_ahead: BTreeSet<i64> = BTreeSet::new();
    let mut cursor: Option<String> = None;
    let limit = PAGE.to_string();
    let mut pages = 0;
    loop {
        let mut args = vec!["row", "list", path.as_str(), "orders", "--limit", limit.as_str(), "--json"];
        if let Some(cursor) = &cursor {
            args.extend(["--after", cursor.as_str()]);
        }
        let page = page_of(&succeeded(&eggdb(&args)), PAGE);
        pages += 1;
        for record in &page.records {
            let id: i64 = scalar(member(record, "id")).parse().unwrap();
            assert_eq!(scalar(member(record, "note")), format!("note {id}"));
            seen.push(id);
        }
        let Some(next) = page.next_cursor else {
            break;
        };
        let last = *seen.last().unwrap();
        let before = free_id(&present, last - 1, -1);
        let soon = free_id(&present, last + 1, 1);
        let later = free_id(&present, last + 9_000, 1);
        let gone: Vec<i64> = [
            present.range(last + 1..).next(),
            present.range(last + 400..).next(),
            present.range(..last).next_back(),
        ]
        .into_iter()
        .flatten()
        .copied()
        .collect();
        let deleted: Vec<String> = gone.iter().map(i64::to_string).collect();
        write(
            file.path(),
            &format!(
                "DELETE FROM orders WHERE id IN ({}); INSERT INTO orders VALUES {}",
                deleted.join(", "),
                values_list([before, soon, later].into_iter())
            ),
        );
        for id in &gone {
            present.remove(id);
            if *id > last {
                removed_ahead.insert(*id);
            }
        }
        present.extend([before, soon, later]);
        added_before.insert(before);
        added_after.extend([soon, later]);
        cursor = Some(next);
    }
    assert!(pages > 10, "{pages} pages");
    assert!(
        seen.windows(2).all(|pair| pair[0] < pair[1]),
        "every page continues strictly after the one before"
    );
    let seen: BTreeSet<i64> = seen.into_iter().collect();
    for id in first_rows.iter().chain(&added_after) {
        assert!(
            removed_ahead.contains(id) || seen.contains(id),
            "{id} stayed and was skipped"
        );
    }
    for id in added_before.iter().chain(&removed_ahead) {
        assert!(!seen.contains(id), "{id} was added behind the cursor or removed ahead of it");
    }

    let everything = page_of(
        &succeeded(&eggdb(&["row", "list", &path, "orders", "--json"])),
        usize::MAX,
    );
    assert!(everything.next_cursor.is_none(), "without --limit nothing is cut");
    assert_eq!(everything.records.len(), present.len());
}

fn free_id(present: &BTreeSet<i64>, from: i64, step: i64) -> i64 {
    let mut id = from;
    while present.contains(&id) {
        id += step;
    }
    id
}

#[test]
fn text_pages_print_the_next_page_s_command_and_it_runs() {
    let file = TempPath::new("paging-text");
    let path = file.path().to_str().unwrap().to_owned();
    write(
        file.path(),
        &format!(
            "CREATE TABLE orders (id integer PRIMARY KEY, note text); INSERT INTO orders VALUES {}; \
             CREATE TABLE bag (word text); INSERT INTO bag VALUES ('y'), ('x'), ('z')",
            values_list(1..=3)
        ),
    );
    let first = eggdb(&["row", "list", &path, "orders", "--limit", "2"]);
    let shown = succeeded(&first);
    assert!(shown.contains("note 1") && shown.contains("note 2") && !shown.contains("note 3"));
    assert!(shown.ends_with("(2 rows)\n\n"), "{shown}");
    let hint = text(&first.stderr);
    let prefix = format!("more rows follow; next page: eggdb row list {path} orders --limit 2 --after r1.");
    assert!(hint.starts_with(&prefix), "{hint}");
    let words: Vec<&str> = hint
        .trim_end()
        .strip_prefix("more rows follow; next page: eggdb ")
        .unwrap()
        .split(' ')
        .collect();
    let second = eggdb(&words);
    let rest = succeeded(&second);
    assert!(rest.contains("note 3") && rest.ends_with("(1 row)\n\n"), "{rest}");
    assert!(second.stderr.is_empty(), "{}", text(&second.stderr));

    let bag = page_of(&succeeded(&eggdb(&["row", "list", &path, "bag", "--json"])), usize::MAX);
    let words: Vec<String> = bag
        .records
        .iter()
        .map(|record| scalar(member(record, "word")))
        .collect();
    assert_eq!(words, ["y", "x", "z"], "a table without a primary key pages in the order its rows were added");
    let bag_page = page_of(
        &succeeded(&eggdb(&["row", "list", &path, "bag", "-n", "1", "--json"])),
        1,
    );
    let Some(after_y) = bag_page.next_cursor else {
        panic!("one of three rows is a cut page");
    };
    let rest = page_of(
        &succeeded(&eggdb(&["row", "list", &path, "bag", "--json", "--after", &after_y])),
        usize::MAX,
    );
    assert_eq!(rest.records.len(), 2);
}

#[test]
fn row_list_refuses_views_foreign_cursors_and_missing_tables() {
    let file = TempPath::new("paging-errors");
    let path = file.path().to_str().unwrap().to_owned();
    write(
        file.path(),
        &format!(
            "CREATE TABLE orders (id integer PRIMARY KEY, note text); INSERT INTO orders VALUES {}; \
             CREATE TABLE users (id integer PRIMARY KEY); INSERT INTO users VALUES (1), (2); \
             CREATE SCHEMA sales; CREATE VIEW sales.totals AS SELECT count(*) AS n FROM orders",
            values_list(1..=3)
        ),
    );
    let first = page_of(
        &succeeded(&eggdb(&["row", "list", &path, "orders", "-n", "2", "--json"])),
        2,
    );
    let Some(after_two) = first.next_cursor else {
        panic!("two of three rows is a cut page");
    };
    let last = page_of(
        &succeeded(&eggdb(&["row", "list", &path, "orders", "-n", "1", "--json", "--after", &after_two])),
        1,
    );
    assert_eq!(last.records.len(), 1);
    assert!(last.next_cursor.is_none(), "the last row ends the last page");
    let whole = page_of(
        &succeeded(&eggdb(&["row", "list", &path, "orders", "-n", "3", "--json"])),
        3,
    );
    assert!(whole.next_cursor.is_none(), "exactly a page of rows is not cut");

    let users = page_of(
        &succeeded(&eggdb(&["row", "list", &path, "users", "-n", "1", "--json"])),
        1,
    );
    let users_cursor = users.next_cursor.unwrap();
    let foreign = eggdb(&["row", "list", &path, "orders", "--after", &users_cursor]);
    assert_eq!(foreign.status.code(), Some(2));
    assert!(
        text(&foreign.stderr).starts_with(
            "eggdb: --after got a cursor of another table, not of public.orders. Page a table with the cursors its own pages print.\n"
        ),
        "{}",
        text(&foreign.stderr)
    );
    let malformed = eggdb(&["row", "list", &path, "orders", "--after", "42", "--json"]);
    assert_eq!(malformed.status.code(), Some(2));
    assert!(text(&malformed.stderr).contains("\"code\":\"invalid_arguments\",\"category\":\"usage\""));

    let view = eggdb(&["row", "list", &path, "sales.totals", "--json"]);
    assert_eq!(view.status.code(), Some(3));
    let error = text(&view.stderr);
    assert!(
        error.starts_with("{\"error\":{\"code\":\"not_a_table\",\"category\":\"refused\",\"message\":\"cannot list the rows of \\\"sales.totals\\\" in "),
        "{error}"
    );
    assert!(
        error.contains(&format!("\"fix_command\":\"eggdb table sample {path} sales.totals\"")),
        "{error}"
    );

    let missing = eggdb(&["row", "list", &path, "nowhere", "--json"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(text(&missing.stderr).contains("\"code\":\"table_not_found\""));
}

#[test]
fn table_list_pages_in_its_order_while_tables_are_added() {
    let file = TempPath::new("paging-tables");
    let path = file.path().to_str().unwrap().to_owned();
    let created: Vec<String> = (0..30)
        .map(|n| format!("CREATE TABLE m{n:02} (id integer PRIMARY KEY)"))
        .collect();
    write(
        file.path(),
        &format!("{}; CREATE VIEW v1 AS SELECT id FROM m00", created.join("; ")),
    );
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut round = 0;
    loop {
        let mut args = vec!["table", "list", path.as_str(), "--limit", "7", "--json"];
        if let Some(cursor) = &cursor {
            args.extend(["--after", cursor.as_str()]);
        }
        let page = page_of(&succeeded(&eggdb(&args)), 7);
        seen.extend(
            page.records
                .iter()
                .map(|record| format!("{}:{}", scalar(member(record, "kind")), scalar(member(record, "name")))),
        );
        let Some(next) = page.next_cursor else {
            break;
        };
        write(
            file.path(),
            &format!(
                "CREATE TABLE a{round:02} (id integer PRIMARY KEY); CREATE TABLE z{round:02} (id integer PRIMARY KEY)"
            ),
        );
        round += 1;
        cursor = Some(next);
    }
    let distinct: BTreeSet<&String> = seen.iter().collect();
    assert_eq!(distinct.len(), seen.len(), "no table is listed twice: {seen:?}");
    for n in 0..30 {
        assert!(seen.contains(&format!("table:m{n:02}")), "m{n:02}");
    }
    assert!(seen.iter().all(|item| !item.starts_with("table:a")), "{seen:?}");
    assert_eq!(
        seen.iter().filter(|item| item.starts_with("table:z")).count(),
        round,
        "every table added after the cursor is read"
    );
    assert_eq!(seen.last().map(String::as_str), Some("view:v1"));

    let text_page = eggdb(&["table", "list", &path, "--limit", "2"]);
    succeeded(&text_page);
    let hint = text(&text_page.stderr);
    assert!(
        hint.starts_with("2 tables, 0 views\nmore tables or views follow; next page: eggdb table list "),
        "{hint}"
    );
    assert!(hint.contains(" --limit 2 --after t1."), "{hint}");
}
