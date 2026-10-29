use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Parameter, Session, SqlError, SqlType};

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

fn prepared(session: &mut Session<'_, MemoryStore>, sql: &str, values: &[&str]) -> String {
    let parameters: Vec<Parameter> = values
        .iter()
        .map(|value| Parameter {
            declared_type: Some(SqlType::Varchar(None)),
            value: Some((*value).to_owned()),
        })
        .collect();
    let mut outputs = Vec::new();
    session
        .execute_prepared(sql, &parameters, &mut |result| {
            outputs.push(render(&result));
            true
        })
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    outputs.pop().unwrap_or_default()
}

const FLYWAY_LOCK: &str = "77433833903597";

#[test]
fn flyways_session_lock_excludes_a_second_session_until_it_is_unlocked() {
    let store = MemoryStore::default();
    let mut first = Session::new(&store);
    let mut second = Session::new(&store);
    let try_lock = format!("SELECT pg_try_advisory_lock({FLYWAY_LOCK})");
    let unlock = format!("SELECT pg_advisory_unlock({FLYWAY_LOCK})");
    assert_eq!(
        last(&mut first, &try_lock),
        "pg_try_advisory_lock\nt\nSELECT 1"
    );
    assert_eq!(
        last(&mut second, &try_lock),
        "pg_try_advisory_lock\nf\nSELECT 1"
    );
    assert_eq!(last(&mut second, &unlock), "pg_advisory_unlock\nf\nSELECT 1");
    assert_eq!(last(&mut first, &unlock), "pg_advisory_unlock\nt\nSELECT 1");
    assert_eq!(
        last(&mut second, &try_lock),
        "pg_try_advisory_lock\nt\nSELECT 1"
    );
    drop(second);
    assert_eq!(
        last(&mut first, &try_lock),
        "pg_try_advisory_lock\nt\nSELECT 1"
    );
    assert_eq!(
        last(&mut first, "SELECT pg_advisory_unlock_all()"),
        "pg_advisory_unlock_all\n\nSELECT 1"
    );
}

#[test]
fn a_transaction_lock_lasts_until_its_transaction_ends() {
    let store = MemoryStore::default();
    let mut first = Session::new(&store);
    let mut second = Session::new(&store);
    let try_lock = format!("SELECT pg_try_advisory_xact_lock({FLYWAY_LOCK})");
    assert_eq!(
        last(&mut first, &try_lock),
        "pg_try_advisory_xact_lock\nt\nSELECT 1"
    );
    assert_eq!(
        last(&mut second, &try_lock),
        "pg_try_advisory_xact_lock\nt\nSELECT 1"
    );
    last(&mut first, "BEGIN");
    assert_eq!(
        last(&mut first, &try_lock),
        "pg_try_advisory_xact_lock\nt\nSELECT 1"
    );
    assert_eq!(
        last(&mut second, &try_lock),
        "pg_try_advisory_xact_lock\nf\nSELECT 1"
    );
    last(&mut first, "COMMIT");
    assert_eq!(
        last(&mut second, &try_lock),
        "pg_try_advisory_xact_lock\nt\nSELECT 1"
    );
    last(&mut second, "BEGIN");
    last(&mut second, "SELECT pg_advisory_xact_lock(1, 2)");
    assert_eq!(
        last(&mut first, "SELECT pg_try_advisory_lock(1, 2)"),
        "pg_try_advisory_lock\nf\nSELECT 1"
    );
    last(&mut second, "ROLLBACK");
    assert_eq!(
        last(&mut first, "SELECT pg_try_advisory_lock(1, 2)"),
        "pg_try_advisory_lock\nt\nSELECT 1"
    );
    assert_eq!(
        state(&mut first, "SELECT pg_try_advisory_lock(1, 2, 3)"),
        "42883"
    );
}

#[test]
fn set_config_takes_its_setting_and_value_from_parameters() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "CREATE SCHEMA s");
    assert_eq!(
        prepared(
            &mut session,
            "SELECT set_config('search_path', $1, false)",
            &["\"s\",\"$user\", public"]
        ),
        "set_config\n\"s\",\"$user\", public\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT current_schema"),
        "current_schema\ns\nSELECT 1"
    );
    assert_eq!(
        prepared(
            &mut session,
            "SELECT set_config($1, $2, false)",
            &["search_path", "\"$user\", public"]
        ),
        "set_config\n\"$user\", public\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT current_schema"),
        "current_schema\npublic\nSELECT 1"
    );
}

#[test]
fn set_role_to_the_sessions_own_user_changes_nothing() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(last(&mut session, "SET ROLE 'eggdb'"), "SET");
    assert_eq!(last(&mut session, "SET ROLE eggdb"), "SET");
    assert_eq!(last(&mut session, "SET SESSION ROLE NONE"), "SET");
    assert_eq!(last(&mut session, "RESET ROLE"), "RESET");
    last(&mut session, "BEGIN");
    assert_eq!(last(&mut session, "SET ROLE 'eggdb'"), "SET");
    last(&mut session, "COMMIT");
    assert_eq!(state(&mut session, "SET ROLE 'someone_else'"), "0A000");
    assert_eq!(
        last(&mut session, "SELECT current_user"),
        "current_user\neggdb\nSELECT 1"
    );
}

