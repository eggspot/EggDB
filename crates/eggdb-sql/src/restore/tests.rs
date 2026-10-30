use super::reader::{DumpReader, Item};
use super::{RestoreOptions, RestoreReport, restore_dump};
use crate::memory_store::MemoryStore;
use crate::session::Session;
use crate::tests::render;
use std::io::{BufRead, BufReader, Read};

fn items_of(text: &[u8]) -> Result<Vec<Item>, crate::SqlError> {
    let mut input = text;
    let mut reader = DumpReader::new(&mut input);
    let mut items = Vec::new();
    loop {
        let item = reader.next_item()?;
        if item == Item::End {
            return Ok(items);
        }
        if let Item::Statement(statement) = &item
            && statement.copy_from_stdin
        {
            let mut data = Vec::new();
            while reader.copy_data(&mut data)? {}
            items.push(item);
            items.push(Item::Meta(super::reader::MetaCommand {
                name: "data".to_owned(),
                argument: String::from_utf8_lossy(&data).into_owned(),
                line: reader.line_number(),
            }));
            continue;
        }
        items.push(item);
    }
}

fn statements_of(text: &str) -> Vec<(String, u64)> {
    items_of(text.as_bytes())
        .unwrap()
        .into_iter()
        .filter_map(|item| match item {
            Item::Statement(statement) => Some((statement.text, statement.line)),
            _ => None,
        })
        .collect()
}

fn restore(store: &MemoryStore, dump: &str, keep_going: bool) -> RestoreReport {
    let mut session = Session::new(store);
    let mut input = dump.as_bytes();
    let report = restore_dump(
        &mut session,
        &mut input,
        RestoreOptions {
            keep_going,
            ..RestoreOptions::default()
        },
        &mut |_| {},
    );
    assert_eq!(session.close(), Ok(false));
    report
}

fn query(store: &MemoryStore, sql: &str) -> String {
    let mut session = Session::new(store);
    let mut out = Vec::new();
    session
        .execute(sql, &mut |result| out.push(render(&result)))
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    out.join("\n")
}

#[test]
fn statements_end_at_semicolons_outside_quotes_comments_and_parentheses() {
    let script = "SELECT 'a;b', E'it\\'s;', \"we;ird\" FROM t; -- c;d\n\
                  /* one; /* nested; */ still; */ SELECT $$x;y$$, $tag$ $$; $tag$;\n\
                  CREATE TABLE p (a int CHECK (a > 0), b text DEFAULT ';');\nSELECT 1";
    let found = statements_of(script);
    let texts: Vec<&str> = found.iter().map(|(text, _)| text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "SELECT 'a;b', E'it\\'s;', \"we;ird\" FROM t;",
            "SELECT $$x;y$$, $tag$ $$; $tag$;",
            "CREATE TABLE p (a int CHECK (a > 0), b text DEFAULT ';');",
            "SELECT 1",
        ]
    );
    assert_eq!(
        found.iter().map(|(_, line)| *line).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
}

#[test]
fn a_statement_spanning_lines_starts_at_its_first_line_and_keeps_its_line_breaks() {
    let found = statements_of("\n-- a comment\n\nCREATE TABLE t (\n    id integer\n);\n\n");
    assert_eq!(
        found,
        [("CREATE TABLE t (\n    id integer\n);".to_owned(), 4)]
    );
}

#[test]
fn a_standard_string_keeps_its_backslashes_and_an_escape_string_does_not() {
    let found = statements_of("SELECT 'a\\';\nSELECT E'b\\';';\n");
    assert_eq!(found[0].0, "SELECT 'a\\';");
    assert_eq!(found[1].0, "SELECT E'b\\';';");
}

#[test]
fn a_function_body_with_begin_atomic_is_one_statement() {
    let script = "CREATE FUNCTION f() RETURNS int LANGUAGE sql\nBEGIN ATOMIC\n  SELECT CASE WHEN true THEN 1 END;\n  SELECT 2;\nEND;\nSELECT 3;\n";
    let found = statements_of(script);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].0.ends_with("SELECT 2;\nEND;"));
    assert_eq!(found[1], ("SELECT 3;".to_owned(), 6));
    let begin = statements_of("BEGIN;\nCREATE TABLE atomic (begin int);\nCOMMIT;\n");
    assert_eq!(begin.len(), 3);
}

#[test]
fn meta_commands_are_lines_that_start_with_a_backslash_outside_quotes() {
    let items = items_of(
        b"\\restrict abc123\n\\connect -reuse-previous=on \"dbname='x'\"\nSELECT '\n\\not a command';\n  \\echo hi\n\\unrestrict abc123\n",
    )
    .unwrap();
    let names: Vec<String> = items
        .iter()
        .map(|item| match item {
            Item::Meta(meta) => format!("\\{} {} @{}", meta.name, meta.argument, meta.line),
            Item::Statement(statement) => format!("{} @{}", statement.text, statement.line),
            Item::End => "end".to_owned(),
        })
        .collect();
    assert_eq!(
        names,
        [
            "\\restrict abc123 @1",
            "\\connect -reuse-previous=on \"dbname='x'\" @2",
            "SELECT '\n\\not a command'; @3",
            "\\echo hi @5",
            "\\unrestrict abc123 @6",
        ]
    );
}

