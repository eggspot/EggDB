//! The PostgreSQL wire protocol spoken over a real loopback socket by a minimal client written
//! here: startup, SCRAM-SHA-256, the simple and extended query protocols, errors and recovery,
//! transaction blocks, concurrent connections, the connection limit and a graceful stop.

mod common;

use common::{Client, DATABASE, Message, TestServer, USER, cstring, ready_status, tags};
use std::time::Duration;

fn bind(
    portal: &str,
    statement: &str,
    formats: &[i16],
    values: &[Option<&[u8]>],
    results: &[i16],
) -> Vec<u8> {
    let mut body = cstring(portal);
    body.extend(cstring(statement));
    body.extend_from_slice(&(formats.len() as i16).to_be_bytes());
    for format in formats {
        body.extend_from_slice(&format.to_be_bytes());
    }
    body.extend_from_slice(&(values.len() as i16).to_be_bytes());
    for value in values {
        match value {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(bytes) => {
                body.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                body.extend_from_slice(bytes);
            }
        }
    }
    body.extend_from_slice(&(results.len() as i16).to_be_bytes());
    for format in results {
        body.extend_from_slice(&format.to_be_bytes());
    }
    body
}

fn parse(name: &str, query: &str, types: &[u32]) -> Vec<u8> {
    let mut body = cstring(name);
    body.extend(cstring(query));
    body.extend_from_slice(&(types.len() as i16).to_be_bytes());
    for oid in types {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    body
}

fn execute(portal: &str, max_rows: i32) -> Vec<u8> {
    let mut body = cstring(portal);
    body.extend_from_slice(&max_rows.to_be_bytes());
    body
}

fn rows(messages: &[Message]) -> Vec<Vec<Option<String>>> {
    messages
        .iter()
        .filter(|message| message.tag == b'D')
        .map(Message::row)
        .collect()
}

#[test]
fn startup_refuses_encryption_authenticates_and_reports_parameters() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    client.send_request(80_877_103);
    assert_eq!(client.read_byte(), Some(b'N'));
    client.send_request(80_877_104);
    assert_eq!(client.read_byte(), Some(b'N'));
    client.send_startup(
        196_608,
        &[
            ("user", USER),
            ("database", DATABASE),
            ("client_encoding", "UTF8"),
            ("DateStyle", "ISO, MDY"),
            ("extra_float_digits", "3"),
            ("no_such_option", "ignored"),
        ],
    );
    let ok = client.authenticate(common::PASSWORD);
    assert_eq!(ok.tag, b'R');
    let greeting = client.read_until_ready();
    let statuses: Vec<String> = greeting
        .iter()
        .filter(|message| message.tag == b'S')
        .map(Message::text)
        .collect();
    for expected in [
        "server_version\u{0}16.0 (EggDB",
        "client_encoding\0UTF8",
        "server_encoding\0UTF8",
        "DateStyle\0ISO, MDY",
        "TimeZone\0UTC",
        "integer_datetimes\0on",
        "standard_conforming_strings\0on",
        "session_authorization\0ada",
        "is_superuser\0on",
    ] {
        assert!(
            statuses.iter().any(|status| status.starts_with(expected)),
            "{expected:?} in {statuses:?}"
        );
    }
    assert!(greeting.iter().any(|message| message.tag == b'K'));
    assert_eq!(ready_status(&greeting), b'I');
    let answer = client.query("SELECT current_user, current_database(), version()");
    let row = &rows(&answer)[0];
    assert_eq!(row[0].as_deref(), Some(USER));
    assert_eq!(row[1].as_deref(), Some(DATABASE));
    assert!(
        row[2]
            .as_deref()
            .unwrap()
            .starts_with("PostgreSQL 16.0 compatible")
    );
}

#[test]
fn a_wrong_password_an_unknown_user_and_another_database_are_refused() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    client.send_startup(196_608, &[("user", USER), ("database", DATABASE)]);
    let refused = client.authenticate("wrong");
    assert_eq!((refused.tag, refused.code()), (b'E', "28P01".to_owned()));
    assert_eq!(refused.field(b'S').as_deref(), Some("FATAL"));
    assert_eq!(client.read(), None);
    let mut client = Client::connect(server.address);
    client.send_startup(196_608, &[("user", "mallory")]);
    let refused = client.authenticate(common::PASSWORD);
    assert_eq!(refused.code(), "28P01");
    let mut client = Client::connect(server.address);
    client.send_startup(196_608, &[("user", USER), ("database", "other")]);
    let ok = client.authenticate(common::PASSWORD);
    assert_eq!(ok.tag, b'R');
    let refused = client.read().unwrap();
    assert_eq!(refused.code(), "3D000");
    let mut client = Client::connect(server.address);
    client.send_startup(196_608, &[("database", DATABASE)]);
    assert_eq!(client.read().unwrap().code(), "28000");
    let mut client = Client::connect(server.address);
    client.send_startup(0x0002_0000, &[]);
    assert_eq!(client.read().unwrap().code(), "0A000");
    let logged = server.logged.lock().unwrap().join("\n");
    assert!(
        logged.contains("password authentication failed for user \"ada\""),
        "{logged}"
    );
    assert!(
        logged.contains("database \"other\" does not exist"),
        "{logged}"
    );
}

