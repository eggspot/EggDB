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

fn prepared<'s>(store: &'s MemoryStore, statements: &[&str]) -> Session<'s, MemoryStore> {
    let mut session = Session::new(store);
    for statement in statements {
        answer(&mut session, statement);
    }
    session
}

const ITEMS: &str = "CREATE TABLE items (id integer PRIMARY KEY, name text, price integer)";
const AUDIT: &str =
    "CREATE TABLE audit (seq serial PRIMARY KEY, op text, tbl text, detail text)";

#[test]
fn a_before_row_trigger_function_changes_the_new_row() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            ITEMS,
            "CREATE FUNCTION tidy() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.name := upper(NEW.name);
    IF NEW.price IS NULL THEN
        NEW.price := 0;
    END IF;
    RETURN NEW;
END $$",
            "CREATE TRIGGER items_tidy BEFORE INSERT OR UPDATE ON items FOR EACH ROW EXECUTE FUNCTION tidy()",
        ],
    );
    answer(&mut session, "INSERT INTO items (id, name) VALUES (1, 'pen')");
    answer(&mut session, "UPDATE items SET name = 'ink', price = 3 WHERE id = 1");
    assert_eq!(
        rows(&mut session, "SELECT id, name, price FROM items"),
        vec!["1|INK|3"]
    );
}

#[test]
fn a_before_row_trigger_returning_null_skips_the_row() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            ITEMS,
            "CREATE FUNCTION no_cheap() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        IF OLD.price > 10 THEN
            RETURN NULL;
        END IF;
        RETURN OLD;
    END IF;
    IF NEW.price < 5 THEN
        RETURN NULL;
    END IF;
    RETURN NEW;
END $$",
            "CREATE TRIGGER items_no_cheap BEFORE INSERT OR DELETE ON items FOR EACH ROW EXECUTE FUNCTION no_cheap()",
        ],
    );
    answer(
        &mut session,
        "INSERT INTO items VALUES (1, 'pen', 2), (2, 'book', 20), (3, 'cup', 7)",
    );
    assert_eq!(
        rows(&mut session, "SELECT id FROM items ORDER BY id"),
        vec!["2", "3"]
    );
    answer(&mut session, "DELETE FROM items");
    assert_eq!(rows(&mut session, "SELECT id FROM items"), vec!["2"]);
}

#[test]
fn an_after_row_trigger_function_sees_the_trigger_variables_and_arguments() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            ITEMS,
            AUDIT,
            "CREATE FUNCTION log_change() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    item text;
BEGIN
    IF TG_OP = 'DELETE' THEN
        item := OLD.name;
    ELSE
        item := NEW.name;
    END IF;
    INSERT INTO audit (op, tbl, detail)
    VALUES (TG_OP, TG_TABLE_SCHEMA || '.' || TG_TABLE_NAME,
            TG_WHEN || ' ' || TG_LEVEL || ' ' || TG_NAME || ' ' || TG_ARGV[0] || ' '
            || TG_NARGS::text || ' ' || item);
    RETURN NULL;
END $$",
            "CREATE TRIGGER items_log AFTER INSERT OR UPDATE OR DELETE ON items FOR EACH ROW EXECUTE FUNCTION log_change('tag')",
        ],
    );
    answer(&mut session, "INSERT INTO items VALUES (1, 'pen', 2)");
    answer(&mut session, "UPDATE items SET name = 'ink'");
    answer(&mut session, "DELETE FROM items");
    assert_eq!(
        rows(&mut session, "SELECT op, tbl, detail FROM audit ORDER BY seq"),
        vec![
            "INSERT|public.items|AFTER ROW items_log tag 1 pen",
            "UPDATE|public.items|AFTER ROW items_log tag 1 ink",
            "DELETE|public.items|AFTER ROW items_log tag 1 ink",
        ]
    );
}

#[test]
fn a_when_condition_decides_whether_the_function_runs() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            ITEMS,
            AUDIT,
            "CREATE FUNCTION price_changed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO audit (op, detail) VALUES (TG_OP, OLD.price::text || '->' || NEW.price::text);
    RETURN NEW;
END $$",
            "CREATE TRIGGER items_price BEFORE UPDATE ON items FOR EACH ROW WHEN (OLD.price IS DISTINCT FROM NEW.price) EXECUTE FUNCTION price_changed()",
            "INSERT INTO items VALUES (1, 'pen', 2), (2, 'cup', 7)",
        ],
    );
    answer(&mut session, "UPDATE items SET price = 9 WHERE id = 1");
    answer(&mut session, "UPDATE items SET name = 'mug' WHERE id = 2");
    assert_eq!(
        rows(&mut session, "SELECT op, detail FROM audit ORDER BY seq"),
        vec!["UPDATE|2->9"]
    );
}

