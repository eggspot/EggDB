use crate::ast::{BinaryOperator, Expr, ExprKind};
use crate::eval::{Scope, Source};
use crate::memory_store::MemoryStore;
use crate::plan::{self, OrderRequest};
use crate::store::{KeyValueRead, KeyValueStore};
use crate::tests::render;
use crate::types::Value;
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn last(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn row_lines(session: &mut Session<'_>, sql: &str) -> Vec<String> {
    let rendered = last(session, sql);
    let mut lines: Vec<String> = rendered.lines().map(str::to_owned).collect();
    lines.pop();
    if !lines.is_empty() {
        lines.remove(0);
    }
    lines
}

struct Seeded(u64);

impl Seeded {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }

    fn nullable(&mut self, bound: u64) -> String {
        match self.below(5) {
            0 => "NULL".to_owned(),
            _ => self.below(bound).to_string(),
        }
    }
}

fn random_table(session: &mut Session<'_>, random: &mut Seeded, rows: u64) {
    last(
        session,
        "CREATE TABLE r (id int PRIMARY KEY, g int, a int, b int)",
    );
    let mut id = 0;
    while id < rows {
        let batch: Vec<String> = (id..rows.min(id + 100))
            .map(|row| {
                let g = random.below(4);
                let a = random.nullable(12);
                let b = random.nullable(5);
                format!("({row}, {g}, {a}, {b})")
            })
            .collect();
        last(
            session,
            &format!("INSERT INTO r VALUES {}", batch.join(", ")),
        );
        id += 100;
    }
}

fn window(dialect: Dialect, order_by: &str, count: u64, skip: u64) -> String {
    match dialect {
        Dialect::SqlServer => {
            format!("{order_by} OFFSET {skip} ROWS FETCH NEXT {count} ROWS ONLY")
        }
        _ => format!("{order_by} LIMIT {count} OFFSET {skip}"),
    }
}

fn slice(rows: &[String], count: u64, skip: u64) -> Vec<String> {
    rows.iter()
        .skip(usize::try_from(skip).unwrap_or(usize::MAX))
        .take(usize::try_from(count).unwrap_or(usize::MAX))
        .cloned()
        .collect()
}

#[test]
fn a_limited_sort_returns_the_full_sorts_rows_with_ties_in_arrival_order() {
    let mut random = Seeded(0x70B0_0341_0000_0001);
    for dialect in [
        Dialect::PostgreSql,
        Dialect::MySql,
        Dialect::Sqlite,
        Dialect::SqlServer,
    ] {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        random_table(&mut session, &mut random, 400);
        for order_by in [
            "SELECT id, a, b FROM r ORDER BY a",
            "SELECT id, a, b FROM r ORDER BY a DESC",
            "SELECT id, a, b FROM r ORDER BY b DESC, a",
            "SELECT id, g FROM r ORDER BY g",
            "SELECT id, a FROM r ORDER BY a % 3, g DESC",
            "SELECT id, a FROM r WHERE g <> 1 ORDER BY b",
            "SELECT DISTINCT a, b FROM r ORDER BY b, a",
            "SELECT g, count(*) FROM r GROUP BY g ORDER BY count(*) DESC, g",
        ] {
            let full = row_lines(&mut session, order_by);
            for _ in 0..12 {
                let count = 1 + random.below(40);
                let skip = match random.below(3) {
                    0 => 0,
                    _ => random.below(60),
                };
                let limited = window(dialect, order_by, count, skip);
                assert_eq!(
                    row_lines(&mut session, &limited),
                    slice(&full, count, skip),
                    "{dialect:?}: {limited}"
                );
            }
        }
    }
}

#[test]
fn a_limited_sort_fails_as_the_full_sort_does() {
    for dialect in [Dialect::PostgreSql, Dialect::SqlServer] {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        let mut random = Seeded(0x70B0_0341_0000_0002);
        random_table(&mut session, &mut random, 50);
        let full = "SELECT id FROM r ORDER BY 10 / (id - 30)";
        let expected = run(&mut session, full).map_err(|error| error.to_string());
        let found =
            run(&mut session, &window(dialect, full, 3, 0)).map_err(|error| error.to_string());
        assert!(expected.is_err(), "{dialect:?}: {expected:?}");
        assert_eq!(found, expected, "{dialect:?}");
    }
}