#[test]
fn a_cancel_request_is_answered_by_closing() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    let mut body = 80_877_102u32.to_be_bytes().to_vec();
    body.extend_from_slice(&1i32.to_be_bytes());
    body.extend_from_slice(&2i32.to_be_bytes());
    let mut packet = ((body.len() + 4) as u32).to_be_bytes().to_vec();
    packet.extend(body);
    client.send_raw(&packet);
    assert_eq!(client.read_byte(), None);
}

#[test]
fn simple_queries_run_statements_and_a_join() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    let answer = client.query(
        "CREATE TABLE items (id integer PRIMARY KEY, name text NOT NULL, price double precision);
         CREATE TABLE sales (id integer PRIMARY KEY, item_id integer, quantity smallint);
         INSERT INTO items VALUES (1, 'egg', 0.25), (2, 'bread', 2.5);
         INSERT INTO sales VALUES (1, 1, 3), (2, 2, 1), (3, 1, 4);",
    );
    assert_eq!(tags(&answer), "CCCCZ");
    let completed: Vec<String> = answer[..4].iter().map(Message::text).collect();
    assert_eq!(
        completed,
        ["CREATE TABLE", "CREATE TABLE", "INSERT 0 2", "INSERT 0 3"]
    );
    let answer = client.query(
        "SELECT i.name, sum(s.quantity) AS sold, i.price FROM items i JOIN sales s ON s.item_id = i.id GROUP BY i.name, i.price ORDER BY i.name",
    );
    assert_eq!(tags(&answer), "TDDCZ");
    assert_eq!(
        answer[0].column_types(),
        [
            ("name".to_owned(), 25, 0),
            ("sold".to_owned(), 20, 0),
            ("price".to_owned(), 701, 0)
        ]
    );
    assert_eq!(
        rows(&answer),
        [
            vec![Some("bread".into()), Some("1".into()), Some("2.5".into())],
            vec![Some("egg".into()), Some("7".into()), Some("0.25".into())],
        ]
    );
    assert_eq!(answer[3].text(), "SELECT 2");
    let answer = client.query("UPDATE items SET price = NULL WHERE id = 2; DELETE FROM sales WHERE id = 3; SELECT price FROM items WHERE id = 2");
    assert_eq!(answer[0].text(), "UPDATE 1");
    assert_eq!(answer[1].text(), "DELETE 1");
    assert_eq!(rows(&answer), [vec![None]]);
    assert_eq!(tags(&client.query("  ;  ")), "IZ");
    assert_eq!(tags(&client.query("")), "IZ");
}

#[test]
fn a_pg_dump_script_with_sequences_and_constraints_runs_as_one_simple_query() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    let script = "SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;
SET default_tablespace = '';
SET default_table_access_method = heap;
CREATE TABLE public.parents (
    id integer NOT NULL,
    name text
);
ALTER TABLE public.parents OWNER TO postgres;
CREATE SEQUENCE public.parents_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.parents_id_seq OWNER TO postgres;
ALTER SEQUENCE public.parents_id_seq OWNED BY public.parents.id;
CREATE TABLE public.kids (
    id integer NOT NULL,
    parent_id integer NOT NULL,
    age integer
);
ALTER TABLE ONLY public.parents ALTER COLUMN id SET DEFAULT nextval('public.parents_id_seq'::regclass);
INSERT INTO public.parents VALUES (1, 'ann');
INSERT INTO public.parents VALUES (2, 'bo');
INSERT INTO public.kids VALUES (1, 1, 4);
SELECT pg_catalog.setval('public.parents_id_seq', 2, true);
ALTER TABLE ONLY public.parents
    ADD CONSTRAINT parents_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.kids
    ADD CONSTRAINT kids_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.kids
    ADD CONSTRAINT kids_age_check CHECK ((age >= 0));
CREATE INDEX kids_parent_idx ON public.kids USING btree (parent_id);
ALTER TABLE ONLY public.kids
    ADD CONSTRAINT kids_parent_id_fkey FOREIGN KEY (parent_id) REFERENCES public.parents(id) ON DELETE CASCADE;
