use crate::memory_store::MemoryStore;
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_, MemoryStore>, text: &str) -> Result<Vec<String>, SqlError> {
    let mut found = Vec::new();
    session.execute(text, &mut |result| {
        for row in result.rows {
            found.push(format!("{row:?}"));
        }
    })?;
    Ok(found)
}

fn rows(session: &mut Session<'_, MemoryStore>, text: &str) -> Vec<String> {
    run(session, text).unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

fn sorted(mut rows: Vec<String>) -> Vec<String> {
    rows.sort();
    rows
}

fn people(store: &MemoryStore, dialect: Dialect) {
    let mut session = Session::with_dialect(store, dialect);
    rows(
        &mut session,
        "CREATE TABLE people (id integer PRIMARY KEY, name text, score integer)",
    );
    for id in 0..60 {
        let score = if id % 7 == 0 {
            "NULL".to_owned()
        } else {
            (id % 9).to_string()
        };
        rows(
            &mut session,
            &format!("INSERT INTO people VALUES ({id}, 'person {id}', {score})"),
        );
    }
}

fn orders(store: &MemoryStore) {
    let mut session = Session::new(store);
    rows(
        &mut session,
        "CREATE TABLE orders (id integer PRIMARY KEY, person_id bigint, amount integer)",
    );
    for id in 0..150 {
        let person = if id % 11 == 0 {
            "NULL".to_owned()
        } else {
            ((id * 7) % 45).to_string()
        };
        let amount = if id % 13 == 0 {
            "NULL".to_owned()
        } else {
            (id % 9).to_string()
        };
        rows(
            &mut session,
            &format!("INSERT INTO orders VALUES ({id}, {person}, {amount})"),
        );
    }
}

#[test]
fn statements_that_differ_only_in_constants_give_their_own_results() {
    for dialect in [Dialect::PostgreSql, Dialect::MySql, Dialect::Sqlite] {
        let store = MemoryStore::default();
        people(&store, dialect);
        let mut cached = Session::with_dialect(&store, dialect);
        for id in 10..60 {
            for text in [
                format!("SELECT name, score FROM people WHERE id = {id}"),
                format!("SELECT id FROM people WHERE score > {} ORDER BY id", id % 9),
                format!("SELECT count(*) FROM people WHERE name <> 'person {id}'"),
                format!("SELECT id * {id}, 'x{id}' FROM people WHERE id BETWEEN {id} AND 59 ORDER BY 1 LIMIT 3"),
            ] {
                let fresh = rows(&mut Session::with_dialect(&store, dialect), &text);
                assert_eq!(rows(&mut cached, &text), fresh, "{dialect:?}: {text}");
            }
        }
    }
}

#[test]
fn a_cached_statement_fails_where_a_parsed_one_does() {
    let store = MemoryStore::default();
    people(&store, Dialect::PostgreSql);
    let mut cached = Session::new(&store);
    rows(&mut cached, "SELECT name FROM people WHERE id = 8 / 2");
    let text = "SELECT name FROM people WHERE id = 8 / 0";
    let fresh = run(&mut Session::new(&store), text).expect_err("division by zero");
    let again = run(&mut cached, text).expect_err("division by zero");
    assert_eq!(again.state(), fresh.state());
    assert_eq!(again.position(), fresh.position());
    assert_eq!(again.to_string(), fresh.to_string());
}

#[test]
fn a_cached_statement_sees_new_settings_and_new_tables() {
    let store = MemoryStore::default();
    people(&store, Dialect::PostgreSql);
    let mut session = Session::new(&store);
    let zone = "SELECT current_setting('TimeZone')";
    rows(&mut session, "SET TimeZone = 'UTC'");
    assert!(rows(&mut session, zone)[0].contains("UTC"));
    rows(&mut session, zone);
    rows(&mut session, "SET TimeZone = 'Asia/Tokyo'");
    assert!(rows(&mut session, zone)[0].contains("Asia/Tokyo"));
    rows(&mut session, "SELECT name FROM people WHERE id = 11");
    rows(
        &mut session,
        "DROP TABLE people; CREATE TABLE people (id integer PRIMARY KEY, other text)",
    );
    let error = run(&mut session, "SELECT name FROM people WHERE id = 12").expect_err("no column");
    assert_eq!(error.state().code(), "42703");
}

#[test]
fn lookups_through_views_find_what_the_tables_hold() {
    let store = MemoryStore::default();
    people(&store, Dialect::PostgreSql);
    orders(&store);
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE VIEW named AS SELECT id, name AS label, score FROM people; \
         CREATE VIEW everyone AS SELECT * FROM people; \
         CREATE VIEW scored AS SELECT p.id, p.score, o.amount FROM people p JOIN orders o ON o.person_id = p.id WHERE p.score IS NOT NULL",
    );
    let pairs = [
        (
            "SELECT label FROM named WHERE id = 17",
            "SELECT name FROM people WHERE id = 17",
        ),
        (
            "SELECT name, score FROM everyone WHERE id = 23 AND score > 1",
            "SELECT name, score FROM people WHERE id = 23 AND score > 1",
        ),
        (
            "SELECT e.id FROM everyone e WHERE e.score IS NULL ORDER BY e.id",
            "SELECT id FROM people WHERE score IS NULL ORDER BY id",
        ),
        (
            "SELECT id, amount FROM scored WHERE id = 14 ORDER BY amount",
            "SELECT p.id, o.amount FROM people p JOIN orders o ON o.person_id = p.id WHERE p.score IS NOT NULL AND p.id = 14 ORDER BY o.amount",
        ),
        (
            "SELECT p.id, n.label FROM people p LEFT JOIN named n ON n.id = p.id + 100 WHERE n.label IS NULL AND p.id < 5 ORDER BY p.id",
            "SELECT id, NULL FROM people WHERE id < 5 ORDER BY id",
        ),
        (
            "SELECT count(*) FROM named n, people p WHERE n.id = p.id AND n.score = 3",
            "SELECT count(*) FROM people WHERE score = 3",
        ),
    ];
    for (through_view, on_tables) in pairs {
        assert_eq!(
            rows(&mut session, through_view),
            rows(&mut session, on_tables),
            "{through_view}"
        );
    }
}

