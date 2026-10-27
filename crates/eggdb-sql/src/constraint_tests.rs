use crate::memory_store::MemoryStore;
use crate::tests::{fuzz, render};
use crate::{Dialect, Session, SqlError};

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

const PG_DUMP_SCRIPT: &str = r"
--
-- PostgreSQL database dump
--

SET statement_timeout = 0;
SET lock_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SET check_function_bodies = false;
SET client_min_messages = warning;
SET row_security = off;

SET default_tablespace = '';

CREATE TABLE public.authors (
    id integer NOT NULL,
    name text NOT NULL
);

ALTER TABLE public.authors OWNER TO postgres;

CREATE SEQUENCE public.authors_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.authors_id_seq OWNER TO postgres;

ALTER SEQUENCE public.authors_id_seq OWNED BY public.authors.id;

CREATE TABLE public.books (
    id bigint NOT NULL,
    author_id integer,
    title text NOT NULL,
    pages integer
);

CREATE SEQUENCE public.books_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

ALTER SEQUENCE public.books_id_seq OWNED BY public.books.id;

ALTER TABLE ONLY public.authors ALTER COLUMN id SET DEFAULT nextval('public.authors_id_seq'::regclass);

ALTER TABLE ONLY public.books ALTER COLUMN id SET DEFAULT nextval('public.books_id_seq'::regclass);

INSERT INTO public.authors (id, name) VALUES (1, 'Ursula');
INSERT INTO public.authors (id, name) VALUES (2, 'Iain');
INSERT INTO public.books (id, author_id, title, pages) VALUES (1, 1, 'The Dispossessed', 387);
INSERT INTO public.books (id, author_id, title, pages) VALUES (2, 2, 'Excession', 451);
INSERT INTO public.books (id, author_id, title, pages) VALUES (3, NULL, 'Anonymous', 10);

SELECT pg_catalog.setval('public.authors_id_seq', 2, true);

SELECT pg_catalog.setval('public.books_id_seq', 3, true);

ALTER TABLE ONLY public.authors
    ADD CONSTRAINT authors_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.books
    ADD CONSTRAINT books_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.books
    ADD CONSTRAINT books_title_key UNIQUE (title);

ALTER TABLE ONLY public.books
    ADD CONSTRAINT books_pages_check CHECK ((pages > 0));

CREATE INDEX books_author_idx ON public.books USING btree (author_id);

ALTER TABLE ONLY public.books
    ADD CONSTRAINT books_author_id_fkey FOREIGN KEY (author_id) REFERENCES public.authors(id) ON DELETE CASCADE;
";

#[test]
fn a_pg_dump_script_restores_with_sequences_and_constraints() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, PG_DUMP_SCRIPT).unwrap();
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO authors (name) VALUES ('Octavia') RETURNING id"
        ),
        "id\n3\nINSERT 0 1"
    );
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO books (author_id, title, pages) VALUES (3, 'Kindred', 264) RETURNING id"
        ),
        "id\n4\nINSERT 0 1"
    );
    let error = fails(
        &mut session,
        "INSERT INTO books (author_id, title, pages) VALUES (9, 'Nowhere', 1)",
    );
    assert_eq!(error.state().code(), "23503");
    assert_eq!(
        error.message(),
        "insert or update on table \"books\" violates foreign key constraint \"books_author_id_fkey\""
    );
    assert_eq!(
        error.detail(),
        Some("Key (author_id)=(9) is not present in table \"authors\".")
    );
    let error = fails(
        &mut session,
        "INSERT INTO books (author_id, title, pages) VALUES (1, 'Zero', 0)",
    );
    assert_eq!(error.state().code(), "23514");
    assert_eq!(
        error.message(),
        "new row for relation \"books\" violates check constraint \"books_pages_check\""
    );
    assert_eq!(
        fails(
            &mut session,
            "INSERT INTO books (author_id, title, pages) VALUES (1, 'Excession', 5)"
        )
        .state()
        .code(),
        "23505"
    );
    assert_eq!(
        fails(
            &mut session,
            "INSERT INTO authors (id, name) VALUES (1, 'again')"
        )
        .state()
        .code(),
        "23505"
    );
    assert_eq!(
        last(&mut session, "DELETE FROM authors WHERE id = 1"),
        "DELETE 1"
    );
    assert_eq!(
        last(&mut session, "SELECT id, title FROM books ORDER BY id"),
        "id|title\n2|Excession\n3|Anonymous\n4|Kindred\nSELECT 3"
    );
    assert_eq!(
        last(&mut session, "SELECT currval('books_id_seq'), lastval()"),
        "currval|lastval\n7|7\nSELECT 1"
    );
    assert_eq!(session.check_indexes().unwrap().indexes, 2);
    let error = fails(&mut session, "DROP TABLE authors");
    assert_eq!(error.state().code(), "2BP01");
    assert_eq!(
        last(&mut session, "DROP TABLE authors CASCADE"),
        "DROP TABLE"
    );
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO books (author_id, title, pages) VALUES (99, 'Free now', 3)"
        ),
        "INSERT 0 1"
    );
    assert_eq!(
        fails(&mut session, "SELECT nextval('authors_id_seq')")
            .state()
            .code(),
        "42P01"
    );
}

