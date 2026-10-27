use crate::memory_store::MemoryStore;
use crate::tests::{fuzz, render};
use crate::{Dialect, Session, SqlError};
use std::collections::BTreeMap;

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn last(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn sorted_rows(session: &mut Session<'_, MemoryStore>, sql: &str) -> Vec<String> {
    let output = last(session, sql);
    let mut lines: Vec<String> = output.lines().map(str::to_owned).collect();
    lines.pop();
    if !lines.is_empty() {
        lines.remove(0);
    }
    lines.sort();
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

    fn maybe(&mut self) -> Option<i64> {
        if self.below(5) == 0 {
            None
        } else {
            Some(self.below(6) as i64)
        }
    }
}

type Row = (i64, Option<i64>, Option<i64>);

fn shown(value: Option<i64>) -> String {
    value.map_or_else(|| "NULL".to_owned(), |value| value.to_string())
}

fn sql_value(value: Option<i64>) -> String {
    value.map_or_else(|| "NULL".to_owned(), |value| value.to_string())
}

fn load(session: &mut Session<'_, MemoryStore>, name: &str, rows: &[Row], indexed: bool) {
    last(
        session,
        &format!("CREATE TABLE {name} (id integer PRIMARY KEY, a integer, b integer)"),
    );
    if indexed {
        last(session, &format!("CREATE INDEX {name}_a ON {name} (a)"));
        last(session, &format!("CREATE INDEX {name}_b ON {name} (b)"));
    }
    if rows.is_empty() {
        return;
    }
    let values: Vec<String> = rows
        .iter()
        .map(|(id, a, b)| format!("({id}, {}, {})", sql_value(*a), sql_value(*b)))
        .collect();
    last(
        session,
        &format!("INSERT INTO {name} VALUES {}", values.join(", ")),
    );
}

fn random_rows(random: &mut Seeded) -> Vec<Row> {
    let count = random.below(14);
    (0..count as i64)
        .map(|id| (id, random.maybe(), random.maybe()))
        .collect()
}

fn equal_truth(left: Option<i64>, right: Option<i64>) -> Option<bool> {
    Some(left? == right?)
}

fn not_in(value: Option<i64>, list: &[Option<i64>]) -> Option<bool> {
    if list.is_empty() {
        return Some(true);
    }
    let mut result = Some(true);
    for item in list {
        match equal_truth(value, *item) {
            Some(true) => return Some(false),
            Some(false) => {}
            None => result = None,
        }
    }
    result
}

fn compared(value: Option<i64>, item: Option<i64>, operator: &str) -> Option<bool> {
    let (value, item) = (value?, item?);
    Some(match operator {
        "<" => value < item,
        ">" => value > item,
        "<=" => value <= item,
        ">=" => value >= item,
        "=" => value == item,
        _ => value != item,
    })
}

fn quantified(value: Option<i64>, list: &[Option<i64>], operator: &str, all: bool) -> Option<bool> {
    let mut result = Some(all);
    for item in list {
        let truth = compared(value, *item, operator);
        result = match (all, result, truth) {
            (true, Some(false), _) | (true, _, Some(false)) => Some(false),
            (true, Some(true), Some(true)) => Some(true),
            (false, Some(true), _) | (false, _, Some(true)) => Some(true),
            (false, Some(false), Some(false)) => Some(false),
            _ => None,
        };
    }
    result
}

fn multiset(values: &[Option<i64>]) -> BTreeMap<Option<i64>, usize> {
    let mut counts = BTreeMap::new();
    for value in values {
        *counts.entry(*value).or_insert(0) += 1;
    }
    counts
}

fn set_operation(left: &[Option<i64>], right: &[Option<i64>], operator: &str) -> Vec<String> {
    let (l, r) = (multiset(left), multiset(right));
    let mut out = Vec::new();
    let keys: std::collections::BTreeSet<Option<i64>> = l.keys().chain(r.keys()).copied().collect();
    for key in keys {
        let (m, n) = (
            l.get(&key).copied().unwrap_or(0),
            r.get(&key).copied().unwrap_or(0),
        );
        let copies = match operator {
            "UNION ALL" => m + n,
            "UNION" => usize::from(m + n > 0),
            "INTERSECT ALL" => m.min(n),
            "INTERSECT" => usize::from(m > 0 && n > 0),
            "EXCEPT ALL" => m.saturating_sub(n),
            _ => usize::from(m > 0 && n == 0),
        };
        for _ in 0..copies {
            out.push(shown(key));
        }
    }
    out.sort();
    out
}

fn reference(r: &[Row], s: &[Row]) -> BTreeMap<String, Vec<String>> {
    let s_a: Vec<Option<i64>> = s.iter().map(|row| row.1).collect();
    let r_a: Vec<Option<i64>> = r.iter().map(|row| row.1).collect();
    let mut expected = BTreeMap::new();
    let ids = |keep: &dyn Fn(&Row) -> bool| -> Vec<String> {
        let mut found: Vec<String> = r
            .iter()
            .filter(|row| keep(row))
            .map(|row| row.0.to_string())
            .collect();
        found.sort();
        found
    };
    expected.insert(
        "exists".to_owned(),
        ids(&|row| {
            s.iter()
                .any(|inner| equal_truth(inner.1, row.1) == Some(true))
        }),
    );
    expected.insert(
        "not exists".to_owned(),
        ids(&|row| {
            !s.iter()
                .any(|inner| equal_truth(inner.1, row.1) == Some(true))
        }),
    );
    expected.insert(
        "not in".to_owned(),
        ids(&|row| not_in(row.1, &s_a) == Some(true)),
    );
    expected.insert(
        "correlated not in".to_owned(),
        ids(&|row| {
            let list: Vec<Option<i64>> = s
                .iter()
                .filter(|inner| equal_truth(inner.2, row.2) == Some(true))
                .map(|inner| inner.1)
                .collect();
            not_in(row.1, &list) == Some(true)
        }),
    );
    for (name, operator, all) in [
        ("< any", "<", false),
        ("> all", ">", true),
        ("<> all", "<>", true),
        ("= any", "=", false),
    ] {
        expected.insert(
            name.to_owned(),
            ids(&|row| quantified(row.1, &s_a, operator, all) == Some(true)),
        );
    }
    let mut counts: Vec<String> = r
        .iter()
        .map(|row| {
            let count = s
                .iter()
                .filter(|inner| equal_truth(inner.1, row.1) == Some(true))
                .count();
            format!("{}|{count}", row.0)
        })
        .collect();
    counts.sort();
    expected.insert("scalar count".to_owned(), counts);
    for operator in [
        "UNION",
        "UNION ALL",
        "INTERSECT",
        "INTERSECT ALL",
        "EXCEPT",
        "EXCEPT ALL",
    ] {
        expected.insert(operator.to_owned(), set_operation(&r_a, &s_a, operator));
    }
    let mut full = Vec::new();
    let mut s_matched = vec![false; s.len()];
    for row in r {
        let mut matched = false;
        for (index, inner) in s.iter().enumerate() {
            if equal_truth(row.1, inner.1) == Some(true) {
                matched = true;
                s_matched[index] = true;
                full.push(format!("{}|{}", row.0, inner.0));
            }
        }
        if !matched {
            full.push(format!("{}|NULL", row.0));
        }
    }
    for (index, inner) in s.iter().enumerate() {
        if !s_matched[index] {
            full.push(format!("NULL|{}", inner.0));
        }
    }
    full.sort();
    expected.insert("full join".to_owned(), full);
    expected
}

fn queries(r: &str, s: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "exists",
            format!("SELECT id FROM {r} WHERE EXISTS (SELECT 1 FROM {s} WHERE {s}.a = {r}.a)"),
        ),
        (
            "exists",
            format!("SELECT id FROM {r} WHERE a IN (SELECT a FROM {s})"),
        ),
        (
            "exists",
            format!("SELECT DISTINCT {r}.id FROM {r} JOIN {s} ON {s}.a = {r}.a"),
        ),
        (
            "exists",
            format!("SELECT id FROM {r} WHERE a = ANY (SELECT a FROM {s})"),
        ),
        (
            "not exists",
            format!("SELECT id FROM {r} x WHERE NOT EXISTS (SELECT 1 FROM {s} y WHERE y.a = x.a)"),
        ),
        (
            "not exists",
            format!(
                "SELECT {r}.id FROM {r} LEFT JOIN (SELECT DISTINCT a FROM {s}) m ON m.a = {r}.a WHERE m.a IS NULL"
            ),
        ),
        (
            "not in",
            format!("SELECT id FROM {r} WHERE a NOT IN (SELECT a FROM {s})"),
        ),
        (
            "not in",
            format!("SELECT id FROM {r} WHERE NOT (a = ANY (SELECT a FROM {s}))"),
        ),
        (
            "correlated not in",
            format!("SELECT id FROM {r} WHERE a NOT IN (SELECT a FROM {s} WHERE {s}.b = {r}.b)"),
        ),
        (
            "< any",
            format!("SELECT id FROM {r} WHERE a < ANY (SELECT a FROM {s})"),
        ),
        (
            "> all",
            format!("SELECT id FROM {r} WHERE a > ALL (SELECT a FROM {s})"),
        ),
        (
            "<> all",
            format!("SELECT id FROM {r} WHERE a <> ALL (SELECT a FROM {s})"),
        ),
        (
            "= any",
            format!("SELECT id FROM {r} WHERE a = SOME (SELECT a FROM {s})"),
        ),
        (
            "scalar count",
            format!("SELECT id, (SELECT count(*) FROM {s} WHERE {s}.a = {r}.a) FROM {r}"),
        ),
        (
            "scalar count",
            format!(
                "SELECT {r}.id, count({s}.id) FROM {r} LEFT JOIN {s} ON {s}.a = {r}.a GROUP BY {r}.id"
            ),
        ),
        (
            "UNION",
            format!("SELECT a FROM {r} UNION SELECT a FROM {s}"),
        ),
        (
            "UNION",
            format!("SELECT DISTINCT a FROM (SELECT a FROM {r} UNION ALL SELECT a FROM {s}) t"),
        ),
        (
            "UNION",
            format!(
                "WITH u AS (SELECT a FROM {r} UNION ALL SELECT a FROM {s}) SELECT a FROM u GROUP BY a"
            ),
        ),
        (
            "UNION ALL",
            format!("SELECT a FROM {r} UNION ALL SELECT a FROM {s}"),
        ),
        (
            "INTERSECT",
            format!("SELECT a FROM {r} INTERSECT SELECT a FROM {s}"),
        ),
        (
            "INTERSECT ALL",
            format!("SELECT a FROM {r} INTERSECT ALL SELECT a FROM {s}"),
        ),
        (
            "EXCEPT",
            format!("SELECT a FROM {r} EXCEPT SELECT a FROM {s}"),
        ),
        (
            "EXCEPT ALL",
            format!("SELECT a FROM {r} EXCEPT ALL SELECT a FROM {s}"),
        ),
        (
            "full join",
            format!("SELECT {r}.id, {s}.id FROM {r} FULL JOIN {s} ON {r}.a = {s}.a"),
        ),
        (
            "full join",
            format!(
                "SELECT {r}.id, {s}.id FROM {s} RIGHT JOIN {r} ON {r}.a = {s}.a UNION ALL SELECT NULL, {s}.id FROM {s} WHERE NOT EXISTS (SELECT 1 FROM {r} WHERE {r}.a = {s}.a)"
            ),
        ),
    ]
}