#[test]
fn copy_data_runs_to_a_line_of_backslash_dot_and_the_rest_of_its_line_runs_after() {
    let items = items_of(
        b"COPY t (a, b) FROM stdin; SELECT 'after';\n1\tx\\.y\n\\.z\n2\t\\N\n\\.\nSELECT 'next';\n",
    )
    .unwrap();
    let Item::Statement(copy) = &items[0] else {
        panic!("{items:?}")
    };
    assert!(copy.copy_from_stdin);
    assert_eq!(copy.text, "COPY t (a, b) FROM stdin;");
    let Item::Meta(data) = &items[1] else {
        panic!("{items:?}")
    };
    assert_eq!(data.argument, "1\tx\\.y\n\\.z\n2\t\\N\n");
    let rest: Vec<String> = items[2..]
        .iter()
        .map(|item| match item {
            Item::Statement(statement) => format!("{}@{}", statement.text, statement.line),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(rest, ["SELECT 'after';@1", "SELECT 'next';@6"]);
}

#[test]
fn copy_data_ends_at_crlf_backslash_dot_and_a_dump_cut_inside_it_is_an_error() {
    let items = items_of(b"COPY t FROM STDIN;\r\n1\r\n\\.\r\nSELECT 1;\r\n").unwrap();
    let Item::Meta(data) = &items[1] else {
        panic!("{items:?}")
    };
    assert_eq!(data.argument, "1\r\n");
    assert!(matches!(&items[2], Item::Statement(statement) if statement.text == "SELECT 1;"));
    for cut in [
        &b"COPY t FROM stdin;\n1\n2"[..],
        b"SELECT 1;\nCOPY t FROM stdin;\n1\n",
    ] {
        let error = items_of(cut).unwrap_err();
        assert_eq!(error.state().code(), "22P04");
        assert!(error.message().contains("COPY at line"), "{error}");
        let mut input = cut;
        let mut reader = DumpReader::new(&mut input);
        while !matches!(reader.next_item(), Ok(Item::Statement(statement)) if statement.copy_from_stdin)
        {
        }
        assert_eq!(reader.skip_copy_data().unwrap_err().state().code(), "22P04");
    }
    assert!(
        !matches!(&items_of(b"COPY (SELECT 1) TO STDOUT;\n").unwrap()[0], Item::Statement(statement) if statement.copy_from_stdin)
    );
    assert!(
        !matches!(&items_of(b"COPY t FROM 'stdin';\n").unwrap()[0], Item::Statement(statement) if statement.copy_from_stdin)
    );
}

#[test]
fn a_byte_order_mark_is_skipped_and_crlf_lines_work() {
    let found = statements_of("\u{feff}SELECT 1;\r\nSELECT\r\n 2;\r\n");
    assert_eq!(found[0], ("SELECT 1;".to_owned(), 1));
    assert_eq!(found[1], ("SELECT\r\n 2;".to_owned(), 2));
}

#[test]
fn a_dump_cut_inside_a_quote_or_comment_is_an_error_naming_its_line() {
    for (script, what) in [
        (
            "SELECT 1;\nSELECT 'open\n",
            "a quoted string that starts at line 2",
        ),
        (
            "SELECT $x$ open\n;\n",
            "a dollar-quoted string that starts at line 1",
        ),
        (
            "SELECT \"open\n",
            "a quoted identifier that starts at line 1",
        ),
        (
            "SELECT 1; /* open /* */\n",
            "a comment that starts at line 1",
        ),
    ] {
        let error = items_of(script.as_bytes()).unwrap_err();
        assert_eq!(error.state().code(), "42601", "{script}");
        assert!(error.message().contains(what), "{script}: {error}");
    }
}

#[test]
fn bytes_that_are_not_utf8_are_an_error_naming_their_line() {
    let error = items_of(b"SELECT 1;\nSELECT '\xff';\n").unwrap_err();
    assert_eq!(error.state().code(), "22021");
    assert!(error.message().contains("line 2"), "{error}");
}

struct Endless(u8);

impl Read for Endless {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        buf.fill(self.0);
        Ok(buf.len())
    }
}

#[test]
fn lines_and_statements_past_their_limits_are_refused_before_they_grow() {
    let mut endless = BufReader::new(Endless(b'a'));
    let mut reader = DumpReader::with_limits(&mut endless, 4096, 8192);
    let error = reader.next_item().unwrap_err();
    assert_eq!(error.state().code(), "54000");
    let long_statement = "SELECT\n".repeat(2000);
    let mut input = long_statement.as_bytes();
    let mut reader = DumpReader::with_limits(&mut input, 4096, 8192);
    assert_eq!(reader.next_item().unwrap_err().state().code(), "54000");
    let copy = format!("COPY t FROM stdin;\n{}\n", "x".repeat(5000));
    let mut input = copy.as_bytes();
    let mut reader = DumpReader::with_limits(&mut input, 4096, 8192);
    assert!(matches!(reader.next_item(), Ok(Item::Statement(_))));
    let mut data = Vec::new();
    assert_eq!(
        reader.copy_data(&mut data).unwrap_err().state().code(),
        "54000"
    );
    let mut input = copy.as_bytes();
    let mut reader = DumpReader::with_limits(&mut input, 4096, 8192);
    assert!(matches!(reader.next_item(), Ok(Item::Statement(_))));
    assert_eq!(reader.next_item().unwrap_err().state().code(), "54000");
}

struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound.max(1) as u64) as usize
    }
}