#[test]
fn sequences_follow_postgresql() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "CREATE SEQUENCE s");
    assert_eq!(
        last(
            &mut session,
            "SELECT nextval('s'), nextval('s'), currval('s')"
        ),
        "nextval|nextval|currval\n1|2|2\nSELECT 1"
    );
    last(&mut session, "BEGIN");
    assert_eq!(
        last(&mut session, "SELECT nextval('s')"),
        "nextval\n3\nSELECT 1"
    );
    last(&mut session, "ROLLBACK");
    assert_eq!(
        last(&mut session, "SELECT nextval('s')"),
        "nextval\n4\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT setval('s', 10, false)"),
        "setval\n10\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT nextval('s')"),
        "nextval\n10\nSELECT 1"
    );
    last(
        &mut session,
        "CREATE SEQUENCE big START 9223372036854775806",
    );
    assert_eq!(
        last(&mut session, "SELECT nextval('big'), nextval('big')"),
        "nextval|nextval\n9223372036854775806|9223372036854775807\nSELECT 1"
    );
    let error = fails(&mut session, "SELECT nextval('big')");
    assert_eq!(error.state().code(), "2200H");
    assert_eq!(
        error.message(),
        "nextval: reached maximum value of sequence \"big\" (9223372036854775807)"
    );
    last(
        &mut session,
        "CREATE SEQUENCE down INCREMENT BY -2 MINVALUE -5 MAXVALUE 0 CYCLE",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT nextval('down'), nextval('down'), nextval('down'), nextval('down')"
        ),
        "nextval|nextval|nextval|nextval\n0|-2|-4|0\nSELECT 1"
    );
    last(
        &mut session,
        "CREATE SEQUENCE huge INCREMENT BY 9223372036854775807 START WITH 9223372036854775807 MINVALUE 1",
    );
    assert_eq!(
        last(&mut session, "SELECT nextval('huge')"),
        "nextval\n9223372036854775807\nSELECT 1"
    );
    assert_eq!(
        fails(&mut session, "SELECT nextval('huge')").state().code(),
        "2200H"
    );
    assert_eq!(
        fails(&mut session, "CREATE SEQUENCE bad INCREMENT 0").message(),
        "INCREMENT must not be zero"
    );
    assert_eq!(
        fails(&mut session, "CREATE SEQUENCE bad START 0").message(),
        "START value (0) cannot be less than MINVALUE (1)"
    );
    assert_eq!(
        fails(
            &mut session,
            "CREATE SEQUENCE bad AS smallint MAXVALUE 40000"
        )
        .message(),
        "MAXVALUE (40000) is out of range for sequence data type smallint"
    );
    assert_eq!(
        fails(&mut session, "SELECT setval('s', 0)").message(),
        "setval: value 0 is out of bounds for sequence \"s\" (1..9223372036854775807)"
    );
    assert_eq!(
        fails(&mut session, "SELECT currval('down2')")
            .state()
            .code(),
        "42P01"
    );
    last(&mut session, "CREATE SEQUENCE fresh");
    assert_eq!(
        fails(&mut session, "SELECT currval('fresh')").message(),
        "currval of sequence \"fresh\" is not yet defined in this session"
    );
    last(&mut session, "ALTER SEQUENCE s RESTART WITH 100");
    assert_eq!(
        last(&mut session, "SELECT nextval('s')"),
        "nextval\n100\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT 's'::regclass, 'public.\"S\"'::regclass"
        ),
        "regclass_cast|regclass_cast\ns|\"S\"\nSELECT 1"
    );
    last(&mut session, "DROP SEQUENCE s, fresh");
    assert_eq!(
        fails(&mut session, "SELECT nextval('s')").state().code(),
        "42P01"
    );
}

