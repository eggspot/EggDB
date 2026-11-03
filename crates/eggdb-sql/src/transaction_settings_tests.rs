use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{QueryResult, Session, SqlError};

fn results(session: &mut Session<'_>, sql: &str) -> Result<Vec<QueryResult>, SqlError> {
    let mut results = Vec::new();
    session.execute(sql, &mut |result| results.push(result))?;
    Ok(results)
}

fn answer(session: &mut Session<'_>, sql: &str) -> String {
    match results(session, sql) {
        Ok(results) => results.last().map(render).unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn state(session: &mut Session<'_>, sql: &str) -> String {
    match results(session, sql) {
        Ok(results) => panic!("{sql} succeeded: {:?}", results.last().map(render)),
        Err(error) => error.state().code().to_owned(),
    }
}

fn shown(session: &mut Session<'_>, name: &str) -> String {
    answer(session, &format!("SHOW {name}"))
        .lines()
        .nth(1)
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn set_local_lasts_to_the_end_of_the_transaction_whether_it_commits_or_not() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, "SET app.mode = 'session'");
    answer(&mut session, "BEGIN");
    assert_eq!(answer(&mut session, "SET LOCAL app.mode = 'local'"), "SET");
    assert_eq!(shown(&mut session, "app.mode"), "local");
    answer(&mut session, "COMMIT");
    assert_eq!(shown(&mut session, "app.mode"), "session");
    answer(&mut session, "BEGIN");
    answer(&mut session, "SET LOCAL app.mode = 'local'");
    answer(&mut session, "ROLLBACK");
    assert_eq!(shown(&mut session, "app.mode"), "session");
    answer(&mut session, "BEGIN");
    answer(&mut session, "SET LOCAL TIME ZONE 'Europe/Paris'");
    assert_eq!(session.parameter("TimeZone").unwrap(), "Europe/Paris");
    answer(&mut session, "COMMIT");
    assert_eq!(session.parameter("TimeZone").unwrap(), "UTC");
}

#[test]
fn a_set_inside_a_transaction_is_undone_by_its_rollback_and_kept_by_its_commit() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, "SET app.mode = 'before'");
    answer(&mut session, "BEGIN");
    answer(&mut session, "SET app.mode = 'rolled back'");
    answer(&mut session, "SET search_path = elsewhere");
    answer(&mut session, "ROLLBACK");
    assert_eq!(shown(&mut session, "app.mode"), "before");
    assert_eq!(shown(&mut session, "search_path"), "\"$user\", public");
    answer(&mut session, "BEGIN");
    answer(&mut session, "SET app.mode = 'committed'");
    answer(&mut session, "SET LOCAL app.mode = 'masked'");
    assert_eq!(shown(&mut session, "app.mode"), "masked");
    answer(&mut session, "COMMIT");
    assert_eq!(shown(&mut session, "app.mode"), "committed");
    answer(&mut session, "BEGIN");
    answer(&mut session, "SET LOCAL app.mode = 'local'");
    answer(&mut session, "SET app.mode = 'plain'");
    answer(&mut session, "COMMIT");
    assert_eq!(shown(&mut session, "app.mode"), "plain");
    answer(&mut session, "BEGIN");
    answer(&mut session, "SET app.added = 'new'");
    answer(&mut session, "RESET ALL");
    answer(&mut session, "ROLLBACK");
    assert_eq!(shown(&mut session, "app.mode"), "plain");
    assert_eq!(state(&mut session, "SHOW app.added"), "42704");
}

#[test]
fn a_failed_transaction_undoes_its_settings_at_rollback_or_commit() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, "BEGIN");
    answer(&mut session, "SET app.failed = 'x'");
    assert_eq!(state(&mut session, "SELECT 1 / 0"), "22012");
    answer(&mut session, "ROLLBACK");
    assert_eq!(state(&mut session, "SHOW app.failed"), "42704");
    answer(&mut session, "BEGIN");
    answer(&mut session, "SET app.failed = 'y'");
    assert_eq!(state(&mut session, "SELECT 1 / 0"), "22012");
    assert_eq!(answer(&mut session, "COMMIT"), "ROLLBACK");
    assert_eq!(state(&mut session, "SHOW app.failed"), "42704");
}

#[test]
fn set_local_outside_a_transaction_warns_and_changes_nothing() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let set = results(&mut session, "SET LOCAL app.mode = 'ignored'").unwrap();
    assert_eq!(
        set.last().map(|result| result.notices.clone()),
        Some(vec![
            "WARNING:  SET LOCAL can only be used in transaction blocks".to_owned()
        ])
    );
    assert_eq!(state(&mut session, "SHOW app.mode"), "42704");
}

#[test]
fn set_config_with_is_local_true_lasts_to_the_end_of_the_transaction() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, "BEGIN");
    assert_eq!(
        answer(&mut session, "SELECT set_config('app.user_id', '42', true)"),
        "set_config\n42\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT current_setting('app.user_id')"),
        "current_setting\n42\nSELECT 1"
    );
    answer(&mut session, "COMMIT");
    assert_eq!(state(&mut session, "SHOW app.user_id"), "42704");
    assert_eq!(
        answer(&mut session, "SELECT set_config('app.user_id', '7', true)"),
        "set_config\n7\nSELECT 1"
    );
    assert_eq!(state(&mut session, "SHOW app.user_id"), "42704");
    answer(&mut session, "SELECT set_config('app.user_id', '9', false)");
    assert_eq!(shown(&mut session, "app.user_id"), "9");
}

#[test]
fn rolling_back_to_a_savepoint_undoes_the_settings_changed_after_it() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, "BEGIN");
    answer(&mut session, "SET app.a = '1'");
    answer(&mut session, "SAVEPOINT s");
    answer(&mut session, "SET LOCAL app.a = '2'");
    answer(&mut session, "SET app.b = '3'");
    answer(&mut session, "ROLLBACK TO SAVEPOINT s");
    assert_eq!(shown(&mut session, "app.a"), "1");
    assert_eq!(
        answer(&mut session, "SELECT current_setting('app.b', true) AS b"),
        "b\nNULL\nSELECT 1"
    );
    answer(&mut session, "SET LOCAL app.a = '4'");
    answer(&mut session, "RELEASE SAVEPOINT s");
    answer(&mut session, "COMMIT");
    assert_eq!(shown(&mut session, "app.a"), "1");
}

#[test]
fn set_local_in_an_implicit_transaction_lasts_until_it_ends() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session.start_implicit_transaction();
    assert_eq!(
        answer(
            &mut session,
            "SET LOCAL app.mode = 'implicit'; SHOW app.mode"
        ),
        "app.mode\nimplicit\nSHOW"
    );
    session.commit_implicit_transaction().unwrap();
    assert_eq!(state(&mut session, "SHOW app.mode"), "42704");
}