#[test]
fn the_reader_survives_hostile_dumps_and_splits_generated_ones_exactly() {
    let fragments: &[&[u8]] = &[
        b"'",
        b"\"",
        b"$$",
        b"$a$",
        b"/*",
        b"*/",
        b"--",
        b";",
        b"(",
        b")",
        b"\n",
        b"\r\n",
        b"\\",
        b"\\.",
        b"\\connect x",
        b"COPY t FROM stdin",
        b"E'",
        b"BEGIN ATOMIC",
        b"END",
        b"CREATE FUNCTION",
        b"CASE",
        b"\xff",
        b"\xef\xbb\xbf",
        b"select",
        b" ",
        b"\t",
    ];
    let mut random = Seeded(0x005E_ED0F_D0D0);
    for _ in 0..20_000 {
        let mut dump = Vec::new();
        for _ in 0..random.below(30) {
            dump.extend_from_slice(fragments[random.below(fragments.len())]);
        }
        let mut input = dump.as_slice();
        let mut reader = DumpReader::with_limits(&mut input, 256, 512);
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(steps < 10_000, "{dump:?}");
            match reader.next_item() {
                Ok(Item::End) | Err(_) => break,
                Ok(Item::Statement(statement)) => {
                    assert!(statement.line >= 1 && statement.line <= statement.end_line);
                    if statement.copy_from_stdin && random.below(2) == 0 {
                        let mut data = Vec::new();
                        while let Ok(true) = reader.copy_data(&mut data) {}
                    }
                }
                Ok(Item::Meta(_)) => {}
            }
        }
    }
    let statements = [
        "SELECT 'x;y'",
        "SELECT $q$;$q$",
        "CREATE TABLE \"a;b\" (c int CHECK (c > 0))",
        "SELECT E'\\';'",
        "SELECT 1 /* ; */",
    ];
    for _ in 0..2_000 {
        let mut expected = Vec::new();
        let mut dump = String::new();
        for _ in 0..1 + random.below(6) {
            let statement = statements[random.below(statements.len())];
            dump.push_str(["", "\n", "-- c;\n", "  /* ; */ "][random.below(4)]);
            dump.push_str(statement);
            dump.push(';');
            dump.push_str(["", "\n", " ", "\r\n"][random.below(4)]);
            expected.push(format!("{statement};"));
        }
        let found: Vec<String> = statements_of(&dump)
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        assert_eq!(found, expected, "{dump:?}");
    }
}

const DUMP: &str = r"--
-- PostgreSQL database dump
--

\restrict k3y

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;
SET default_toast_compression = 'lz4';

CREATE SCHEMA shop;
ALTER SCHEMA shop OWNER TO postgres;
COMMENT ON SCHEMA shop IS 'the shop';

CREATE EXTENSION IF NOT EXISTS plpgsql WITH SCHEMA pg_catalog;
COMMENT ON EXTENSION plpgsql IS 'PL/pgSQL procedural language';
CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA public;
COMMENT ON EXTENSION pg_trgm IS 'text similarity';

CREATE TYPE public.mood AS ENUM (
    'calm',
    'busy'
);
ALTER TYPE public.mood OWNER TO postgres;

CREATE DOMAIN public.positive AS integer
	CONSTRAINT positive_check CHECK ((VALUE > 0));

CREATE FUNCTION public.touch() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RETURN NEW;
END;
$$;
ALTER FUNCTION public.touch() OWNER TO postgres;

SET default_tablespace = '';
SET default_table_access_method = heap;

CREATE TABLE public.people (
    id integer NOT NULL,
    name text NOT NULL,
    status text,
    CONSTRAINT people_status_check CHECK ((status = ANY (ARRAY['in'::text, 'out'::text])))
);
ALTER TABLE public.people OWNER TO postgres;
COMMENT ON TABLE public.people IS 'who';

CREATE TABLE public.feelings (
    id integer NOT NULL,
    feeling public.mood
);

CREATE TABLE public.events (
    id integer NOT NULL,
    at date NOT NULL
)
PARTITION BY RANGE (at);

CREATE TABLE shop.items (
    id bigint NOT NULL,
    person_id integer,
    label character varying(20)
);
ALTER TABLE shop.items ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME shop.items_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);

CREATE SEQUENCE public.people_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.people_id_seq OWNED BY public.people.id;
ALTER TABLE ONLY public.people ALTER COLUMN id SET DEFAULT nextval('public.people_id_seq'::regclass);

CREATE VIEW shop.labels AS
 SELECT p.name,
    i.label
   FROM (public.people p
     JOIN shop.items i ON ((i.person_id = p.id)));
ALTER VIEW shop.labels OWNER TO postgres;

CREATE MATERIALIZED VIEW public.counts AS
 SELECT count(*) AS n
   FROM public.people
  WITH NO DATA;

COPY public.people (id, name, status) FROM stdin;
1	Ann	in
2	Bo	\N
\.

COPY public.feelings (id, feeling) FROM stdin;
1	calm
\.

COPY public.events (id, at) FROM stdin;
1	2024-01-01
2	2024-01-02
3	2024-01-03
\.

INSERT INTO shop.items OVERRIDING SYSTEM VALUE VALUES (1, 1, 'pen');
INSERT INTO shop.items (id, person_id, label) OVERRIDING SYSTEM VALUE VALUES (2, 2, 'cup');

SELECT pg_catalog.setval('public.people_id_seq', 2, true);
SELECT pg_catalog.setval('shop.items_id_seq', 2, true);

ALTER TABLE ONLY public.people
    ADD CONSTRAINT people_pkey PRIMARY KEY (id);
ALTER TABLE ONLY shop.items
    ADD CONSTRAINT items_pkey PRIMARY KEY (id);
CREATE INDEX items_person_idx ON shop.items USING btree (person_id);
ALTER TABLE ONLY public.people REPLICA IDENTITY FULL;
ALTER TABLE ONLY public.people ALTER COLUMN name SET STATISTICS 500;
CREATE TRIGGER people_touch BEFORE UPDATE ON public.people FOR EACH ROW EXECUTE FUNCTION public.touch();
ALTER TABLE ONLY shop.items
    ADD CONSTRAINT items_person_id_fkey FOREIGN KEY (person_id) REFERENCES public.people(id) ON DELETE CASCADE;
CREATE POLICY people_own ON public.people USING ((name = CURRENT_USER));
ALTER TABLE public.people ENABLE ROW LEVEL SECURITY;
REFRESH MATERIALIZED VIEW public.counts;
REVOKE USAGE ON SCHEMA public FROM PUBLIC;
GRANT SELECT ON TABLE public.people TO reader;
ALTER DEFAULT PRIVILEGES FOR ROLE postgres GRANT SELECT ON TABLES TO reader;