#[test]
fn flyways_empty_schema_query_finds_nothing_in_a_new_database_and_tables_after() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let query = "SELECT EXISTS (\n    SELECT c.oid FROM pg_catalog.pg_class c\n    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace\n    LEFT JOIN pg_catalog.pg_depend d ON d.objid = c.oid AND d.deptype = 'e'\n    WHERE  n.nspname = $1 AND d.objid IS NULL AND c.relkind IN ('r', 'v', 'S', 't')\n  UNION ALL\n    SELECT t.oid FROM pg_catalog.pg_type t\n    JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace\n    LEFT JOIN pg_catalog.pg_depend d ON d.objid = t.oid AND d.deptype = 'e'\n    WHERE n.nspname = $2 AND d.objid IS NULL AND t.typcategory NOT IN ('A', 'C')\n  UNION ALL\n    SELECT p.oid FROM pg_catalog.pg_proc p\n    JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace\n    LEFT JOIN pg_catalog.pg_depend d ON d.objid = p.oid AND d.deptype = 'e'\n    WHERE n.nspname = $3 AND d.objid IS NULL\n)";
    assert_eq!(
        prepared(&mut session, query, &["public", "public", "public"]),
        "exists\nf\nSELECT 1"
    );
    last(&mut session, "CREATE TABLE customers (id integer PRIMARY KEY)");
    assert_eq!(
        prepared(&mut session, query, &["public", "public", "public"]),
        "exists\nt\nSELECT 1"
    );
}

#[test]
fn flyways_history_table_is_created_filled_and_read_back() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "BEGIN");
    last(
        &mut session,
        "CREATE TABLE \"public\".\"flyway_schema_history\" (\n    \"installed_rank\" INT NOT NULL,\n    \"version\" VARCHAR(50),\n    \"description\" VARCHAR(200) NOT NULL,\n    \"type\" VARCHAR(20) NOT NULL,\n    \"script\" VARCHAR(1000) NOT NULL,\n    \"checksum\" INTEGER,\n    \"installed_by\" VARCHAR(100) NOT NULL,\n    \"installed_on\" TIMESTAMP NOT NULL DEFAULT now(),\n    \"execution_time\" INTEGER NOT NULL,\n    \"success\" BOOLEAN NOT NULL\n)",
    );
    last(
        &mut session,
        "ALTER TABLE \"public\".\"flyway_schema_history\" ADD CONSTRAINT \"flyway_schema_history_pk\" PRIMARY KEY (\"installed_rank\")",
    );
    last(
        &mut session,
        "CREATE INDEX \"flyway_schema_history_s_idx\" ON \"public\".\"flyway_schema_history\" (\"success\")",
    );
    last(&mut session, "COMMIT");
    let parameters = [
        Parameter {
            declared_type: Some(SqlType::Integer),
            value: Some("1".to_owned()),
        },
        Parameter {
            declared_type: Some(SqlType::Varchar(None)),
            value: Some("1".to_owned()),
        },
        Parameter {
            declared_type: Some(SqlType::Varchar(None)),
            value: Some("create shop".to_owned()),
        },
        Parameter {
            declared_type: Some(SqlType::Varchar(None)),
            value: Some("SQL".to_owned()),
        },
        Parameter {
            declared_type: Some(SqlType::Varchar(None)),
            value: Some("V1__create_shop.sql".to_owned()),
        },
        Parameter {
            declared_type: Some(SqlType::Integer),
            value: Some("1041414593".to_owned()),
        },
        Parameter {
            declared_type: Some(SqlType::Varchar(None)),
            value: Some("eggdb".to_owned()),
        },
        Parameter {
            declared_type: Some(SqlType::Integer),
            value: Some("4".to_owned()),
        },
        Parameter {
            declared_type: Some(SqlType::Boolean),
            value: Some("t".to_owned()),
        },
    ];
    session
        .execute_prepared(
            "INSERT INTO \"public\".\"flyway_schema_history\" (\"installed_rank\", \"version\", \"description\", \"type\", \"script\", \"checksum\", \"installed_by\", \"execution_time\", \"success\") VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &parameters,
            &mut |_| true,
        )
        .unwrap();
    assert_eq!(
        last(
            &mut session,
            "/*NO LOAD BALANCE*/\nSELECT \"installed_rank\",\"version\",\"description\",\"type\",\"script\",\"checksum\",\"installed_by\",\"execution_time\",\"success\" FROM \"public\".\"flyway_schema_history\" WHERE \"installed_rank\" > -1 ORDER BY \"installed_rank\""
        ),
        "installed_rank|version|description|type|script|checksum|installed_by|execution_time|success\n1|1|create shop|SQL|V1__create_shop.sql|1041414593|eggdb|4|t\nSELECT 1"
    );
}
