use crate::dialect::Dialect;
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Session, SqlError};

fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

fn answer(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => output,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn refusal(session: &mut Session<'_>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error,
    }
}

fn rows(session: &mut Session<'_>, sql: &str) -> Vec<String> {
    let rendered = answer(session, sql);
    let lines: Vec<&str> = rendered.lines().collect();
    match lines.len() {
        0..=2 => Vec::new(),
        count => lines[1..count - 1]
            .iter()
            .map(|line| (*line).to_owned())
            .collect(),
    }
}

fn batch(session: &mut Session<'_>, text: &str) {
    session
        .execute_batch(text, &mut |step| step.outcome.is_ok())
        .unwrap_or_else(|error| panic!("{text}: {error}"));
}

const FAMILY: &str = "CREATE TABLE parent (id integer PRIMARY KEY);
CREATE TABLE child (id integer PRIMARY KEY, parent_id integer DEFAULT 0 REFERENCES parent ON DELETE CASCADE ON UPDATE CASCADE, note text);
CREATE TABLE log (seq serial PRIMARY KEY, what text);
INSERT INTO parent VALUES (0), (1), (2);
INSERT INTO child VALUES (10, 1, NULL), (11, 1, NULL), (20, 2, NULL)";

const LOG_ROW: &str = "CREATE FUNCTION log_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        INSERT INTO log (what) VALUES (format('%s %s %s', TG_WHEN, TG_OP, OLD.id));
        RETURN OLD;
    END IF;
    INSERT INTO log (what) VALUES (format('%s %s %s %s>%s', TG_WHEN, TG_OP, NEW.id, OLD.parent_id, NEW.parent_id));
    RETURN NEW;
END $$";

fn postgresql_family(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::new(store);
    answer(&mut session, FAMILY);
    answer(&mut session, LOG_ROW);
    session
}

#[test]
fn postgresql_cascaded_rows_fire_row_triggers_and_statement_triggers_once() {
    let store = MemoryStore::default();
    let mut session = postgresql_family(&store);
    answer(
        &mut session,
        "CREATE FUNCTION log_statement() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO log (what) VALUES (format('%s STATEMENT %s', TG_WHEN, TG_OP));
    RETURN NULL;
END $$",
    );
    answer(
        &mut session,
        "CREATE TRIGGER child_row AFTER UPDATE OR DELETE ON child FOR EACH ROW EXECUTE FUNCTION log_row()",
    );
    answer(
        &mut session,
        "CREATE TRIGGER child_statement AFTER UPDATE OR DELETE ON child FOR EACH STATEMENT EXECUTE FUNCTION log_statement()",
    );
    answer(&mut session, "UPDATE parent SET id = 3 WHERE id = 2");
    answer(&mut session, "DELETE FROM parent WHERE id IN (1, 3)");
    assert_eq!(
        rows(&mut session, "SELECT what FROM log ORDER BY what"),
        vec![
            "AFTER DELETE 10",
            "AFTER DELETE 11",
            "AFTER DELETE 20",
            "AFTER STATEMENT DELETE",
            "AFTER STATEMENT UPDATE",
            "AFTER UPDATE 20 2>3",
        ]
    );
    assert!(rows(&mut session, "SELECT id FROM child").is_empty());
}

#[test]
fn postgresql_cascaded_after_row_triggers_fire_once_their_action_has_written_every_row() {
    let store = MemoryStore::default();
    let mut session = postgresql_family(&store);
    answer(
        &mut session,
        "CREATE FUNCTION log_remaining() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO log (what) VALUES (format('%s %s %s remaining %s', TG_WHEN, TG_OP, OLD.id,
        (SELECT count(*) FROM child WHERE parent_id = OLD.parent_id)));
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END $$",
    );
    answer(
        &mut session,
        "CREATE TRIGGER child_before BEFORE DELETE OR UPDATE ON child FOR EACH ROW EXECUTE FUNCTION log_remaining()",
    );
    answer(
        &mut session,
        "CREATE TRIGGER child_after AFTER DELETE OR UPDATE ON child FOR EACH ROW EXECUTE FUNCTION log_remaining()",
    );
    answer(&mut session, "DELETE FROM parent WHERE id = 1");
    answer(&mut session, "INSERT INTO parent VALUES (1)");
    answer(&mut session, "INSERT INTO child VALUES (12, 2, NULL)");
    answer(&mut session, "UPDATE parent SET id = 3 WHERE id = 2");
    assert_eq!(
        rows(&mut session, "SELECT what FROM log ORDER BY seq"),
        vec![
            "BEFORE DELETE 10 remaining 2",
            "BEFORE DELETE 11 remaining 1",
            "AFTER DELETE 10 remaining 0",
            "AFTER DELETE 11 remaining 0",
            "BEFORE UPDATE 12 remaining 2",
            "BEFORE UPDATE 20 remaining 1",
            "AFTER UPDATE 12 remaining 0",
            "AFTER UPDATE 20 remaining 0",
        ]
    );
}

