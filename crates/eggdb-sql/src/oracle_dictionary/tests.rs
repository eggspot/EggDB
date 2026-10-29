use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Parameter, Session, SqlError};

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

fn fails(session: &mut Session<'_, MemoryStore>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error,
    }
}

fn hr_schema(store: &MemoryStore) {
    let mut setup = Session::new(store);
    last(
        &mut setup,
        "CREATE SCHEMA hr;
         CREATE TABLE hr.regions (region_id integer PRIMARY KEY, region_name varchar(25));
         CREATE TABLE hr.countries (country_id char(2) PRIMARY KEY, country_name varchar(40) NOT NULL,
           region_id integer CONSTRAINT countr_reg_fk REFERENCES hr.regions (region_id) ON DELETE CASCADE,
           CONSTRAINT country_name_ck CHECK (country_name <> ''));
         CREATE UNIQUE INDEX country_name_uk ON hr.countries (country_name);
         CREATE INDEX countries_region_ix ON hr.countries (region_id);
         CREATE SEQUENCE hr.regions_seq START WITH 10 INCREMENT BY 10;
         CREATE VIEW hr.region_names AS SELECT region_name FROM hr.regions;
         COMMENT ON TABLE hr.regions IS 'Regions of the world';
         COMMENT ON COLUMN hr.regions.region_name IS 'Its name';
         INSERT INTO hr.regions VALUES (1, 'Europe'), (2, 'Americas');
         INSERT INTO hr.countries VALUES ('UK', 'United Kingdom', 1), ('US', 'United States of America', 2);
         CREATE TABLE notes (id integer PRIMARY KEY)",
    );
    setup.close().unwrap();
}

fn hr_session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::new(store);
    session.identify("hr", "eggdb");
    session.set_parameter("eggdb.dialect", "oracle").unwrap();
    session
}

#[test]
fn a_user_with_a_schema_of_its_own_sees_it_as_its_own_objects() {
    let store = MemoryStore::default();
    hr_schema(&store);
    let mut session = hr_session(&store);
    assert_eq!(session.dialect(), Dialect::Oracle);
    assert_eq!(
        last(&mut session, "SELECT table_name FROM user_tables ORDER BY table_name"),
        "TABLE_NAME\nCOUNTRIES\nREGIONS\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT owner, table_name FROM all_tables ORDER BY owner, table_name"
        ),
        "OWNER|TABLE_NAME\nHR|COUNTRIES\nHR|REGIONS\nPUBLIC|NOTES\nSELECT 3"
    );
    assert_eq!(
        last(&mut session, "SELECT COUNT(*) FROM countries"),
        "COUNT(*)\n2\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT region_name FROM HR.regions WHERE region_id = 1"),
        "REGION_NAME\nEurope\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') AS s, SYS_CONTEXT('userenv', 'session_user') AS u FROM dual"
        ),
        "S|U\nHR|HR\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT username FROM user_users"),
        "USERNAME\nHR\nSELECT 1"
    );
}