#[test]
fn subquery_forms_agree_with_joins_and_a_reference_with_and_without_indexes() {
    let mut random = Seeded(0x9E37_79B9_7F4A_7C15);
    for round in 0..24 {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        let r = random_rows(&mut random);
        let s = random_rows(&mut random);
        load(&mut session, "r", &r, false);
        load(&mut session, "s", &s, false);
        load(&mut session, "ri", &r, true);
        load(&mut session, "si", &s, true);
        let expected = reference(&r, &s);
        for ((name, sql), (_, indexed_sql)) in queries("r", "s").iter().zip(&queries("ri", "si")) {
            let want = &expected[*name];
            let found = sorted_rows(&mut session, sql);
            assert_eq!(&found, want, "round {round}: {sql}\nr={r:?}\ns={s:?}");
            let found = sorted_rows(&mut session, indexed_sql);
            assert_eq!(
                &found, want,
                "round {round}: {indexed_sql}\nr={r:?}\ns={s:?}"
            );
        }
    }
}

#[test]
fn nested_subqueries_are_bounded_and_never_overflow_the_stack() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "CREATE TABLE t (id integer PRIMARY KEY)");
    last(&mut session, "INSERT INTO t VALUES (1), (2)");
    let nested = |depth: usize| {
        let mut sql = "SELECT id FROM t".to_owned();
        for _ in 0..depth {
            sql = format!("SELECT id FROM t WHERE id IN ({sql})");
        }
        sql
    };
    assert_eq!(last(&mut session, &nested(20)), "id\n1\n2\nSELECT 2");
    let error = run(&mut session, &nested(40)).unwrap_err();
    assert_eq!(error.state().code(), "54001", "{error}");
    let mut derived = "SELECT id FROM t".to_owned();
    for level in 0..31 {
        derived = format!("SELECT id FROM ({derived}) d{level}");
    }
    assert_eq!(last(&mut session, &derived), "id\n1\n2\nSELECT 2");
    let error = run(
        &mut session,
        &format!("SELECT id FROM (SELECT id FROM ({derived}) d31) d32"),
    )
    .unwrap_err();
    assert_eq!(error.state().code(), "54001", "{error}");
    let mut correlated = "SELECT 1 FROM t z WHERE z.id = t0.id".to_owned();
    for level in (0..15).rev() {
        correlated = format!(
            "SELECT id FROM t t{level} WHERE EXISTS ({})",
            correlated.replace("t0.id", &format!("t{level}.id"))
        );
    }
    let _ = run(&mut session, &correlated);
    let unions = vec!["SELECT 1"; 2000].join(" UNION ALL ");
    let error = run(&mut session, &unions).unwrap_err();
    assert_eq!(error.state().code(), "54001", "{error}");
    let unions = vec!["SELECT id FROM t"; 500].join(" UNION ALL ");
    assert!(last(&mut session, &unions).ends_with("SELECT 1000"));
}