#[test]
fn huge_limits_and_offsets_sort_every_row() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    let mut random = Seeded(0x70B0_0341_0000_0003);
    random_table(&mut session, &mut random, 120);
    let full = row_lines(&mut session, "SELECT id, a FROM r ORDER BY a, b");
    assert_eq!(
        row_lines(
            &mut session,
            "SELECT id, a FROM r ORDER BY a, b LIMIT 9223372036854775807"
        ),
        full
    );
    assert_eq!(
        row_lines(
            &mut session,
            "SELECT id, a FROM r ORDER BY a, b LIMIT 9223372036854775807 OFFSET 5"
        ),
        slice(&full, u64::MAX, 5)
    );
    assert!(
        row_lines(
            &mut session,
            "SELECT id FROM r ORDER BY a LIMIT 9223372036854775807 OFFSET 9223372036854775807"
        )
        .is_empty()
    );
    assert!(row_lines(&mut session, "SELECT id FROM r ORDER BY a LIMIT 0").is_empty());
}

#[test]
fn with_ties_and_percent_keep_every_tied_row() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    last(&mut session, "CREATE TABLE t (id int PRIMARY KEY, v int)");
    last(
        &mut session,
        "INSERT INTO t VALUES (1, 3), (2, 1), (3, 2), (4, 1), (5, 2), (6, NULL), (7, 2)",
    );
    assert_eq!(
        row_lines(&mut session, "SELECT TOP 3 WITH TIES id FROM t ORDER BY v"),
        ["6", "2", "4"]
    );
    assert_eq!(
        row_lines(&mut session, "SELECT TOP 4 WITH TIES id FROM t ORDER BY v"),
        ["6", "2", "4", "3", "5", "7"]
    );
    assert_eq!(
        row_lines(&mut session, "SELECT TOP 50 PERCENT id FROM t ORDER BY v"),
        ["6", "2", "4", "3"]
    );
}

fn create_indexes(session: &mut Session<'_>) {
    last(session, "CREATE INDEX r_a ON r (a)");
    last(session, "CREATE INDEX r_g_a ON r (g, a)");
    last(session, "CREATE INDEX r_b_a ON r (b, a)");
}

#[test]
fn a_nullable_index_read_in_two_ranges_returns_the_full_sorts_rows() {
    let mut random = Seeded(0x70B0_0341_0000_0004);
    for dialect in [
        Dialect::MySql,
        Dialect::Sqlite,
        Dialect::SqlServer,
        Dialect::PostgreSql,
    ] {
        let plain_store = MemoryStore::default();
        let indexed_store = MemoryStore::default();
        let mut plain = Session::with_dialect(&plain_store, dialect);
        let mut indexed = Session::with_dialect(&indexed_store, dialect);
        let seed = random.0;
        random_table(&mut plain, &mut random, 300);
        random_table(&mut indexed, &mut Seeded(seed), 300);
        create_indexes(&mut indexed);
        let mut queries = Vec::new();
        for _ in 0..10 {
            let g = random.below(4);
            let v = random.below(12);
            let count = 1 + random.below(30);
            let skip = random.below(20);
            for order_by in [
                "SELECT id, a FROM r ORDER BY a".to_owned(),
                "SELECT id, a FROM r ORDER BY a, id".to_owned(),
                "SELECT id, a FROM r ORDER BY a DESC, id DESC".to_owned(),
                "SELECT id, a FROM r ORDER BY a NULLS FIRST".to_owned(),
                "SELECT id, a FROM r ORDER BY a DESC NULLS LAST, id DESC".to_owned(),
                format!("SELECT id, a FROM r WHERE g = {g} ORDER BY a, id"),
                format!("SELECT id, a FROM r WHERE g = {g} ORDER BY g, a DESC, id DESC"),
                "SELECT id, a FROM r WHERE a IS NOT NULL ORDER BY a, id".to_owned(),
                format!("SELECT id, a FROM r WHERE a > {v} ORDER BY a DESC, id DESC"),
                format!("SELECT id, a FROM r WHERE a <> {v} ORDER BY a, id"),
                "SELECT id, a, b FROM r ORDER BY b, a, id".to_owned(),
                format!("SELECT id, a, b FROM r WHERE b = {g} ORDER BY a, id"),
                "SELECT id, a, b FROM r WHERE b IS NULL ORDER BY a DESC, id DESC".to_owned(),
            ] {
                let explicit_nulls = order_by.contains("NULLS");
                if explicit_nulls && matches!(dialect, Dialect::MySql | Dialect::SqlServer) {
                    continue;
                }
                queries.push(window(dialect, &order_by, count, skip));
            }
        }
        for query in &queries {
            let expected = run(&mut plain, query).map_err(|error| error.to_string());
            let found = run(&mut indexed, query).map_err(|error| error.to_string());
            assert_eq!(found, expected, "{dialect:?}: {query}");
        }
    }
}

