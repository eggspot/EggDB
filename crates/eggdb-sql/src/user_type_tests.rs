use crate::memory_store::MemoryStore;
use crate::tests::{fuzz, render};
use crate::{Session, SqlError};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

fn ok(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    run(session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"))
}

fn state(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error.state().code().to_owned(),
    }
}

#[test]
fn enums_store_compare_in_declared_order_and_take_new_labels() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        ok(&mut session, "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')"),
        "CREATE TYPE"
    );
    ok(&mut session, "CREATE TABLE person (name text, current_mood mood)");
    assert_eq!(
        ok(
            &mut session,
            "INSERT INTO person VALUES ('Moe', 'happy'), ('Larry', 'sad'), ('Curly', 'ok')"
        ),
        "INSERT 0 3"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT name, current_mood FROM person WHERE current_mood > 'sad' ORDER BY current_mood"
        ),
        "name|current_mood\nCurly|ok\nMoe|happy\nSELECT 2"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO person VALUES ('Shemp', 'angry')"),
        "22P02"
    );
    assert_eq!(state(&mut session, "SELECT 'angry'::mood"), "22P02");
    assert_eq!(
        ok(&mut session, "ALTER TYPE mood ADD VALUE 'meh' BEFORE 'ok'"),
        "ALTER TYPE"
    );
    assert_eq!(
        ok(&mut session, "ALTER TYPE mood ADD VALUE IF NOT EXISTS 'meh'"),
        "ALTER TYPE"
    );
    assert_eq!(
        state(&mut session, "ALTER TYPE mood ADD VALUE 'meh'"),
        "42710"
    );
    assert_eq!(
        ok(&mut session, "SELECT enum_range(NULL::mood)"),
        "enum_range\n{sad,meh,ok,happy}\nSELECT 1"
    );
    assert_eq!(
        ok(&mut session, "SELECT enum_first(NULL::mood), enum_last(NULL::mood)"),
        "enum_first|enum_last\nsad|happy\nSELECT 1"
    );
    ok(&mut session, "ALTER TYPE mood RENAME VALUE 'meh' TO 'fine'");
    assert_eq!(
        ok(
            &mut session,
            "SELECT 'fine'::mood < 'ok'::mood AS before, 'fine'::mood > 'sad'::mood AS after"
        ),
        "before|after\nt|t\nSELECT 1"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT e.enumlabel FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid \
             WHERE t.typname = 'mood' ORDER BY e.enumsortorder"
        ),
        "enumlabel\nsad\nfine\nok\nhappy\nSELECT 4"
    );
    assert_eq!(state(&mut session, "DROP TYPE mood"), "2BP01");
    session.close().unwrap();

    let mut reopened = Session::new(&store);
    assert_eq!(
        ok(
            &mut reopened,
            "SELECT name FROM person WHERE current_mood = 'happy'"
        ),
        "name\nMoe\nSELECT 1"
    );
    assert_eq!(
        ok(&mut reopened, "DROP TABLE person; DROP TYPE mood"),
        "DROP TYPE"
    );
    assert_eq!(
        ok(
            &mut reopened,
            "SELECT count(*) FROM pg_type WHERE typname = 'mood'"
        ),
        "count\n0\nSELECT 1"
    );
    assert_eq!(state(&mut reopened, "SELECT 'ok'::mood"), "42704");
}

#[test]
fn composite_types_build_select_fields_and_store_rows() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    ok(
        &mut session,
        "CREATE TYPE complex AS (r double precision, i double precision)",
    );
    ok(
        &mut session,
        "CREATE TABLE points (id integer PRIMARY KEY, c complex)",
    );
    ok(
        &mut session,
        "INSERT INTO points VALUES (1, ROW(1.5, -2)), (2, '(3,4)'), (3, NULL)",
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT id, c, (c).r AS r FROM points ORDER BY id"
        ),
        "id|c|r\n1|(1.5,-2)|1.5\n2|(3,4)|3\n3|NULL|NULL\nSELECT 3"
    );
    assert_eq!(
        state(&mut session, "SELECT (c).nope FROM points"),
        "42703"
    );
    assert_eq!(
        ok(&mut session, "SELECT ROW(1, 'a b', NULL) AS r"),
        "r\n(1,\"a b\",)\nSELECT 1"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT typtype FROM pg_type WHERE typname = 'complex'"
        ),
        "typtype\nc\nSELECT 1"
    );
    assert_eq!(
        state(&mut session, "CREATE TABLE bad (r record)"),
        "42P16"
    );
}

