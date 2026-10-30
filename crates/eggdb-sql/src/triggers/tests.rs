use super::{
    TableTriggers, TriggerAction, TriggerDefinition, TriggerEvent, TriggerEvents, TriggerLevel,
    TriggerTiming,
};
use crate::dialect::Dialect;
use crate::encoding::{decode_triggers, encode_triggers};
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Session, SqlError};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

fn answer(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => output,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn refusal(session: &mut Session<'_, MemoryStore>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error,
    }
}

fn rows(session: &mut Session<'_, MemoryStore>, sql: &str) -> Vec<String> {
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

fn batch(session: &mut Session<'_, MemoryStore>, text: &str) {
    session
        .execute_batch(text, &mut |step| step.outcome.is_ok())
        .unwrap_or_else(|error| panic!("{text}: {error}"));
}

fn sample_trigger(name: &str, dialect: Dialect) -> TriggerDefinition {
    TriggerDefinition {
        name: name.to_owned(),
        dialect,
        timing: TriggerTiming::After,
        events: TriggerEvents::default()
            .with(TriggerEvent::Insert)
            .with(TriggerEvent::Update),
        columns: vec!["n".to_owned()],
        level: TriggerLevel::Row,
        condition: Some("NEW.n > 1".to_owned()),
        old_table: None,
        new_table: None,
        action: TriggerAction::Statements("BEGIN INSERT INTO log VALUES (NEW.n); END".to_owned()),
        enabled: true,
        text: format!("CREATE TRIGGER {name} AFTER UPDATE OF n ON t BEGIN SELECT 1; END"),
    }
}

#[test]
fn trigger_records_round_trip() {
    let triggers = TableTriggers {
        table: "t".to_owned(),
        triggers: vec![
            sample_trigger("first", Dialect::Sqlite),
            TriggerDefinition {
                enabled: false,
                ..sample_trigger("second", Dialect::MySql)
            },
        ],
    };
    let bytes = encode_triggers(&triggers);
    assert_eq!(decode_triggers("t", &bytes).ok(), Some(triggers));
}

#[test]
fn damaged_trigger_records_are_errors() {
    let triggers = TableTriggers {
        table: "t".to_owned(),
        triggers: vec![sample_trigger("first", Dialect::Sqlite)],
    };
    let bytes = encode_triggers(&triggers);
    for length in 0..bytes.len() {
        assert!(decode_triggers("t", &bytes[..length]).is_err(), "{length}");
    }
    let mut flipped = bytes.clone();
    for at in 0..flipped.len() {
        flipped[at] ^= 0xff;
        let _ = decode_triggers("t", &flipped);
        flipped[at] ^= 0xff;
    }
}

#[test]
fn trigger_events_keep_only_known_bits() {
    assert_eq!(TriggerEvents::from_bits(0), None);
    assert_eq!(TriggerEvents::from_bits(0b1_0000), None);
    let events = TriggerEvents::from_bits(0b0101).expect("known bits");
    assert_eq!(
        events.listed(),
        vec![TriggerEvent::Insert, TriggerEvent::Delete]
    );
}

fn sqlite_session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::with_dialect(store, Dialect::Sqlite);
    answer(
        &mut session,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, label TEXT);
         CREATE TABLE log (what TEXT, id INTEGER, n INTEGER)",
    );
    session
}

#[test]
fn sqlite_after_insert_trigger_logs_new_rows() {
    let store = MemoryStore::default();
    let mut session = sqlite_session(&store);
    answer(
        &mut session,
        "CREATE TRIGGER t_added AFTER INSERT ON t BEGIN
           INSERT INTO log VALUES ('added', NEW.id, NEW.n);
         END",
    );
    answer(&mut session, "INSERT INTO t VALUES (1, 10, 'a'), (2, 20, 'b')");
    assert_eq!(
        rows(&mut session, "SELECT what, id, n FROM log ORDER BY id"),
        vec!["added|1|10", "added|2|20"]
    );
}