#[test]
fn a_huge_in_list_and_a_huge_subquery_stay_linear() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "CREATE TABLE t (id integer PRIMARY KEY)");
    let values: Vec<String> = (0..20_000).map(|id| format!("({id})")).collect();
    last(
        &mut session,
        &format!("INSERT INTO t VALUES {}", values.join(", ")),
    );
    let list: Vec<String> = (0..50_000).map(|id| (id * 2).to_string()).collect();
    let started = std::time::Instant::now();
    assert_eq!(
        last(
            &mut session,
            &format!("SELECT count(*) FROM t WHERE id IN ({})", list.join(", "))
        ),
        "count\n10000\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM t WHERE id IN (SELECT id * 2 FROM t)"
        ),
        "count\n10000\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM t WHERE id NOT IN (SELECT id + 1 FROM t)"
        ),
        "count\n1\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM (SELECT id FROM t UNION SELECT id + 10000 FROM t) u"
        ),
        "count\n30000\nSELECT 1"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn correlated_subqueries_run_once_per_distinct_outer_value() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE big (id integer PRIMARY KEY, k integer); CREATE INDEX big_k ON big (k)",
    );
    let values: Vec<String> = (0..5_000)
        .map(|id| format!("({id}, {})", id % 10))
        .collect();
    last(
        &mut session,
        &format!("INSERT INTO big VALUES {}", values.join(", ")),
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM big o WHERE o.id = (SELECT max(i.id) FROM big i WHERE i.k = o.k)"
        ),
        "count\n10\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM big o WHERE EXISTS (SELECT 1 FROM big i WHERE i.k = o.id AND i.id > 4990)"
        ),
        "count\n9\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM big o WHERE o.id < 100 AND NOT EXISTS (SELECT 1 FROM big i WHERE o.id::float8 / 2 = i.k)"
        ),
        "count\n90\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM big o WHERE EXISTS (SELECT 1 FROM big i WHERE i.k = o.id::numeric / 2)"
        ),
        "count\n10\nSELECT 1"
    );
}

