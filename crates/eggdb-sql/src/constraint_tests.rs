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
        "regclass|regclass\ns|\"S\"\nSELECT 1"
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
    last(&mut Session::new(&store), "CREATE SEQUENCE s");
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

#[test]
fn prepared_sequence_calls_describe_without_running() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "CREATE SEQUENCE s");
    let description = session.describe("SELECT nextval('s')", &[]).unwrap();
    assert_eq!(
        description.columns.unwrap()[0].sql_type,
        Some(crate::SqlType::BigInt)
    );
    assert_eq!(
        last(&mut session, "SELECT nextval('s')"),
        "nextval\n1\nSELECT 1"
    );
}

struct Seeded(u64);

impl Seeded {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Referential {
    Cascade,
    SetNull,
    SetDefault,
    Restrict,
    NoAction,
}

impl Referential {
    fn sql(self) -> &'static str {
        match self {
            Self::Cascade => "CASCADE",
            Self::SetNull => "SET NULL",
            Self::SetDefault => "SET DEFAULT",
            Self::Restrict => "RESTRICT",
            Self::NoAction => "NO ACTION",
        }
    }
}

#[derive(Clone, Default)]
struct Model {
    parents: std::collections::BTreeSet<i64>,
    children: std::collections::BTreeMap<i64, Option<i64>>,
}

impl Model {
    fn referencing(&self, parent: i64) -> Vec<i64> {
        self.children
            .iter()
            .filter(|(_, pid)| **pid == Some(parent))
            .map(|(id, _)| *id)
            .collect()
    }

    fn remove_or_move(&mut self, old: i64, new: Option<i64>, action: Referential) -> bool {
        let children = self.referencing(old);
        if children.is_empty() {
            return true;
        }
        match action {
            Referential::Restrict | Referential::NoAction => false,
            Referential::Cascade => {
                for child in children {
                    match new {
                        Some(new) => {
                            self.children.insert(child, Some(new));
                        }
                        None => {
                            self.children.remove(&child);
                        }
                    }
                }
                true
            }
            Referential::SetNull => {
                for child in children {
                    self.children.insert(child, None);
                }
                true
            }
            Referential::SetDefault => {
                if !self.parents.contains(&0) {
                    return false;
                }
                for child in children {
                    self.children.insert(child, Some(0));
                }
                true
            }
        }
    }

    fn apply(&mut self, operation: &Operation, action: Referential) -> bool {
        let mut next = self.clone();
        let ok = match *operation {
            Operation::InsertParent(id) => next.parents.insert(id),
            Operation::InsertChild(id, pid) => {
                !next.children.contains_key(&id)
                    && pid.is_none_or(|pid| next.parents.contains(&pid))
                    && next.children.insert(id, pid).is_none()
            }
            Operation::DeleteParent(id) => {
                next.parents.remove(&id) && next.remove_or_move(id, None, action)
                    || !self.parents.contains(&id)
            }
            Operation::UpdateParent(old, new) => {
                if !next.parents.contains(&old) || old == new {
                    true
                } else if next.parents.contains(&new) {
                    false
                } else {
                    next.parents.remove(&old);
                    next.parents.insert(new);
                    next.remove_or_move(old, Some(new), action)
                }
            }
            Operation::UpdateChild(id, pid) => {
                if !next.children.contains_key(&id) {
                    true
                } else if pid.is_some_and(|pid| !next.parents.contains(&pid)) {
                    false
                } else {
                    next.children.insert(id, pid);
                    true
                }
            }
        };
        if ok {
            *self = next;
        }
        ok
    }
}

enum Operation {
    InsertParent(i64),
    InsertChild(i64, Option<i64>),
    DeleteParent(i64),
    UpdateParent(i64, i64),
    UpdateChild(i64, Option<i64>),
}