\unrestrict k3y
";

#[test]
fn a_pg_dump_restores_what_eggdb_has_and_reports_the_rest() {
    let store = MemoryStore::default();
    let report = restore(&store, DUMP, false);
    assert_eq!(report.error, None, "{report:#?}");
    assert!(report.succeeded());
    assert_eq!(
        query(
            &store,
            "SELECT p.id, p.name, p.status, i.id, i.label FROM people p JOIN shop.items i ON i.person_id = p.id ORDER BY 1"
        ),
        "id|name|status|id|label\n1|Ann|in|1|pen\n2|Bo|NULL|2|cup\nSELECT 2"
    );
    assert_eq!(
        query(&store, "SELECT * FROM shop.labels ORDER BY 1"),
        "name|label\nAnn|pen\nBo|cup\nSELECT 2"
    );
    assert_eq!(
        query(
            &store,
            "INSERT INTO people (name) VALUES ('Cy') RETURNING id; INSERT INTO shop.items (label) VALUES ('ink') RETURNING id"
        ),
        "id\n3\nINSERT 0 1\nid\n3\nINSERT 0 1"
    );
    assert_eq!(
        (
            report.schemas,
            report.tables,
            report.views,
            report.sequences,
            report.indexes,
            report.constraints,
            report.comments,
            report.rows
        ),
        (1, 3, 1, 2, 1, 4, 3, 5)
    );
    let skipped: Vec<(u64, &str, &str, Option<u64>)> = report
        .skipped
        .iter()
        .map(|object| {
            (
                object.line,
                object.kind.as_str(),
                object.name.as_str(),
                object.rows,
            )
        })
        .collect();
    assert_eq!(
        skipped,
        [
            (26, "extension", "pg_trgm", None),
            (38, "function", "public.touch()", None),
            (64, "table", "public.events", Some(3)),
            (101, "materialized view", "public.counts", None),
            (134, "trigger", "people_touch on public.people", None),
            (137, "policy", "people_own on public.people", None),
            (138, "row-level security", "public.people", None),
        ]
    );
    assert_eq!(report.skipped[2].detail.as_deref(), Some("partitioned"));
    assert_eq!(report.tables_skipped, 1);
    assert_eq!(report.rows_skipped, 3);
    let consequences: Vec<(u64, &str)> = report
        .consequences
        .iter()
        .map(|consequence| (consequence.line, consequence.uses.as_str()))
        .collect();
    assert_eq!(
        consequences,
        [(139, "materialized view public.counts")]
    );
    let ignored = &report.ignored;
    assert_eq!((ignored.owner_to, ignored.grant, ignored.revoke), (5, 1, 1));
    assert_eq!(ignored.set, ["default_toast_compression"]);
    assert_eq!(
        ignored.other,
        [
            ("COMMENT ON EXTENSION".to_owned(), 1),
            ("storage settings".to_owned(), 2),
            ("ALTER DEFAULT PRIVILEGES".to_owned(), 1)
        ]
    );
    assert!(ignored.meta_commands.is_empty());
    let rows: Vec<(&str, u64)> = report
        .rows_per_table
        .iter()
        .map(|table| (table.table.as_str(), table.rows))
        .collect();
    assert_eq!(
        rows,
        [("public.people", 2), ("public.feelings", 1), ("shop.items", 2)]
    );
    assert_eq!(
        query(&store, "SELECT id, feeling FROM public.feelings"),
        "id|feeling\n1|calm\nSELECT 1"
    );
    assert_eq!(
        query(&store, "SELECT 5::public.positive AS n"),
        "n\n5\nSELECT 1"
    );
    assert!(
        Session::new(&store)
            .execute("SELECT 0::public.positive", &mut |_| {})
            .is_err(),
        "the domain's check was restored"
    );
}

#[test]
fn a_row_that_breaks_a_constraint_stops_the_restore_at_its_data_line() {
    let store = MemoryStore::default();
    let dump = "CREATE TABLE t (id integer PRIMARY KEY, n integer CHECK (n > 0));\nCOPY t (id, n) FROM stdin;\n1\t1\n2\t-5\n\\.\nCREATE TABLE after (id integer);\n";
    let report = restore(&store, dump, false);
    let error = report.error.expect("an error");
    assert_eq!(error.error.state().code(), "23514");
    assert_eq!(error.line, 4);
    assert_eq!(error.statement, "COPY t (id, n) FROM stdin;");
    assert_eq!(error.statements_before, 1);
    assert_eq!(report.statements, 2);
    assert_eq!(
        query(&store, "SELECT count(*) FROM t"),
        "count\n0\nSELECT 1"
    );
    assert!(
        Session::new(&store)
            .execute("SELECT * FROM after", &mut |_| {})
            .is_err()
    );
}

#[test]
fn a_varchar_check_against_a_text_array_holds_after_a_restore() {
    let store = MemoryStore::default();
    let dump = "CREATE TABLE logs (\n    id integer NOT NULL,\n    level character varying(10),\n    CONSTRAINT logs_level_check CHECK (((level)::text = ANY ((ARRAY['debug'::character varying, 'info'::character varying])::text[])))\n);\nCOPY public.logs (id, level) FROM stdin;\n1\tdebug\n2\tinfo\n\\.\n";
    let report = restore(&store, dump, false);
    assert_eq!(report.error, None);
    assert_eq!(
        query(&store, "SELECT level FROM logs ORDER BY id"),
        "level\ndebug\ninfo\nSELECT 2"
    );
    let refused = Session::new(&store)
        .execute("INSERT INTO logs VALUES (3, 'trace')", &mut |_| {})
        .expect_err("the CHECK holds");
    assert_eq!(refused.state().code(), "23514");
}

