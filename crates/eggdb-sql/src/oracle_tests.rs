use crate::datetime::{MICROS_PER_DAY, days_from_civil};
use crate::memory_store::MemoryStore;
use crate::oracle_nls::{NlsParameters, enter};
use crate::types::SqlType;
use crate::{Dialect, QueryResult, Session, SqlError, Value};
use std::sync::Arc;

fn values(session: &mut Session<'_, MemoryStore>, text: &str) -> Vec<Vec<Value>> {
    let mut last = None;
    session
        .execute(text, &mut |result: QueryResult| last = Some(result.rows))
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    last.unwrap_or_default()
}

fn failure(session: &mut Session<'_, MemoryStore>, text: &str) -> SqlError {
    session
        .execute(text, &mut |_| {})
        .expect_err("the statement fails")
}

fn number(value: i64) -> Value {
    Value::Numeric(crate::numeric::Numeric::from_i64(value))
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

#[test]
fn a_text_constant_meets_a_date_through_the_nls_format_under_the_statement_guards() {
    let _context = crate::constraints::enter(
        Dialect::Oracle,
        true,
        String::new(),
        String::new(),
        None,
        false,
    );
    let _clock = crate::datetime::enter_statement(0);
    let _nls = enter(Arc::new(NlsParameters::default()));
    let june = days_from_civil(2003, 6, 17) * MICROS_PER_DAY;
    assert_eq!(
        crate::eval::string_constant("17-JUN-03", Some(SqlType::Timestamp(Some(0)))),
        Ok(Value::Timestamp(june))
    );
    assert_eq!(
        crate::eval::string_constant("2003-06-17", Some(SqlType::Timestamp(Some(0)))),
        Ok(Value::Timestamp(june))
    );
    assert!(crate::eval::string_constant("now", Some(SqlType::Timestamp(Some(0)))).is_err());
    assert_eq!(
        crate::eval::string_constant("20.5", Some(SqlType::SmallInt)),
        Ok(Value::Numeric(
            crate::numeric::Numeric::parse("20.5").expect("decimal")
        ))
    );
}

#[test]
fn other_dialects_keep_their_own_conversions() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    values(
        &mut session,
        "CREATE TABLE t (id integer PRIMARY KEY, d timestamp)",
    );
    values(
        &mut session,
        "INSERT INTO t VALUES (5, '2024-02-03 10:00:00')",
    );
    assert_eq!(
        failure(&mut session, "SELECT d + 1 FROM t").state().code(),
        "42883"
    );
    assert_eq!(
        values(
            &mut session,
            "SELECT CAST('2024-02-03' AS date) - DATE '2024-02-01'"
        ),
        vec![vec![Value::Integer(2)]]
    );
    for dialect in [
        Dialect::PostgreSql,
        Dialect::MySql,
        Dialect::Sqlite,
        Dialect::Oracle,
        Dialect::SqlServer,
    ] {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        values(&mut session, "CREATE TABLE five (id INTEGER PRIMARY KEY)");
        values(&mut session, "INSERT INTO five VALUES (5)");
        assert_eq!(
            values(&mut session, "SELECT id FROM five WHERE id = '5'").len(),
            1,
            "{dialect:?}"
        );
    }
}

#[test]
fn nls_settings_last_for_the_session_and_reach_every_statement() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    values(
        &mut session,
        "ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-DD-MM' NLS_SORT = BINARY",
    );
    assert_eq!(
        values(
            &mut session,
            "SELECT TO_CHAR(DATE '2024-02-03', 'DD MON'), TO_CHAR(CAST('2024-02-03' AS DATE), 'DD MON') FROM dual"
        ),
        vec![vec![text("03 FEB"), text("02 MAR")]]
    );
    values(&mut session, "ALTER SESSION SET NLS_TERRITORY = America");
    assert_eq!(
        values(&mut session, "SELECT TO_CHAR(DATE '2024-02-03') FROM dual"),
        vec![vec![text("03-FEB-24")]]
    );
    assert_eq!(
        failure(&mut session, "ALTER SESSION SET NO_SUCH_THING = 1").vendor_code(),
        Some(2248)
    );
    assert_eq!(
        failure(&mut session, "ALTER SESSION SET CURRENT_SCHEMA = nowhere").vendor_code(),
        Some(1435)
    );
}