#[test]
fn pg_expandarray_gives_pgjdbc_its_key_columns_in_order() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        ok(
            &mut session,
            "SELECT (information_schema._pg_expandarray(ARRAY[10, 20])).x AS x, \
             (information_schema._pg_expandarray(ARRAY[10, 20])).n AS n"
        ),
        "x|n\n10|1\n20|2\nSELECT 2"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT r.k FROM (SELECT information_schema._pg_expandarray(ARRAY['p', 'q']) AS k) r"
        ),
        "k\n(p,1)\n(q,2)\nSELECT 2"
    );
    ok(
        &mut session,
        "CREATE TABLE pk_parent (a integer, b text, c text, PRIMARY KEY (b, a))",
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT result.COLUMN_NAME, result.KEY_SEQ, result.PK_NAME FROM (\
             SELECT NULL AS TABLE_CAT, n.nspname AS TABLE_SCHEM, ct.relname AS TABLE_NAME, \
             a.attname AS COLUMN_NAME, (information_schema._pg_expandarray(i.indkey)).n AS KEY_SEQ, \
             ci.relname AS PK_NAME, information_schema._pg_expandarray(i.indkey) AS KEYS, \
             a.attnum AS A_ATTNUM FROM pg_catalog.pg_class ct \
             JOIN pg_catalog.pg_attribute a ON (ct.oid = a.attrelid) \
             JOIN pg_catalog.pg_namespace n ON (ct.relnamespace = n.oid) \
             JOIN pg_catalog.pg_index i ON (a.attrelid = i.indrelid) \
             JOIN pg_catalog.pg_class ci ON (ci.oid = i.indexrelid) \
             WHERE true AND ct.relname = E'pk_parent' AND i.indisprimary) result \
             where result.A_ATTNUM = (result.KEYS).x \
             ORDER BY result.table_name, result.pk_name, result.key_seq"
        ),
        "column_name|key_seq|pk_name\nb|1|pk_parent_pkey\na|2|pk_parent_pkey\nSELECT 2"
    );
}

#[test]
fn ranges_take_canonical_forms_and_answer_their_operators() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        ok(
            &mut session,
            "SELECT int4range(1, 5) AS a, '[1,5]'::int4range AS b, 'empty'::int4range AS c"
        ),
        "a|b|c\n[1,5)|[1,6)|empty\nSELECT 1"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT int4range(1, 5) @> 3 AS has, int4range(1, 5) && int4range(4, 8) AS overlaps, \
             lower(int4range(1, 5)) AS low, upper(int4range(1, 5)) AS high, \
             isempty(int4range(3, 3)) AS empty"
        ),
        "has|overlaps|low|high|empty\nt|t|1|5|t\nSELECT 1"
    );
    assert_eq!(state(&mut session, "SELECT int4range(5, 1)"), "22000");
    ok(
        &mut session,
        "CREATE TYPE floatrange AS RANGE (subtype = float8)",
    );
    ok(
        &mut session,
        "CREATE TABLE bookings (id integer PRIMARY KEY, during daterange, weight floatrange)",
    );
    ok(
        &mut session,
        "INSERT INTO bookings VALUES (1, '[2024-01-01,2024-01-05]', '[1.5,2.5)'), (2, NULL, '(,3]')",
    );
    assert_eq!(
        ok(&mut session, "SELECT * FROM bookings ORDER BY id"),
        "id|during|weight\n1|[2024-01-01,2024-01-06)|[1.5,2.5)\n2|NULL|(,3]\nSELECT 2"
    );
    session.close().unwrap();
    let mut reopened = Session::new(&store);
    assert_eq!(
        ok(
            &mut reopened,
            "SELECT id FROM bookings WHERE during @> date '2024-01-03'"
        ),
        "id\n1\nSELECT 1"
    );
}