";
    let answer = client.query(script);
    assert!(!tags(&answer).contains('E'), "{answer:?}");
    assert_eq!(ready_status(&answer), b'I');
    assert_eq!(
        rows(&client.query("INSERT INTO parents (name) VALUES ('cy') RETURNING id")),
        [vec![Some("3".into())]]
    );
    let refused = client.query("INSERT INTO kids VALUES (2, 9, 1)");
    assert_eq!(refused[0].code(), "23503");
    assert_eq!(
        refused[0].field(b'D').as_deref(),
        Some("Key (parent_id)=(9) is not present in table \"parents\".")
    );
    assert_eq!(
        client.query("INSERT INTO kids VALUES (2, 1, -1)")[0].code(),
        "23514"
    );
    client.query("DELETE FROM parents WHERE id = 1");
    assert_eq!(
        rows(&client.query("SELECT count(*) FROM kids")),
        [vec![Some("0".into())]]
    );
}

#[test]
fn a_pg_dump_script_with_schemas_views_and_comments_runs_as_one_simple_query() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    let script = "SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;
CREATE SCHEMA shop;
ALTER SCHEMA shop OWNER TO postgres;
COMMENT ON SCHEMA shop IS 'the shop';
CREATE EXTENSION IF NOT EXISTS plpgsql WITH SCHEMA pg_catalog;
COMMENT ON EXTENSION plpgsql IS 'PL/pgSQL procedural language';
SET default_tablespace = '';
SET default_table_access_method = heap;
CREATE TABLE shop.customers (
    id integer NOT NULL,
    name text
);
ALTER TABLE shop.customers OWNER TO postgres;
COMMENT ON TABLE shop.customers IS 'who buys';
COMMENT ON COLUMN shop.customers.name IS 'their name';
CREATE SEQUENCE shop.customers_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE shop.customers_id_seq OWNER TO postgres;
ALTER SEQUENCE shop.customers_id_seq OWNED BY shop.customers.id;
CREATE TABLE public.orders (
    id integer NOT NULL,
    customer_id integer,
    total numeric(10,2)
);
ALTER TABLE public.orders OWNER TO postgres;
CREATE VIEW shop.order_totals AS
 SELECT c.name,
    sum(o.total) AS total
   FROM (shop.customers c
     JOIN public.orders o ON ((o.customer_id = c.id)))
  GROUP BY c.name;
ALTER VIEW shop.order_totals OWNER TO postgres;
COMMENT ON VIEW shop.order_totals IS 'spend per customer';
ALTER TABLE ONLY shop.customers ALTER COLUMN id SET DEFAULT nextval('shop.customers_id_seq'::regclass);
INSERT INTO shop.customers VALUES (1, 'ann');
INSERT INTO public.orders VALUES (1, 1, 2.50);
INSERT INTO public.orders VALUES (2, 1, 1.25);
SELECT pg_catalog.setval('shop.customers_id_seq', 1, true);
ALTER TABLE ONLY shop.customers
    ADD CONSTRAINT customers_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_customer_id_fkey FOREIGN KEY (customer_id) REFERENCES shop.customers(id);
REVOKE USAGE ON SCHEMA public FROM PUBLIC;
GRANT ALL ON SCHEMA public TO PUBLIC;
GRANT SELECT ON TABLE shop.order_totals TO reader;
ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA shop GRANT SELECT ON TABLES TO reader;
";
    let answer = client.query(script);
    assert!(!tags(&answer).contains('E'), "{answer:?}");
    assert_eq!(ready_status(&answer), b'I');
    assert_eq!(
        rows(&client.query("SELECT * FROM shop.order_totals")),
        [vec![Some("ann".into()), Some("3.75".into())]]
    );
    assert_eq!(
        rows(&client.query(
            "SET search_path = shop, public; INSERT INTO customers (name) VALUES ('bo') RETURNING id"
        )),
        [vec![Some("2".into())]]
    );
    assert_eq!(
        client.query("INSERT INTO orders VALUES (3, 9, 1)")[0].code(),
        "23503"
    );
    assert_eq!(client.query("DROP SCHEMA shop")[0].code(), "2BP01");
    assert_eq!(
        client.query("CREATE MATERIALIZED VIEW m AS SELECT 1")[0].code(),
        "0A000"
    );
}

#[test]
fn errors_carry_their_sqlstate_and_position_and_blocks_fail_until_rollback() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    let answer = client.query("SELECT * FROM missing");
    assert_eq!(tags(&answer), "EZ");
    assert_eq!(answer[0].code(), "42P01");
    assert_eq!(answer[0].field(b'S').as_deref(), Some("ERROR"));
    assert_eq!(answer[0].field(b'V').as_deref(), Some("ERROR"));
    assert_eq!(answer[0].field(b'P').as_deref(), Some("15"));
    assert_eq!(ready_status(&answer), b'I');
    client.query("CREATE TABLE t (id integer PRIMARY KEY)");
    assert_eq!(ready_status(&client.query("BEGIN")), b'T');
    assert_eq!(
        ready_status(&client.query("INSERT INTO t VALUES (1)")),
        b'T'
    );
    let failed = client.query("INSERT INTO t VALUES (1)");
    assert_eq!(
        (failed[0].code(), ready_status(&failed)),
        ("23505".to_owned(), b'E')
    );
    let aborted = client.query("SELECT 1");
    assert_eq!(
        (aborted[0].code(), ready_status(&aborted)),
        ("25P02".to_owned(), b'E')
    );
    let rolled_back = client.query("COMMIT");
    assert_eq!(
        (rolled_back[0].text(), ready_status(&rolled_back)),
        ("ROLLBACK".to_owned(), b'I')
    );
    assert_eq!(
        rows(&client.query("SELECT count(*) FROM t")),
        [vec![Some("0".into())]]
    );
    let warned = client.query("COMMIT");
    assert_eq!(tags(&warned), "NCZ");
    assert_eq!(warned[0].code(), "25P01");
    assert_eq!(warned[0].field(b'S').as_deref(), Some("WARNING"));
}

