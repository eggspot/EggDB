use crate::memory_store::MemoryStore;
use crate::tests::render;
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

fn fails(session: &mut Session<'_>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error,
    }
}

fn with_distributors(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::new(store);
    last(
        &mut session,
        "CREATE TABLE distributors (did integer PRIMARY KEY, dname text, zipcode text, \
         is_active boolean DEFAULT true, \
         CONSTRAINT distributors_dname_unique UNIQUE (dname), \
         CONSTRAINT distributors_did_positive CHECK (did > 0))",
    );
    last(
        &mut session,
        "INSERT INTO distributors (did, dname, zipcode) VALUES \
         (5, 'Gizmo', '21201'), (9, 'Antwerp', '10001')",
    );
    session
}

#[test]
fn on_conflict_on_constraint_names_the_primary_key_as_the_manual_shows() {
    let store = MemoryStore::default();
    let mut session = with_distributors(&store);
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO distributors (did, dname) VALUES (9, 'Antwerp Design') \
             ON CONFLICT ON CONSTRAINT distributors_pkey DO NOTHING"
        ),
        "INSERT 0 0"
    );
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO distributors (did, dname) VALUES (10, 'Conrad International') \
             ON CONFLICT ON CONSTRAINT distributors_pkey DO NOTHING"
        ),
        "INSERT 0 1"
    );
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO distributors (did, dname) VALUES (5, 'Gizmo Transglobal') \
             ON CONFLICT ON CONSTRAINT distributors_pkey DO UPDATE SET dname = EXCLUDED.dname"
        ),
        "INSERT 0 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT did, dname FROM distributors ORDER BY did"
        ),
        "did|dname\n5|Gizmo Transglobal\n9|Antwerp\n10|Conrad International\nSELECT 3"
    );
}

#[test]
fn on_conflict_on_a_unique_constraint_arbitrates_only_that_constraint() {
    let store = MemoryStore::default();
    let mut session = with_distributors(&store);
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO distributors (did, dname) VALUES (11, 'Gizmo') \
             ON CONFLICT ON CONSTRAINT distributors_dname_unique \
             DO UPDATE SET zipcode = '99999'"
        ),
        "INSERT 0 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT did, zipcode FROM distributors WHERE dname = 'Gizmo'"
        ),
        "did|zipcode\n5|99999\nSELECT 1"
    );
    let other_key = fails(
        &mut session,
        "INSERT INTO distributors (did, dname) VALUES (9, 'Brand New') \
         ON CONFLICT ON CONSTRAINT distributors_dname_unique DO NOTHING",
    );
    assert_eq!(other_key.state(), crate::SqlState::UNIQUE_VIOLATION);
}

#[test]
fn on_conflict_on_constraint_refuses_names_that_are_not_unique_constraints() {
    let store = MemoryStore::default();
    let mut session = with_distributors(&store);
    last(
        &mut session,
        "CREATE UNIQUE INDEX distributors_zipcode_index ON distributors (zipcode)",
    );
    let missing = fails(
        &mut session,
        "INSERT INTO distributors (did) VALUES (12) ON CONFLICT ON CONSTRAINT nope DO NOTHING",
    );
    assert_eq!(missing.state(), crate::SqlState::UNDEFINED_OBJECT);
    assert_eq!(
        missing.message(),
        "constraint \"nope\" for table \"distributors\" does not exist"
    );
    let index_only = fails(
        &mut session,
        "INSERT INTO distributors (did) VALUES (12) \
         ON CONFLICT ON CONSTRAINT distributors_zipcode_index DO NOTHING",
    );
    assert_eq!(index_only.state(), crate::SqlState::UNDEFINED_OBJECT);
    let check = fails(
        &mut session,
        "INSERT INTO distributors (did) VALUES (12) \
         ON CONFLICT ON CONSTRAINT distributors_did_positive DO NOTHING",
    );
    assert_eq!(check.state(), crate::SqlState::WRONG_OBJECT_TYPE);
    assert_eq!(
        check.message(),
        "constraint in ON CONFLICT clause has no associated index"
    );
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO distributors (did, zipcode) VALUES (12, '21201') \
             ON CONFLICT (zipcode) DO NOTHING"
        ),
        "INSERT 0 0"
    );
}