#[test]
fn hostile_recursion_and_cross_products_end_with_an_error() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    let error = run(
        &mut session,
        "WITH RECURSIVE t (n) AS (SELECT 1 UNION ALL SELECT n FROM t) SELECT count(*) FROM t",
    )
    .unwrap_err();
    assert_eq!(error.vendor_code(), Some(3636), "{error}");
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let error = run(
        &mut session,
        "WITH RECURSIVE t(s) AS (SELECT 'x' UNION ALL SELECT s || s FROM t) SELECT count(*) FROM t",
    )
    .unwrap_err();
    assert!(
        ["53200", "54000", "54001"].contains(&error.state().code()),
        "{error}"
    );
}

#[test]
fn set_operations_and_subqueries_report_their_column_types() {
    use crate::SqlType;
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE t (i integer, b bigint, n numeric(6,2), d double precision, s text, v varchar(5))",
    );
    let description = session
        .describe("SELECT i, n, s FROM t UNION SELECT b, d, v FROM t", &[])
        .unwrap();
    let types: Vec<Option<SqlType>> = description
        .columns
        .unwrap()
        .iter()
        .map(|column| column.sql_type)
        .collect();
    assert_eq!(
        types,
        [
            Some(SqlType::BigInt),
            Some(SqlType::Double),
            Some(SqlType::Text)
        ]
    );
    let description = session
        .describe(
            "SELECT (SELECT max(n) FROM t), EXISTS (SELECT 1 FROM t), i IN (SELECT b FROM t), x.v FROM t, (SELECT v FROM t) x",
            &[],
        )
        .unwrap();
    let types: Vec<Option<SqlType>> = description
        .columns
        .unwrap()
        .iter()
        .map(|column| column.sql_type)
        .collect();
    assert_eq!(
        types,
        [
            Some(SqlType::Numeric(None)),
            Some(SqlType::Boolean),
            Some(SqlType::Boolean),
            Some(SqlType::Varchar(Some(5)))
        ]
    );
}