#[test]
fn sqlite_update_of_column_and_when_choose_the_rows() {
    let store = MemoryStore::default();
    let mut session = sqlite_session(&store);
    answer(&mut session, "INSERT INTO t VALUES (1, 10, 'a'), (2, 20, 'b')");
    answer(
        &mut session,
        "CREATE TRIGGER n_grew AFTER UPDATE OF n ON t WHEN NEW.n > OLD.n BEGIN
           INSERT INTO log VALUES ('grew', OLD.id, NEW.n - OLD.n);
         END",
    );
    answer(&mut session, "UPDATE t SET label = 'z'");
    answer(&mut session, "UPDATE t SET n = n + 5 WHERE id = 1");
    answer(&mut session, "UPDATE t SET n = n - 5 WHERE id = 2");
    assert_eq!(
        rows(&mut session, "SELECT what, id, n FROM log"),
        vec!["grew|1|5"]
    );
}

#[test]
fn sqlite_raise_abort_stops_the_statement_and_ignore_skips_the_row() {
    let store = MemoryStore::default();
    let mut session = sqlite_session(&store);
    answer(
        &mut session,
        "CREATE TRIGGER no_negative BEFORE INSERT ON t WHEN NEW.n < 0 BEGIN
           SELECT RAISE(ABORT, 'n must not be negative');
         END;
         CREATE TRIGGER skip_zero BEFORE INSERT ON t WHEN NEW.n = 0 BEGIN
           SELECT RAISE(IGNORE);
         END",
    );
    let error = refusal(&mut session, "INSERT INTO t VALUES (1, -1, 'a')");
    assert_eq!(error.message(), "n must not be negative");
    answer(&mut session, "INSERT INTO t VALUES (2, 0, 'b'), (3, 3, 'c')");
    assert_eq!(rows(&mut session, "SELECT id FROM t"), vec!["3"]);
}

#[test]
fn sqlite_triggers_do_not_recurse() {
    let store = MemoryStore::default();
    let mut session = sqlite_session(&store);
    answer(
        &mut session,
        "CREATE TRIGGER again AFTER INSERT ON t BEGIN
           INSERT INTO t VALUES (NEW.id + 100, NEW.n, 'copy');
         END",
    );
    answer(&mut session, "INSERT INTO t VALUES (1, 1, 'a')");
    assert_eq!(
        rows(&mut session, "SELECT id FROM t ORDER BY id"),
        vec!["1", "101"]
    );
}

#[test]
fn sqlite_instead_of_trigger_makes_a_view_writable() {
    let store = MemoryStore::default();
    let mut session = sqlite_session(&store);
    answer(
        &mut session,
        "CREATE VIEW labels AS SELECT id, label FROM t;
         CREATE TRIGGER labels_insert INSTEAD OF INSERT ON labels BEGIN
           INSERT INTO t VALUES (NEW.id, 0, upper(NEW.label));
         END",
    );
    answer(&mut session, "INSERT INTO labels VALUES (7, 'seven')");
    assert_eq!(
        rows(&mut session, "SELECT id, n, label FROM t"),
        vec!["7|0|SEVEN"]
    );
}

#[test]
fn sqlite_dropping_the_table_drops_its_triggers() {
    let store = MemoryStore::default();
    let mut session = sqlite_session(&store);
    answer(
        &mut session,
        "CREATE TRIGGER t_added AFTER INSERT ON t BEGIN INSERT INTO log VALUES ('x', 1, 1); END",
    );
    answer(&mut session, "DROP TABLE t");
    answer(&mut session, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, label TEXT)");
    answer(&mut session, "INSERT INTO t VALUES (1, 1, 'a')");
    assert!(rows(&mut session, "SELECT what FROM log").is_empty());
    let error = refusal(&mut session, "DROP TRIGGER t_added");
    assert_eq!(error.message(), "no such trigger: t_added");
    answer(&mut session, "DROP TRIGGER IF EXISTS t_added");
}

#[test]
fn raise_outside_a_trigger_is_an_error() {
    let store = MemoryStore::default();
    let mut session = sqlite_session(&store);
    let error = refusal(&mut session, "SELECT RAISE(ABORT, 'no')");
    assert_eq!(
        error.message(),
        "RAISE() may only be used within a trigger-program"
    );
}

fn mysql_session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::with_dialect(store, Dialect::MySql);
    answer(
        &mut session,
        "CREATE TABLE account (id INT PRIMARY KEY, amount DECIMAL(10,2));
         CREATE TABLE history (note VARCHAR(40))",
    );
    session
}