#[test]
fn current_schema_sets_the_search_path() {
    let store = MemoryStore::default();
    let mut setup = Session::new(&store);
    values(&mut setup, "CREATE SCHEMA hr");
    values(
        &mut setup,
        "CREATE TABLE hr.regions (id integer PRIMARY KEY)",
    );
    values(&mut setup, "INSERT INTO hr.regions VALUES (1)");
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    values(&mut session, "ALTER SESSION SET CURRENT_SCHEMA = HR");
    assert_eq!(session.parameter("search_path").ok().as_deref(), Some("hr"));
    assert_eq!(values(&mut session, "SELECT id FROM regions").len(), 1);
}

#[test]
fn to_char_is_refused_before_it_builds_past_the_value_limit() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    session.set_max_value_bytes(16);
    let error = failure(
        &mut session,
        "SELECT TO_CHAR(SYSDATE, 'MONTH MONTH MONTH') FROM dual",
    );
    assert_eq!(error.vendor_code(), Some(1489));
    assert_eq!(
        values(
            &mut session,
            "SELECT TO_CHAR(DATE '2024-02-03', 'DD') FROM dual"
        ),
        vec![vec![text("03")]]
    );
}

#[test]
fn merge_is_destructive_when_on_does_not_restrict_the_rows_it_updates_or_deletes() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    let classify = |session: &mut Session<'_, MemoryStore>, text: &str| {
        session
            .destructive_statement(text)
            .map(|found| found.is_some())
            .unwrap_or_else(|_| panic!("{text}"))
    };
    assert!(!classify(
        &mut session,
        "MERGE INTO t USING s ON (t.id = s.id) WHEN MATCHED THEN UPDATE SET t.v = s.v"
    ));
    assert!(!classify(
        &mut session,
        "MERGE INTO t USING s ON (t.id = s.id) WHEN MATCHED THEN UPDATE SET t.v = s.v DELETE WHERE s.v > 1"
    ));
    assert!(classify(
        &mut session,
        "MERGE INTO t USING s ON (1 = 1) WHEN MATCHED THEN UPDATE SET t.v = s.v DELETE WHERE s.v > 1"
    ));
    assert!(classify(
        &mut session,
        "MERGE INTO accounts a USING dual ON (1 = 1) WHEN MATCHED THEN UPDATE SET a.balance = 0"
    ));
    assert!(!classify(
        &mut session,
        "MERGE INTO t USING s ON (1 = 1) WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v)"
    ));
    assert!(classify(
        &mut session,
        "MERGE INTO accounts a USING dual ON (dummy = 'X') WHEN MATCHED THEN UPDATE SET a.balance = 0"
    ));
    assert!(classify(
        &mut session,
        "MERGE INTO accounts a USING (SELECT 1 x FROM dual) s ON (x = 1) WHEN MATCHED THEN UPDATE SET a.balance = 0"
    ));
    assert!(!classify(
        &mut session,
        "MERGE INTO accounts a USING (SELECT 1 x FROM dual) s ON (a.id = s.x) WHEN MATCHED THEN UPDATE SET a.balance = 0"
    ));
    assert!(!classify(
        &mut session,
        "MERGE INTO accounts USING (SELECT 1 x FROM dual) s ON (accounts.id = s.x) WHEN MATCHED THEN UPDATE SET balance = 0"
    ));
    assert!(classify(
        &mut session,
        "MERGE INTO accounts t USING (SELECT 1 id FROM dual) accounts ON (accounts.id = 1) WHEN MATCHED THEN UPDATE SET t.balance = 0"
    ));
    for always_true in [
        "t.id = t.id",
        "t.id IS NOT NULL",
        "t.id = s.id OR 1 = 1",
        "t.id = id",
    ] {
        assert!(
            classify(
                &mut session,
                &format!(
                    "MERGE INTO t USING s ON ({always_true}) WHEN MATCHED THEN UPDATE SET t.v = s.v"
                )
            ),
            "{always_true}"
        );
    }
    for pinned in [
        "t.id = s.id AND t.v > 0",
        "s.id = t.id",
        "t.id = 5",
        "(t.id = s.id AND t.k = s.k)",
    ] {
        assert!(
            !classify(
                &mut session,
                &format!(
                    "MERGE INTO t USING s ON ({pinned}) WHEN MATCHED THEN UPDATE SET t.v = s.v"
                )
            ),
            "{pinned}"
        );
    }
}