impl Operation {
    fn sql(&self) -> String {
        let shown = |pid: Option<i64>| pid.map_or_else(|| "NULL".to_owned(), |pid| pid.to_string());
        match self {
            Self::InsertParent(id) => format!("INSERT INTO p VALUES ({id})"),
            Self::InsertChild(id, pid) => format!("INSERT INTO c VALUES ({id}, {})", shown(*pid)),
            Self::DeleteParent(id) => format!("DELETE FROM p WHERE id = {id}"),
            Self::UpdateParent(old, new) => format!("UPDATE p SET id = {new} WHERE id = {old}"),
            Self::UpdateChild(id, pid) => {
                format!("UPDATE c SET pid = {} WHERE id = {id}", shown(*pid))
            }
        }
    }
}

fn table_state(session: &mut Session<'_, MemoryStore>) -> Model {
    let mut model = Model::default();
    let mut outputs = Vec::new();
    session
        .execute("SELECT id FROM p; SELECT id, pid FROM c", &mut |result| {
            outputs.push(result)
        })
        .unwrap();
    for row in &outputs[0].rows {
        model.parents.insert(row[0].as_integer().unwrap());
    }
    for row in &outputs[1].rows {
        model
            .children
            .insert(row[0].as_integer().unwrap(), row[1].as_integer());
    }
    model
}

#[test]
fn referential_actions_agree_with_a_model_with_and_without_an_index() {
    for action in [
        Referential::Cascade,
        Referential::SetNull,
        Referential::SetDefault,
        Referential::Restrict,
        Referential::NoAction,
    ] {
        for indexed in [false, true] {
            let store = MemoryStore::default();
            let mut session = Session::new(&store);
            last(
                &mut session,
                &format!(
                    "CREATE TABLE p (id int PRIMARY KEY);
                     CREATE TABLE c (id int PRIMARY KEY, pid int DEFAULT 0 REFERENCES p ON DELETE {0} ON UPDATE {0});
                     INSERT INTO p VALUES (0)",
                    action.sql()
                ),
            );
            if indexed {
                last(&mut session, "CREATE INDEX c_pid ON c (pid)");
            }
            let mut model = Model::default();
            model.parents.insert(0);
            let mut random = Seeded(0x00C0_FFEE + indexed as u64);
            for step in 0..600 {
                let value = |random: &mut Seeded| random.below(12) as i64;
                let maybe = |random: &mut Seeded| {
                    let pid = random.below(14) as i64;
                    (pid < 12).then_some(pid)
                };
                let operation = match random.below(5) {
                    0 => Operation::InsertParent(value(&mut random)),
                    1 => Operation::InsertChild(random.below(40) as i64, maybe(&mut random)),
                    2 => Operation::DeleteParent(value(&mut random)),
                    3 => Operation::UpdateParent(value(&mut random), value(&mut random)),
                    _ => Operation::UpdateChild(random.below(40) as i64, maybe(&mut random)),
                };
                let sql = operation.sql();
                let expected = model.apply(&operation, action);
                let found = run(&mut session, &sql);
                assert_eq!(
                    found.is_ok(),
                    expected,
                    "{} indexed={indexed} step {step}: {sql}: {found:?}",
                    action.sql()
                );
                let state = table_state(&mut session);
                assert_eq!(state.parents, model.parents, "{sql}");
                assert_eq!(state.children, model.children, "{sql}");
            }
            session.check_indexes().unwrap();
        }
    }
}