#[test]
fn keep_going_lists_every_error_and_restores_the_rest() {
    let store = MemoryStore::default();
    let dump = "CREATE TABLE t (id integer PRIMARY KEY);\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (1);\nSELEC 2;\nINSERT INTO t VALUES (2);\n\\unrestrict nope\n";
    let report = restore(&store, dump, true);
    assert_eq!(report.error, None);
    let errors: Vec<(u64, String)> = report
        .errors
        .iter()
        .map(|error| (error.line, error.error.state().code().to_owned()))
        .collect();
    assert_eq!(
        errors,
        [
            (3, "23505".to_owned()),
            (4, "42601".to_owned()),
            (6, "42601".to_owned())
        ]
    );
    assert!(!report.succeeded());
    assert_eq!(report.rows, 2);
    assert_eq!(
        query(&store, "SELECT count(*) FROM t"),
        "count\n2\nSELECT 1"
    );
}

#[test]
fn restrict_keys_must_match() {
    let store = MemoryStore::default();
    let report = restore(
        &store,
        "\\restrict one\nCREATE TABLE t (a int);\n\\unrestrict two\n",
        false,
    );
    let error = report.error.expect("a mismatch");
    assert_eq!(error.line, 3);
    assert!(
        error
            .error
            .message()
            .contains("does not match the \\restrict key of line 1"),
        "{error:?}"
    );
}

#[test]
fn meta_commands_other_than_restrict_are_listed_and_never_run() {
    let store = MemoryStore::default();
    let report = restore(
        &store,
        "\\connect app\n\\set ON_ERROR_STOP on\n\\! rm -rf /\n\\i other.sql\n\\.\nCREATE TABLE t (a int);\n",
        false,
    );
    assert!(report.succeeded());
    let notes: Vec<(u64, &str, &str)> = report
        .ignored
        .meta_commands
        .iter()
        .map(|note| (note.line, note.command.as_str(), note.reason.as_str()))
        .collect();
    assert_eq!(
        notes,
        [
            (1, "\\connect", "one database per file"),
            (2, "\\set", "never run"),
            (3, "\\!", "never run"),
            (4, "\\i", "never run"),
        ]
    );
    assert_eq!(report.tables, 1);
}

#[test]
fn every_kind_eggdb_cannot_create_yet_is_reported_with_its_name() {
    let cases = [
        (
            "CREATE OR REPLACE FUNCTION s.f(a integer) RETURNS integer LANGUAGE c AS 'lib', 'f';",
            "function",
            "s.f(…)",
        ),
        (
            "CREATE PROCEDURE p() LANGUAGE plperl AS $$ return; $$;",
            "procedure",
            "p()",
        ),
        (
            "CREATE AGGREGATE public.total(integer) (SFUNC = int4pl, STYPE = integer);",
            "aggregate",
            "public.total(…)",
        ),
        (
            "CREATE CONSTRAINT TRIGGER c AFTER INSERT ON public.t FOR EACH ROW EXECUTE FUNCTION f();",
            "trigger",
            "c on public.t",
        ),
        (
            "CREATE EVENT TRIGGER e ON ddl_command_start EXECUTE FUNCTION f();",
            "event trigger",
            "e",
        ),
        (
            "CREATE RULE r AS ON INSERT TO public.t DO INSTEAD NOTHING;",
            "rule",
            "r on public.t",
        ),
        (
            "CREATE MATERIALIZED VIEW m AS SELECT 1 WITH NO DATA;",
            "materialized view",
            "m",
        ),
        (
            "CREATE PUBLICATION pub FOR ALL TABLES;",
            "publication",
            "pub",
        ),
        (
            "CREATE SUBSCRIPTION sub CONNECTION 'x' PUBLICATION pub;",
            "subscription",
            "sub",
        ),
        (
            "CREATE EXTENSION IF NOT EXISTS hstore WITH SCHEMA public;",
            "extension",
            "hstore",
        ),
        (
            "CREATE TABLE public.child (b integer) INHERITS (public.parent);",
            "table",
            "public.child",
        ),
        (
            "CREATE TABLE public.part PARTITION OF public.events FOR VALUES FROM (1) TO (2);",
            "table",
            "public.part",
        ),
        (
            "CREATE COLLATION public.c (provider = icu, locale = 'und');",
            "collation",
            "public.c",
        ),
        (
            "CREATE TEXT SEARCH CONFIGURATION public.ts (COPY = english);",
            "text search object",
            "public.ts",
        ),
        (
            "CREATE FOREIGN TABLE public.f (a integer) SERVER s;",
            "foreign table",
            "public.f",
        ),
        ("CREATE SERVER s FOREIGN DATA WRAPPER w;", "server", "s"),
        (
            "CREATE OPERATOR public.=== (FUNCTION = f, LEFTARG = integer, RIGHTARG = integer);",
            "operator",
            "public",
        ),
        ("CREATE ROLE app;", "role", "app"),
        ("CREATE TABLE t (a money);", "table", "t"),
    ];
    for (statement, kind, name) in cases {
        let store = MemoryStore::default();
        let report = restore(&store, &format!("SELECT 1;\n{statement}\n"), false);
        assert!(report.succeeded(), "{statement}: {report:#?}");
        assert_eq!(report.skipped.len(), 1, "{statement}: {report:#?}");
        let object = &report.skipped[0];
        assert_eq!(
            (object.line, object.kind.as_str(), object.name.as_str()),
            (2, kind, name),
            "{statement}"
        );
        assert!(!object.reason.is_empty());
    }
}

#[test]
fn rows_of_a_skipped_table_are_counted_for_copy_and_inserts() {
    let store = MemoryStore::default();
    let dump = "CREATE TABLE public.m (a money);\nCOPY public.m (a) FROM stdin;\n1\n2\n\\.\nINSERT INTO public.m VALUES ('1'), ('2'), ('3');\nINSERT INTO m (a) VALUES ('4');\n";
    let report = restore(&store, dump, false);
    assert!(report.succeeded(), "{report:#?}");
    assert_eq!(
        report.skipped[0].detail.as_deref(),
        Some("not supported yet")
    );
    assert_eq!(report.skipped[0].rows, Some(6));
    assert_eq!((report.tables_skipped, report.rows_skipped), (1, 6));
}