#[test]
fn set_and_show_report_parameter_changes() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    let answer = client.query("SET application_name = 'reports'; SHOW application_name");
    assert_eq!(answer[0].text(), "SET");
    assert!(
        answer
            .iter()
            .any(|message| message.tag == b'S' && message.text() == "application_name\0reports")
    );
    assert_eq!(rows(&answer), [vec![Some("reports".into())]]);
    let refused = client.query("SET client_encoding = 'LATIN1'");
    assert_eq!(refused[0].code(), "0A000");
    let answer = client.query("SET extra_float_digits = 3");
    assert_eq!(tags(&answer), "CZ");
}

#[test]
fn the_extended_protocol_binds_text_and_binary_parameters() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    client.query("CREATE TABLE people (id integer PRIMARY KEY, name text, age smallint)");
    client.send(
        b'P',
        &parse("", "INSERT INTO people VALUES ($1, $2, $3)", &[23, 0, 0]),
    );
    client.send(
        b'B',
        &bind(
            "",
            "",
            &[1, 0, 0],
            &[Some(&7i32.to_be_bytes()), Some(b"Ada"), None],
            &[],
        ),
    );
    client.send(b'D', b"P\0");
    client.send(b'E', &execute("", 0));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12nCZ");
    assert_eq!(answer[3].text(), "INSERT 0 1");
    client.send(
        b'P',
        &parse(
            "by_age",
            "SELECT id, name, age FROM people WHERE age IS NULL OR age > $1",
            &[],
        ),
    );
    client.send(b'D', b"Sby_age\0");
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "1tTZ");
    assert_eq!(answer[1].body, [0, 1, 0, 0, 0, 25]);
    assert_eq!(
        answer[2].column_types(),
        [
            ("id".to_owned(), 23, 0),
            ("name".to_owned(), 25, 0),
            ("age".to_owned(), 21, 0)
        ]
    );
    client.send(b'B', &bind("p1", "by_age", &[], &[Some(b"30")], &[1]));
    client.send(b'D', b"Pp1\0");
    client.send(b'E', &execute("p1", 0));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "2TDCZ");
    assert_eq!(answer[1].column_types()[0], ("id".to_owned(), 23, 1));
    assert_eq!(
        answer[2].raw_row(),
        [
            Some(7i32.to_be_bytes().to_vec()),
            Some(b"Ada".to_vec()),
            None
        ]
    );
    assert_eq!(answer[3].text(), "SELECT 1");
    client.send(b'C', b"Pp1\0");
    client.send(b'C', b"Sby_age\0");
    client.send(b'S', b"");
    assert_eq!(tags(&client.read_until_ready()), "33Z");
}

#[test]
fn numeric_date_time_and_json_values_round_trip_in_text_and_binary() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    client.query(
        "CREATE TABLE typed (id serial PRIMARY KEY, price numeric(8,2), day date, at timestamptz, doc jsonb)",
    );
    client.send(
        b'P',
        &parse(
            "",
            "INSERT INTO typed (price, day, at, doc) VALUES ($1, $2, $3, $4)",
            &[1700, 1082, 1184, 3802],
        ),
    );
    let price: Vec<u8> = [2i16, 0, 0, 2, 19, 9900]
        .iter()
        .flat_map(|word| word.to_be_bytes())
        .collect();
    let day = 8_825i32.to_be_bytes();
    let at = 762_529_501_123_456i64.to_be_bytes();
    let doc = b"\x01{\"b\": 1, \"a\": 2}";
    client.send(
        b'B',
        &bind(
            "",
            "",
            &[1],
            &[Some(&price), Some(&day), Some(&at), Some(doc)],
            &[],
        ),
    );
    client.send(b'E', &execute("", 0));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12CZ", "{answer:?}");
    client.query("SET TimeZone = 'UTC-01'");
    client.send(
        b'P',
        &parse("read", "SELECT price, day, at, doc, id FROM typed", &[]),
    );
    client.send(b'B', &bind("text", "read", &[], &[], &[0]));
    client.send(b'D', b"Ptext\0");
    client.send(b'E', &execute("text", 0));
    client.send(b'B', &bind("binary", "read", &[], &[], &[1]));
    client.send(b'E', &execute("binary", 0));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12TDC2DCZ", "{answer:?}");
    assert_eq!(
        answer[2].column_types(),
        [
            ("price".to_owned(), 1700, 0),
            ("day".to_owned(), 1082, 0),
            ("at".to_owned(), 1184, 0),
            ("doc".to_owned(), 3802, 0),
            ("id".to_owned(), 23, 0),
        ]
    );
    assert_eq!(
        answer[3].row(),
        [
            Some("19.99".to_owned()),
            Some("2024-02-29".to_owned()),
            Some("2024-02-29 14:45:01.123456+01".to_owned()),
            Some("{\"a\": 2, \"b\": 1}".to_owned()),
            Some("1".to_owned()),
        ]
    );
    let mut doc = vec![1u8];
    doc.extend_from_slice(b"{\"a\": 2, \"b\": 1}");
    assert_eq!(
        answer[6].raw_row(),
        [
            Some(price),
            Some(day.to_vec()),
            Some(at.to_vec()),
            Some(doc),
            Some(1i32.to_be_bytes().to_vec()),
        ]
    );
}