#[test]
fn decorrelated_in_and_aggregate_subqueries_match_running_them_per_row() {
    let store = MemoryStore::default();
    people(&store, Dialect::PostgreSql);
    orders(&store);
    let mut session = Session::new(&store);
    let queries = [
        "SELECT id FROM people p WHERE score IN (SELECT amount FROM orders o WHERE o.person_id = p.id{}) ORDER BY id",
        "SELECT id FROM people p WHERE score NOT IN (SELECT amount FROM orders o WHERE o.person_id = p.id{}) ORDER BY id",
        "SELECT id, score IN (SELECT amount FROM orders o WHERE o.person_id = p.id{}) FROM people p ORDER BY id",
        "SELECT id, score = ANY (SELECT amount FROM orders o WHERE o.person_id = p.id AND o.amount > 2{}) FROM people p ORDER BY id",
        "SELECT id, score <> ALL (SELECT amount FROM orders o WHERE o.person_id = p.id{}) FROM people p ORDER BY id",
        "SELECT id, (SELECT count(*) FROM orders o WHERE o.person_id = p.id{}) FROM people p ORDER BY id",
        "SELECT id, (SELECT sum(amount) FROM orders o WHERE o.person_id = p.id AND o.amount < 7{}) FROM people p ORDER BY id",
        "SELECT id, (SELECT coalesce(max(amount), -1) * 2 FROM orders o WHERE p.id = o.person_id{}) FROM people p ORDER BY id",
        "SELECT id FROM people p WHERE (SELECT count(*) FROM orders o WHERE o.person_id = p.id{}) > 3 ORDER BY id",
        "SELECT id FROM people p WHERE EXISTS (SELECT 1 FROM orders o WHERE o.person_id = p.id{}) ORDER BY id",
    ];
    for query in queries {
        let decorrelated = query.replace("{}", "");
        let per_row = query.replace("{}", " OFFSET 0");
        assert_eq!(
            sorted(rows(&mut session, &decorrelated)),
            sorted(rows(&mut session, &per_row)),
            "{decorrelated}"
        );
    }
    assert_eq!(
        rows(
            &mut session,
            "SELECT count(*) FROM people p WHERE EXISTS (SELECT 1 FROM orders o WHERE o.person_id = p.id LIMIT 0)"
        ),
        rows(&mut session, "SELECT count(*) FROM people WHERE false")
    );
}

#[test]
fn in_lists_of_integers_answer_as_the_comparison_does() {
    let store = MemoryStore::default();
    people(&store, Dialect::PostgreSql);
    orders(&store);
    let mut session = Session::new(&store);
    for (query, expected) in [
        (
            "SELECT count(*) FROM orders WHERE person_id IN (SELECT id FROM people WHERE score > 4)",
            "SELECT count(*) FROM orders o JOIN people p ON p.id = o.person_id WHERE p.score > 4",
        ),
        (
            "SELECT count(*) FROM orders WHERE 2.0 IN (SELECT score FROM people)",
            "SELECT count(*) FROM orders",
        ),
        (
            "SELECT count(*) FROM orders WHERE amount NOT IN (SELECT score FROM people)",
            "SELECT count(*) FROM orders WHERE false",
        ),
    ] {
        assert_eq!(rows(&mut session, query), rows(&mut session, expected), "{query}");
    }
}