#[test]
fn domains_check_their_constraints_defaults_and_not_null_everywhere() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        ok(
            &mut session,
            "CREATE DOMAIN posint AS integer NOT NULL CHECK (VALUE > 0)"
        ),
        "CREATE DOMAIN"
    );
    ok(
        &mut session,
        "CREATE DOMAIN code AS varchar(3) DEFAULT 'xyz'",
    );
    ok(
        &mut session,
        "CREATE TABLE stock (item text, qty posint, kind code)",
    );
    ok(&mut session, "INSERT INTO stock (item, qty) VALUES ('a', 3)");
    assert_eq!(
        ok(&mut session, "SELECT item, qty, kind FROM stock"),
        "item|qty|kind\na|3|xyz\nSELECT 1"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO stock VALUES ('b', 0, 'abc')"),
        "23514"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO stock VALUES ('c', NULL, 'abc')"),
        "23502"
    );
    assert_eq!(
        state(&mut session, "UPDATE stock SET qty = -1"),
        "23514"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO stock VALUES ('d', 1, 'abcd')"),
        "22001"
    );
    assert_eq!(ok(&mut session, "SELECT 5::posint AS n"), "n\n5\nSELECT 1");
    assert_eq!(state(&mut session, "SELECT (-1)::posint"), "23514");
    assert_eq!(
        ok(
            &mut session,
            "ALTER DOMAIN posint ADD CONSTRAINT below_100 CHECK (VALUE < 100)"
        ),
        "ALTER DOMAIN"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO stock VALUES ('e', 100, 'abc')"),
        "23514"
    );
    assert_eq!(
        state(
            &mut session,
            "ALTER DOMAIN posint ADD CONSTRAINT below_2 CHECK (VALUE < 2)"
        ),
        "23514"
    );
    ok(
        &mut session,
        "ALTER DOMAIN posint ADD CONSTRAINT below_2 CHECK (VALUE < 2) NOT VALID",
    );
    assert_eq!(
        state(&mut session, "ALTER DOMAIN posint VALIDATE CONSTRAINT below_2"),
        "23514"
    );
    ok(&mut session, "ALTER DOMAIN posint DROP CONSTRAINT below_2");
    ok(
        &mut session,
        "ALTER DOMAIN posint RENAME CONSTRAINT below_100 TO under_100",
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT typname, typtype, typnotnull, format_type(typbasetype, typtypmod) AS base \
             FROM pg_type WHERE typtype = 'd' ORDER BY typname"
        ),
        "typname|typtype|typnotnull|base\ncode|d|f|character varying(3)\nposint|d|t|integer\nSELECT 2"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT domain_name FROM information_schema.columns \
             WHERE table_name = 'stock' ORDER BY ordinal_position"
        ),
        "domain_name\nNULL\nposint\ncode\nSELECT 3"
    );
    assert_eq!(state(&mut session, "DROP DOMAIN posint"), "2BP01");
    assert_eq!(state(&mut session, "DROP TYPE posint"), "42809");
    assert_eq!(
        ok(&mut session, "DROP DOMAIN posint CASCADE"),
        "DROP DOMAIN"
    );
    assert_eq!(
        ok(&mut session, "SELECT * FROM stock"),
        "item|kind\na|xyz\nSELECT 1"
    );
    assert_eq!(
        ok(&mut session, "DROP DOMAIN IF EXISTS posint"),
        "DROP DOMAIN"
    );
}

#[test]
fn array_columns_store_one_dimension_of_any_storable_element() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    ok(
        &mut session,
        "CREATE TABLE tagged (id integer PRIMARY KEY, tags text[], scores integer ARRAY)",
    );
    ok(
        &mut session,
        "INSERT INTO tagged VALUES (1, ARRAY['a', 'b c'], '{1,2,NULL}'), (2, '{}', NULL)",
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT id, tags, scores[2] AS second FROM tagged ORDER BY id"
        ),
        "id|tags|second\n1|{a,\"b c\"}|2\n2|{}|NULL\nSELECT 2"
    );
    ok(
        &mut session,
        "UPDATE tagged SET tags = tags || 'z'::text WHERE id = 2",
    );
    ok(&mut session, "ALTER TABLE tagged ADD COLUMN flags boolean[]");
    session.close().unwrap();
    let mut reopened = Session::new(&store);
    assert_eq!(
        ok(
            &mut reopened,
            "SELECT id, tags, flags FROM tagged WHERE 'z' = ANY (tags) OR 'a' = ANY (tags) ORDER BY id"
        ),
        "id|tags|flags\n1|{a,\"b c\"}|NULL\n2|{z}|NULL\nSELECT 2"
    );
    assert_eq!(
        state(&mut reopened, "INSERT INTO tagged VALUES (3, '{{a}}', NULL)"),
        "0A000"
    );
}

