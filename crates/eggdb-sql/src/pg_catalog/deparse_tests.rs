use crate::memory_store::MemoryStore;
use crate::{Session, SqlError, Value};

const COLUMNS: &str = "i integer, s smallint, b bigint, n numeric(10,2), r real, \
    d double precision, t text, v varchar(10), c char(3), dt date, ts timestamp, tm time, \
    iv interval, u uuid, f boolean, g boolean";

const CHECKS: [(&str, &str, &str, &str); 79] = [
    ("c01", "i > 0", "(i > 0)", "i > 0"),
    (
        "c02",
        "s >= -3",
        "(s >= '-3'::integer)",
        "s >= '-3'::integer",
    ),
    ("c03", "b <> 5", "(b <> 5)", "b <> 5"),
    ("c04", "n >= 0", "(n >= (0)::numeric)", "n >= 0::numeric"),
    ("c05", "n < 2.5", "(n < 2.5)", "n < 2.5"),
    (
        "c06",
        "r > 0",
        "(r > (0)::double precision)",
        "r > 0::double precision",
    ),
    (
        "c07",
        "d > 1.5",
        "(d > (1.5)::double precision)",
        "d > 1.5::double precision",
    ),
    ("c08", "t <> ''", "(t <> ''::text)", "t <> ''::text"),
    (
        "c09",
        "v = 'x'",
        "((v)::text = 'x'::text)",
        "v::text = 'x'::text",
    ),
    ("c10", "c = 'ab'", "(c = 'ab'::bpchar)", "c = 'ab'::bpchar"),
    (
        "c11",
        "dt > '2020-01-01'",
        "(dt > '2020-01-01'::date)",
        "dt > '2020-01-01'::date",
    ),
    (
        "c12",
        "ts < '2030-01-01 00:00'",
        "(ts < '2030-01-01 00:00:00'::timestamp without time zone)",
        "ts < '2030-01-01 00:00:00'::timestamp without time zone",
    ),
    (
        "c14",
        "tm <> '12:00'",
        "(tm <> '12:00:00'::time without time zone)",
        "tm <> '12:00:00'::time without time zone",
    ),
    (
        "c15",
        "iv > '1 day'",
        "(iv > '1 day'::interval)",
        "iv > '1 day'::interval",
    ),
    (
        "c16",
        "u <> '00000000-0000-0000-0000-000000000000'",
        "(u <> '00000000-0000-0000-0000-000000000000'::uuid)",
        "u <> '00000000-0000-0000-0000-000000000000'::uuid",
    ),
    ("c17", "f", "f", "f"),
    ("c18", "NOT f", "(NOT f)", "NOT f"),
    (
        "c19",
        "i IN (1, 2, 3)",
        "(i = ANY (ARRAY[1, 2, 3]))",
        "i = ANY (ARRAY[1, 2, 3])",
    ),
    (
        "c20",
        "i NOT IN (4, 5)",
        "(i <> ALL (ARRAY[4, 5]))",
        "i <> ALL (ARRAY[4, 5])",
    ),
    (
        "c21",
        "t IN ('a', 'b')",
        "(t = ANY (ARRAY['a'::text, 'b'::text]))",
        "t = ANY (ARRAY['a'::text, 'b'::text])",
    ),
    (
        "c22",
        "v IN ('a', 'b')",
        "((v)::text = ANY ((ARRAY['a'::character varying, 'b'::character varying])::text[]))",
        "v::text = ANY (ARRAY['a'::character varying, 'b'::character varying]::text[])",
    ),
    (
        "c23",
        "c IN ('ab', 'cd')",
        "(c = ANY (ARRAY['ab'::bpchar, 'cd'::bpchar]))",
        "c = ANY (ARRAY['ab'::bpchar, 'cd'::bpchar])",
    ),
    (
        "c25",
        "i BETWEEN 1 AND 10",
        "((i >= 1) AND (i <= 10))",
        "i >= 1 AND i <= 10",
    ),
    (
        "c26",
        "(i + s) * 2 > 0",
        "(((i + s) * 2) > 0)",
        "((i + s) * 2) > 0",
    ),
    (
        "c27",
        "i > 0 AND s > 0 OR b > 0",
        "(((i > 0) AND (s > 0)) OR (b > 0))",
        "i > 0 AND s > 0 OR b > 0",
    ),
    ("c28", "t IS NOT NULL", "(t IS NOT NULL)", "t IS NOT NULL"),
    (
        "c29",
        "length(t) < 10",
        "(length(t) < 10)",
        "length(t) < 10",
    ),
    (
        "c30",
        "n IN (1, 2.5)",
        "(n = ANY (ARRAY[(1)::numeric, 2.5]))",
        "n = ANY (ARRAY[1::numeric, 2.5])",
    ),
    ("c31", "i > -3", "(i > '-3'::integer)", "i > '-3'::integer"),
    ("c32", "lower(t) = t", "(lower(t) = t)", "lower(t) = t"),
    ("c33", "i % 2 = 0", "((i % 2) = 0)", "(i % 2) = 0"),
    (
        "c34",
        "i >= s AND i <= b",
        "((i >= s) AND (i <= b))",
        "i >= s AND i <= b",
    ),
    (
        "c35",
        "n * 2 >= i",
        "((n * (2)::numeric) >= (i)::numeric)",
        "(n * 2::numeric) >= i::numeric",
    ),
    ("c36", "r < d", "(r < d)", "r < d"),
    ("c37", "v <> t", "((v)::text <> t)", "v::text <> t"),
    (
        "c38",
        "t NOT IN ('z')",
        "(t <> 'z'::text)",
        "t <> 'z'::text",
    ),
    (
        "c39",
        "i > 0 AND (s > 0 OR b > 0)",
        "((i > 0) AND ((s > 0) OR (b > 0)))",
        "i > 0 AND (s > 0 OR b > 0)",
    ),
    (
        "c40",
        "NOT (i > 0 AND s > 0)",
        "(NOT ((i > 0) AND (s > 0)))",
        "NOT (i > 0 AND s > 0)",
    ),
    (
        "c41",
        "i IS NULL OR i > 0",
        "((i IS NULL) OR (i > 0))",
        "i IS NULL OR i > 0",
    ),
    ("c42", "-i < 0", "((- i) < 0)", "(- i) < 0"),
    (
        "c43",
        "i * -1 < 5",
        "((i * '-1'::integer) < 5)",
        "(i * '-1'::integer) < 5",
    ),
    (
        "c44",
        "b > 3000000000",
        "(b > '3000000000'::bigint)",
        "b > '3000000000'::bigint",
    ),
    (
        "c45",
        "t || v <> 'ab'",
        "((t || (v)::text) <> 'ab'::text)",
        "(t || v::text) <> 'ab'::text",
    ),
    (
        "c46",
        "v > 'a'",
        "((v)::text > 'a'::text)",
        "v::text > 'a'::text",
    ),
    ("c47", "c > 'a'", "(c > 'a'::bpchar)", "c > 'a'::bpchar"),
    (
        "c48",
        "n > -1.5",
        "(n > '-1.5'::numeric)",
        "n > '-1.5'::numeric",
    ),
    (
        "c49",
        "d IN (1, 2)",
        "(d = ANY (ARRAY[(1)::double precision, (2)::double precision]))",
        "d = ANY (ARRAY[1::double precision, 2::double precision])",
    ),
    ("c50", "f = true", "(f = true)", "f = true"),
    (
        "d01",
        "i + s + 2 > 0",
        "(((i + s) + 2) > 0)",
        "(i + s + 2) > 0",
    ),
    (
        "d02",
        "i - (s - 2) > 0",
        "((i - (s - 2)) > 0)",
        "(i - (s - 2)) > 0",
    ),
    (
        "d03",
        "i * s + 2 > 0",
        "(((i * s) + 2) > 0)",
        "(i * s + 2) > 0",
    ),
    (
        "d04",
        "i + s * 2 > 0",
        "((i + (s * 2)) > 0)",
        "(i + s * 2) > 0",
    ),
    (
        "d05",
        "(i + 1)::text <> t",
        "(((i + 1))::text <> t)",
        "((i + 1)::text) <> t",
    ),
    ("d06", "f AND g", "(f AND g)", "f AND g"),
    (
        "d07",
        "f OR g AND i > 0",
        "(f OR (g AND (i > 0)))",
        "f OR g AND i > 0",
    ),
    ("d08", "f = g", "(f = g)", "f = g"),
    (
        "d09",
        "coalesce(i, 0) > 0",
        "(COALESCE(i, 0) > 0)",
        "COALESCE(i, 0) > 0",
    ),
    (
        "d11",
        "i IS NOT NULL AND s IS NULL",
        "((i IS NOT NULL) AND (s IS NULL))",
        "i IS NOT NULL AND s IS NULL",
    ),
    (
        "d12",
        "upper(t) <> lower(v)",
        "(upper(t) <> lower((v)::text))",
        "upper(t) <> lower(v::text)",
    ),
    ("d13", "i / 2 > 1", "((i / 2) > 1)", "(i / 2) > 1"),
    (
        "d14",
        "t <> 'it''s'",
        "(t <> 'it''s'::text)",
        "t <> 'it''s'::text",
    ),
    (
        "d15",
        "i NOT BETWEEN 1 AND 2",
        "((i < 1) OR (i > 2))",
        "i < 1 OR i > 2",
    ),
    (
        "d18",
        "s IN (1, 2)",
        "(s = ANY (ARRAY[1, 2]))",
        "s = ANY (ARRAY[1, 2])",
    ),
    (
        "d19",
        "b IN (1, 2)",
        "(b = ANY (ARRAY[(1)::bigint, (2)::bigint]))",
        "b = ANY (ARRAY[1::bigint, 2::bigint])",
    ),
    (
        "d20",
        "char_length(t) <= 5",
        "(char_length(t) <= 5)",
        "char_length(t) <= 5",
    ),
    ("d21", "i::bigint > b", "((i)::bigint > b)", "i::bigint > b"),
    (
        "d22",
        "n IN (1.5, 2.5)",
        "(n = ANY (ARRAY[1.5, 2.5]))",
        "n = ANY (ARRAY[1.5, 2.5])",
    ),
    (
        "d23",
        "NOT f OR NOT g",
        "((NOT f) OR (NOT g))",
        "NOT f OR NOT g",
    ),
    (
        "d24",
        "i > 0 OR i < 0 OR i IS NULL",
        "((i > 0) OR (i < 0) OR (i IS NULL))",
        "i > 0 OR i < 0 OR i IS NULL",
    ),
    (
        "d25",
        "i > 0 AND s > 0 AND b > 0",
        "((i > 0) AND (s > 0) AND (b > 0))",
        "i > 0 AND s > 0 AND b > 0",
    ),
    ("d26", "abs(i) < 5", "(abs(i) < 5)", "abs(i) < 5"),
    (
        "d27",
        "i ^ 2 > 0",
        "(((i)::double precision ^ (2)::double precision) > (0)::double precision)",
        "(i::double precision ^ 2::double precision) > 0::double precision",
    ),
    (
        "d28",
        "t IS DISTINCT FROM v",
        "(t IS DISTINCT FROM (v)::text)",
        "t IS DISTINCT FROM v::text",
    ),
    ("d29", "f IS TRUE", "(f IS TRUE)", "f IS TRUE"),
    (
        "d30",
        "v IN ('a')",
        "((v)::text = 'a'::text)",
        "v::text = 'a'::text",
    ),
    ("d31", "t = v", "(t = (v)::text)", "t = v::text"),
    ("d32", "t LIKE 'a%'", "(t LIKE 'a%')", "t ~~ 'a%'::text"),
    (
        "d33",
        "t <> 'pg_class'::regclass::text",
        "(t <> ('pg_class'::regclass)::text)",
        "t <> 'pg_class'::regclass::text",
    ),
    (
        "d34",
        "b <> CAST('pg_catalog.pg_class' AS regclass)::oid::bigint",
        "(b <> (('pg_class'::regclass)::oid)::bigint)",
        "b <> 'pg_class'::regclass::oid::bigint",
    ),
];