#[test]
fn sequence_functions_stay_in_postgresql() {
    let store = MemoryStore::default();
    for dialect in [Dialect::MySql, Dialect::Sqlite, Dialect::Oracle] {
        let mut session = Session::with_dialect(&store, dialect);
        for sql in [
            "SELECT nextval('s')",
            "SELECT setval('s', 1)",
            "SELECT regclass_cast('s')",
            "SELECT oracle_nextval('s')",
        ] {
            assert!(run(&mut session, sql).is_err(), "{dialect:?}: {sql}");
        }
    }
}

#[test]
fn oracle_sequences_use_nextval_and_currval() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    last(
        &mut session,
        "CREATE SEQUENCE ids START WITH 10 INCREMENT BY 5 NOCACHE NOCYCLE",
    );
    last(
        &mut session,
        "CREATE TABLE t (id NUMBER(10) PRIMARY KEY, name VARCHAR2(20))",
    );
    last(&mut session, "INSERT INTO t VALUES (ids.NEXTVAL, 'a')");
    last(&mut session, "INSERT INTO t VALUES (ids.NEXTVAL, 'b')");
    assert_eq!(
        last(&mut session, "SELECT ids.CURRVAL FROM dual"),
        "IDS.CURRVAL\n15\nSELECT 1"
    );
    let error = fails(&mut session, "SELECT nope.NEXTVAL FROM dual");
    assert_eq!(error.message(), "sequence does not exist");
}

#[test]
fn foreign_keys_cascade_set_null_and_restrict() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE p (id int PRIMARY KEY, code text UNIQUE);
         CREATE TABLE c1 (id int PRIMARY KEY, p int REFERENCES p ON DELETE CASCADE ON UPDATE CASCADE);
         CREATE TABLE c2 (id int PRIMARY KEY, p int REFERENCES p (id) ON DELETE SET NULL);
         CREATE TABLE c3 (id int PRIMARY KEY, code text, FOREIGN KEY (code) REFERENCES p (code));
         INSERT INTO p VALUES (1, 'a'), (2, 'b'), (3, 'c');
         INSERT INTO c1 VALUES (10, 1), (11, 1), (12, 2);
         INSERT INTO c2 VALUES (20, 2), (21, 2);
         INSERT INTO c3 VALUES (30, 'c');",
    );
    assert_eq!(
        last(&mut session, "UPDATE p SET id = 5 WHERE id = 1"),
        "UPDATE 1"
    );
    assert_eq!(
        last(&mut session, "SELECT id, p FROM c1 ORDER BY id"),
        "id|p\n10|5\n11|5\n12|2\nSELECT 3"
    );
    let error = fails(&mut session, "DELETE FROM p WHERE id = 3");
    assert_eq!(error.state().code(), "23503");
    assert_eq!(
        error.message(),
        "update or delete on table \"p\" violates foreign key constraint \"c3_code_fkey\" on table \"c3\""
    );
    assert_eq!(last(&mut session, "DELETE FROM p WHERE id = 2"), "DELETE 1");
    assert_eq!(
        last(&mut session, "SELECT id, p FROM c1 ORDER BY id"),
        "id|p\n10|5\n11|5\nSELECT 2"
    );
    assert_eq!(
        last(&mut session, "SELECT id, p FROM c2 ORDER BY id"),
        "id|p\n20|NULL\n21|NULL\nSELECT 2"
    );
}

#[test]
fn self_references_and_multi_row_inserts_check_at_statement_end() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE tree (id int PRIMARY KEY, parent int REFERENCES tree ON DELETE CASCADE)",
    );
    last(
        &mut session,
        "INSERT INTO tree VALUES (2, 1), (1, NULL), (3, 2), (4, 3), (5, 5)",
    );
    assert_eq!(
        last(&mut session, "DELETE FROM tree WHERE id = 1"),
        "DELETE 1"
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM tree ORDER BY id"),
        "id\n5\nSELECT 1"
    );
}