#[test]
fn constraint_bounds_hold() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE chain (id int PRIMARY KEY, parent int REFERENCES chain ON DELETE CASCADE)",
    );
    let mut script = String::from("INSERT INTO chain VALUES (0, NULL)");
    for id in 1..2_000 {
        script.push_str(&format!(", ({id}, {})", id - 1));
    }
    last(&mut session, &script);
    assert_eq!(
        last(&mut session, "DELETE FROM chain WHERE id = 0"),
        "DELETE 1"
    );
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM chain"),
        "count\n0\nSELECT 1"
    );
    last(&mut session, "CREATE TABLE many (a int)");
    let mut checks = String::new();
    for number in 0..crate::encoding::MAX_CONSTRAINTS {
        checks.push_str(&format!(
            "ALTER TABLE many ADD CONSTRAINT c{number} CHECK (a <> {number});"
        ));
    }
    run(&mut session, &checks).unwrap();
    assert_eq!(
        fails(&mut session, "ALTER TABLE many ADD CHECK (a > -1)")
            .state()
            .code(),
        "54000"
    );
    assert_eq!(
        fails(&mut session, "INSERT INTO many VALUES (1000)")
            .state()
            .code(),
        "23514"
    );
    last(&mut session, "INSERT INTO many VALUES (5000)");
    let long = "n".repeat(20_000);
    last(
        &mut session,
        &format!("CREATE TABLE named (a int CONSTRAINT {long} CHECK (a > 0))"),
    );
    let error = fails(&mut session, "INSERT INTO named VALUES (0)");
    assert_eq!(error.state().code(), "23514");
    assert!(error.message().contains(&"n".repeat(63)), "{error}");
    let nested = format!(
        "CREATE TABLE deep (a int CHECK ({}a > 0{}))",
        "(".repeat(5_000),
        ")".repeat(5_000)
    );
    assert_eq!(fails(&mut session, &nested).state().code(), "54001");
    let explained = last(&mut session, "EXPLAIN SELECT * FROM chain WHERE id = 1");
    assert!(explained.contains("chain"), "{explained}");
}

#[test]
fn foreign_keys_refuse_definitions_their_encoding_cannot_hold() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE p (a int, b int, PRIMARY KEY (a, b))",
    );
    assert_eq!(
        fails(
            &mut session,
            "CREATE TABLE c (x int, y int, FOREIGN KEY (x, y) REFERENCES p (a, a))"
        )
        .state()
        .code(),
        "42830"
    );
    let columns: Vec<String> = (0..33).map(|n| format!("c{n} int")).collect();
    let names: Vec<String> = (0..33).map(|n| format!("c{n}")).collect();
    let sql = format!(
        "CREATE TABLE wide ({}, FOREIGN KEY ({}) REFERENCES p)",
        columns.join(", "),
        names.join(", ")
    );
    assert_eq!(fails(&mut session, &sql).state().code(), "54011");
    session.tables().unwrap();
}

fn definition_version(store: &MemoryStore, table: &str) -> u8 {
    use crate::store::{KeyValueRead, KeyValueStore};
    store
        .begin_read()
        .unwrap()
        .get(&crate::encoding::table_key(table))
        .unwrap()
        .unwrap()[0]
}

#[test]
fn self_references_protect_their_parent_key_in_every_dialect() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE sd (id int PRIMARY KEY, p int REFERENCES sd ON DELETE CASCADE, q int);
         INSERT INTO sd VALUES (1, NULL, 0), (2, 1, 0);",
    );
    let error = fails(&mut session, "ALTER TABLE sd DROP COLUMN id");
    assert_eq!(error.state().code(), "2BP01");
    assert_eq!(
        error.message(),
        "cannot drop column id of table sd because other objects depend on it"
    );
    last(&mut session, "ALTER TABLE sd DROP COLUMN id CASCADE");
    last(&mut session, "INSERT INTO sd VALUES (7, 0)");
    session.tables().unwrap();
    assert_eq!(definition_version(&store, "sd"), 1);
    last(
        &mut session,
        "CREATE TABLE sr (id int PRIMARY KEY, p int REFERENCES sr)",
    );
    assert_eq!(
        fails(&mut session, "ALTER TABLE sr DROP CONSTRAINT sr_pkey")
            .state()
            .code(),
        "2BP01"
    );
    last(
        &mut session,
        "ALTER TABLE sr DROP CONSTRAINT sr_pkey CASCADE",
    );
    last(&mut session, "INSERT INTO sr VALUES (1, 99)");
    session.tables().unwrap();
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    last(
        &mut mysql,
        "CREATE TABLE ms (id INT PRIMARY KEY, p INT, FOREIGN KEY (p) REFERENCES ms (id))",
    );
    let error = fails(&mut mysql, "ALTER TABLE ms DROP PRIMARY KEY");
    assert_eq!(error.vendor_code(), Some(1553));
    assert!(error.message().contains("'PRIMARY'"), "{error}");
    let mut oracle = Session::with_dialect(&store, Dialect::Oracle);
    last(
        &mut oracle,
        "CREATE TABLE os32 (id NUMBER(5) PRIMARY KEY, p NUMBER(5) REFERENCES os32 (id))",
    );
    assert_eq!(
        fails(&mut oracle, "ALTER TABLE os32 DROP PRIMARY KEY").vendor_code(),
        Some(2273)
    );
    assert_eq!(
        fails(&mut oracle, "ALTER TABLE os32 DROP COLUMN id").vendor_code(),
        Some(12992)
    );
}