#[test]
fn time_interval_bytea_uuid_and_character_values_round_trip_in_text_and_binary() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    client.query(
        "CREATE TABLE more (id serial PRIMARY KEY, at time, zoned timetz, span interval, raw bytea, tag uuid, code char(4))",
    );
    client.send(
        b'P',
        &parse(
            "",
            "INSERT INTO more (at, zoned, span, raw, tag, code) VALUES ($1, $2, $3, $4, $5, $6)",
            &[1083, 1266, 1186, 17, 2950, 1042],
        ),
    );
    let at = 45_296_789_000i64.to_be_bytes();
    let mut zoned = 3_600_000_000i64.to_be_bytes().to_vec();
    zoned.extend_from_slice(&(-7200i32).to_be_bytes());
    let mut span = (-1i64).to_be_bytes().to_vec();
    span.extend_from_slice(&3i32.to_be_bytes());
    span.extend_from_slice(&14i32.to_be_bytes());
    let raw = [0u8, 0xFF, b'\\', 0x01];
    let tag: Vec<u8> = (0u8..16).collect();
    client.send(
        b'B',
        &bind(
            "",
            "",
            &[1],
            &[
                Some(&at),
                Some(&zoned),
                Some(&span),
                Some(&raw),
                Some(&tag),
                Some(b"ab"),
            ],
            &[],
        ),
    );
    client.send(b'E', &execute("", 0));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12CZ", "{answer:?}");
    client.send(
        b'P',
        &parse(
            "read",
            "SELECT at, zoned, span, raw, tag, code FROM more",
            &[],
        ),
    );
    client.send(b'B', &bind("text", "read", &[], &[], &[0]));
    client.send(b'D', b"Ptext\0");
    client.send(b'E', &execute("text", 0));
    client.send(b'B', &bind("binary", "read", &[], &[], &[1]));
    client.send(b'E', &execute("binary", 0));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12TDC2DCZ", "{answer:?}");
    assert_eq!(
        answer[2].column_types(),
        [
            ("at".to_owned(), 1083, 0),
            ("zoned".to_owned(), 1266, 0),
            ("span".to_owned(), 1186, 0),
            ("raw".to_owned(), 17, 0),
            ("tag".to_owned(), 2950, 0),
            ("code".to_owned(), 1042, 0),
        ]
    );
    assert_eq!(
        answer[3].row(),
        [
            Some("12:34:56.789".to_owned()),
            Some("01:00:00+02".to_owned()),
            Some("1 year 2 mons 3 days -00:00:00.000001".to_owned()),
            Some("\\x00ff5c01".to_owned()),
            Some("00010203-0405-0607-0809-0a0b0c0d0e0f".to_owned()),
            Some("ab  ".to_owned()),
        ]
    );
    assert_eq!(
        answer[6].raw_row(),
        [
            Some(at.to_vec()),
            Some(zoned),
            Some(span),
            Some(raw.to_vec()),
            Some(tag),
            Some(b"ab  ".to_vec()),
        ]
    );
    for (oid, bad) in [
        (1083u32, (-1i64).to_be_bytes().to_vec()),
        (1083, 86_400_000_001i64.to_be_bytes().to_vec()),
        (1266, vec![0; 11]),
        (
            1266,
            [vec![0; 8], 57_600i32.to_be_bytes().to_vec()].concat(),
        ),
        (1186, vec![0; 15]),
        (2950, vec![0; 17]),
    ] {
        client.send(b'P', &parse("", "SELECT $1", &[oid]));
        client.send(b'B', &bind("", "", &[1], &[Some(&bad)], &[]));
        client.send(b'E', &execute("", 0));
        client.send(b'S', b"");
        let answer = client.read_until_ready();
        assert_eq!(tags(&answer), "1EZ", "{oid}: {answer:?}");
    }
}