#[test]
fn a_statement_failing_on_a_name_it_shares_with_a_skipped_object_is_still_an_error() {
    let store = MemoryStore::default();
    let dump = "CREATE FUNCTION public.total() RETURNS integer LANGUAGE c AS 'lib', 'total';\nCREATE TABLE t (total integer PRIMARY KEY);\nINSERT INTO t (total) VALUES (1);\nINSERT INTO t (total) VALUES (1);\n";
    let report = restore(&store, dump, false);
    let error = report.error.expect("the duplicate is an error");
    assert_eq!((error.line, error.error.state().code()), (4, "23505"));
}

#[test]
fn unknown_set_parameters_are_ignored_and_bad_values_of_known_ones_are_errors() {
    let store = MemoryStore::default();
    let report = restore(
        &store,
        "SET default_toast_compression = 'pglz';\nSET my.setting = 1;\nSET SESSION AUTHORIZATION 'postgres';\nSET client_encoding = 'LATIN1';\n",
        false,
    );
    assert_eq!(
        report.ignored.set,
        ["default_toast_compression", "my.setting"]
    );
    assert_eq!(
        report.ignored.other,
        [("SET SESSION AUTHORIZATION".to_owned(), 1)]
    );
    assert_eq!(report.error.expect("LATIN1 is refused").line, 4);
}

#[test]
fn inserts_and_column_inserts_restore_as_copy_does() {
    let store = MemoryStore::default();
    let report = restore(
        &store,
        "CREATE TABLE public.t (id integer, body text, raw bytea);\nINSERT INTO public.t VALUES (1, 'a\tb\nc', '\\x00ff');\nINSERT INTO public.t (id, body, raw) VALUES (2, 'it''s', NULL);\nINSERT INTO public.t (id, body, raw) VALUES (3, NULL, '\\x'), (4, '', '\\x41');\n",
        false,
    );
    assert!(report.succeeded(), "{report:#?}");
    assert_eq!(report.rows, 4);
    let mut session = Session::new(&store);
    let mut out = Vec::new();
    session
        .execute("SELECT id, body, raw FROM t ORDER BY id", &mut |result| {
            out.push(render(&result));
        })
        .unwrap();
    assert_eq!(
        out[0],
        "id|body|raw\n1|a\tb\nc|\\x00ff\n2|it's|NULL\n3|NULL|\\x\n4||\\x41\nSELECT 4"
    );
}

#[test]
fn a_dialect_other_than_postgresql_is_refused() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, crate::Dialect::MySql);
    let mut input: &[u8] = b"CREATE TABLE t (a int);\n";
    let report = restore_dump(
        &mut session,
        &mut input,
        RestoreOptions::default(),
        &mut |_| {},
    );
    assert_eq!(
        report
            .error
            .map(|error| error.error.state().code().to_owned()),
        Some("0A000".to_owned())
    );
}

#[test]
fn progress_reports_lines_tables_and_rows_during_a_long_copy() {
    let store = MemoryStore::default();
    let mut dump = String::from("CREATE TABLE t (n integer);\nCOPY t (n) FROM stdin;\n");
    for n in 0..30_000 {
        dump.push_str(&format!("{n:08}\n"));
    }
    dump.push_str("\\.\n");
    let mut session = Session::new(&store);
    let mut seen = Vec::new();
    let mut input = BufReader::with_capacity(512, dump.as_bytes());
    let input: &mut dyn BufRead = &mut input;
    let report = restore_dump(
        &mut session,
        input,
        RestoreOptions::default(),
        &mut |progress| seen.push(*progress),
    );
    assert!(report.succeeded());
    assert_eq!(report.rows, 30_000);
    assert!(
        seen.iter()
            .any(|progress| progress.rows > 0 && progress.rows < 30_000)
    );
    assert_eq!(seen.last().map(|progress| progress.rows), Some(30_000));
}

#[test]
fn a_line_of_a_million_empty_statements_is_read_in_one_pass() {
    let mut line = ";".repeat(1_000_000);
    line.push_str(" SELECT 1; SELECT 2;\n");
    let started = std::time::Instant::now();
    let found = statements_of(&line);
    assert_eq!(
        found,
        [("SELECT 1;".to_owned(), 1), ("SELECT 2;".to_owned(), 1)]
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}

const IDENTITY_ON_SKIPPED_TABLES: &str = r"\restrict key1
SELECT pg_catalog.set_config('search_path', '', false);
CREATE TYPE public.status AS ENUM (
    'new',
    'done'
);
CREATE TABLE public.inherit (
    id integer NOT NULL
);
CREATE TABLE public.m (
    id integer NOT NULL,
    at date NOT NULL
)
PARTITION BY RANGE (at);
CREATE TABLE public.m_2024 (
    id integer NOT NULL,
    at date NOT NULL
);
ALTER TABLE public.m ALTER COLUMN id ADD GENERATED BY DEFAULT AS IDENTITY (
    SEQUENCE NAME public.m_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);
CREATE TABLE public.tasks (
    id bigint NOT NULL,
    s public.status NOT NULL,
    m integer
);
ALTER TABLE public.tasks ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.tasks_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);
CREATE TABLE public.refs (
    id integer NOT NULL,
    r integer
);
ALTER TABLE ONLY public.m ATTACH PARTITION public.m_2024 FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
COPY public.inherit (id) FROM stdin;
1
\.
COPY public.m_2024 (id, at) FROM stdin;
1	2024-02-02
\.
COPY public.tasks (id, s, m) FROM stdin;
1	new	\N
\.
INSERT INTO public.tasks OVERRIDING SYSTEM VALUE VALUES (2, 'done', NULL);
COPY public.refs (id, r) FROM stdin;
1	1
\.
SELECT pg_catalog.setval('public.m_id_seq', 2, true);
SELECT pg_catalog.setval('public.tasks_id_seq', 2, true);
ALTER TABLE ONLY public.tasks
    ADD CONSTRAINT tasks_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.inherit
    ADD CONSTRAINT inherit_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.refs
    ADD CONSTRAINT refs_r_fkey FOREIGN KEY (r) REFERENCES public.inherit(id);