#[test]
fn merge_resolves_names_against_both_tables() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    values(
        &mut session,
        "CREATE TABLE t (id NUMBER PRIMARY KEY, v NUMBER)",
    );
    values(
        &mut session,
        "CREATE TABLE s (id NUMBER, v NUMBER, w NUMBER)",
    );
    values(&mut session, "INSERT INTO s VALUES (1, 10, 100)");
    let code =
        |session: &mut Session<'_, MemoryStore>, text: &str| failure(session, text).vendor_code();
    assert_eq!(
        code(
            &mut session,
            "MERGE INTO t USING s ON (t.id = s.id) WHEN NOT MATCHED THEN INSERT (id, v) VALUES (id, v)"
        ),
        Some(918)
    );
    assert_eq!(
        code(
            &mut session,
            "MERGE INTO t USING s ON (t.id = s.id) WHEN MATCHED THEN UPDATE SET t.id = s.w"
        ),
        Some(38104)
    );
    assert_eq!(
        code(
            &mut session,
            "MERGE INTO t USING s ON (t.id = s.id) WHEN MATCHED THEN UPDATE SET t.nothing = 1"
        ),
        Some(904)
    );
    values(
        &mut session,
        "MERGE INTO t USING s ON (t.id = s.id) WHEN NOT MATCHED THEN INSERT (t.id, t.v) VALUES (s.id, w)",
    );
    assert_eq!(
        values(&mut session, "SELECT id, v FROM t"),
        vec![vec![number(1), number(100)]]
    );
}

#[test]
fn the_session_users_own_tables_stay_its_own_after_current_schema() {
    let store = MemoryStore::default();
    let mut setup = Session::new(&store);
    for statement in [
        "CREATE SCHEMA hr",
        "CREATE TABLE hr.t (a integer)",
        "INSERT INTO hr.t VALUES (2)",
        "CREATE TABLE t (a integer)",
        "INSERT INTO t VALUES (1)",
        "CREATE SCHEMA \"Mixed\"",
        "CREATE TABLE \"Mixed\".m (a integer)",
        "INSERT INTO \"Mixed\".m VALUES (3)",
    ] {
        values(&mut setup, statement);
    }
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    values(&mut session, "ALTER SESSION SET CURRENT_SCHEMA = hr");
    assert_eq!(
        values(&mut session, "SELECT a FROM t"),
        vec![vec![Value::Integer(2)]]
    );
    assert_eq!(
        values(&mut session, "SELECT a FROM eggdb.t"),
        vec![vec![Value::Integer(1)]]
    );
    values(&mut session, "UPDATE eggdb.t SET a = 10");
    values(&mut session, "DELETE FROM eggdb.t WHERE a = 10");
    values(&mut session, "COMMIT");
    assert_eq!(
        values(&mut setup, "SELECT a FROM hr.t"),
        vec![vec![Value::Integer(2)]]
    );
    assert!(values(&mut setup, "SELECT a FROM public.t").is_empty());
    values(&mut session, "ALTER SESSION SET CURRENT_SCHEMA = \"Mixed\"");
    assert_eq!(
        values(&mut session, "SELECT a FROM m"),
        vec![vec![Value::Integer(3)]]
    );
    assert_eq!(
        failure(&mut session, "ALTER SESSION SET CURRENT_SCHEMA = 'hr'").vendor_code(),
        Some(922)
    );
}

#[test]
fn a_schema_named_like_the_session_user_does_not_hide_its_own_tables_in_oracle() {
    let store = MemoryStore::default();
    let mut setup = Session::new(&store);
    for statement in [
        "CREATE SCHEMA eggdb",
        "CREATE TABLE eggdb.t (a integer)",
        "INSERT INTO eggdb.t VALUES (2)",
        "CREATE TABLE public.t (a integer)",
        "INSERT INTO public.t VALUES (1)",
    ] {
        values(&mut setup, statement);
    }
    assert_eq!(
        values(&mut setup, "SELECT a FROM t"),
        vec![vec![Value::Integer(2)]]
    );
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    assert_eq!(
        values(&mut session, "SELECT a FROM t"),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        values(&mut session, "SELECT a FROM eggdb.t"),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn oracle_written_dates_and_views_read_the_same_from_postgresql() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    for statement in [
        "ALTER SESSION SET NLS_DATE_FORMAT = 'DD/MM/YYYY'",
        "CREATE TABLE hires (id NUMBER(4) PRIMARY KEY, hired DATE)",
        "INSERT INTO hires VALUES (1, '17/06/2013')",
        "CREATE VIEW hires_shown AS SELECT id, TO_CHAR(hired, 'DD-MON-YYYY') AS shown FROM hires",
        "COMMIT",
    ] {
        values(&mut session, statement);
    }
    let june = days_from_civil(2013, 6, 17) * MICROS_PER_DAY;
    let mut postgresql = Session::new(&store);
    assert_eq!(
        values(&mut postgresql, "SELECT hired FROM hires"),
        vec![vec![Value::Timestamp(june)]]
    );
    assert_eq!(
        values(&mut postgresql, "SELECT shown FROM hires_shown"),
        vec![vec![text("17-JUN-2013")]]
    );
}

#[test]
fn a_source_named_with_its_owner_is_referred_to_by_its_own_name() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    for statement in [
        "CREATE TABLE t (id NUMBER PRIMARY KEY, v NUMBER)",
        "CREATE TABLE s (id NUMBER, v NUMBER)",
        "INSERT INTO t VALUES (1, 0)",
        "INSERT INTO s VALUES (1, 10)",
        "MERGE INTO t USING eggdb.s ON (t.id = s.id) WHEN MATCHED THEN UPDATE SET t.v = s.v",
    ] {
        values(&mut session, statement);
    }
    assert_eq!(
        values(&mut session, "SELECT v FROM t"),
        vec![vec![number(10)]]
    );
}