#[test]
fn postgresql_set_null_and_set_default_fire_before_update_triggers_that_change_the_row() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE parent (id integer PRIMARY KEY);
         CREATE TABLE nulled (id integer PRIMARY KEY, parent_id integer REFERENCES parent ON DELETE SET NULL, note text);
         CREATE TABLE defaulted (id integer PRIMARY KEY, parent_id integer DEFAULT 0 REFERENCES parent ON DELETE SET DEFAULT, note text);
         INSERT INTO parent VALUES (0), (1);
         INSERT INTO nulled VALUES (1, 1, NULL);
         INSERT INTO defaulted VALUES (1, 1, NULL)",
    );
    answer(
        &mut session,
        "CREATE FUNCTION mark_orphan() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.note := format('was %s', OLD.parent_id);
    RETURN NEW;
END $$",
    );
    answer(
        &mut session,
        "CREATE TRIGGER nulled_mark BEFORE UPDATE OF parent_id ON nulled FOR EACH ROW EXECUTE FUNCTION mark_orphan()",
    );
    answer(
        &mut session,
        "CREATE TRIGGER defaulted_mark BEFORE UPDATE ON defaulted FOR EACH ROW EXECUTE FUNCTION mark_orphan()",
    );
    answer(&mut session, "DELETE FROM parent WHERE id = 1");
    assert_eq!(
        rows(
            &mut session,
            "SELECT coalesce(parent_id::text, 'null'), note FROM nulled"
        ),
        vec!["null|was 1"]
    );
    assert_eq!(
        rows(&mut session, "SELECT parent_id, note FROM defaulted"),
        vec!["0|was 1"]
    );
}

#[test]
fn postgresql_update_of_other_columns_does_not_fire_for_a_cascaded_key() {
    let store = MemoryStore::default();
    let mut session = postgresql_family(&store);
    answer(
        &mut session,
        "CREATE TRIGGER child_note AFTER UPDATE OF note ON child FOR EACH ROW EXECUTE FUNCTION log_row()",
    );
    answer(&mut session, "UPDATE parent SET id = 5 WHERE id = 1");
    assert!(rows(&mut session, "SELECT what FROM log").is_empty());
    assert_eq!(
        rows(
            &mut session,
            "SELECT id FROM child WHERE parent_id = 5 ORDER BY id"
        ),
        vec!["10", "11"]
    );
}

#[test]
fn postgresql_a_before_trigger_that_skips_a_cascaded_row_fails_the_statement() {
    let store = MemoryStore::default();
    let mut session = postgresql_family(&store);
    answer(
        &mut session,
        "CREATE FUNCTION keep() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$",
    );
    answer(
        &mut session,
        "CREATE TRIGGER child_keep BEFORE DELETE ON child FOR EACH ROW EXECUTE FUNCTION keep()",
    );
    let error = refusal(&mut session, "DELETE FROM parent WHERE id = 1");
    assert_eq!(error.state().code(), "23503", "{error}");
    assert_eq!(
        rows(&mut session, "SELECT id FROM parent ORDER BY id"),
        vec!["0", "1", "2"]
    );
    assert_eq!(rows(&mut session, "SELECT count(*) FROM child"), vec!["3"]);
}

#[test]
fn postgresql_a_before_trigger_that_changes_its_own_cascaded_row_is_refused() {
    let store = MemoryStore::default();
    let mut session = postgresql_family(&store);
    answer(
        &mut session,
        "CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE child SET note = 'touched' WHERE id = OLD.id;
    RETURN OLD;
END $$",
    );
    answer(
        &mut session,
        "CREATE TRIGGER child_touch BEFORE DELETE ON child FOR EACH ROW EXECUTE FUNCTION touch()",
    );
    let error = refusal(&mut session, "DELETE FROM parent WHERE id = 2");
    assert_eq!(error.state().code(), "27000", "{error}");
    assert_eq!(rows(&mut session, "SELECT count(*) FROM child"), vec!["3"]);
}