#[test]
fn an_index_predicate_infers_a_full_unique_index_as_the_manual_shows() {
    let store = MemoryStore::default();
    let mut session = with_distributors(&store);
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO distributors (did, dname) VALUES (9, 'Antwerp Design') \
             ON CONFLICT (did) WHERE is_active DO NOTHING"
        ),
        "INSERT 0 0"
    );
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO distributors AS d (did, dname) VALUES (5, 'Gizmo Global') \
             ON CONFLICT (did) WHERE d.is_active AND zipcode LIKE '2%' \
             DO UPDATE SET dname = EXCLUDED.dname"
        ),
        "INSERT 0 1"
    );
    assert_eq!(
        last(&mut session, "SELECT dname FROM distributors WHERE did = 5"),
        "dname\nGizmo Global\nSELECT 1"
    );
    let unknown = fails(
        &mut session,
        "INSERT INTO distributors (did) VALUES (5) ON CONFLICT (did) WHERE nosuch DO NOTHING",
    );
    assert_eq!(unknown.state(), crate::SqlState::UNDEFINED_COLUMN);
    let no_index = fails(
        &mut session,
        "INSERT INTO distributors (did) VALUES (5) \
         ON CONFLICT (zipcode) WHERE is_active DO NOTHING",
    );
    assert_eq!(no_index.state(), crate::SqlState::INVALID_COLUMN_REFERENCE);
}

#[test]
fn an_expression_target_needs_an_expression_index_and_order_is_refused() {
    let store = MemoryStore::default();
    let mut session = with_distributors(&store);
    let expression = fails(
        &mut session,
        "INSERT INTO distributors (did, dname) VALUES (13, 'x') \
         ON CONFLICT (lower(dname)) DO NOTHING",
    );
    assert_eq!(
        expression.state(),
        crate::SqlState::INVALID_COLUMN_REFERENCE
    );
    assert_eq!(
        expression.message(),
        "there is no unique or exclusion constraint matching the ON CONFLICT specification"
    );
    let ordered = fails(
        &mut session,
        "INSERT INTO distributors (did) VALUES (13) ON CONFLICT (did DESC) DO NOTHING",
    );
    assert_eq!(ordered.state(), crate::SqlState::SYNTAX_ERROR);
    assert_eq!(
        ordered.message(),
        "ASC/DESC is not allowed in ON CONFLICT clause"
    );
}

#[test]
fn on_conflict_on_constraint_reaches_the_partition_a_row_goes_to() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE m (id integer NOT NULL, at date NOT NULL, v integer, PRIMARY KEY (id, at)) \
         PARTITION BY RANGE (at)",
    );
    last(
        &mut session,
        "CREATE TABLE m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
    );
    last(&mut session, "INSERT INTO m VALUES (1, '2024-03-01', 10)");
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO m VALUES (1, '2024-03-01', 99) \
             ON CONFLICT ON CONSTRAINT m_pkey DO UPDATE SET v = EXCLUDED.v"
        ),
        "INSERT 0 1"
    );
    assert_eq!(
        last(&mut session, "SELECT id, v FROM m"),
        "id|v\n1|99\nSELECT 1"
    );
}

#[test]
fn sqlite_upsert_targets_take_a_where_clause_and_ordering() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    last(
        &mut session,
        "CREATE TABLE vocabulary (word TEXT PRIMARY KEY, count INT DEFAULT 1)",
    );
    last(
        &mut session,
        "INSERT INTO vocabulary (word) VALUES ('jovial')",
    );
    last(
        &mut session,
        "INSERT INTO vocabulary (word) VALUES ('jovial') \
         ON CONFLICT (word) DO UPDATE SET count = count + 1",
    );
    last(
        &mut session,
        "INSERT INTO vocabulary (word) VALUES ('jovial') \
         ON CONFLICT (word ASC) WHERE count > 0 DO UPDATE SET count = count + 1",
    );
    assert_eq!(
        last(&mut session, "SELECT word, count FROM vocabulary"),
        "word|count\njovial|3\nSELECT 1"
    );
}