#[test]
fn mysql_before_insert_sets_new_values() {
    let store = MemoryStore::default();
    let mut session = mysql_session(&store);
    answer(
        &mut session,
        "CREATE TRIGGER ins_sum BEFORE INSERT ON account
         FOR EACH ROW SET NEW.amount = NEW.amount * 2",
    );
    answer(&mut session, "INSERT INTO account VALUES (1, 14.98), (2, 1937.50)");
    assert_eq!(
        rows(&mut session, "SELECT amount FROM account ORDER BY id"),
        vec!["29.96", "3875.00"]
    );
}

#[test]
fn mysql_compound_body_checks_and_signals() {
    let store = MemoryStore::default();
    let mut session = mysql_session(&store);
    answer(
        &mut session,
        "CREATE TRIGGER upd_check BEFORE UPDATE ON account
         FOR EACH ROW
         BEGIN
           DECLARE floor DECIMAL(10,2) DEFAULT 0;
           IF NEW.amount < floor THEN
             SET NEW.amount = floor;
           ELSEIF NEW.amount > 100 THEN
             SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'amount too large';
           END IF;
         END",
    );
    answer(&mut session, "INSERT INTO account VALUES (1, 10)");
    answer(&mut session, "UPDATE account SET amount = -5 WHERE id = 1");
    assert_eq!(rows(&mut session, "SELECT amount FROM account"), vec!["0.00"]);
    let error = refusal(&mut session, "UPDATE account SET amount = 500 WHERE id = 1");
    assert_eq!(error.state().code(), "45000");
    assert_eq!(error.message(), "amount too large");
}

#[test]
fn mysql_follows_orders_triggers_of_the_same_event() {
    let store = MemoryStore::default();
    let mut session = mysql_session(&store);
    answer(
        &mut session,
        "CREATE TRIGGER second AFTER INSERT ON account FOR EACH ROW
           INSERT INTO history VALUES ('second')",
    );
    answer(
        &mut session,
        "CREATE TRIGGER first AFTER INSERT ON account FOR EACH ROW PRECEDES second
           INSERT INTO history VALUES ('first')",
    );
    answer(&mut session, "INSERT INTO account VALUES (1, 1)");
    assert_eq!(
        rows(&mut session, "SELECT note FROM history"),
        vec!["first", "second"]
    );
}

#[test]
fn mysql_duplicate_key_update_fires_update_triggers() {
    let store = MemoryStore::default();
    let mut session = mysql_session(&store);
    answer(
        &mut session,
        "CREATE TRIGGER added AFTER INSERT ON account FOR EACH ROW
           INSERT INTO history VALUES ('insert');
         CREATE TRIGGER changed AFTER UPDATE ON account FOR EACH ROW
           INSERT INTO history VALUES (CONCAT('update ', OLD.amount, ' to ', NEW.amount))",
    );
    answer(&mut session, "INSERT INTO account VALUES (1, 1)");
    answer(
        &mut session,
        "INSERT INTO account VALUES (1, 5) ON DUPLICATE KEY UPDATE amount = VALUES(amount)",
    );
    assert_eq!(
        rows(&mut session, "SELECT note FROM history"),
        vec!["insert", "update 1.00 to 5.00"]
    );
}

#[test]
fn mysql_duplicate_trigger_names_are_refused() {
    let store = MemoryStore::default();
    let mut session = mysql_session(&store);
    let create = "CREATE TRIGGER t1 AFTER DELETE ON account FOR EACH ROW
                    INSERT INTO history VALUES ('gone')";
    answer(&mut session, create);
    let error = refusal(&mut session, create);
    assert_eq!(error.message(), "Trigger already exists");
    answer(
        &mut session,
        "CREATE TRIGGER IF NOT EXISTS t1 AFTER DELETE ON account FOR EACH ROW
           INSERT INTO history VALUES ('gone')",
    );
}

fn sqlserver_session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::with_dialect(store, Dialect::SqlServer);
    batch(
        &mut session,
        "CREATE TABLE dbo.orders (id int PRIMARY KEY, qty int, status nvarchar(20));
         CREATE TABLE dbo.audit (action nvarchar(20), id int, qty int);",
    );
    session
}