const DEFAULTS: [(&str, &str, &str, &str); 17] = [
    ("i integer", "0", "0", "0"),
    ("s smallint", "-3", "'-3'::integer", "'-3'::integer"),
    ("b bigint", "5", "5", "5"),
    ("n numeric(10,2)", "1.5", "1.5", "1.5"),
    ("r real", "2", "2", "2"),
    ("d double precision", "2.5", "2.5", "2.5"),
    ("t text", "'x'", "'x'::text", "'x'::text"),
    (
        "v varchar(10)",
        "'y'",
        "'y'::character varying",
        "'y'::character varying",
    ),
    ("c char(3)", "'ab'", "'ab'::bpchar", "'ab'::bpchar"),
    ("dt date", "CURRENT_DATE", "CURRENT_DATE", "CURRENT_DATE"),
    (
        "ts timestamp",
        "'2020-01-01 00:00'",
        "'2020-01-01 00:00:00'::timestamp without time zone",
        "'2020-01-01 00:00:00'::timestamp without time zone",
    ),
    ("tz timestamptz", "now()", "now()", "now()"),
    (
        "iv interval",
        "'1 day'",
        "'1 day'::interval",
        "'1 day'::interval",
    ),
    ("f boolean", "false", "false", "false"),
    (
        "rt text",
        "'pg_class'::regclass::text",
        "('pg_class'::regclass)::text",
        "'pg_class'::regclass::text",
    ),
    (
        "ro bigint",
        "CAST('pg_class' AS regclass)::oid::bigint",
        "(('pg_class'::regclass)::oid)::bigint",
        "'pg_class'::regclass::oid::bigint",
    ),
    (
        "rc text",
        "regclass('pg_catalog.pg_class')::text || 'x'",
        "(('pg_class'::regclass)::text || 'x'::text)",
        "'pg_class'::regclass::text || 'x'::text",
    ),
];