fn rendered(session: &mut Session<'_, MemoryStore>, text: &str) -> String {
    let mut last = String::new();
    session
        .execute(text, &mut |result: QueryResult| {
            last = crate::tests::render(&result);
        })
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    last
}

#[test]
fn generated_always_identities_refuse_given_values_with_oracle_numbers() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    rendered(
        &mut session,
        "CREATE TABLE tickets (id NUMBER GENERATED ALWAYS AS IDENTITY PRIMARY KEY, code VARCHAR2(10))",
    );
    rendered(&mut session, "INSERT INTO tickets (code) VALUES ('a')");
    rendered(
        &mut session,
        "INSERT INTO tickets (id, code) VALUES (DEFAULT, 'b')",
    );
    let inserted = failure(
        &mut session,
        "INSERT INTO tickets (id, code) VALUES (5, 'c')",
    );
    assert_eq!(
        inserted.to_string(),
        "ORA-32795: cannot insert into a generated always identity column"
    );
    assert_eq!(inserted.hint(), None);
    let updated = failure(&mut session, "UPDATE tickets SET id = 9 WHERE code = 'a'");
    assert_eq!(
        updated.to_string(),
        "ORA-32796: cannot update a generated always identity column"
    );
    assert_eq!(
        rendered(&mut session, "SELECT id, code FROM tickets ORDER BY id"),
        "ID|CODE\n1|a\n2|b\nSELECT 2"
    );
    rendered(
        &mut session,
        "CREATE TABLE free (id NUMBER GENERATED BY DEFAULT ON NULL FOR INSERT ONLY AS IDENTITY PRIMARY KEY, code VARCHAR2(10))",
    );
    rendered(&mut session, "INSERT INTO free (id, code) VALUES (7, 'c')");
    rendered(&mut session, "INSERT INTO free (code) VALUES ('d')");
    assert_eq!(
        rendered(&mut session, "SELECT id FROM free ORDER BY id"),
        "ID\n7\n8\nSELECT 2"
    );
    assert_eq!(
        rendered(
            &mut session,
            "SELECT table_name, column_name, generation_type FROM user_tab_identity_cols ORDER BY table_name"
        ),
        "TABLE_NAME|COLUMN_NAME|GENERATION_TYPE\nFREE|ID|BY DEFAULT\nTICKETS|ID|ALWAYS\nSELECT 2"
    );
}