#[test]
fn deferred_foreign_keys_are_checked_at_commit_or_set_constraints() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    ok(&mut session, "CREATE TABLE parent (id integer PRIMARY KEY)");
    ok(
        &mut session,
        "CREATE TABLE child (id integer PRIMARY KEY, \
         parent_id integer REFERENCES parent DEFERRABLE INITIALLY DEFERRED)",
    );
    assert_eq!(
        ok(
            &mut session,
            "BEGIN; INSERT INTO child VALUES (1, 10); INSERT INTO parent VALUES (10); COMMIT"
        ),
        "COMMIT"
    );
    ok(&mut session, "BEGIN; INSERT INTO child VALUES (2, 20)");
    assert_eq!(state(&mut session, "COMMIT"), "23503");
    assert_eq!(
        ok(&mut session, "SELECT id FROM child ORDER BY id"),
        "id\n1\nSELECT 1"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO child VALUES (3, 30)"),
        "23503"
    );
    ok(
        &mut session,
        "BEGIN; DELETE FROM parent WHERE id = 10; INSERT INTO parent VALUES (10); COMMIT",
    );
    ok(&mut session, "BEGIN; SET CONSTRAINTS ALL IMMEDIATE");
    assert_eq!(
        state(&mut session, "INSERT INTO child VALUES (4, 40)"),
        "23503"
    );
    ok(&mut session, "ROLLBACK");

    ok(
        &mut session,
        "CREATE TABLE child2 (id integer PRIMARY KEY, \
         parent_id integer CONSTRAINT child2_parent REFERENCES parent DEFERRABLE)",
    );
    assert_eq!(
        ok(
            &mut session,
            "BEGIN; SET CONSTRAINTS child2_parent DEFERRED; INSERT INTO child2 VALUES (1, 50); \
             INSERT INTO parent VALUES (50); COMMIT"
        ),
        "COMMIT"
    );
    ok(
        &mut session,
        "BEGIN; SET CONSTRAINTS child2_parent DEFERRED; INSERT INTO child2 VALUES (2, 60)",
    );
    assert_eq!(
        state(&mut session, "SET CONSTRAINTS ALL IMMEDIATE"),
        "23503"
    );
    ok(&mut session, "ROLLBACK");
    ok(&mut session, "BEGIN");
    assert_eq!(
        state(&mut session, "SET CONSTRAINTS no_such DEFERRED"),
        "42704"
    );
    ok(&mut session, "ROLLBACK");
    ok(
        &mut session,
        "CREATE TABLE child3 (id integer, parent_id integer CONSTRAINT child3_parent REFERENCES parent)",
    );
    ok(&mut session, "BEGIN");
    assert_eq!(
        state(&mut session, "SET CONSTRAINTS child3_parent DEFERRED"),
        "42809"
    );
    ok(&mut session, "ROLLBACK");
    assert_eq!(
        ok(
            &mut session,
            "SELECT conname, condeferrable, condeferred FROM pg_constraint \
             WHERE contype = 'f' ORDER BY conname"
        ),
        "conname|condeferrable|condeferred\nchild2_parent|t|f\nchild3_parent|f|f\nchild_parent_id_fkey|t|t\nSELECT 3"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT pg_get_constraintdef(oid) AS def FROM pg_constraint WHERE conname = 'child_parent_id_fkey'"
        ),
        "def\nFOREIGN KEY (parent_id) REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED\nSELECT 1"
    );
}