#[test]
fn constraints_indexes_sequences_views_and_objects_are_listed_and_joined() {
    let store = MemoryStore::default();
    hr_schema(&store);
    let mut session = hr_session(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT c.constraint_name, c.constraint_type, c.r_constraint_name, c.delete_rule, cc.column_name, cc.position
             FROM user_constraints c JOIN user_cons_columns cc ON cc.constraint_name = c.constraint_name
             WHERE c.table_name = 'COUNTRIES' AND c.constraint_type IN ('P', 'R') ORDER BY c.constraint_type"
        ),
        "CONSTRAINT_NAME|CONSTRAINT_TYPE|R_CONSTRAINT_NAME|DELETE_RULE|COLUMN_NAME|POSITION\nCOUNTRIES_PKEY|P|NULL|NULL|COUNTRY_ID|1\nCOUNTR_REG_FK|R|REGIONS_PKEY|CASCADE|REGION_ID|1\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT search_condition FROM user_constraints WHERE constraint_name = 'COUNTRY_NAME_CK'"
        )
        .lines()
        .count(),
        3
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT i.index_name, i.uniqueness, ic.column_name FROM user_indexes i JOIN user_ind_columns ic ON ic.index_name = i.index_name ORDER BY i.index_name"
        ),
        "INDEX_NAME|UNIQUENESS|COLUMN_NAME\nCOUNTRIES_REGION_IX|NONUNIQUE|REGION_ID\nCOUNTRY_NAME_UK|UNIQUE|COUNTRY_NAME\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT sequence_name, increment_by, last_number FROM user_sequences"
        ),
        "SEQUENCE_NAME|INCREMENT_BY|LAST_NUMBER\nREGIONS_SEQ|10|10\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT view_name FROM user_views"),
        "VIEW_NAME\nREGION_NAMES\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT object_type, COUNT(*) FROM user_objects GROUP BY object_type ORDER BY object_type"
        ),
        "OBJECT_TYPE|COUNT(*)\nINDEX|2\nSEQUENCE|1\nTABLE|2\nVIEW|1\nSELECT 4"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT comments FROM user_tab_comments WHERE table_name = 'REGIONS'"
        ),
        "COMMENTS\nRegions of the world\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT column_name, data_type, data_length, nullable FROM user_tab_columns WHERE table_name = 'COUNTRIES' ORDER BY column_id"
        ),
        "COLUMN_NAME|DATA_TYPE|DATA_LENGTH|NULLABLE\nCOUNTRY_ID|CHAR|2|N\nCOUNTRY_NAME|VARCHAR2|40|N\nREGION_ID|NUMBER|22|Y\nSELECT 3"
    );
}

#[test]
fn version_nls_parameters_and_userenv_answer_like_oracle() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    let banner = last(&mut session, "SELECT banner FROM v$version");
    assert!(
        banner.contains("compatible with Oracle Database 23ai"),
        "{banner}"
    );
    last(
        &mut session,
        "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD'",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT value FROM nls_session_parameters WHERE parameter = 'NLS_DATE_FORMAT'"
        ),
        "VALUE\nYYYY-MM-DD\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT value FROM nls_database_parameters WHERE parameter = 'NLS_DATE_FORMAT'"
        ),
        "VALUE\nDD-MON-RR\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT value FROM nls_database_parameters WHERE parameter = 'NLS_CHARACTERSET'"
        ),
        "VALUE\nAL32UTF8\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT SYS_CONTEXT('USERENV', 'NLS_DATE_FORMAT') AS f, SYS_CONTEXT('USERENV', 'HOST') AS h, SYS_CONTEXT('MY_CONTEXT', 'X') AS x FROM dual"
        ),
        "F|H|X\nYYYY-MM-DD|NULL|NULL\nSELECT 1"
    );
    assert_eq!(
        fails(
            &mut session,
            "SELECT SYS_CONTEXT('USERENV', 'NO_SUCH_PARAMETER') FROM dual"
        )
        .vendor_code(),
        Some(2003)
    );
    assert_eq!(
        fails(&mut session, "SELECT SYS_CONTEXT('USERENV') FROM dual").vendor_code(),
        Some(938)
    );
}

#[test]
fn an_own_table_named_like_a_dictionary_view_hides_it_and_dictionary_views_stay_read_only() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    last(&mut session, "CREATE TABLE emp (id NUMBER(9) PRIMARY KEY)");
    let refused = fails(&mut session, "DELETE FROM user_tables");
    assert!(refused.vendor_code().is_some() || !refused.message().is_empty());
    last(&mut session, "CREATE TABLE tabs (id NUMBER(9))");
    assert_eq!(
        last(&mut session, "SELECT COUNT(*) FROM tabs"),
        "COUNT(*)\n0\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT COUNT(*) FROM sys.tabs"),
        "COUNT(*)\n2\nSELECT 1"
    );
}