#[test]
fn sqlserver_after_trigger_reads_inserted_and_deleted() {
    let store = MemoryStore::default();
    let mut session = sqlserver_session(&store);
    batch(
        &mut session,
        "CREATE TRIGGER dbo.orders_audit ON dbo.orders AFTER INSERT, UPDATE, DELETE AS
         BEGIN
           SET NOCOUNT ON;
           INSERT INTO dbo.audit (action, id, qty)
             SELECT 'new', id, qty FROM inserted;
           INSERT INTO dbo.audit (action, id, qty)
             SELECT 'old', id, qty FROM deleted;
         END",
    );
    batch(&mut session, "INSERT INTO dbo.orders VALUES (1, 5, N'open'), (2, 7, N'open')");
    batch(&mut session, "UPDATE dbo.orders SET qty = qty + 1 WHERE id = 1");
    batch(&mut session, "DELETE FROM dbo.orders WHERE id = 2");
    assert_eq!(
        rows(
            &mut session,
            "SELECT action, id, qty FROM dbo.audit ORDER BY action, id, qty"
        ),
        vec!["new|1|5", "new|1|6", "new|2|7", "old|1|5", "old|2|7"]
    );
}

#[test]
fn sqlserver_update_function_names_the_changed_columns() {
    let store = MemoryStore::default();
    let mut session = sqlserver_session(&store);
    batch(
        &mut session,
        "CREATE TRIGGER qty_changed ON dbo.orders AFTER UPDATE AS
         IF UPDATE(qty)
           INSERT INTO dbo.audit (action, id, qty) SELECT 'qty', id, qty FROM inserted;",
    );
    batch(&mut session, "INSERT INTO dbo.orders VALUES (1, 5, N'open')");
    batch(&mut session, "UPDATE dbo.orders SET status = N'shipped'");
    batch(&mut session, "UPDATE dbo.orders SET qty = 9");
    assert_eq!(
        rows(&mut session, "SELECT action, qty FROM dbo.audit"),
        vec!["qty|9"]
    );
}

#[test]
fn sqlserver_instead_of_delete_replaces_the_delete() {
    let store = MemoryStore::default();
    let mut session = sqlserver_session(&store);
    batch(
        &mut session,
        "CREATE TRIGGER soft_delete ON dbo.orders INSTEAD OF DELETE AS
           UPDATE dbo.orders SET status = N'cancelled'
           WHERE id IN (SELECT id FROM deleted);",
    );
    batch(&mut session, "INSERT INTO dbo.orders VALUES (1, 5, N'open')");
    batch(&mut session, "DELETE FROM dbo.orders WHERE id = 1");
    assert_eq!(
        rows(&mut session, "SELECT id, status FROM dbo.orders"),
        vec!["1|cancelled"]
    );
}

#[test]
fn sqlserver_disabled_triggers_do_not_fire() {
    let store = MemoryStore::default();
    let mut session = sqlserver_session(&store);
    batch(
        &mut session,
        "CREATE TRIGGER added ON dbo.orders AFTER INSERT AS
           INSERT INTO dbo.audit (action, id, qty) SELECT 'added', id, qty FROM inserted;",
    );
    batch(&mut session, "DISABLE TRIGGER added ON dbo.orders");
    batch(&mut session, "INSERT INTO dbo.orders VALUES (1, 5, N'open')");
    batch(&mut session, "ENABLE TRIGGER added ON dbo.orders");
    batch(&mut session, "INSERT INTO dbo.orders VALUES (2, 6, N'open')");
    assert_eq!(
        rows(&mut session, "SELECT action, id FROM dbo.audit"),
        vec!["added|2"]
    );
}

#[test]
fn sqlserver_create_trigger_must_start_the_batch() {
    let store = MemoryStore::default();
    let mut session = sqlserver_session(&store);
    let mut step_messages = Vec::new();
    let outcome = session.execute_batch(
        "SELECT 1;
         CREATE TRIGGER late ON dbo.orders AFTER INSERT AS SELECT 1;",
        &mut |step| {
            if let Err(error) = &step.outcome {
                step_messages.push(error.message().to_owned());
            }
            true
        },
    );
    let message = match outcome {
        Err(error) => Some(error.message().to_owned()),
        Ok(_) => step_messages.pop(),
    };
    assert_eq!(
        message.as_deref(),
        Some("'CREATE TRIGGER' must be the first statement in a query batch.")
    );
}

