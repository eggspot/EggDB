use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Parameter, Session, SqlError, SqlType};

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

fn state(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error.state().code().to_owned(),
    }
}

fn with_authors(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::new(store);
    last(
        &mut session,
        "CREATE TABLE authors (id SERIAL NOT NULL, name VARCHAR(60) NOT NULL, born DATE, \
         PRIMARY KEY (id), UNIQUE (name))",
    );
    session
}

#[test]
fn insert_select_adds_the_rows_of_its_query_in_the_query_order() {
    let store = MemoryStore::default();
    let mut session = with_authors(&store);
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO authors (name, born) SELECT p0::VARCHAR, p1::DATE FROM (VALUES \
             ('Ada'::VARCHAR, '1815-12-10'::DATE, 0), ('Grace'::VARCHAR, NULL::DATE, 2), \
             ('Edsger'::VARCHAR, '1930-05-11'::DATE, 1)) AS imp_sen(p0, p1, sen_counter) \
             ORDER BY sen_counter RETURNING authors.id, authors.id AS id__1"
        ),
        "id|id__1\n1|1\n2|2\n3|3\nINSERT 0 3"
    );
    assert_eq!(
        last(&mut session, "SELECT id, name, born FROM authors ORDER BY id"),
        "id|name|born\n1|Ada|1815-12-10\n2|Edsger|1930-05-11\n3|Grace|NULL\nSELECT 3"
    );
}

#[test]
fn insert_select_reads_the_table_as_it_was_before_the_statement() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE n (x integer); INSERT INTO n VALUES (1), (2)",
    );
    assert_eq!(
        last(&mut session, "INSERT INTO n SELECT x + 10 FROM n"),
        "INSERT 0 2"
    );
    assert_eq!(
        last(&mut session, "INSERT INTO n (x) (SELECT max(x) FROM n)"),
        "INSERT 0 1"
    );
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO n WITH big AS (SELECT x FROM n WHERE x > 11) SELECT x * 2 FROM big"
        ),
        "INSERT 0 2"
    );
    assert_eq!(
        last(&mut session, "SELECT x FROM n ORDER BY x"),
        "x\n1\n2\n11\n12\n12\n24\n24\nSELECT 7"
    );
    assert_eq!(
        last(&mut session, "INSERT INTO n SELECT x FROM n WHERE x < 0"),
        "INSERT 0 0"
    );
}

#[test]
fn insert_select_checks_constraints_and_converts_like_insert_values() {
    let store = MemoryStore::default();
    let mut session = with_authors(&store);
    last(&mut session, "INSERT INTO authors (name) VALUES ('Ada')");
    assert_eq!(
        state(
            &mut session,
            "INSERT INTO authors (name) SELECT 'Ada'"
        ),
        "23505"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO authors (name) SELECT NULL"),
        "23502"
    );
    assert_eq!(
        state(
            &mut session,
            "INSERT INTO authors (name, born) SELECT 'x', 'y', 'z'"
        ),
        "42601"
    );
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO authors (name, born) SELECT 'Grace', '1906-12-09'::date RETURNING name, born"
        ),
        "name|born\nGrace|1906-12-09\nINSERT 0 1"
    );
}

#[test]
fn insert_select_binds_parameters_in_its_query() {
    let store = MemoryStore::default();
    let mut session = with_authors(&store);
    let text = |value: &str| Parameter {
        declared_type: None,
        value: Some(value.to_owned()),
    };
    let mut outputs = Vec::new();
    session
        .execute_prepared(
            "INSERT INTO authors (name, born) SELECT p0::VARCHAR, p1::DATE FROM (VALUES \
             ($1::VARCHAR, $2::DATE, 0), ($3::VARCHAR, $4::DATE, 1)) AS imp_sen(p0, p1, sen_counter) \
             ORDER BY sen_counter RETURNING authors.id, authors.id AS id__1",
            &[
                text("Ada"),
                text("1815-12-10"),
                text("Grace"),
                Parameter {
                    declared_type: Some(SqlType::Date),
                    value: None,
                },
            ],
            &mut |result| {
                outputs.push(render(&result));
                true
            },
        )
        .unwrap();
    assert_eq!(outputs, ["id|id__1\n1|1\n2|2\nINSERT 0 2"]);
}

#[test]
fn insert_select_runs_in_the_other_dialects() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    last(
        &mut session,
        "CREATE TABLE src (id INT PRIMARY KEY, name NVARCHAR(20)); \
         CREATE TABLE dst (id INT IDENTITY(1,1) PRIMARY KEY, name NVARCHAR(20)); \
         INSERT INTO src VALUES (1, N'a'), (2, N'b')",
    );
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO dst (name) OUTPUT inserted.id, inserted.name SELECT name FROM src ORDER BY id DESC"
        ),
        "id|name\n1|b\n2|a\nINSERT 0 2"
    );
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    last(&mut mysql, "CREATE TABLE m (a INT, b INT)");
    assert_eq!(
        last(&mut mysql, "INSERT INTO m SELECT 1, 2 UNION ALL SELECT 3, 4"),
        "INSERT 0 2"
    );
}