#[test]
fn a_postgresql_session_switches_to_oracle_and_back_between_statements() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let outputs = run(
        &mut session,
        "CREATE TABLE t (id integer PRIMARY KEY, name text);
         SET eggdb.dialect = oracle;
         INSERT INTO t VALUES (1, '');
         SELECT NVL(name, 'none') AS name FROM t WHERE ROWNUM <= 1;
         SET eggdb.dialect = postgresql;
         SELECT name IS NULL AS missing FROM t",
    )
    .unwrap();
    assert_eq!(outputs[3], "NAME\nnone\nSELECT 1");
    assert_eq!(outputs[5], "missing\nt\nSELECT 1");
    assert_eq!(session.dialect(), Dialect::PostgreSql);
    let mut other = Session::new(&store);
    assert_eq!(
        last(&mut other, "SELECT count(*) FROM t"),
        "count\n1\nSELECT 1"
    );
    other.close().unwrap();
    assert_eq!(
        fails(&mut session, "SET eggdb.dialect = mysql")
            .state()
            .code(),
        "0A000"
    );
    assert_eq!(
        fails(&mut session, "SET eggdb.dialect = cobol")
            .state()
            .code(),
        "22023"
    );
    last(&mut session, "BEGIN");
    assert_eq!(
        fails(&mut session, "SET eggdb.dialect = oracle")
            .state()
            .code(),
        "25001"
    );
    last(&mut session, "ROLLBACK");
    last(&mut session, "SET eggdb.dialect TO oracle");
    assert_eq!(session.dialect(), Dialect::Oracle);
    assert!(session.autocommit());
    last(&mut session, "BEGIN");
    last(&mut session, "INSERT INTO t VALUES (2, 'b')");
    last(&mut session, "ROLLBACK");
    assert_eq!(last(&mut session, "SELECT COUNT(*) FROM t"), "COUNT(*)\n1\nSELECT 1");
    last(&mut session, "SET eggdb.dialect = DEFAULT");
    assert_eq!(session.dialect(), Dialect::PostgreSql);
    last(&mut session, "RESET eggdb.dialect");
    assert_eq!(
        last(&mut session, "SHOW eggdb.dialect"),
        "eggdb.dialect\npostgresql\nSHOW"
    );
}

#[test]
fn prepared_oracle_statements_take_dollar_parameters_and_describe_dictionary_columns() {
    let store = MemoryStore::default();
    hr_schema(&store);
    let mut session = hr_session(&store);
    let description = session
        .describe(
            "SELECT table_name, num_rows FROM user_tables WHERE table_name = $1",
            &[],
        )
        .unwrap();
    let names: Vec<String> = description
        .columns
        .unwrap_or_default()
        .into_iter()
        .map(|column| column.name)
        .collect();
    assert_eq!(names, ["TABLE_NAME", "NUM_ROWS"]);
    assert_eq!(description.parameter_types.len(), 1);
    let mut shown = Vec::new();
    session
        .execute_prepared(
            "SELECT region_name FROM regions WHERE region_id = $1",
            &[Parameter {
                declared_type: Some(crate::types::SqlType::Integer),
                value: Some("2".to_owned()),
            }],
            &mut |result| {
                shown.push(render(&result));
                true
            },
        )
        .unwrap();
    assert_eq!(shown, ["REGION_NAME\nAmericas\nSELECT 1"]);
}

#[test]
fn a_transact_sql_batch_switches_to_oracle_for_the_statements_after_the_set() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    let mut steps = Vec::new();
    session
        .execute_batch(
            "CREATE TABLE t (id int PRIMARY KEY)\nSET eggdb.dialect oracle\nSELECT COUNT(*) FROM dual WHERE ROWNUM <= 1",
            &mut |step| {
                steps.push(step.outcome.map(|result| render(&result)));
                true
            },
        )
        .unwrap();
    assert_eq!(steps.len(), 3, "{steps:?}");
    assert_eq!(steps[2].as_ref().unwrap(), "COUNT(*)\n1\nSELECT 1");
    assert_eq!(session.dialect(), Dialect::Oracle);
    let table = session.in_connection_dialect(|session| {
        let mut found = Vec::new();
        session
            .execute_batch("SELECT TOP 1 id FROM t", &mut |step| {
                found.push(step.outcome.is_ok());
                true
            })
            .unwrap();
        found
    });
    assert_eq!(table, [true]);
    assert_eq!(session.dialect(), Dialect::Oracle);
    session.reset_connection(false).unwrap();
    assert_eq!(session.dialect(), Dialect::SqlServer);
}