#[test]
fn changing_a_key_column_type_keeps_foreign_keys_sound() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE p3 (id int PRIMARY KEY); CREATE TABLE c3 (pid int REFERENCES p3);
         INSERT INTO p3 VALUES (1); INSERT INTO c3 VALUES (1);",
    );
    let error = fails(&mut session, "ALTER TABLE c3 ALTER COLUMN pid TYPE text");
    assert_eq!(error.state().code(), "42804");
    assert_eq!(
        error.message(),
        "foreign key constraint \"c3_pid_fkey\" cannot be implemented"
    );
    assert_eq!(
        fails(&mut session, "ALTER TABLE p3 ALTER COLUMN id TYPE text")
            .state()
            .code(),
        "42804"
    );
    last(&mut session, "ALTER TABLE c3 ALTER COLUMN pid TYPE bigint");
    last(
        &mut session,
        "ALTER TABLE p3 ALTER COLUMN id TYPE numeric(5,1)",
    );
    assert_eq!(
        fails(&mut session, "INSERT INTO c3 VALUES (2)")
            .state()
            .code(),
        "23503"
    );
    last(
        &mut session,
        "CREATE TABLE s3 (id int PRIMARY KEY, up int REFERENCES s3)",
    );
    assert_eq!(
        fails(&mut session, "ALTER TABLE s3 ALTER COLUMN up TYPE text")
            .state()
            .code(),
        "42804"
    );
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    last(
        &mut mysql,
        "CREATE TABLE mp (id INT PRIMARY KEY); CREATE TABLE mc (id INT PRIMARY KEY, pid INT, CONSTRAINT mfk FOREIGN KEY (pid) REFERENCES mp (id))",
    );
    assert_eq!(
        fails(&mut mysql, "ALTER TABLE mc MODIFY pid VARCHAR(10)").vendor_code(),
        Some(3780)
    );
    let mut oracle = Session::with_dialect(&store, Dialect::Oracle);
    last(&mut oracle, "CREATE TABLE op3 (id NUMBER(5) PRIMARY KEY)");
    last(
        &mut oracle,
        "CREATE TABLE oc3 (id NUMBER(5) PRIMARY KEY, pid NUMBER(5) REFERENCES op3 (id))",
    );
    assert_eq!(
        fails(&mut oracle, "ALTER TABLE oc3 MODIFY (pid VARCHAR2(10))").vendor_code(),
        Some(2267)
    );
}

#[test]
fn referenced_parents_and_sequence_owners_are_written_for_this_release_only() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE plain (id int PRIMARY KEY);
         CREATE TABLE parent (id int PRIMARY KEY);
         CREATE TABLE owner (id int);
         CREATE SEQUENCE owned OWNED BY owner.id;",
    );
    assert_eq!(definition_version(&store, "parent"), 1);
    assert_eq!(definition_version(&store, "owner"), 6);
    last(
        &mut session,
        "CREATE TABLE child (pid int REFERENCES parent)",
    );
    assert_eq!(definition_version(&store, "parent"), 6);
    assert_eq!(definition_version(&store, "child"), 6);
    assert_eq!(definition_version(&store, "plain"), 1);
    last(
        &mut session,
        "ALTER TABLE child DROP CONSTRAINT child_pid_fkey",
    );
    assert_eq!(definition_version(&store, "parent"), 1);
    last(
        &mut session,
        "ALTER TABLE child ADD FOREIGN KEY (pid) REFERENCES parent",
    );
    last(&mut session, "ALTER TABLE parent RENAME TO parent2");
    assert_eq!(definition_version(&store, "parent2"), 6);
    last(&mut session, "ALTER TABLE owner RENAME TO owner2");
    assert_eq!(definition_version(&store, "owner2"), 6);
    last(&mut session, "DROP SEQUENCE owned");
    assert_eq!(definition_version(&store, "owner2"), 1);
    last(&mut session, "DROP TABLE child");
    let parent = session
        .tables()
        .unwrap()
        .into_iter()
        .find(|table| table.name == "parent2")
        .unwrap();
    assert_eq!(parent.primary_key, ["id"]);
    use crate::store::{KeyValueRead, KeyValueStore};
    let bytes = store
        .begin_read()
        .unwrap()
        .get(&crate::encoding::table_key("parent2"))
        .unwrap()
        .unwrap();
    let decoded = crate::encoding::TableDefinition::decode("parent2", &bytes).unwrap();
    assert!(!decoded.referenced);
    assert_eq!(decoded.primary_key_name.as_deref(), Some("parent_pkey"));
}