fn run(session: &mut Session<'_>, sql: &str) -> Result<Vec<Vec<Value>>, SqlError> {
    let mut rows = Vec::new();
    session.execute(sql, &mut |result| rows = result.rows)?;
    Ok(rows)
}

fn single_text(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(rows) => rows
            .first()
            .and_then(|row| row.first())
            .and_then(Value::to_text)
            .unwrap_or_else(|| "NULL".to_owned()),
        Err(error) => panic!("{sql}: {error}"),
    }
}

#[test]
fn check_constraints_written_either_way_show_as_postgresql_16_shows_them() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let mut wrong = Vec::new();
    for (name, created, dumped, expected) in CHECKS {
        for (form, text) in [("created", created), ("dumped", dumped)] {
            let table = format!("{name}_{form}");
            if let Err(error) = run(
                &mut session,
                &format!("CREATE TABLE {table} ({COLUMNS}, CONSTRAINT {name} CHECK ({text}))"),
            ) {
                wrong.push(format!("{table}: CREATE TABLE failed: {error}"));
                continue;
            }
            let shown = single_text(
                &mut session,
                &format!(
                    "SELECT pg_get_constraintdef(oid, true) FROM pg_constraint \
                     WHERE conrelid = '{table}'::regclass AND contype = 'c'"
                ),
            );
            if shown != format!("CHECK ({expected})") {
                wrong.push(format!("{table}: {shown} instead of CHECK ({expected})"));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn column_defaults_written_either_way_show_as_postgresql_16_shows_them() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for (form, at) in [("created", 1), ("dumped", 2)] {
        let columns: Vec<String> = DEFAULTS
            .iter()
            .map(|case| {
                let text = if at == 1 { case.1 } else { case.2 };
                format!("{} DEFAULT {text}", case.0)
            })
            .collect();
        run(
            &mut session,
            &format!("CREATE TABLE defaults_{form} ({})", columns.join(", ")),
        )
        .unwrap_or_else(|error| panic!("{form}: {error}"));
        let rows = run(
            &mut session,
            &format!(
                "SELECT pg_get_expr(d.adbin, d.adrelid, true) FROM pg_attrdef d \
                 WHERE d.adrelid = 'defaults_{form}'::regclass ORDER BY d.adnum"
            ),
        )
        .unwrap_or_else(|error| panic!("{form}: {error}"));
        let shown: Vec<String> = rows
            .iter()
            .map(|row| row.first().and_then(Value::to_text).unwrap_or_default())
            .collect();
        let expected: Vec<String> = DEFAULTS.iter().map(|case| case.3.to_owned()).collect();
        assert_eq!(shown, expected, "{form}");
    }
}

#[test]
fn an_expression_the_deparser_cannot_render_is_shown_as_stored() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        "CREATE TABLE odd (a integer, CONSTRAINT odd_a CHECK (CASE WHEN a > 0 THEN true ELSE false END))",
    )
    .unwrap();
    let shown = single_text(
        &mut session,
        "SELECT pg_get_constraintdef(oid, true) FROM pg_constraint WHERE conname = 'odd_a'",
    );
    assert!(shown.starts_with("CHECK ("), "{shown}");
    assert!(shown.contains("CASE WHEN a > 0"), "{shown}");
}