fn events(session: &mut Session<'_>, rows: u64, null_every: u64) {
    last(
        session,
        "CREATE TABLE events (id int PRIMARY KEY, created_at int, kind int)",
    );
    last(
        session,
        "CREATE INDEX events_created_at ON events (created_at)",
    );
    last(
        session,
        "CREATE INDEX events_kind_created_at ON events (kind, created_at)",
    );
    let mut id = 0;
    while id < rows {
        let batch: Vec<String> = (id..rows.min(id + 200))
            .map(|row| {
                let created_at = if row % null_every == 0 {
                    "NULL".to_owned()
                } else {
                    ((row * 7919) % rows).to_string()
                };
                format!("({row}, {created_at}, {})", row % 3)
            })
            .collect();
        last(
            session,
            &format!("INSERT INTO events VALUES {}", batch.join(", ")),
        );
        id += 200;
    }
}

#[test]
fn explain_shows_no_sort_for_a_nullable_index_read_in_two_ranges() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    events(&mut session, 50, 10);
    for (query, index) in [
        (
            "SELECT id FROM events ORDER BY created_at DESC LIMIT 10",
            "Index scan on events using events_created_at (reverse)",
        ),
        (
            "SELECT id FROM events ORDER BY created_at LIMIT 10",
            "Index scan on events using events_created_at",
        ),
        (
            "SELECT id FROM events WHERE kind = 1 ORDER BY created_at DESC LIMIT 10",
            "Index lookup on events using events_kind_created_at (kind=1)",
        ),
    ] {
        let plan = last(&mut session, &format!("EXPLAIN FORMAT=TREE {query}"));
        assert!(!plan.contains("Sort"), "{query}: {plan}");
        assert!(plan.contains("-> Limit: 10 row(s)"), "{query}: {plan}");
        assert!(plan.contains(index), "{query}: {plan}");
    }
    let sorted = last(
        &mut session,
        "EXPLAIN FORMAT=TREE SELECT id FROM events ORDER BY kind, created_at LIMIT 10",
    );
    assert!(sorted.contains("Sort"), "{sorted}");

    let sqlite_store = MemoryStore::default();
    let mut sqlite = Session::with_dialect(&sqlite_store, Dialect::Sqlite);
    events(&mut sqlite, 50, 10);
    let plan = last(
        &mut sqlite,
        "EXPLAIN QUERY PLAN SELECT id FROM events ORDER BY created_at DESC LIMIT 10",
    );
    assert!(
        plan.contains("SCAN events USING INDEX events_created_at"),
        "{plan}"
    );
    assert!(!plan.contains("TEMP B-TREE"), "{plan}");

    let postgres_store = MemoryStore::default();
    let mut postgres = Session::new(&postgres_store);
    events(&mut postgres, 50, 10);
    assert_eq!(
        last(
            &mut postgres,
            "EXPLAIN SELECT id FROM events ORDER BY created_at NULLS FIRST LIMIT 3"
        ),
        "QUERY PLAN\nLimit\n  ->  Index Scan using events_created_at on events\nEXPLAIN"
    );
}