#[test]
fn not_valid_constraints_validate_later_and_constraints_rename() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    ok(&mut session, "CREATE TABLE amounts (n integer)");
    ok(&mut session, "INSERT INTO amounts VALUES (-1), (5)");
    assert_eq!(
        state(
            &mut session,
            "ALTER TABLE amounts ADD CONSTRAINT positive CHECK (n > 0)"
        ),
        "23514"
    );
    assert_eq!(
        ok(
            &mut session,
            "ALTER TABLE amounts ADD CONSTRAINT positive CHECK (n > 0) NOT VALID"
        ),
        "ALTER TABLE"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO amounts VALUES (-2)"),
        "23514"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT convalidated, pg_get_constraintdef(oid) AS def FROM pg_constraint WHERE conname = 'positive'"
        ),
        "convalidated|def\nf|CHECK ((n > 0)) NOT VALID\nSELECT 1"
    );
    assert_eq!(
        state(&mut session, "ALTER TABLE amounts VALIDATE CONSTRAINT positive"),
        "23514"
    );
    ok(&mut session, "DELETE FROM amounts WHERE n < 0");
    ok(&mut session, "ALTER TABLE amounts VALIDATE CONSTRAINT positive");
    assert_eq!(
        ok(
            &mut session,
            "ALTER TABLE amounts RENAME CONSTRAINT positive TO above_zero"
        ),
        "ALTER TABLE"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT conname, convalidated FROM pg_constraint WHERE conrelid = 'amounts'::regclass"
        ),
        "conname|convalidated\nabove_zero|t\nSELECT 1"
    );
    assert_eq!(
        state(
            &mut session,
            "ALTER TABLE amounts RENAME CONSTRAINT positive TO other"
        ),
        "42704"
    );
    ok(&mut session, "CREATE TABLE owners (id integer PRIMARY KEY)");
    ok(&mut session, "CREATE TABLE pets (owner integer)");
    ok(&mut session, "INSERT INTO pets VALUES (7)");
    ok(
        &mut session,
        "ALTER TABLE pets ADD CONSTRAINT pets_owner FOREIGN KEY (owner) REFERENCES owners NOT VALID",
    );
    assert_eq!(state(&mut session, "INSERT INTO pets VALUES (8)"), "23503");
    assert_eq!(
        state(&mut session, "ALTER TABLE pets VALIDATE CONSTRAINT pets_owner"),
        "23503"
    );
    ok(&mut session, "INSERT INTO owners VALUES (7)");
    ok(&mut session, "ALTER TABLE pets VALIDATE CONSTRAINT pets_owner");
}

#[test]
fn identity_columns_take_their_options_and_change_them_later() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    ok(
        &mut session,
        "CREATE TABLE ids (id integer GENERATED BY DEFAULT AS IDENTITY (START WITH 100 INCREMENT BY 5), v text)",
    );
    ok(&mut session, "INSERT INTO ids (v) VALUES ('a'), ('b')");
    assert_eq!(
        ok(&mut session, "SELECT id, v FROM ids ORDER BY id"),
        "id|v\n100|a\n105|b\nSELECT 2"
    );
    assert_eq!(
        ok(
            &mut session,
            "ALTER TABLE ids ALTER COLUMN id SET GENERATED ALWAYS"
        ),
        "ALTER TABLE"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO ids (id, v) VALUES (1, 'x')"),
        "428C9"
    );
    ok(&mut session, "ALTER TABLE ids ALTER COLUMN id DROP IDENTITY");
    assert_eq!(
        state(&mut session, "ALTER TABLE ids ALTER COLUMN id DROP IDENTITY"),
        "55000"
    );
    assert_eq!(
        ok(
            &mut session,
            "ALTER TABLE ids ALTER COLUMN id DROP IDENTITY IF EXISTS"
        ),
        "ALTER TABLE"
    );
    ok(&mut session, "INSERT INTO ids (id, v) VALUES (1, 'x')");
    assert_eq!(
        ok(&mut session, "SELECT count(*) FROM ids"),
        "count\n3\nSELECT 1"
    );
}

#[test]
fn hostile_type_statements_are_errors_never_panics() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    ok(&mut session, "CREATE TYPE e AS ENUM ('a', 'b')");
    ok(&mut session, "CREATE DOMAIN d AS integer CHECK (VALUE > 0)");
    ok(&mut session, "CREATE TABLE h (x e, y d, z int4range, w text[])");
    fuzz(
        &mut session,
        &[
            "CREATE TYPE",
            "CREATE DOMAIN",
            "ALTER TYPE",
            "ALTER DOMAIN",
            "DROP TYPE",
            "DROP DOMAIN",
            "e",
            "d",
            "AS",
            "ENUM",
            "RANGE",
            "(",
            ")",
            "subtype = int4",
            "'a'",
            "'(1,'",
            "'[1,2'",
            "'{a,'",
            "'(\"\\'",
            "ADD VALUE",
            "BEFORE",
            "AFTER",
            "RENAME VALUE",
            "TO",
            "CHECK (VALUE > 0)",
            "NOT NULL",
            "DEFAULT 1",
            "NOT VALID",
            "VALIDATE CONSTRAINT",
            "CONSTRAINT c",
            "CASCADE",
            "SET CONSTRAINTS ALL DEFERRED",
            "SELECT",
            "ROW(1, 2)",
            ".x",
            "(h.x).y",
            "::e",
            "::d",
            "::int4range",
            "::text[]",
            "int4range(1, 2)",
            "@>",
            "&&",
            "-|-",
            "information_schema._pg_expandarray(ARRAY[1])",
            "FROM h",
            "INSERT INTO h VALUES",
            ",",
        ],
    );
}