\unrestrict key1
";

#[test]
fn identity_columns_of_skipped_tables_and_their_setval_are_consequences() {
    let store = MemoryStore::default();
    let report = restore(&store, IDENTITY_ON_SKIPPED_TABLES, false);
    assert!(report.succeeded(), "{report:#?}");
    let consequences: Vec<(u64, &str)> = report
        .consequences
        .iter()
        .map(|consequence| (consequence.line, consequence.uses.as_str()))
        .collect();
    assert_eq!(
        consequences,
        [(19, "table public.m"), (58, "sequence public.m_id_seq")]
    );
    assert_eq!(report.skipped[1].kind, "partition");
    assert_eq!((report.tables_skipped, report.rows_skipped), (1, 0));
    assert_eq!((report.tables, report.constraints), (4, 3));
    assert_eq!(
        query(&store, "SELECT id, s FROM tasks ORDER BY id"),
        "id|s\n1|new\n2|done\nSELECT 2"
    );
    assert_eq!(
        query(
            &store,
            "SELECT * FROM refs JOIN inherit ON inherit.id = refs.r"
        ),
        "id|r|id\n1|1|1\nSELECT 1"
    );
    assert!(
        Session::new(&store)
            .execute("INSERT INTO refs VALUES (2, 9)", &mut |_| {})
            .is_err(),
        "the foreign key to the table named inherit was created"
    );
}

#[test]
fn a_cut_dump_is_an_error_not_a_success() {
    for (dump, said) in [
        (
            "\\restrict k\nCREATE TABLE public.t (a integer);\nINSERT INTO public.t VALUES (1);\n",
            "the dump ends without the \\unrestrict that closes the \\restrict of line 1",
        ),
        (
            "CREATE TABLE public.t (a integer);\nCOPY public.t (a) FROM stdin;\n1\n2\n",
            "the dump ends inside the data of the COPY at line 2",
        ),
        (
            "CREATE TABLE public.p (a integer) PARTITION BY RANGE (a);\nCOPY public.p (a) FROM stdin;\n1\n",
            "the dump ends inside the data of the COPY at line 2",
        ),
    ] {
        for keep_going in [false, true] {
            let store = MemoryStore::default();
            let report = restore(&store, dump, keep_going);
            assert!(!report.succeeded(), "{dump}");
            let error = report
                .error
                .as_ref()
                .or(report.errors.first())
                .expect("an error");
            assert!(error.error.message().contains(said), "{dump}: {error:?}");
        }
    }
}

#[test]
fn a_data_statement_naming_a_skipped_object_is_still_an_error() {
    let store = MemoryStore::default();
    let report = restore(
        &store,
        "CREATE FUNCTION public.f() RETURNS integer LANGUAGE c AS 'lib', 'f';\nCREATE TABLE public.t (a integer);\nINSERT INTO public.t (a) VALUES (public.f());\n",
        false,
    );
    let error = report.error.expect("the INSERT is an error");
    assert_eq!(error.line, 3);
    assert!(report.consequences.is_empty());
}