fn ids(values: impl Iterator<Item = u64>) -> Vec<String> {
    values.map(|id| id.to_string()).collect()
}

#[test]
fn a_nullable_index_read_stops_after_the_rows_it_returns() {
    for dialect in [Dialect::MySql, Dialect::Sqlite] {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        events(&mut session, 2_000, 10);
        let mut measured = |sql: &str| {
            let before = store.entries_read();
            let rows = row_lines(&mut session, sql);
            (rows, store.entries_read() - before)
        };
        let (rows, read) = measured("SELECT id FROM events ORDER BY created_at LIMIT 10");
        assert_eq!(rows, ids((0..10).map(|row| row * 10)), "{dialect:?}");
        assert!(
            read <= 60,
            "{dialect:?}: {read} entries read for NULLs first"
        );
        let (latest, read) = measured("SELECT id FROM events ORDER BY created_at DESC LIMIT 3");
        let (full, _) = measured("SELECT id FROM events ORDER BY created_at DESC");
        assert_eq!(latest, slice(&full, 3, 0), "{dialect:?}");
        assert!(
            read <= 30,
            "{dialect:?}: {read} entries read for the latest three"
        );
        let (_, read) = measured(
            "SELECT id FROM events WHERE created_at IS NOT NULL ORDER BY created_at LIMIT 10",
        );
        assert!(
            read <= 60,
            "{dialect:?}: {read} entries read past the NULLs"
        );
        let (_, read) = measured("SELECT id FROM events ORDER BY kind, created_at LIMIT 10");
        assert!(read >= 2_000, "{dialect:?}: {read} entries read for a sort");
    }
}

#[test]
fn a_short_null_range_is_followed_by_the_smallest_values() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    last(&mut session, "CREATE TABLE t (id int PRIMARY KEY, v int)");
    last(&mut session, "CREATE INDEX t_v ON t (v)");
    let values: Vec<String> = (0..500u64)
        .map(|id| match id {
            7 | 300 | 451 => format!("({id}, NULL)"),
            _ => format!("({id}, {})", 1_000 - id),
        })
        .collect();
    last(
        &mut session,
        &format!("INSERT INTO t VALUES {}", values.join(", ")),
    );
    let before = store.entries_read();
    assert_eq!(
        row_lines(&mut session, "SELECT TOP 6 id FROM t ORDER BY v"),
        ["7", "300", "451", "499", "498", "497"]
    );
    assert!(store.entries_read() - before <= 40);
    let before = store.entries_read();
    assert_eq!(
        row_lines(
            &mut session,
            "SELECT id FROM t ORDER BY v DESC OFFSET 495 ROWS FETCH NEXT 5 ROWS ONLY"
        ),
        ["498", "499", "451", "300", "7"]
    );
    assert!(store.entries_read() - before <= 1_100);
}

#[test]
fn a_range_read_whose_nulls_sort_at_the_other_end_is_left_unordered() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    events(&mut session, 50, 10);
    let Ok(read) = store.begin_read() else {
        panic!("the store opens a read");
    };
    let Ok(Some(bytes)) = read.get(&crate::encoding::table_key("events")) else {
        panic!("events is stored");
    };
    let Ok(table) = crate::encoding::TableDefinition::decode("events", &bytes) else {
        panic!("events decodes");
    };
    let Some(created_at) = table.column_index("created_at") else {
        panic!("events has created_at");
    };
    let expr = |kind| Expr { kind, position: 0 };
    let filter = expr(ExprKind::Binary(
        BinaryOperator::Greater,
        Box::new(expr(ExprKind::Column {
            qualifier: None,
            name: "created_at".to_owned(),
        })),
        Box::new(expr(ExprKind::Constant {
            value: Value::Integer(5),
            sql_type: None,
        })),
    ));
    let order = OrderRequest {
        columns: vec![(created_at, false)],
        limited: true,
        nulls_at_other_end: vec![created_at],
    };
    let source = Source {
        table: &table,
        label: "events",
        offset: 0,
    };
    let access = plan::plan(&table, Scope::One(source), Some(&filter), Some(&order));
    assert!(matches!(access.used, plan::Use::Range));
    assert!(!access.ordered);
}