#[test]
fn row_comparisons_show_as_sql_that_restores_them() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "CREATE TABLE pairs (a integer, b integer, \
         CONSTRAINT pairs_low CHECK ((a, b) < (100, 100)), \
         CONSTRAINT pairs_listed CHECK ((a, b) IN ((1, 2), (3, 4), (50, 50))))",
        "ALTER TABLE pairs ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY pairs_small ON pairs USING ((a, b) <= (9, 9))",
    ] {
        run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    let definition = |session: &mut Session<'_>, name: &str| {
        single_text(
            session,
            &format!(
                "SELECT pg_get_constraintdef(oid, true) FROM pg_constraint WHERE conname = '{name}'"
            ),
        )
    };
    let low = definition(&mut session, "pairs_low");
    assert_eq!(low, "CHECK ((ROW(a, b) < ROW(100, 100)))");
    let listed = definition(&mut session, "pairs_listed");
    assert_eq!(
        listed,
        "CHECK (a = 1 AND b = 2 OR a = 3 AND b = 4 OR a = 50 AND b = 50)"
    );
    let clause = single_text(
        &mut session,
        "SELECT check_clause FROM information_schema.check_constraints \
         WHERE constraint_name = 'pairs_low'",
    );
    assert!(clause.contains("(ROW(a, b) < ROW(100, 100))"), "{clause}");
    let qual = single_text(
        &mut session,
        "SELECT qual FROM pg_policies WHERE policyname = 'pairs_small'",
    );
    assert_eq!(qual, "(ROW(a, b) <= ROW(9, 9))");
    for sql in [
        format!(
            "CREATE TABLE restored (a integer, b integer, \
             CONSTRAINT restored_low {low}, CONSTRAINT restored_listed {listed})"
        ),
        "ALTER TABLE restored ENABLE ROW LEVEL SECURITY".to_owned(),
        format!("CREATE POLICY restored_small ON restored USING {qual}"),
        "INSERT INTO restored VALUES (3, 4), (50, 50)".to_owned(),
    ] {
        run(&mut session, &sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    assert_eq!(definition(&mut session, "restored_low"), low);
    assert_eq!(definition(&mut session, "restored_listed"), listed);
    assert_eq!(
        single_text(
            &mut session,
            "SELECT qual FROM pg_policies WHERE policyname = 'restored_small'"
        ),
        qual
    );
    for row in ["(100, 0)", "(5, 5)"] {
        let error =
            run(&mut session, &format!("INSERT INTO restored VALUES {row}")).expect_err(row);
        assert_eq!(
            error.state(),
            crate::SqlState::CHECK_VIOLATION,
            "{row}: {error}"
        );
    }
}

#[test]
fn explain_shows_row_comparisons_as_each_database_writes_them() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE pairs (a integer, b integer)").unwrap();
    let filter = |session: &mut Session<'_>, sql: &str| {
        let rows = run(session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        rows.iter()
            .filter_map(|row| row.first().and_then(Value::to_text))
            .find(|line| line.contains("Filter: "))
            .unwrap_or_else(|| panic!("{sql}: {rows:?}"))
    };
    let ordering = filter(
        &mut session,
        "EXPLAIN SELECT * FROM pairs WHERE (a, b) < (5, 6) AND a = (SELECT 1)",
    );
    assert!(
        ordering.contains("Filter: ((ROW(a, b) < ROW(5, 6)) AND "),
        "{ordering}"
    );
    let membership = filter(
        &mut session,
        "EXPLAIN SELECT * FROM pairs WHERE (a, b) IN ((1, 2), (3, 4)) AND a = (SELECT 1)",
    );
    assert!(
        membership.contains("Filter: ((((a = 1) AND (b = 2)) OR ((a = 3) AND (b = 4))) AND "),
        "{membership}"
    );
    let mut mysql = Session::with_dialect(&store, crate::Dialect::MySql);
    run(&mut mysql, "CREATE TABLE duo (a INT, b INT)").unwrap();
    let text = run(
        &mut mysql,
        "EXPLAIN FORMAT=TREE SELECT * FROM duo WHERE (a, b) < (5, 6) OR (a, b) IN ((1, 2), (3, 4))",
    )
    .unwrap()
    .iter()
    .filter_map(|row| row.first().and_then(Value::to_text))
    .collect::<Vec<_>>()
    .join("\n");
    assert!(text.contains("< (5,6))"), "{text}");
    assert!(text.contains(" in ((1,2),(3,4)))"), "{text}");
    assert!(!text.contains("*row"), "{text}");
}