const CLEAN_DUMP_WITH_USER_TYPES: &str = "SELECT pg_catalog.set_config('search_path', '', false);
ALTER TABLE ONLY public.child DROP CONSTRAINT child_parent_id_fkey;
ALTER TABLE ONLY public.parent DROP CONSTRAINT parent_pkey;
DROP TABLE public.child;
DROP TABLE public.parent;
DROP TYPE public.pair;
DROP TYPE public.span;
DROP DOMAIN public.positive;
DROP TYPE public.mood;
CREATE TYPE public.mood AS ENUM (
    'calm',
    'busy'
);
CREATE DOMAIN public.positive AS integer
\tCONSTRAINT positive_check CHECK ((VALUE > 0));
CREATE TYPE public.span AS RANGE (
    subtype = integer,
    multirange_type_name = public.span_multi
);
CREATE TYPE public.pair AS (
\ta integer,
\tb text
);
CREATE TABLE public.parent (
    id integer NOT NULL,
    feeling public.mood,
    amount public.positive,
    tags text[],
    during public.span,
    p public.pair
);
CREATE TABLE public.child (
    id integer NOT NULL,
    parent_id integer
);
COPY public.child (id, parent_id) FROM stdin;
1\t1
\\.
COPY public.parent (id, feeling, amount, tags, during, p) FROM stdin;
1\tbusy\t5\t{a,\"b c\"}\t[1,4)\t(7,x)
\\.
ALTER TABLE ONLY public.parent
    ADD CONSTRAINT parent_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.child
    ADD CONSTRAINT child_parent_id_fkey FOREIGN KEY (parent_id) REFERENCES public.parent(id) DEFERRABLE INITIALLY DEFERRED;
ALTER TABLE public.parent
    ADD CONSTRAINT parent_amount_small CHECK (((amount)::integer < 3)) NOT VALID;
";

#[test]
fn a_clean_dump_with_user_types_restores_over_itself() {
    let store = MemoryStore::default();
    for round in 0..2 {
        let report = restore(&store, CLEAN_DUMP_WITH_USER_TYPES, false);
        assert_eq!(report.error, None, "round {round}: {report:#?}");
        assert!(report.skipped.is_empty(), "round {round}: {report:#?}");
        assert!(report.consequences.is_empty(), "round {round}: {report:#?}");
        assert_eq!(report.tables, 2, "round {round}");
        assert_eq!(report.rows, 2, "round {round}");
        let nothing_dropped = report
            .ignored
            .other
            .iter()
            .find(|(what, _)| what == super::classify_oracle::NOTHING_TO_DROP)
            .map(|(_, count)| *count);
        assert_eq!(
            nothing_dropped,
            if round == 0 { Some(8) } else { None },
            "round {round}: {report:#?}"
        );
    }
    assert_eq!(
        query(
            &store,
            "SELECT id, feeling, amount, tags, during, p, (p).b AS b FROM public.parent"
        ),
        "id|feeling|amount|tags|during|p|b\n1|busy|5|{a,\"b c\"}|[1,4)|(7,x)|x\nSELECT 1"
    );
    assert_eq!(
        query(
            &store,
            "SELECT conname, convalidated, condeferred FROM pg_constraint \
             WHERE conname IN ('parent_amount_small', 'child_parent_id_fkey') ORDER BY conname"
        ),
        "conname|convalidated|condeferred\nchild_parent_id_fkey|t|t\nparent_amount_small|f|f\nSELECT 2"
    );
    assert_eq!(
        query(
            &store,
            "SELECT typname, typtype FROM pg_type \
             WHERE typname IN ('mood', 'positive', 'span', 'pair') ORDER BY typname"
        ),
        "typname|typtype\nmood|e\npair|c\npositive|d\nspan|r\nSELECT 4"
    );
}

#[test]
fn pg_dumpall_globals_go_to_the_report_and_the_database_restores() {
    let store = MemoryStore::default();
    let dump = "SET default_transaction_read_only = off;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
CREATE ROLE app;
ALTER ROLE app WITH NOSUPERUSER INHERIT NOCREATEROLE NOCREATEDB LOGIN NOREPLICATION NOBYPASSRLS PASSWORD 'SCRAM-SHA-256$4096:c2FsdA==$a2V5:c2VydmVy';
CREATE ROLE postgres;
ALTER ROLE postgres WITH SUPERUSER INHERIT CREATEROLE CREATEDB LOGIN REPLICATION BYPASSRLS;
GRANT pg_read_all_data TO app GRANTED BY postgres;
\\connect template1
\\connect postgres
CREATE DATABASE shop WITH TEMPLATE = template0 ENCODING = 'UTF8' LOCALE_PROVIDER = libc LOCALE = 'C';
ALTER DATABASE shop OWNER TO postgres;
\\connect shop
CREATE TYPE public.mood AS ENUM ('calm', 'busy');
CREATE TABLE public.t (a public.mood);
INSERT INTO public.t VALUES ('calm');
";
    let report = restore(&store, dump, false);
    assert_eq!(report.error, None, "{report:#?}");
    let skipped: Vec<(&str, &str)> = report
        .skipped
        .iter()
        .map(|object| (object.kind.as_str(), object.name.as_str()))
        .collect();
    assert_eq!(
        skipped,
        [("role", "app"), ("role", "postgres"), ("database", "shop")]
    );
    assert_eq!(report.ignored.grant, 1);
    assert_eq!(report.ignored.owner_to, 1);
    assert!(
        report
            .ignored
            .other
            .iter()
            .any(|(what, count)| what == "ALTER DATABASE and ALTER ROLE" && *count == 2),
        "{report:#?}"
    );
    assert_eq!(report.ignored.meta_commands.len(), 3, "{report:#?}");
    assert_eq!(query(&store, "SELECT a FROM t"), "a\ncalm\nSELECT 1");
}

#[test]
fn postgresql_functions_procedures_and_triggers_are_restored_and_run() {
    let store = MemoryStore::default();
    let dump = "SET check_function_bodies = false;
CREATE TABLE public.t (id integer PRIMARY KEY, note text, hits integer DEFAULT 0);
CREATE FUNCTION public.touch() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.hits := NEW.hits + 1;
    RETURN NEW;
END;
$$;
ALTER FUNCTION public.touch() OWNER TO postgres;
CREATE PROCEDURE public.reset_hits(IN target integer)
    LANGUAGE sql
    AS $$ UPDATE public.t SET hits = 0 WHERE id = target $$;
CREATE FUNCTION public.native() RETURNS integer LANGUAGE c AS 'lib', 'native';
COPY public.t (id, note, hits) FROM stdin;
1\tone\t5
\\.
CREATE TRIGGER t_touch BEFORE UPDATE ON public.t FOR EACH ROW EXECUTE FUNCTION public.touch();
CREATE TRIGGER t_off BEFORE UPDATE ON public.t FOR EACH ROW EXECUTE FUNCTION public.touch();
ALTER TABLE public.t DISABLE TRIGGER t_off;
";
    let report = restore(&store, dump, false);
    assert!(report.succeeded(), "{report:#?}");
    let skipped: Vec<(&str, &str)> = report
        .skipped
        .iter()
        .map(|object| (object.kind.as_str(), object.name.as_str()))
        .collect();
    assert_eq!(skipped, [("function", "public.native()")]);
    assert_eq!(report.rows, 1);
    let mut session = Session::new(&store);
    let mut found = Vec::new();
    session
        .execute(
            "UPDATE public.t SET note = 'two'; SELECT hits FROM public.t",
            &mut |result| found.extend(result.rows.into_iter().map(|row| row[0].to_text())),
        )
        .unwrap();
    assert_eq!(found, [Some("6".to_owned())]);
    found.clear();
    session
        .execute(
            "CALL public.reset_hits(1); SELECT hits FROM public.t",
            &mut |result| found.extend(result.rows.into_iter().map(|row| row[0].to_text())),
        )
        .unwrap();
    assert_eq!(found, [Some("1".to_owned())]);
}