#[test]
fn portals_return_rows_in_batches_and_errors_skip_to_sync() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    client.query(
        "CREATE TABLE n (x integer PRIMARY KEY); INSERT INTO n VALUES (1), (2), (3), (4), (5)",
    );
    client.send(b'P', &parse("", "SELECT x FROM n ORDER BY x", &[]));
    client.send(b'B', &bind("", "", &[], &[], &[]));
    client.send(b'E', &execute("", 2));
    client.send(b'E', &execute("", 2));
    client.send(b'E', &execute("", 2));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12DDsDDsDCZ");
    assert_eq!(answer[9].text(), "SELECT 1");
    client.send(b'P', &parse("", "SELECT * FROM missing", &[]));
    client.send(b'B', &bind("", "", &[], &[], &[]));
    client.send(b'E', &execute("", 0));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12EZ");
    assert_eq!(answer[2].code(), "42P01");
    client.send(b'B', &bind("", "nope", &[], &[], &[]));
    client.send(b'S', b"");
    assert_eq!(client.read_until_ready()[0].code(), "26000");
    client.send(b'P', &parse("", "SELECT $1", &[]));
    client.send(b'B', &bind("", "", &[], &[], &[]));
    client.send(b'S', b"");
    assert_eq!(client.read_until_ready()[1].code(), "08P01");
    client.send(b'P', &parse("", "SELECT 1; SELECT 2", &[]));
    client.send(b'S', b"");
    assert_eq!(client.read_until_ready()[0].code(), "42601");
    client.send(b'P', &parse("", "", &[]));
    client.send(b'B', &bind("", "", &[], &[], &[]));
    client.send(b'E', &execute("", 0));
    client.send(b'S', b"");
    assert_eq!(tags(&client.read_until_ready()), "12IZ");
    assert_eq!(
        rows(&client.query("SELECT count(*) FROM n")),
        [vec![Some("5".into())]]
    );
}

#[test]
fn two_connections_share_the_database_and_a_second_writer_waits_then_fails() {
    let server = TestServer::start(8);
    let (mut first, _) = Client::login(server.address);
    let (mut second, _) = Client::login(server.address);
    first.query("CREATE TABLE t (id integer PRIMARY KEY)");
    assert_eq!(tags(&second.query("INSERT INTO t VALUES (1)")), "CZ");
    assert_eq!(
        rows(&first.query("SELECT id FROM t")),
        [vec![Some("1".into())]]
    );
    first.query("BEGIN; INSERT INTO t VALUES (2)");
    let busy = second.query("INSERT INTO t VALUES (3)");
    assert_eq!(busy[0].code(), "55P03");
    assert_eq!(
        rows(&second.query("SELECT count(*) FROM t")),
        [vec![Some("1".into())]]
    );
    first.query("COMMIT");
    assert_eq!(tags(&second.query("INSERT INTO t VALUES (3)")), "CZ");
    assert_eq!(
        rows(&first.query("SELECT count(*) FROM t")),
        [vec![Some("3".into())]]
    );
}

#[test]
fn connections_over_the_limit_get_too_many_connections() {
    let server = TestServer::start(1);
    let (mut first, _) = Client::login(server.address);
    let mut second = Client::connect(server.address);
    second.send_request(80_877_103);
    assert_eq!(second.read_byte(), Some(b'N'));
    second.send_startup(196_608, &[("user", USER)]);
    let refused = second.read().unwrap();
    assert_eq!(refused.code(), "53300");
    assert_eq!(tags(&first.query("SELECT 1")), "TDCZ");
}

#[test]
fn a_stop_ends_idle_connections_with_admin_shutdown() {
    let mut server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    client.query("CREATE TABLE t (id integer PRIMARY KEY); BEGIN; INSERT INTO t VALUES (1)");
    let open = server.stop();
    assert_eq!(open, 1);
    let goodbye = client.read().unwrap();
    assert_eq!((goodbye.tag, goodbye.code()), (b'E', "57P01".to_owned()));
    assert_eq!(goodbye.field(b'S').as_deref(), Some("FATAL"));
}

#[test]
fn a_slow_client_is_cut_off_at_the_authentication_timeout() {
    let server = TestServer::start_with(8, Duration::from_millis(600));
    let mut client = Client::connect(server.address);
    client.send_raw(&[0, 0]);
    let started = std::time::Instant::now();
    assert_eq!(client.read_byte(), None);
    assert!(started.elapsed() < Duration::from_secs(10));
    let (mut working, _) = Client::login(server.address);
    assert_eq!(tags(&working.query("SELECT 1")), "TDCZ");
}