#[test]
fn postgresql_cascades_that_set_off_themselves_stop_at_the_depth_limit() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE node (id integer PRIMARY KEY);
         CREATE TABLE leaf (id integer PRIMARY KEY, node_id integer REFERENCES node ON DELETE CASCADE);
         INSERT INTO node VALUES (1);
         INSERT INTO leaf VALUES (1, 1)",
    );
    answer(
        &mut session,
        "CREATE FUNCTION regrow() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO node VALUES (OLD.id + 1);
    INSERT INTO leaf VALUES (OLD.id + 1, OLD.id + 1);
    DELETE FROM node WHERE id = OLD.id + 1;
    RETURN OLD;
END $$",
    );
    answer(
        &mut session,
        "CREATE TRIGGER leaf_regrow AFTER DELETE ON leaf FOR EACH ROW EXECUTE FUNCTION regrow()",
    );
    let error = refusal(&mut session, "DELETE FROM node WHERE id = 1");
    assert_eq!(error.state().code(), "54001", "{error}");
    assert_eq!(rows(&mut session, "SELECT id FROM leaf"), vec!["1"]);
}

#[test]
fn sqlite_cascaded_deletes_fire_delete_triggers() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    answer(&mut session, "PRAGMA foreign_keys = ON");
    answer(
        &mut session,
        "CREATE TABLE parent (id INTEGER PRIMARY KEY);
         CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent (id) ON DELETE CASCADE ON UPDATE SET NULL);
         CREATE TABLE log (what TEXT);
         INSERT INTO parent VALUES (1), (2);
         INSERT INTO child VALUES (10, 1), (20, 2)",
    );
    answer(
        &mut session,
        "CREATE TRIGGER child_gone AFTER DELETE ON child BEGIN
           INSERT INTO log VALUES ('gone ' || old.id);
         END",
    );
    answer(
        &mut session,
        "CREATE TRIGGER child_moved AFTER UPDATE ON child BEGIN
           INSERT INTO log VALUES ('moved ' || new.id || ' ' || ifnull(new.parent_id, 'null'));
         END",
    );
    answer(&mut session, "DELETE FROM parent WHERE id = 1");
    answer(&mut session, "UPDATE parent SET id = 3 WHERE id = 2");
    assert_eq!(
        rows(&mut session, "SELECT what FROM log ORDER BY what"),
        vec!["gone 10", "moved 20 null"]
    );
}

#[test]
fn sqlite_cascaded_after_row_triggers_fire_as_each_row_is_written() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    answer(&mut session, "PRAGMA foreign_keys = ON");
    answer(
        &mut session,
        "CREATE TABLE parent (id INTEGER PRIMARY KEY);
         CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent (id) ON DELETE CASCADE);
         CREATE TABLE log (what TEXT);
         INSERT INTO parent VALUES (1);
         INSERT INTO child VALUES (10, 1), (11, 1)",
    );
    answer(
        &mut session,
        "CREATE TRIGGER child_gone AFTER DELETE ON child BEGIN
           INSERT INTO log VALUES ('gone ' || old.id || ' remaining ' || (SELECT count(*) FROM child WHERE parent_id = old.parent_id));
         END",
    );
    answer(&mut session, "DELETE FROM parent WHERE id = 1");
    assert_eq!(
        rows(&mut session, "SELECT what FROM log ORDER BY what"),
        vec!["gone 10 remaining 1", "gone 11 remaining 0"]
    );
}

fn mysql_family(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::MySql);
    answer(
        &mut session,
        "CREATE TABLE parent (id INT PRIMARY KEY, label VARCHAR(20));
         CREATE TABLE child (id INT PRIMARY KEY, parent_id INT, FOREIGN KEY (parent_id) REFERENCES parent (id) ON DELETE CASCADE);
         CREATE TABLE history (seq INT AUTO_INCREMENT PRIMARY KEY, note VARCHAR(60));
         INSERT INTO parent VALUES (1, 'one'), (2, 'two');
         INSERT INTO child VALUES (10, 1), (20, 2)",
    );
    session
}

#[test]
fn mysql_cascaded_foreign_key_actions_do_not_activate_triggers() {
    let store = MemoryStore::default();
    let mut session = mysql_family(&store);
    answer(
        &mut session,
        "CREATE TRIGGER child_gone AFTER DELETE ON child FOR EACH ROW
           INSERT INTO history (note) VALUES (CONCAT('gone ', OLD.id))",
    );
    answer(&mut session, "DELETE FROM parent WHERE id = 1");
    assert!(rows(&mut session, "SELECT note FROM history").is_empty());
    assert_eq!(rows(&mut session, "SELECT id FROM child"), vec!["20"]);
    answer(&mut session, "DELETE FROM child WHERE id = 20");
    assert_eq!(
        rows(&mut session, "SELECT note FROM history"),
        vec!["gone 20"]
    );
}