#[test]
fn statement_level_trigger_functions_run_before_and_after_the_statement() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            ITEMS,
            AUDIT,
            "CREATE FUNCTION count_items() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO audit (op, detail)
    VALUES (TG_WHEN || ' ' || TG_LEVEL || ' ' || TG_OP, (SELECT count(*) FROM items)::text);
    RETURN NULL;
END $$",
            "CREATE TRIGGER items_before BEFORE INSERT OR TRUNCATE ON items FOR EACH STATEMENT EXECUTE FUNCTION count_items()",
            "CREATE TRIGGER items_after AFTER INSERT OR TRUNCATE ON items FOR EACH STATEMENT EXECUTE FUNCTION count_items()",
        ],
    );
    answer(&mut session, "INSERT INTO items VALUES (1, 'pen', 2), (2, 'cup', 7)");
    answer(&mut session, "TRUNCATE items");
    assert_eq!(
        rows(&mut session, "SELECT op, detail FROM audit ORDER BY seq"),
        vec![
            "BEFORE STATEMENT INSERT|0",
            "AFTER STATEMENT INSERT|2",
            "BEFORE STATEMENT TRUNCATE|2",
            "AFTER STATEMENT TRUNCATE|0",
        ]
    );
}

#[test]
fn an_error_in_a_trigger_function_undoes_the_statement() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            ITEMS,
            "CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.price < 0 THEN
        RAISE EXCEPTION 'negative price % for %', NEW.price, NEW.name
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$",
            "CREATE TRIGGER items_guard AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION guard()",
        ],
    );
    let error = refusal(
        &mut session,
        "INSERT INTO items VALUES (1, 'pen', 2), (2, 'cup', -1)",
    );
    assert_eq!(error.state().code(), "23514");
    assert!(error.message().contains("negative price -1 for cup"), "{error}");
    assert!(rows(&mut session, "SELECT id FROM items").is_empty());
}

#[test]
fn an_instead_of_trigger_function_writes_through_a_view() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            ITEMS,
            "CREATE VIEW cheap AS SELECT id, name FROM items WHERE price < 5",
            "CREATE FUNCTION add_cheap() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO items VALUES (NEW.id, NEW.name, 1);
    RETURN NEW;
END $$",
            "CREATE TRIGGER cheap_insert INSTEAD OF INSERT ON cheap FOR EACH ROW EXECUTE FUNCTION add_cheap()",
        ],
    );
    answer(&mut session, "INSERT INTO cheap VALUES (4, 'clip')");
    assert_eq!(
        rows(&mut session, "SELECT id, name, price FROM items"),
        vec!["4|clip|1"]
    );
}

#[test]
fn a_trigger_needs_a_function_that_returns_trigger() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            ITEMS,
            "CREATE FUNCTION one() RETURNS integer LANGUAGE sql AS 'SELECT 1'",
        ],
    );
    let error = refusal(
        &mut session,
        "CREATE TRIGGER items_one BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION one()",
    );
    assert_eq!(error.state().code(), "42P17");
    let error = refusal(
        &mut session,
        "CREATE TRIGGER items_missing BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION missing()",
    );
    assert_eq!(error.state().code(), "42883");
}

#[test]
fn dropping_a_trigger_function_needs_cascade() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            ITEMS,
            "CREATE FUNCTION keep() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
            "CREATE TRIGGER items_keep BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION keep()",
        ],
    );
    let error = refusal(&mut session, "DROP FUNCTION keep()");
    assert_eq!(error.state().code(), "2BP01");
    answer(&mut session, "DROP FUNCTION keep() CASCADE");
    answer(&mut session, "INSERT INTO items VALUES (1, 'pen', 2)");
    assert_eq!(rows(&mut session, "SELECT id FROM items"), vec!["1"]);
}

#[test]
fn transition_tables_are_refused_when_the_trigger_fires() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            ITEMS,
            "CREATE FUNCTION summary() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$",
            "CREATE TRIGGER items_summary AFTER INSERT ON items REFERENCING NEW TABLE AS added FOR EACH STATEMENT EXECUTE FUNCTION summary()",
        ],
    );
    let error = refusal(&mut session, "INSERT INTO items VALUES (1, 'pen', 2)");
    assert_eq!(error.state().code(), "0A000");
    assert!(rows(&mut session, "SELECT id FROM items").is_empty());
}

#[test]
fn a_trigger_function_that_fires_itself_stops_at_the_depth_limit() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            ITEMS,
            "CREATE FUNCTION again() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO items VALUES (NEW.id + 1, NEW.name, NEW.price);
    RETURN NEW;
END $$",
            "CREATE TRIGGER items_again AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION again()",
        ],
    );
    let error = refusal(&mut session, "INSERT INTO items VALUES (1, 'pen', 2)");
    assert_eq!(error.state().code(), "54001");
    assert!(rows(&mut session, "SELECT id FROM items").is_empty());
}