#[test]
fn unsupported_startup_settings_and_function_calls_are_answered_without_hanging() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    client.send_startup(
        196_608,
        &[
            ("user", USER),
            ("database", DATABASE),
            ("client_encoding", "LATIN1"),
            ("_pq_.unknown_option", "1"),
        ],
    );
    assert_eq!(client.authenticate(common::PASSWORD).tag, b'R');
    let greeting = client.read_until_ready();
    assert_eq!(greeting[0].tag, b'v');
    assert!(
        greeting
            .iter()
            .any(|message| message.text() == "client_encoding\0UTF8")
    );
    client.send(b'F', &[0, 0, 0, 1]);
    let answer = client.read_until_ready();
    assert_eq!(
        (tags(&answer), answer[0].code()),
        ("EZ".to_owned(), "0A000".to_owned())
    );
    client.send(b'd', b"stray copy data");
    assert_eq!(tags(&client.query("SELECT 1")), "TDCZ");
}

fn trickle(mut stream: std::net::TcpStream, first: Vec<u8>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::Write;
        if stream.write_all(&first).is_err() {
            return;
        }
        for _ in 0..600 {
            std::thread::sleep(Duration::from_millis(100));
            if stream.write_all(b"x").is_err() {
                return;
            }
        }
    })
}

#[test]
fn a_trickling_client_is_cut_off_at_the_authentication_deadline() {
    let server = TestServer::start_with(8, Duration::from_millis(600));
    let mut client = Client::connect(server.address);
    let writer = trickle(
        client.stream.try_clone().unwrap(),
        9_999u32.to_be_bytes().to_vec(),
    );
    let started = std::time::Instant::now();
    assert_eq!(client.read_byte(), None);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    drop(client);
    writer.join().unwrap();
}

#[test]
fn a_stop_is_seen_while_clients_trickle_before_and_after_authentication() {
    let mut server = TestServer::start_with(8, Duration::from_secs(30));
    let before = Client::connect(server.address);
    let before_writer = trickle(
        before.stream.try_clone().unwrap(),
        9_999u32.to_be_bytes().to_vec(),
    );
    let (after, _) = Client::login(server.address);
    let mut header = vec![b'Q'];
    header.extend_from_slice(&10_000u32.to_be_bytes());
    let after_writer = trickle(after.stream.try_clone().unwrap(), header);
    std::thread::sleep(Duration::from_millis(500));
    let started = std::time::Instant::now();
    server.stop();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    drop(before);
    drop(after);
    before_writer.join().unwrap();
    after_writer.join().unwrap();
}

#[test]
fn client_supplied_names_are_logged_on_one_line() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    client.send_startup(196_608, &[("user", "eve\n2026-10-27 forged line")]);
    assert_eq!(client.authenticate("wrong").code(), "28P01");
    let mut client = Client::connect(server.address);
    client.send_startup(196_608, &[("user", USER), ("database", "x\r\ny\u{1b}[2J")]);
    assert_eq!(client.authenticate(common::PASSWORD).tag, b'R');
    assert_eq!(client.read().unwrap().code(), "3D000");
    let logged = server.logged.lock().unwrap().clone();
    assert_eq!(logged.len(), 2, "{logged:?}");
    assert!(
        logged
            .iter()
            .all(|line| !line.chars().any(char::is_control)),
        "{logged:?}"
    );
    assert!(
        logged[0].contains("user \"eve\\n2026-10-27 forged line\""),
        "{logged:?}"
    );
    assert!(
        logged[1].contains("database \"x\\r\\ny\\u{1b}[2J\""),
        "{logged:?}"
    );
}

#[test]
fn execute_sends_values_as_the_statement_description_said() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    client.query("CREATE TABLE items (id integer PRIMARY KEY); INSERT INTO items VALUES (1)");
    client.send(
        b'P',
        &parse(
            "s",
            "SELECT CASE WHEN id > 0 THEN id ELSE 1.5 END FROM items",
            &[],
        ),
    );
    client.send(b'D', b"Ss\0");
    client.send(b'B', &bind("", "s", &[], &[], &[1]));
    client.send(b'E', &execute("", 0));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "1tT2DCZ");
    let (_, oid, _) = answer[2].column_types()[0].clone();
    assert_eq!(oid, 25);
    assert_eq!(answer[4].raw_row(), [Some(b"1".to_vec())]);
}

#[test]
fn set_operations_and_subqueries_describe_their_column_types() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    client.query(
        "CREATE TABLE items (id integer PRIMARY KEY, price numeric(6,2)); INSERT INTO items VALUES (1, 2.50), (2, NULL)",
    );
    let answer = client.query(
        "SELECT id FROM items UNION SELECT 2.5 ORDER BY 1; SELECT (SELECT max(price) FROM items) AS top, EXISTS (SELECT 1 FROM items WHERE price IS NULL) AS gap, x.id FROM (SELECT id FROM items WHERE id = 2) x",
    );
    let descriptions: Vec<Vec<(String, u32, i16)>> = answer
        .iter()
        .filter(|message| message.tag == b'T')
        .map(|message| message.column_types())
        .collect();
    let oids: Vec<Vec<u32>> = descriptions
        .iter()
        .map(|columns| columns.iter().map(|(_, oid, _)| *oid).collect())
        .collect();
    assert_eq!(oids, [vec![1700], vec![1700, 16, 23]]);
    let rows: Vec<Vec<Option<Vec<u8>>>> = answer
        .iter()
        .filter(|message| message.tag == b'D')
        .map(|message| message.raw_row())
        .collect();
    assert_eq!(
        rows,
        [
            vec![Some(b"1".to_vec())],
            vec![Some(b"2".to_vec())],
            vec![Some(b"2.5".to_vec())],
            vec![
                Some(b"2.50".to_vec()),
                Some(b"t".to_vec()),
                Some(b"2".to_vec())
            ],
        ]
    );
}