#[test]
fn postgresql_trigger_needs_its_function() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, "CREATE TABLE t (id integer PRIMARY KEY)");
    let error = refusal(
        &mut session,
        "CREATE TRIGGER t_changed AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION log_change()",
    );
    assert_eq!(error.state().code(), "42883");
}

#[test]
fn inconsistent_trigger_definitions_are_refused() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, "CREATE TABLE t (id integer PRIMARY KEY)");
    let error = refusal(
        &mut session,
        "CREATE TRIGGER t_cut BEFORE TRUNCATE ON t FOR EACH ROW EXECUTE FUNCTION f()",
    );
    assert_eq!(error.state().code(), "42P17");
}

#[test]
fn sqlite_schema_lists_triggers() {
    let store = MemoryStore::default();
    let mut session = sqlite_session(&store);
    answer(
        &mut session,
        "CREATE TRIGGER t_added AFTER INSERT ON t BEGIN INSERT INTO log VALUES ('x', 1, 1); END",
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT type, name, tbl_name, sql FROM sqlite_schema WHERE type = 'trigger'"
        ),
        vec![
            "trigger|t_added|t|CREATE TRIGGER t_added AFTER INSERT ON t BEGIN INSERT INTO log VALUES ('x', 1, 1); END"
        ]
    );
}

#[test]
fn postgresql_catalogs_show_triggers_of_every_dialect() {
    let store = MemoryStore::default();
    let mut sqlite = sqlite_session(&store);
    answer(
        &mut sqlite,
        "CREATE TRIGGER n_changed BEFORE UPDATE OF n ON t BEGIN SELECT 1; END",
    );
    drop(sqlite);
    let mut session = Session::new(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT tgname, tgrelid::regclass, tgtype, tgenabled, tgattr::text
             FROM pg_trigger"
        ),
        vec!["n_changed|t|19|O|2"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT trigger_name, event_manipulation, event_object_table, action_timing,
                    action_orientation
             FROM information_schema.triggers"
        ),
        vec!["n_changed|UPDATE|t|BEFORE|ROW"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT relhastriggers FROM pg_class WHERE relname = 't'"
        ),
        vec!["t"]
    );
}

#[test]
fn mysql_show_triggers_and_show_create_trigger() {
    let store = MemoryStore::default();
    let mut session = mysql_session(&store);
    let create = "CREATE TRIGGER ins_sum BEFORE INSERT ON account FOR EACH ROW SET NEW.amount = NEW.amount";
    answer(&mut session, create);
    assert_eq!(
        rows(&mut session, "SHOW TRIGGERS LIKE 'acc%'")
            .first()
            .map(|row| row.split('|').take(5).collect::<Vec<_>>().join("|")),
        Some("ins_sum|INSERT|account|SET NEW.amount = NEW.amount|BEFORE".to_owned())
    );
    assert!(rows(&mut session, "SHOW TRIGGERS LIKE 'other'").is_empty());
    assert_eq!(
        rows(&mut session, "SHOW CREATE TRIGGER ins_sum")
            .first()
            .and_then(|row| row.split('|').nth(2).map(str::to_owned)),
        Some(create.to_owned())
    );
}

#[test]
fn sqlserver_sys_triggers_lists_triggers() {
    let store = MemoryStore::default();
    let mut session = sqlserver_session(&store);
    batch(
        &mut session,
        "CREATE TRIGGER soft_delete ON dbo.orders INSTEAD OF DELETE AS
           UPDATE dbo.orders SET status = N'cancelled' WHERE id IN (SELECT id FROM deleted);",
    );
    batch(&mut session, "DISABLE TRIGGER soft_delete ON dbo.orders");
    assert_eq!(
        rows(
            &mut session,
            "SELECT t.name, OBJECT_NAME(t.parent_id), t.is_disabled, t.is_instead_of_trigger, o.type
             FROM sys.triggers AS t JOIN sys.objects AS o ON o.object_id = t.object_id"
        ),
        vec!["soft_delete|orders|1|1|TR"]
    );
}