#[test]
fn alter_table_changes_columns() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE t (a int PRIMARY KEY, b text);
         INSERT INTO t VALUES (1, 'x'), (2, NULL);",
    );
    last(&mut session, "ALTER TABLE t ADD COLUMN c int DEFAULT 7");
    assert_eq!(
        last(&mut session, "SELECT * FROM t ORDER BY a"),
        "a|b|c\n1|x|7\n2|NULL|7\nSELECT 2"
    );
    last(&mut session, "ALTER TABLE t RENAME COLUMN b TO bee");
    last(&mut session, "ALTER TABLE t DROP COLUMN c");
    assert_eq!(
        last(&mut session, "SELECT * FROM t ORDER BY a"),
        "a|bee\n1|x\n2|NULL\nSELECT 2"
    );
    assert_eq!(
        fails(&mut session, "ALTER TABLE t ALTER COLUMN bee SET NOT NULL")
            .state()
            .code(),
        "23502"
    );
    last(
        &mut session,
        "ALTER TABLE t ALTER COLUMN bee SET DEFAULT 'd'",
    );
    last(&mut session, "INSERT INTO t (a) VALUES (3)");
    assert_eq!(
        last(&mut session, "SELECT bee FROM t WHERE a = 3"),
        "bee\nd\nSELECT 1"
    );
    last(&mut session, "ALTER TABLE t ALTER COLUMN a TYPE bigint");
    last(&mut session, "ALTER TABLE t RENAME TO t2");
    assert_eq!(
        last(&mut session, "SELECT a FROM t2 ORDER BY a"),
        "a\n1\n2\n3\nSELECT 3"
    );
    let error = fails(
        &mut session,
        "ALTER TABLE t2 ADD CONSTRAINT positive CHECK (a > 1)",
    );
    assert_eq!(
        error.message(),
        "check constraint \"positive\" of relation \"t2\" is violated by some row"
    );
    last(
        &mut session,
        "ALTER TABLE t2 ADD CONSTRAINT positive CHECK (a > 0)",
    );
    last(&mut session, "ALTER TABLE t2 DROP CONSTRAINT positive");
    last(&mut session, "INSERT INTO t2 VALUES (-1, 'n')");
}