#[test]
fn mysql_replace_fires_insert_and_delete_triggers_in_mysql_order() {
    let store = MemoryStore::default();
    let mut session = mysql_family(&store);
    answer(
        &mut session,
        "CREATE TRIGGER p_bi BEFORE INSERT ON parent FOR EACH ROW
           INSERT INTO history (note) VALUES (CONCAT('before insert ', NEW.label))",
    );
    answer(
        &mut session,
        "CREATE TRIGGER p_bd BEFORE DELETE ON parent FOR EACH ROW
           INSERT INTO history (note) VALUES (CONCAT('before delete ', OLD.label))",
    );
    answer(
        &mut session,
        "CREATE TRIGGER p_ad AFTER DELETE ON parent FOR EACH ROW
           INSERT INTO history (note) VALUES (CONCAT('after delete ', OLD.label))",
    );
    answer(
        &mut session,
        "CREATE TRIGGER p_ai AFTER INSERT ON parent FOR EACH ROW
           INSERT INTO history (note) VALUES (CONCAT('after insert ', NEW.label))",
    );
    answer(&mut session, "REPLACE INTO parent VALUES (2, 'deux')");
    assert_eq!(
        rows(&mut session, "SELECT note FROM history ORDER BY seq"),
        vec![
            "before insert deux",
            "before delete two",
            "after delete two",
            "after insert deux",
        ]
    );
    assert_eq!(rows(&mut session, "SELECT id FROM child"), vec!["10"]);
}

fn sqlserver_family(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::SqlServer);
    batch(
        &mut session,
        "CREATE TABLE dbo.parent (id int PRIMARY KEY);
         CREATE TABLE dbo.child (id int PRIMARY KEY, parent_id int REFERENCES dbo.parent (id) ON DELETE CASCADE);
         CREATE TABLE dbo.audit (removed int);
         INSERT INTO dbo.parent VALUES (1), (2);
         INSERT INTO dbo.child VALUES (10, 1), (11, 1), (20, 2);",
    );
    session
}

#[test]
fn sqlserver_cascaded_rows_reach_one_after_trigger_call() {
    let store = MemoryStore::default();
    let mut session = sqlserver_family(&store);
    batch(
        &mut session,
        "CREATE TRIGGER dbo.child_removed ON dbo.child AFTER DELETE AS
         BEGIN
           SET NOCOUNT ON;
           INSERT INTO dbo.audit (removed) SELECT count(*) FROM deleted;
         END",
    );
    batch(&mut session, "DELETE FROM dbo.parent");
    assert_eq!(
        rows(&mut session, "SELECT removed FROM dbo.audit"),
        vec!["3"]
    );
}

#[test]
fn sqlserver_instead_of_delete_on_a_cascaded_table_is_refused() {
    let store = MemoryStore::default();
    let mut session = sqlserver_family(&store);
    batch(
        &mut session,
        "CREATE TRIGGER dbo.child_kept ON dbo.child INSTEAD OF DELETE AS
           INSERT INTO dbo.audit (removed) SELECT count(*) FROM deleted;",
    );
    let error = refusal(&mut session, "DELETE FROM dbo.parent WHERE id = 1");
    assert!(error.message().contains("INSTEAD OF DELETE"), "{error}");
    assert_eq!(
        rows(&mut session, "SELECT count(*) FROM dbo.child"),
        vec!["3"]
    );
    assert!(rows(&mut session, "SELECT removed FROM dbo.audit").is_empty());
}

#[test]
fn postgresql_transition_tables_hold_the_rows_cascades_changed() {
    let store = MemoryStore::default();
    let mut session = postgresql_family(&store);
    answer(
        &mut session,
        "CREATE FUNCTION log_gone() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO log (what)
        SELECT format('%s gone %s-%s %s', TG_OP, min(id), max(id), count(*)) FROM gone;
    RETURN NULL;
END $$",
    );
    answer(
        &mut session,
        "CREATE TRIGGER child_gone AFTER DELETE ON child REFERENCING OLD TABLE AS gone \
         FOR EACH STATEMENT EXECUTE FUNCTION log_gone()",
    );
    answer(&mut session, "DELETE FROM parent WHERE id = 1");
    assert_eq!(
        rows(&mut session, "SELECT what FROM log"),
        vec!["DELETE gone 10-11 2"]
    );
}