#[test]
fn identity_options_set_the_first_value_and_the_step_and_refuse_what_eggdb_cannot_keep() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    rendered(
        &mut session,
        "CREATE TABLE stepped (id NUMBER GENERATED BY DEFAULT AS IDENTITY (START WITH 100 INCREMENT BY 10) PRIMARY KEY, code VARCHAR2(5))",
    );
    rendered(
        &mut session,
        "CREATE TABLE exported (\"ID\" NUMBER GENERATED ALWAYS AS IDENTITY MINVALUE 1 MAXVALUE 9999999999999999999999999999 INCREMENT BY 1 START WITH 21 CACHE 20 NOORDER NOCYCLE NOKEEP NOSCALE NOT NULL ENABLE, \"CODE\" VARCHAR2(5), CONSTRAINT \"EXPORTED_PK\" PRIMARY KEY (\"ID\") USING INDEX ENABLE)",
    );
    rendered(
        &mut session,
        "CREATE TABLE falling (id NUMBER GENERATED AS IDENTITY (INCREMENT BY -1) PRIMARY KEY, code VARCHAR2(5))",
    );
    for table in ["stepped", "exported", "falling"] {
        rendered(
            &mut session,
            &format!("INSERT INTO {table} (code) VALUES ('a')"),
        );
        rendered(
            &mut session,
            &format!("INSERT INTO {table} (code) VALUES ('b')"),
        );
    }
    assert_eq!(
        rendered(&mut session, "SELECT id FROM stepped ORDER BY id"),
        "ID\n100\n110\nSELECT 2"
    );
    assert_eq!(
        rendered(&mut session, "SELECT id FROM exported ORDER BY id"),
        "ID\n21\n22\nSELECT 2"
    );
    assert_eq!(
        rendered(&mut session, "SELECT id FROM falling ORDER BY id"),
        "ID\n-2\n-1\nSELECT 2"
    );
    assert_eq!(
        rendered(
            &mut session,
            "SELECT identity_options FROM user_tab_identity_cols WHERE table_name = 'STEPPED'"
        ),
        "IDENTITY_OPTIONS\nSTART WITH: 100, INCREMENT BY: 10, MAX_VALUE: 9999999999999999999999999999, MIN_VALUE: 1, CYCLE_FLAG: N, CACHE_SIZE: 20, ORDER_FLAG: N, SCALE_FLAG: N, EXTEND_FLAG: N, SESSION_FLAG: N, KEEP_VALUE: N\nSELECT 1"
    );
    for (options, code) in [
        ("(INCREMENT BY 0)", 4002),
        ("(START WITH 0 MINVALUE 1)", 4006),
        ("(MINVALUE 5 MAXVALUE 5)", 4004),
        ("(MAXVALUE 1000)", 3001),
        ("(CYCLE)", 3001),
        ("(START WITH LIMIT VALUE)", 3001),
    ] {
        let error = failure(
            &mut session,
            &format!(
                "CREATE TABLE refused (id NUMBER GENERATED AS IDENTITY {options} PRIMARY KEY)"
            ),
        );
        assert_eq!(error.vendor_code(), Some(code), "{options}: {error}");
    }
}

#[test]
fn a_key_using_an_index_of_another_name_takes_it_over_when_the_columns_match() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    for statement in [
        "CREATE TABLE emp (id NUMBER(6), email VARCHAR2(25), dept NUMBER(4))",
        "INSERT INTO emp VALUES (1, 'ada@example.com', 10)",
        "CREATE UNIQUE INDEX emp_id_ix ON emp (id)",
        "ALTER TABLE emp ADD CONSTRAINT emp_pk PRIMARY KEY (id) USING INDEX emp_id_ix ENABLE",
        "CREATE INDEX emp_mail_dept_ix ON emp (email, dept)",
        "ALTER TABLE emp ADD (CONSTRAINT emp_mail_uk UNIQUE (email) USING INDEX emp_mail_dept_ix)",
    ] {
        rendered(&mut session, statement);
    }
    assert_eq!(
        rendered(
            &mut session,
            "SELECT index_name, uniqueness FROM user_indexes WHERE table_name = 'EMP' ORDER BY index_name"
        ),
        "INDEX_NAME|UNIQUENESS\nEMP_MAIL_DEPT_IX|NONUNIQUE\nEMP_MAIL_UK|UNIQUE\nSELECT 2"
    );
    assert_eq!(
        rendered(
            &mut session,
            "SELECT constraint_name, constraint_type FROM user_constraints WHERE table_name = 'EMP' AND constraint_type IN ('P', 'U') ORDER BY constraint_name"
        ),
        "CONSTRAINT_NAME|CONSTRAINT_TYPE\nEMP_MAIL_UK|U\nEMP_PK|P\nSELECT 2"
    );
    let not_leading = failure(
        &mut session,
        "ALTER TABLE emp ADD CONSTRAINT emp_dept_uk UNIQUE (dept) USING INDEX emp_mail_dept_ix",
    );
    assert_eq!(not_leading.vendor_code(), Some(14196));
    let missing = failure(
        &mut session,
        "ALTER TABLE emp ADD CONSTRAINT emp_dept_uk UNIQUE (dept) USING INDEX no_such_ix",
    );
    assert_eq!(missing.vendor_code(), Some(1418));
    let duplicate = failure(&mut session, "INSERT INTO emp VALUES (1, 'grace@example.com', 20)");
    assert_eq!(duplicate.vendor_code(), Some(1));
}