#[test]
fn sequence_review_fixes_hold() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE o1 (id int); CREATE SEQUENCE os OWNED BY o1.id;
         CREATE TABLE o2 (id int DEFAULT nextval('os'), v int);",
    );
    assert_eq!(fails(&mut session, "DROP TABLE o1").state().code(), "2BP01");
    last(&mut session, "DROP TABLE o1 CASCADE");
    last(&mut session, "INSERT INTO o2 (v) VALUES (1)");
    assert_eq!(
        last(&mut session, "SELECT id FROM o2"),
        "id\nNULL\nSELECT 1"
    );
    assert_eq!(
        fails(
            &mut session,
            "CREATE TABLE o3 (id int DEFAULT nextval('missing'))"
        )
        .state()
        .code(),
        "42P01"
    );
    last(&mut session, "CREATE SEQUENCE s");
    assert_eq!(
        last(&mut session, "SELECT setval('s', NULL)"),
        "setval\nNULL\nSELECT 1"
    );
    last(&mut session, "SELECT setval('s', 100)");
    let error = fails(&mut session, "ALTER SEQUENCE s MAXVALUE 50");
    assert_eq!(
        error.message(),
        "RESTART value (100) cannot be greater than MAXVALUE (50)"
    );
    last(
        &mut session,
        "CREATE TABLE uses (a bigint DEFAULT currval('s'), b int CHECK (b < nextval('s')))",
    );
    last(&mut session, "INSERT INTO uses (b) VALUES (1)");
    assert_eq!(
        fails(&mut session, "CREATE TABLE \"bad\u{1}name\" (a int)")
            .state()
            .code(),
        "42602"
    );
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    last(&mut mysql, "CREATE TABLE mr (a INT)");
    assert_eq!(
        fails(&mut mysql, "ALTER TABLE mr RENAME COLUMN nope TO b").vendor_code(),
        Some(1054)
    );
}

#[test]
fn a_rolled_back_sequence_never_rewinds_what_another_transaction_saved() {
    let store = MemoryStore::default();
    let mut first = Session::new(&store);
    last(&mut first, "CREATE SEQUENCE s");
    last(&mut first, "BEGIN");
    last(&mut first, "SELECT setval('s', 500)");
    last(&mut first, "ROLLBACK");
    assert_eq!(
        last(&mut first, "SELECT nextval('s')"),
        "nextval\n501\nSELECT 1"
    );
    let advances = vec![crate::sequences::SequenceAdvance {
        name: "s".into(),
        sequence: crate::encoding::SequenceDefinition {
            sql_type: crate::SqlType::BigInt,
            start: 1,
            increment: 1,
            min: 1,
            max: i64::MAX,
            cache: 1,
            cycle: false,
            last_value: 7,
            is_called: true,
            owned_by: None,
        },
        before: (1, false),
    }];
    use crate::store::KeyValueStore;
    let mut writer = store.begin_write().unwrap();
    crate::sequences::reapply(&mut writer, &advances).unwrap();
    crate::store::KeyValueWrite::commit(writer).unwrap();
    assert_eq!(
        last(&mut first, "SELECT nextval('s')"),
        "nextval\n502\nSELECT 1"
    );
}