#[test]
fn a_finished_portal_never_runs_its_statement_again() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    client.query("CREATE TABLE t (id integer PRIMARY KEY)");
    client.send(b'P', &parse("", "INSERT INTO t VALUES (1)", &[]));
    client.send(b'B', &bind("ins", "", &[], &[], &[]));
    client.send(b'E', &execute("ins", 0));
    client.send(b'E', &execute("ins", 0));
    client.send(b'P', &parse("sel", "SELECT id FROM t", &[]));
    client.send(b'B', &bind("sel", "sel", &[], &[], &[]));
    client.send(b'E', &execute("sel", 0));
    client.send(b'E', &execute("sel", 0));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12CC12DCCZ");
    let completed: Vec<String> = answer
        .iter()
        .filter(|message| message.tag == b'C')
        .map(Message::text)
        .collect();
    assert_eq!(
        completed,
        ["INSERT 0 1", "INSERT 0 0", "SELECT 1", "SELECT 0"]
    );
    assert_eq!(
        rows(&client.query("SELECT count(*) FROM t")),
        [vec![Some("1".into())]]
    );
}

#[test]
fn logins_in_progress_are_capped_below_the_connection_limit() {
    let server = TestServer::start(64);
    let idle: Vec<Client> = (0..eggdb_pgwire::MAX_UNAUTHENTICATED_CONNECTIONS)
        .map(|_| Client::connect(server.address))
        .collect();
    std::thread::sleep(Duration::from_millis(500));
    let mut extra = Client::connect(server.address);
    extra.send_startup(196_608, &[("user", USER)]);
    assert_eq!(extra.read().unwrap().code(), "53300");
    drop(idle);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let mut client = Client::connect(server.address);
        client.send_startup(196_608, &[("user", USER), ("database", DATABASE)]);
        let first = client.read().unwrap();
        if first.tag == b'R' {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{first:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_connection_reads_oracle_sql_after_choosing_the_dialect_in_its_startup_options() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    client.send_startup(
        196_608,
        &[
            ("user", USER),
            ("database", DATABASE),
            ("options", "-c eggdb.dialect=oracle"),
        ],
    );
    assert_eq!(client.authenticate(common::PASSWORD).tag, b'R');
    client.read_until_ready();
    let answer = client.query(
        "CREATE TABLE emp (id NUMBER(9) PRIMARY KEY, name VARCHAR2(20));
         INSERT INTO emp VALUES (1, 'Ada');
         SELECT table_name FROM user_tables;
         SELECT name FROM emp WHERE ROWNUM <= 1",
    );
    assert_eq!(
        rows(&answer),
        [vec![Some("EMP".into())], vec![Some("Ada".into())]]
    );
    assert_eq!(ready_status(&answer), b'I');
    client.send(
        b'P',
        &parse("", "SELECT name FROM emp WHERE id = $1", &[23]),
    );
    client.send(b'B', &bind("", "", &[0], &[Some(b"1")], &[]));
    client.send(b'D', b"P\0");
    client.send(b'E', &execute("", 0));
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert_eq!(tags(&answer), "12TDCZ");
    assert_eq!(answer[2].column_types()[0].0, "NAME");
    assert_eq!(rows(&answer), [vec![Some("Ada".into())]]);
    let answer = client.query("SET eggdb.dialect = postgresql; SELECT name FROM emp");
    assert_eq!(rows(&answer), [vec![Some("Ada".into())]]);
    let refused = client.query("SET eggdb.dialect = mysql");
    assert_eq!(refused[0].code(), "0A000");
}

#[test]
fn an_unknown_dialect_in_the_startup_options_ends_the_connection() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    client.send_startup(
        196_608,
        &[
            ("user", USER),
            ("database", DATABASE),
            ("options", "-c eggdb.dialect=cobol"),
        ],
    );
    assert_eq!(client.authenticate(common::PASSWORD).tag, b'R');
    let refused = client.read().unwrap();
    assert_eq!(
        (refused.code(), refused.field(b'S').as_deref()),
        ("22023".to_owned(), Some("FATAL"))
    );
    assert_eq!(client.read(), None);
}