#[test]
fn other_dialects_spell_alter_and_constraints() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(
        &mut session,
        "CREATE TABLE parent (id INT PRIMARY KEY);
         CREATE TABLE child (id INT PRIMARY KEY, pid INT, CONSTRAINT fk FOREIGN KEY (pid) REFERENCES parent (id), CONSTRAINT ck CHECK (id > 0));
         INSERT INTO parent VALUES (1);",
    );
    let error = fails(&mut session, "INSERT INTO child VALUES (1, 2)");
    assert_eq!(error.vendor_code(), Some(1452));
    let error = fails(&mut session, "INSERT INTO child VALUES (0, 1)");
    assert_eq!(error.vendor_code(), Some(3819));
    last(&mut session, "INSERT INTO child VALUES (1, 1)");
    assert_eq!(
        fails(&mut session, "DELETE FROM parent").vendor_code(),
        Some(1451)
    );
    last(
        &mut session,
        "ALTER TABLE child ADD COLUMN note VARCHAR(10), MODIFY COLUMN note VARCHAR(20) DEFAULT 'n'",
    );
    last(
        &mut session,
        "ALTER TABLE child CHANGE note memo VARCHAR(20)",
    );
    last(&mut session, "ALTER TABLE child DROP FOREIGN KEY fk");
    last(&mut session, "DELETE FROM parent");

    let mut sqlite = Session::with_dialect(&store, Dialect::Sqlite);
    last(
        &mut sqlite,
        "CREATE TABLE sp (id INTEGER PRIMARY KEY);
         CREATE TABLE sc (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES sp (id), qty INTEGER CHECK (qty >= 0));",
    );
    last(&mut sqlite, "INSERT INTO sc VALUES (1, 99, 1)");
    last(&mut sqlite, "PRAGMA foreign_keys = ON");
    assert_eq!(
        fails(&mut sqlite, "INSERT INTO sc VALUES (2, 99, 1)").message(),
        "FOREIGN KEY constraint failed"
    );
    assert_eq!(
        fails(&mut sqlite, "INSERT INTO sc VALUES (3, NULL, -1)").message(),
        "CHECK constraint failed: qty >= 0"
    );
    last(
        &mut sqlite,
        "ALTER TABLE sc ADD COLUMN note TEXT DEFAULT 'x'",
    );
    last(&mut sqlite, "ALTER TABLE sc RENAME COLUMN note TO memo");
    last(&mut sqlite, "ALTER TABLE sc DROP COLUMN memo");
    last(&mut sqlite, "ALTER TABLE sc RENAME TO sc2");

    let mut oracle = Session::with_dialect(&store, Dialect::Oracle);
    last(&mut oracle, "CREATE TABLE op (id NUMBER(10) PRIMARY KEY)");
    last(
        &mut oracle,
        "CREATE TABLE oc (id NUMBER(10) PRIMARY KEY, pid NUMBER(10) CONSTRAINT oc_fk REFERENCES op (id) ON DELETE CASCADE)",
    );
    assert_eq!(
        fails(&mut oracle, "INSERT INTO oc VALUES (1, 5)").vendor_code(),
        Some(2291)
    );
    last(&mut oracle, "ALTER TABLE oc ADD (note VARCHAR2(10))");
    last(
        &mut oracle,
        "ALTER TABLE oc MODIFY (note VARCHAR2(20) DEFAULT 'n')",
    );
    last(
        &mut oracle,
        "ALTER TABLE oc ADD CONSTRAINT oc_ck CHECK (id > 0)",
    );
    assert_eq!(
        fails(&mut oracle, "INSERT INTO oc (id) VALUES (0)").vendor_code(),
        Some(2290)
    );
    assert_eq!(
        fails(&mut oracle, "DROP TABLE op").vendor_code(),
        Some(2449)
    );
    last(&mut oracle, "ALTER TABLE oc DROP COLUMN note");
}

#[test]
fn hostile_constraint_statements_never_panic() {
    let fragments = [
        "CREATE SEQUENCE s",
        "AS integer",
        "START WITH",
        "INCREMENT BY",
        "-9223372036854775808",
        "9223372036854775807",
        "NO MINVALUE",
        "MAXVALUE",
        "CACHE 1",
        "CYCLE",
        "OWNED BY t.a",
        "ALTER SEQUENCE s",
        "RESTART",
        "SELECT nextval('s')",
        "SELECT pg_catalog.setval('s', 10, true)",
        "'s'::regclass",
        "s.NEXTVAL",
        "s.CURRVAL",
        "ALTER TABLE ONLY t",
        "ALTER TABLE t",
        "ADD CONSTRAINT c",
        "PRIMARY KEY (a)",
        "UNIQUE (b)",
        "CHECK ((a > 0))",
        "FOREIGN KEY (b) REFERENCES t(a)",
        "REFERENCES t",
        "ON DELETE CASCADE",
        "ON UPDATE SET NULL",
        "ON DELETE SET DEFAULT",
        "DEFERRABLE INITIALLY DEFERRED",
        "DROP CONSTRAINT c",
        "DROP COLUMN b",
        "ADD COLUMN z int DEFAULT 3",
        "ALTER COLUMN a SET DEFAULT nextval('s')",
        "ALTER COLUMN b TYPE bigint",
        "SET NOT NULL",
        "DROP NOT NULL",
        "RENAME COLUMN a TO q",
        "RENAME TO u",
        "MODIFY",
        "CHANGE",
        "(",
        ")",
        ",",
        ";",
        "CASCADE",
        "INSERT INTO t VALUES (1, 1)",
        "DELETE FROM t",
        "UPDATE t SET a = a + 1",
        "DROP TABLE t",
        "CREATE TABLE t (a int PRIMARY KEY, b int REFERENCES t ON DELETE CASCADE CHECK (b <> 7))",
    ];
    for dialect in Dialect::ALL {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        let _ = run(
            &mut session,
            "CREATE TABLE t (a int PRIMARY KEY, b int REFERENCES t ON DELETE CASCADE)",
        );
        fuzz(&mut session, &fragments);
        session.check_indexes().unwrap();
    }
}