#[test]
fn dialect_spellings_stay_in_their_dialects() {
    let store = MemoryStore::default();
    for (dialect, sql, works) in [
        (Dialect::PostgreSql, "SELECT 1 MINUS SELECT 2", false),
        (Dialect::MySql, "SELECT 1 MINUS SELECT 2", false),
        (Dialect::Sqlite, "SELECT 1 MINUS SELECT 2", false),
        (
            Dialect::Oracle,
            "SELECT 1 FROM dual MINUS SELECT 2 FROM dual",
            true,
        ),
        (
            Dialect::MySql,
            "SELECT 1 FROM t FULL JOIN t u ON true",
            false,
        ),
        (Dialect::Sqlite, "SELECT 1 = ANY (SELECT 1)", false),
        (Dialect::Sqlite, "SELECT 1 INTERSECT ALL SELECT 1", false),
        (Dialect::Sqlite, "(SELECT 1) UNION SELECT 1", false),
        (
            Dialect::Oracle,
            "WITH RECURSIVE t AS (SELECT 1 FROM dual) SELECT * FROM t",
            false,
        ),
        (
            Dialect::Oracle,
            "SELECT * FROM (SELECT 1 FROM dual) x (a)",
            false,
        ),
        (Dialect::MySql, "SELECT * FROM (SELECT 1) AS x", true),
        (Dialect::MySql, "SELECT mysql_greatest(1, 2)", false),
        (Dialect::PostgreSql, "SELECT mysql_greatest(1, 2)", false),
        (Dialect::PostgreSql, "SELECT greatest(1, 2)", true),
        (Dialect::Sqlite, "SELECT greatest(1, 2)", false),
        (
            Dialect::PostgreSql,
            "WITH t AS MATERIALIZED (SELECT 1) SELECT * FROM t",
            true,
        ),
        (
            Dialect::MySql,
            "WITH t AS MATERIALIZED (SELECT 1) SELECT * FROM t",
            false,
        ),
    ] {
        let mut session = Session::with_dialect(&store, dialect);
        let _ = run(&mut session, "CREATE TABLE t (x integer)");
        let outcome = run(&mut session, sql);
        assert_eq!(outcome.is_ok(), works, "{dialect:?}: {sql}: {outcome:?}");
    }
}

#[test]
fn hostile_subquery_and_set_operation_statements_never_panic_in_any_dialect() {
    let shared = [
        "SELECT",
        "FROM t",
        "FROM t AS x",
        "WHERE",
        "(",
        ")",
        ",",
        "x",
        "t.x",
        "u.x",
        "1",
        "NULL",
        "'a'",
        "IN",
        "NOT IN",
        "EXISTS",
        "NOT EXISTS",
        "ANY",
        "SOME",
        "ALL",
        "=",
        "<",
        "UNION",
        "UNION ALL",
        "INTERSECT",
        "INTERSECT ALL",
        "EXCEPT",
        "EXCEPT ALL",
        "MINUS",
        "WITH",
        "WITH RECURSIVE",
        "r(n) AS",
        "AS",
        "JOIN",
        "RIGHT JOIN",
        "FULL JOIN",
        "FULL OUTER JOIN",
        "LEFT JOIN",
        "ON",
        "USING (x)",
        "LATERAL",
        "ORDER BY",
        "LIMIT",
        "OFFSET",
        "GROUP BY",
        "HAVING",
        "count(*)",
        "max(x)",
        "CASE WHEN",
        "THEN",
        "ELSE",
        "END",
        "coalesce(",
        "nullif(",
        "greatest(",
        "least(",
        "(SELECT x FROM t)",
        "(SELECT 1 UNION SELECT 2)",
        "(SELECT x FROM t u WHERE u.x = t.x)",
        "FROM (SELECT x FROM t) s",
        "FROM r",
        "SELECT n + 1 FROM r",
        "VALUES",
        "FROM dual",
        "FETCH FIRST 1 ROWS ONLY",
        ";",
    ];
    for dialect in [
        Dialect::PostgreSql,
        Dialect::MySql,
        Dialect::Sqlite,
        Dialect::Oracle,
    ] {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        let _ = run(&mut session, "CREATE TABLE t (x integer)");
        let _ = run(&mut session, "INSERT INTO t VALUES (1)");
        let _ = run(&mut session, "INSERT INTO t VALUES (NULL)");
        if dialect == Dialect::Oracle {
            let _ = run(&mut session, "COMMIT");
        }
        fuzz(&mut session, &shared);
    }
}
