use crate::copy::{
    CopyChannel, CopyFormat, CopyStart, FieldKind, Fields, Input, Layout, MAX_RECORD_BYTES,
    RecordReader, RowWriter,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::memory_store::MemoryStore;
use crate::session::{QueryResult, Session};
use crate::tests::{fuzz, render};

struct Rng(u64);

impl Rng {
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

#[derive(Default)]
struct Client {
    data: Vec<u8>,
    at: usize,
    chunk: usize,
    fail_at: Option<usize>,
    output: Vec<u8>,
    started_in: Option<CopyStart>,
    started_out: Option<CopyStart>,
    finished_out: bool,
    abandoned: bool,
}

impl Client {
    fn sending(data: &[u8], chunk: usize) -> Self {
        Self {
            data: data.to_vec(),
            chunk: chunk.max(1),
            ..Self::default()
        }
    }
}

impl CopyChannel for Client {
    fn start_copy_in(&mut self, start: &CopyStart) -> SqlResult<()> {
        self.started_in = Some(*start);
        Ok(())
    }

    fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool> {
        if self.fail_at.is_some_and(|limit| self.at >= limit) {
            return Err(SqlError::new(
                SqlState::QUERY_CANCELED,
                "COPY from stdin failed: the client gave up",
            ));
        }
        if self.at >= self.data.len() {
            return Ok(false);
        }
        let end = (self.at + self.chunk).min(self.data.len());
        data.extend_from_slice(&self.data[self.at..end]);
        self.at = end;
        Ok(true)
    }

    fn abandon_copy_in(&mut self) {
        self.abandoned = true;
    }

    fn start_copy_out(&mut self, start: &CopyStart) -> SqlResult<()> {
        self.started_out = Some(*start);
        Ok(())
    }

    fn write_copy_data(&mut self, data: &[u8]) -> SqlResult<()> {
        self.output.extend_from_slice(data);
        Ok(())
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        self.finished_out = true;
        Ok(())
    }
}

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> SqlResult<String> {
    let mut last = String::new();
    session.execute(sql, &mut |result| last = render(&result))?;
    Ok(last)
}

fn rows(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    run(session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"))
}

fn copy_in_chunks(
    session: &mut Session<'_, MemoryStore>,
    sql: &str,
    data: &str,
    chunk: usize,
) -> SqlResult<String> {
    let mut client = Client::sending(data.as_bytes(), chunk);
    let mut tag = String::new();
    session.execute_with_copy(
        sql,
        &mut |result: QueryResult| {
            tag = result.tag;
            true
        },
        &mut client,
    )?;
    Ok(tag)
}

fn copy_in(session: &mut Session<'_, MemoryStore>, sql: &str, data: &str) -> SqlResult<String> {
    copy_in_chunks(session, sql, data, 7)
}

fn copy_out(session: &mut Session<'_, MemoryStore>, sql: &str) -> SqlResult<String> {
    let mut client = Client::default();
    let mut tag = String::new();
    session.execute_with_copy(
        sql,
        &mut |result: QueryResult| {
            tag = result.tag;
            true
        },
        &mut client,
    )?;
    assert!(client.finished_out, "{sql}");
    Ok(format!(
        "{}{tag}",
        String::from_utf8(client.output).unwrap_or_default()
    ))
}

fn script(session: &mut Session<'_, MemoryStore>, text: &str) -> SqlResult<Vec<String>> {
    let mut client = Client::default();
    let mut shown = Vec::new();
    let outcome = session.execute_script(
        text,
        &mut |result: QueryResult| {
            shown.push(render(&result));
            true
        },
        &mut client,
    );
    if !client.output.is_empty() {
        shown.push(String::from_utf8_lossy(&client.output).into_owned());
    }
    outcome.map(|()| shown)
}

fn failure(outcome: SqlResult<String>) -> SqlError {
    match outcome {
        Ok(found) => panic!("succeeded: {found}"),
        Err(error) => error,
    }
}

type Row = Vec<Option<String>>;

fn read_all(data: &[u8], layout: &Layout, chunk: usize) -> SqlResult<Vec<Row>> {
    read_with_limit(data, layout, chunk, MAX_RECORD_BYTES)
}

fn read_with_limit(
    data: &[u8],
    layout: &Layout,
    chunk: usize,
    limit: usize,
) -> SqlResult<Vec<Row>> {
    let mut client = Client::sending(data, chunk);
    let mut reader = RecordReader::new(Input::Channel(&mut client), layout, limit);
    let mut fields = Fields::default();
    let mut rows = Vec::new();
    while let Some(range) = reader.next_record()? {
        fields.split(reader.bytes(range), layout, &[], &[])?;
        rows.push(
            fields
                .fields
                .iter()
                .map(|field| match field.kind {
                    FieldKind::Null => None,
                    FieldKind::Default => Some("<default>".to_owned()),
                    FieldKind::Value => {
                        Some(String::from_utf8_lossy(fields.value(field)).into_owned())
                    }
                })
                .collect(),
        );
    }
    Ok(rows)
}

fn text(values: &[&str]) -> Row {
    values
        .iter()
        .map(|value| (*value != "NULL").then(|| (*value).to_owned()))
        .collect()
}

fn csv() -> Layout {
    Layout {
        csv: true,
        delimiter: b',',
        null: Vec::new(),
        default: None,
        quote: b'"',
        escape: b'"',
    }
}

#[test]
fn the_text_format_reads_the_escapes_its_manual_lists() {
    let data = b"a\\tb\\\\c\\nd\t\\N\t\\101\\x41\\x4g\\q\t\\\\N\ne\\\nf\t\t\\.x\n";
    for chunk in [1, 2, 3, 5, 64] {
        assert_eq!(
            read_all(data, &Layout::text(), chunk).unwrap(),
            vec![
                text(&["a\tb\\c\nd", "NULL", "AA\u{4}gq", "\\N"]),
                text(&["e\nf", "", ".x"]),
            ],
            "chunks of {chunk}"
        );
    }
}

#[test]
fn a_line_of_backslash_period_ends_the_data_and_the_rest_is_never_read() {
    let layout = Layout::text();
    assert_eq!(
        read_all(b"1\n\\.\n2\n", &layout, 3).unwrap(),
        vec![text(&["1"])]
    );
    assert_eq!(
        read_all(b"1\r\n\\.\r\n", &layout, 1).unwrap(),
        vec![text(&["1"])]
    );
    assert_eq!(
        read_all(b"1\n2", &layout, 1).unwrap(),
        vec![text(&["1"]), text(&["2"])]
    );
    assert_eq!(read_all(b"", &layout, 1).unwrap(), Vec::<Row>::new());
    let data = b"1\n\\.\nSELECT 1;";
    let mut reader = RecordReader::new(Input::Inline(data), &layout, MAX_RECORD_BYTES);
    assert!(reader.next_record().unwrap().is_some());
    assert_eq!(reader.next_record().unwrap(), None);
    assert_eq!(reader.consumed(), 5);
}

#[test]
fn line_endings_follow_the_first_line_and_a_mix_is_refused() {
    let layout = Layout::text();
    for data in [&b"1\r\n2\r\n"[..], b"1\r2\r", b"1\n2\n"] {
        assert_eq!(
            read_all(data, &layout, 1).unwrap(),
            vec![text(&["1"]), text(&["2"])]
        );
    }
    for data in [&b"1\n2\r\n"[..], b"1\r\n2\n", b"1\r2\n3\r"] {
        let error = read_all(data, &layout, 2).unwrap_err();
        assert_eq!(error.state(), SqlState::BAD_COPY_FILE_FORMAT, "{data:?}");
        assert!(error.hint().is_some());
    }
}

#[test]
fn csv_reads_quotes_escapes_and_line_breaks_inside_quotes() {
    let data = b"a,\"b,c\",\"d\"\"e\",,\"\"\n\"x\ny\",1\n\"\\.\"\n\\.\nnot read\n";
    for chunk in [1, 4, 100] {
        assert_eq!(
            read_all(data, &csv(), chunk).unwrap(),
            vec![
                text(&["a", "b,c", "d\"e", "NULL", ""]),
                text(&["x\ny", "1"]),
                text(&["\\."]),
            ]
        );
    }
    let mut escaped = csv();
    escaped.escape = b'\\';
    assert_eq!(
        read_all(b"\"a\\\"b\\\\c\\d\"\n", &escaped, 2).unwrap(),
        vec![text(&["a\"b\\c\\d"])]
    );
    let error = read_all(b"1,\"open\n", &csv(), 3).unwrap_err();
    assert_eq!(error.state(), SqlState::BAD_COPY_FILE_FORMAT);
    assert_eq!(error.message(), "unterminated CSV quoted field");
}

#[test]
fn a_line_longer_than_the_limit_fails_without_reading_on() {
    let error = read_with_limit(b"0123456789abcdef\n1\n", &Layout::text(), 3, 8).unwrap_err();
    assert_eq!(error.state(), SqlState::PROGRAM_LIMIT_EXCEEDED);
    assert!(read_with_limit(b"01234567\n", &Layout::text(), 3, 8).is_ok());
}

fn random_value(rng: &mut Rng) -> Option<String> {
    const PIECES: [&str; 16] = [
        "a", "\t", "\n", "\r", "\\", "\"", ",", ";", "N", "\\N", "\\.", ".", " ", "é", "日本",
        "NULL",
    ];
    if rng.below(6) == 0 {
        return None;
    }
    let length = rng.below(6);
    Some(
        (0..length)
            .map(|_| PIECES[rng.below(PIECES.len())])
            .collect(),
    )
}

#[test]
fn random_rows_round_trip_through_the_writer_and_the_reader_in_both_formats() {
    let mut rng = Rng(0x5eed_c0e7);
    let layouts = [
        Layout::text(),
        Layout {
            delimiter: b'|',
            ..Layout::text()
        },
        csv(),
        Layout {
            delimiter: b';',
            null: b"\\N".to_vec(),
            escape: b'\\',
            ..csv()
        },
    ];
    for round in 0..400 {
        let layout = &layouts[round % layouts.len()];
        let columns = 1 + rng.below(4);
        let force = vec![rng.below(2) == 0; columns];
        let mut writer = RowWriter::new(layout.clone(), force);
        let mut rows = Vec::new();
        let mut data = Vec::new();
        for _ in 0..rng.below(8) {
            let row: Row = (0..columns).map(|_| random_value(&mut rng)).collect();
            data.extend_from_slice(writer.line(&row));
            rows.push(row);
        }
        let chunk = 1 + rng.below(9);
        assert_eq!(
            read_all(&data, layout, chunk).unwrap(),
            rows,
            "round {round}: {}",
            String::from_utf8_lossy(&data)
        );
    }
}

#[test]
fn random_bytes_never_panic_the_reader() {
    let mut rng = Rng(0xfeed_beef);
    let alphabet = b"ab\t\n\r\\\".,N0x7\xc3\xa9\xff";
    for round in 0..20_000 {
        let length = rng.below(48);
        let data: Vec<u8> = (0..length)
            .map(|_| alphabet[rng.below(alphabet.len())])
            .collect();
        let mut layout = if round % 2 == 0 {
            Layout::text()
        } else {
            csv()
        };
        if round % 5 == 0 {
            layout.escape = b'\\';
        }
        let _ = read_with_limit(&data, &layout, 1 + rng.below(5), 1 + rng.below(40));
    }
}

fn typed_table(session: &mut Session<'_, MemoryStore>) {
    rows(
        session,
        "CREATE TABLE typed (id integer PRIMARY KEY, name text NOT NULL, price numeric(6,2), at timestamp, ok boolean, raw bytea, tag uuid, doc jsonb)",
    );
}

#[test]
fn copy_from_stdin_loads_every_type_through_its_text_input() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    typed_table(&mut session);
    let data = "2\tbob\t\\N\t2024-02-29 13:45:01.5\tf\t\\\\x00ff\ta0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11\t{\"b\": 1, \"a\": 2}\n1\tada\t1.5\t2024-01-02 03:04:05\tt\t\\\\x\t\\N\t[]\n";
    assert_eq!(
        copy_in(&mut session, "COPY typed FROM STDIN", data).unwrap(),
        "COPY 2"
    );
    assert_eq!(
        rows(&mut session, "SELECT * FROM typed ORDER BY id"),
        "id|name|price|at|ok|raw|tag|doc\n\
         1|ada|1.50|2024-01-02 03:04:05|t|\\x|NULL|[]\n\
         2|bob|NULL|2024-02-29 13:45:01.5|f|\\x00ff|a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11|{\"a\": 2, \"b\": 1}\n\
         SELECT 2"
    );
    assert_eq!(
        copy_out(&mut session, "COPY typed TO STDOUT").unwrap(),
        "1\tada\t1.50\t2024-01-02 03:04:05\tt\t\\\\x\t\\N\t[]\n\
         2\tbob\t\\N\t2024-02-29 13:45:01.5\tf\t\\\\x00ff\ta0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11\t{\"a\": 2, \"b\": 1}\n\
         COPY 2"
    );
}

#[test]
fn columns_left_out_take_their_defaults_and_serial_columns_count_on() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE TABLE d (id serial PRIMARY KEY, a text DEFAULT 'x', b integer NOT NULL)",
    );
    assert_eq!(
        copy_in(&mut session, "COPY d (b) FROM STDIN", "1\n2\n").unwrap(),
        "COPY 2"
    );
    assert_eq!(
        copy_in(
            &mut session,
            "COPY d (a, b) FROM STDIN WITH (DEFAULT '\\D')",
            "\\D\t5\nz\t6\n"
        )
        .unwrap(),
        "COPY 2"
    );
    rows(&mut session, "INSERT INTO d (b) VALUES (7)");
    assert_eq!(
        rows(&mut session, "SELECT id, a, b FROM d ORDER BY id"),
        "id|a|b\n1|x|1\n2|x|2\n3|x|5\n4|z|6\n5|x|7\nSELECT 5"
    );
    assert_eq!(
        copy_out(&mut session, "COPY d (b, a) TO STDOUT WHERE").map_err(|error| error.state()),
        Err(SqlState::SYNTAX_ERROR)
    );
}

#[test]
fn bad_data_fails_with_postgresql_states_and_a_context_line_and_loads_nothing() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE TABLE p (id integer PRIMARY KEY, name text NOT NULL, n integer CHECK (n > 0));
         CREATE TABLE c (id integer PRIMARY KEY, p integer REFERENCES p (id))",
    );
    let cases: [(&str, &str, &str, &str, &str); 9] = [
        (
            "p",
            "1\ta\t1\textra\n",
            "22P04",
            "extra data after last expected column",
            "COPY p, line 1: \"1\ta\t1\textra\"",
        ),
        (
            "p",
            "1\ta\t1\n2\tb\n",
            "22P04",
            "missing data for column \"n\"",
            "COPY p, line 2: \"2\tb\"",
        ),
        (
            "p",
            "1\ta\t1\nx\tb\t2\n",
            "22P02",
            "invalid input syntax for type integer: \"x\"",
            "COPY p, line 2, column id: \"x\"",
        ),
        (
            "p",
            "1\ta\t1\n1\tb\t2\n",
            "23505",
            "duplicate key value violates unique constraint \"p_pkey\"",
            "COPY p, line 2",
        ),
        (
            "p",
            "1\t\\N\t1\n",
            "23502",
            "null value in column \"name\" of relation \"p\" violates not-null constraint",
            "COPY p, line 1",
        ),
        ("p", "1\ta\t0\n", "23514", "", "COPY p, line 1"),
        (
            "p",
            "1\t\\xff\t1\n",
            "22021",
            "invalid byte sequence for encoding \"UTF8\": 0xff",
            "COPY p, line 1, column name: \"\"",
        ),
        (
            "p",
            "1\ta\t99999999999\n",
            "22003",
            "",
            "COPY p, line 1, column n: \"99999999999\"",
        ),
        ("c", "1\t1\n", "23503", "", ""),
    ];
    for (table, data, code, message, context) in cases {
        let error = failure(copy_in(
            &mut session,
            &format!("COPY {table} FROM STDIN"),
            data,
        ));
        assert_eq!(error.state().code(), code, "{data:?}: {error}");
        if !message.is_empty() {
            assert_eq!(error.message(), message, "{data:?}");
        }
        if !context.is_empty() {
            assert_eq!(error.context(), Some(context), "{data:?}");
        }
        assert_eq!(
            rows(&mut session, "SELECT count(*) FROM p"),
            "count\n0\nSELECT 1"
        );
    }
}

#[test]
fn foreign_keys_are_checked_in_batches_and_may_point_forward_in_their_own_table() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE TABLE parent (id integer PRIMARY KEY);
         CREATE TABLE child (id integer PRIMARY KEY, parent integer REFERENCES parent (id));
         CREATE TABLE tree (id integer PRIMARY KEY, up integer REFERENCES tree (id));
         INSERT INTO parent VALUES (1)",
    );
    let mut data: String = (0..9000).map(|id| format!("{id}\t1\n")).collect();
    assert_eq!(
        copy_in(&mut session, "COPY child FROM STDIN", &data).unwrap(),
        "COPY 9000"
    );
    rows(&mut session, "DELETE FROM child");
    data.push_str("9000\t2\n");
    assert_eq!(
        failure(copy_in(&mut session, "COPY child FROM STDIN", &data)).state(),
        SqlState::FOREIGN_KEY_VIOLATION
    );
    assert_eq!(
        rows(&mut session, "SELECT count(*) FROM child"),
        "count\n0\nSELECT 1"
    );
    assert_eq!(
        copy_in(&mut session, "COPY tree FROM STDIN", "1\t2\n2\t\\N\n3\t1\n").unwrap(),
        "COPY 3"
    );
}

#[test]
fn a_failed_copy_in_a_block_fails_the_block_and_a_client_failure_loads_nothing() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE TABLE t (id integer PRIMARY KEY, name text)",
    );
    rows(&mut session, "BEGIN");
    rows(
        &mut session,
        "INSERT INTO t VALUES (1, 'kept until the block fails')",
    );
    assert_eq!(
        copy_in(&mut session, "COPY t FROM STDIN", "2\tb\n").unwrap(),
        "COPY 1"
    );
    assert!(copy_in(&mut session, "COPY t FROM STDIN", "3\tc\n3\td\n").is_err());
    assert_eq!(
        failure(run(&mut session, "SELECT 1")).state(),
        SqlState::IN_FAILED_SQL_TRANSACTION
    );
    rows(&mut session, "ROLLBACK");
    assert_eq!(
        rows(&mut session, "SELECT count(*) FROM t"),
        "count\n0\nSELECT 1"
    );
    let mut client = Client::sending(b"1\ta\n2\tb\n3\tc\n", 4);
    client.fail_at = Some(8);
    let error = session
        .execute_with_copy("COPY t FROM STDIN", &mut |_| true, &mut client)
        .unwrap_err();
    assert_eq!(error.state(), SqlState::QUERY_CANCELED);
    assert!(client.abandoned);
    assert_eq!(
        rows(&mut session, "SELECT count(*) FROM t"),
        "count\n0\nSELECT 1"
    );
    rows(&mut session, "BEGIN");
    copy_in(&mut session, "COPY t FROM STDIN", "5\te\n").unwrap();
    rows(&mut session, "ROLLBACK");
    assert_eq!(
        rows(&mut session, "SELECT count(*) FROM t"),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn copy_to_stdout_writes_the_text_and_csv_forms_with_their_options() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE TABLE w (id integer PRIMARY KEY, name text, note text);
         INSERT INTO w VALUES (2, E'tab\there', NULL), (1, 'say \"hi\", ok', ''), (3, 'back\\slash', 'line
break')",
    );
    let mut client = Client::default();
    session
        .execute_with_copy("COPY w TO STDOUT", &mut |_| true, &mut client)
        .unwrap();
    assert_eq!(
        client.started_out,
        Some(CopyStart {
            format: CopyFormat::Text,
            columns: 3
        })
    );
    let cases = [
        (
            "COPY w TO STDOUT",
            "1\tsay \"hi\", ok\t\n2\ttab\\there\t\\N\n3\tback\\\\slash\tline\\nbreak\nCOPY 3",
        ),
        (
            "COPY w TO STDOUT WITH (FORMAT csv, HEADER)",
            "id,name,note\n1,\"say \"\"hi\"\", ok\",\"\"\n2,tab\there,\n3,back\\slash,\"line\nbreak\"\nCOPY 3",
        ),
        (
            "COPY w (note, id) TO STDOUT (FORMAT csv, FORCE_QUOTE *, NULL 'NULL')",
            "\"\",\"1\"\nNULL,\"2\"\n\"line\nbreak\",\"3\"\nCOPY 3",
        ),
        (
            "COPY w TO STDOUT WITH CSV QUOTE '''' FORCE QUOTE name DELIMITER ';'",
            "1;'say \"hi\", ok';''\n2;'tab\there';\n3;'back\\slash';'line\nbreak'\nCOPY 3",
        ),
        (
            "COPY w TO STDOUT (DELIMITER '|', NULL '')",
            "1|say \"hi\", ok|\n2|tab\\there|\n3|back\\\\slash|line\\nbreak\nCOPY 3",
        ),
        (
            "COPY (SELECT id * 10 AS ten, upper(name) FROM w WHERE id < 3 ORDER BY id DESC) TO STDOUT (FORMAT csv, HEADER true)",
            "ten,upper\n20,TAB\tHERE\n10,\"SAY \"\"HI\"\", OK\"\nCOPY 2",
        ),
        (
            "COPY (SELECT 1 WHERE false) TO STDOUT (HEADER)",
            "?column?\nCOPY 0",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(copy_out(&mut session, sql).unwrap(), expected, "{sql}");
    }
}

#[test]
fn options_are_checked_as_postgresql_checks_them() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE TABLE t (a integer, b text); CREATE VIEW v AS SELECT a FROM t",
    );
    let cases = [
        ("COPY t FROM STDIN (FORMAT binary)", "22P04"),
        ("COPY BINARY t FROM STDIN", "22P04"),
        ("COPY BINARY t FROM STDIN (FORMAT csv)", "42601"),
        ("COPY t FROM STDIN (FORMAT binary, DELIMITER ',')", "42601"),
        ("COPY t FROM STDIN (FORMAT binary, NULL 'x')", "42601"),
        ("COPY t TO STDOUT (FORMAT binary, HEADER)", "42601"),
        ("COPY t FROM STDIN (FORMAT binary, ON_ERROR ignore)", "0A000"),
        ("COPY t FROM STDIN (FORMAT xml)", "22023"),
        ("COPY t FROM STDIN (DELIMITER '::')", "0A000"),
        ("COPY t FROM STDIN (DELIMITER E'\\n')", "22023"),
        ("COPY t FROM STDIN (DELIMITER 'a')", "22023"),
        ("COPY t FROM STDIN (QUOTE '''')", "0A000"),
        ("COPY t FROM STDIN (FORMAT csv, QUOTE ',')", "22023"),
        ("COPY t FROM STDIN (FORMAT csv, FORCE_QUOTE (a))", "0A000"),
        ("COPY t TO STDOUT (FORMAT csv, FORCE_NOT_NULL (a))", "0A000"),
        ("COPY t TO STDOUT (DEFAULT 'd')", "0A000"),
        ("COPY t FROM STDIN (NULL 'x', DEFAULT 'x')", "22023"),
        ("COPY t TO STDOUT (HEADER match)", "0A000"),
        ("COPY t FROM STDIN (HEADER maybe)", "22023"),
        ("COPY t FROM STDIN (FORMAT csv, FORMAT text)", "42601"),
        ("COPY t FROM STDIN (SPEED fast)", "42601"),
        ("COPY t FROM STDIN (ENCODING 'EUC_JP')", "0A000"),
        ("COPY t FROM STDIN (ENCODING 'klingon')", "22023"),
        ("COPY t FROM STDIN (ON_ERROR skip)", "22023"),
        ("COPY t FROM STDIN (LOG_VERBOSITY loud)", "22023"),
        ("COPY t TO STDOUT (ON_ERROR ignore)", "0A000"),
        (
            "COPY t FROM STDIN (FORMAT csv, FORCE_NOT_NULL (zzz))",
            "42P10",
        ),
        ("COPY t (zzz) FROM STDIN", "42703"),
        ("COPY t (a, a) FROM STDIN", "42701"),
        ("COPY missing FROM STDIN", "42P01"),
        ("COPY v FROM STDIN", "42809"),
        ("COPY v TO STDOUT", "42809"),
        ("COPY (SELECT 1) FROM STDIN", "42601"),
        ("COPY t FROM '/etc/passwd'", "42501"),
        ("COPY t TO '/tmp/x'", "42501"),
        ("COPY t FROM PROGRAM 'cat /etc/passwd'", "42501"),
        ("COPY t TO PROGRAM 'sh'", "42501"),
        ("COPY t TO STDOUT WHERE a > 1", "0A000"),
        ("COPY t FROM STDIN WHERE zzz > 1", "42703"),
        ("COPY t FROM STDIN WHERE count(*) > 1", "42803"),
        ("COPY t FROM STDIN WHERE row_number() OVER () > 1", "42P20"),
        ("COPY t FROM STDIN WHERE a IN (SELECT 1)", "0A000"),
        ("COPY t FROM STDIN WHERE a + 1", "42804"),
        ("COPY t FROM", "42601"),
    ];
    for (sql, code) in cases {
        let error = failure(copy_in(&mut session, sql, "1\tx\n"));
        assert_eq!(error.state().code(), code, "{sql}: {error}");
    }
    for sql in [
        "COPY t FROM STDIN (FORMAT 'csv', HEADER false, ENCODING 'utf-8', FREEZE, ON_ERROR stop)",
        "COPY t FROM STDIN WITH DELIMITER AS ',' NULL AS 'NA' CSV HEADER QUOTE AS '\"' ESCAPE '\\' FORCE NOT NULL b",
        "COPY public.t (b, a) FROM stdout",
        "COPY t FROM STDIN WITH",
        "COPY t FROM STDIN (ON_ERROR ignore, LOG_VERBOSITY verbose, ENCODING 'latin1')",
        "COPY t FROM STDIN (ENCODING 'UTF-16LE', LOG_VERBOSITY silent)",
        "COPY t FROM STDIN WHERE a > 1",
    ] {
        assert!(copy_in(&mut session, sql, "").is_ok(), "{sql}");
    }
}

#[test]
fn header_match_compares_the_names_with_the_columns() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(&mut session, "CREATE TABLE h (a integer, b text)");
    let sql = "COPY h (b, a) FROM STDIN (FORMAT csv, HEADER match)";
    assert_eq!(copy_in(&mut session, sql, "b,a\nx,1\n").unwrap(), "COPY 1");
    let error = failure(copy_in(&mut session, sql, "a,b\n"));
    assert_eq!(
        error.message(),
        "column name mismatch in header line field 1: got \"a\", expected \"b\""
    );
    let error = failure(copy_in(&mut session, sql, "b\n"));
    assert_eq!(
        error.message(),
        "wrong number of fields in header line: got 1, expected 2"
    );
    assert_eq!(
        copy_in(
            &mut session,
            "COPY h FROM STDIN (HEADER)",
            "anything\tat all\n2\ty\n"
        )
        .unwrap(),
        "COPY 1"
    );
}

#[test]
fn force_null_and_force_not_null_change_what_csv_reads_as_null() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE TABLE f (id integer PRIMARY KEY, a text, b text)",
    );
    copy_in(
        &mut session,
        "COPY f FROM STDIN (FORMAT csv, FORCE_NOT_NULL (a), FORCE_NULL (b))",
        "1,,\"\"\n2,\"\",x\n",
    )
    .unwrap();
    assert_eq!(
        rows(
            &mut session,
            "SELECT id, a IS NULL, a, b IS NULL, b FROM f ORDER BY id"
        ),
        "id|?column?|a|?column?|b\n1|f||t|NULL\n2|f||f|x\nSELECT 2"
    );
    copy_in(
        &mut session,
        "COPY f FROM STDIN (FORMAT csv, FORCE_NOT_NULL (a, b), FORCE_NULL (a, b))",
        "3,,\"\"\n",
    )
    .unwrap();
    assert_eq!(
        rows(
            &mut session,
            "SELECT a IS NULL, a, b IS NULL FROM f WHERE id = 3"
        ),
        "?column?|a|?column?\nf||t\nSELECT 1"
    );
}

#[test]
fn other_dialects_refuse_copy_with_their_own_errors() {
    let store = MemoryStore::default();
    for dialect in [Dialect::MySql, Dialect::Sqlite] {
        let mut session = Session::with_dialect(&store, dialect);
        let _ = run(&mut session, "CREATE TABLE t (a integer)");
        let error = failure(copy_in(&mut session, "COPY t FROM STDIN", "1\n"));
        assert_eq!(error.vendor(), Some(dialect), "{dialect:?}: {error}");
        let error = failure(copy_out(&mut session, "COPY t TO STDOUT"));
        assert_eq!(error.vendor(), Some(dialect), "{dialect:?}: {error}");
        let mut client = Client::default();
        let error = session
            .execute_script("COPY t FROM STDIN;\n1\n\\.\n", &mut |_| true, &mut client)
            .unwrap_err();
        assert_eq!(error.vendor(), Some(dialect), "{dialect:?}: {error}");
    }
}

#[test]
fn oracle_copies_csv_with_empty_fields_as_null() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    run(
        &mut session,
        "CREATE TABLE t (a NUMBER, b VARCHAR2(5), d DATE)",
    )
    .unwrap();
    copy_in(
        &mut session,
        "COPY t FROM STDIN (FORMAT csv, HEADER)",
        "\"A\",\"B\",\"D\"\n1,,17-JUN-13\n2,\"\",2013-06-18 00:00:00\n",
    )
    .unwrap();
    run(&mut session, "COMMIT").unwrap();
    assert_eq!(
        run(
            &mut session,
            "SELECT a, NVL(b, 'null'), TO_CHAR(d, 'YYYY-MM-DD') FROM t ORDER BY a"
        )
        .unwrap(),
        "A|NVL(B,'NULL')|TO_CHAR(D,'YYYY-MM-DD')\n1|null|2013-06-17\n2|null|2013-06-18\nSELECT 2"
    );
    assert_eq!(
        copy_out(&mut session, "COPY (SELECT a FROM t ORDER BY a) TO STDOUT").unwrap(),
        "1\n2\nCOPY 2"
    );
}

#[test]
fn copy_without_a_client_fails_before_touching_anything() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(&mut session, "CREATE TABLE t (a integer)");
    for sql in ["COPY t FROM STDIN", "COPY t TO STDOUT"] {
        let error = failure(run(&mut session, sql));
        assert_eq!(error.state(), SqlState::FEATURE_NOT_SUPPORTED, "{sql}");
    }
    assert!(!session.in_block());
}

#[test]
fn a_script_carries_its_copy_data_inline_up_to_backslash_period() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let text = "CREATE TABLE a (id integer PRIMARY KEY, s text);\n\
                CREATE TABLE b (id integer PRIMARY KEY);\n\
                COPY a (id, s) FROM stdin;\n\
                1\tit's; not -- SQL\n\
                2\t\\N\n\
                \\.\n\
                COPY b FROM stdin; SELECT count(*) FROM a;\n\
                7\n\
                8\n\
                \\.\n\
                COPY a FROM STDIN (FORMAT csv);\r\n\
                3,\"multi\r\nline\"\r\n\
                \\.\r\n\
                SELECT id, s FROM a ORDER BY id;\n\
                COPY b TO STDOUT;\n\
                COPY b FROM STDIN;\n\
                9";
    assert_eq!(
        script(&mut session, text).unwrap(),
        vec![
            "CREATE TABLE".to_owned(),
            "CREATE TABLE".into(),
            "COPY 2".into(),
            "COPY 2".into(),
            "count\n2\nSELECT 1".into(),
            "COPY 1".into(),
            "id|s\n1|it's; not -- SQL\n2|NULL\n3|multi\r\nline\nSELECT 3".into(),
            "COPY 2".into(),
            "COPY 1".into(),
            "7\n8\n".into(),
        ]
    );
    assert_eq!(
        rows(&mut session, "SELECT id FROM b ORDER BY id"),
        "id\n7\n8\n9\nSELECT 3"
    );
    assert_eq!(
        script(&mut session, "COPY b FROM STDIN").unwrap(),
        vec!["COPY 0".to_owned()]
    );
    let error = script(
        &mut session,
        "SELECT 1;\nCOPY b FROM STDIN;\n10\nx\n\\.\nSELECT 2;",
    )
    .unwrap_err();
    assert_eq!(error.state(), SqlState::INVALID_TEXT_REPRESENTATION);
    assert_eq!(error.context(), Some("COPY b, line 2, column id: \"x\""));
    assert_eq!(
        rows(&mut session, "SELECT count(*) FROM b"),
        "count\n3\nSELECT 1"
    );
    let error = script(&mut session, "SELECT 1;\nSELECT * FROM nope;").unwrap_err();
    assert_eq!(error.position(), Some(24));
    let two = "COPY b FROM stdin; COPY a (id) FROM stdin; SELECT count(*) FROM b;\n\
               20\n\
               \\.\n\
               21\n\
               \\.\n\
               SELECT * FROM nope;";
    let error = script(&mut session, two).unwrap_err();
    assert_eq!(error.position(), Some(two.len() - 5));
    assert_eq!(
        rows(&mut session, "SELECT count(*) FROM b WHERE id = 20"),
        "count\n1\nSELECT 1"
    );
    assert_eq!(
        rows(&mut session, "SELECT s IS NULL FROM a WHERE id = 21"),
        "?column?\nt\nSELECT 1"
    );
}

#[test]
fn a_pg_dump_data_section_restores_in_a_script() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let dump = "SET client_encoding = 'UTF8';\n\
                SET standard_conforming_strings = on;\n\
                CREATE TABLE public.items (\n    id integer NOT NULL,\n    name text,\n    price numeric(8,2)\n);\n\
                COPY public.items (id, name, price) FROM stdin;\n\
                1\tegg\t0.25\n\
                2\tbread \\\\ butter\t2.50\n\
                3\t\\N\t\\N\n\
                \\.\n\
                \n\
                SELECT 1;\n";
    script(&mut session, dump).unwrap();
    assert_eq!(
        rows(&mut session, "SELECT * FROM items ORDER BY id"),
        "id|name|price\n1|egg|0.25\n2|bread \\ butter|2.50\n3|NULL|NULL\nSELECT 3"
    );
}

fn random_rows(rng: &mut Rng, count: usize) -> Vec<(i64, Option<String>, Option<i32>)> {
    let mut used = std::collections::BTreeSet::new();
    let mut rows = Vec::new();
    while rows.len() < count {
        let id = rng.below(5_000) as i64 - 1_000;
        if !used.insert(id) {
            continue;
        }
        let name = random_value(rng).map(|name| name.replace('\r', ""));
        let score = (rng.below(4) != 0).then(|| rng.below(200) as i32 - 100);
        rows.push((id, name, score));
    }
    rows
}

fn literal(text: &Option<String>) -> String {
    match text {
        Some(text) => format!("'{}'", text.replace('\'', "''")),
        None => "NULL".to_owned(),
    }
}

#[test]
fn copy_from_agrees_with_insert_of_the_same_rows_with_and_without_indexes() {
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    for round in 0..24 {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for table in ["by_insert", "by_copy"] {
            rows(
                &mut session,
                &format!(
                    "CREATE TABLE {table} (id bigint PRIMARY KEY, name text, score integer CHECK (score > -90))"
                ),
            );
            if round % 2 == 1 {
                rows(
                    &mut session,
                    &format!("CREATE INDEX {table}_name ON {table} (name)"),
                );
                rows(
                    &mut session,
                    &format!("CREATE UNIQUE INDEX {table}_score ON {table} (score)"),
                );
            }
        }
        let count = 1 + rng.below(60);
        let batch = random_rows(&mut rng, count);
        let values: Vec<String> = batch
            .iter()
            .map(|(id, name, score)| {
                let score = score.map_or("NULL".to_owned(), |score| score.to_string());
                format!("({id}, {}, {score})", literal(name))
            })
            .collect();
        let insert = format!("INSERT INTO by_insert VALUES {}", values.join(", "));
        let csv = round % 3 == 0;
        let mut writer =
            RowWriter::new(if csv { csv_layout() } else { Layout::text() }, Vec::new());
        let mut data = Vec::new();
        for (id, name, score) in &batch {
            data.extend_from_slice(writer.line(&[
                Some(id.to_string()),
                name.clone(),
                score.map(|score| score.to_string()),
            ]));
        }
        let copy = if csv {
            "COPY by_copy FROM STDIN (FORMAT csv)"
        } else {
            "COPY by_copy FROM STDIN"
        };
        let in_block = round % 4 == 2;
        if in_block {
            rows(&mut session, "BEGIN");
        }
        let inserted = run(&mut session, &insert);
        if in_block && inserted.is_err() {
            rows(&mut session, "ROLLBACK");
            rows(&mut session, "BEGIN");
        }
        let copied = copy_in(&mut session, copy, &String::from_utf8(data).unwrap());
        assert_eq!(
            inserted.as_ref().map(|_| ()).map_err(SqlError::state),
            copied.as_ref().map(|_| ()).map_err(SqlError::state),
            "round {round}: {inserted:?} {copied:?}"
        );
        if in_block {
            let ending = if round % 8 == 2 { "ROLLBACK" } else { "COMMIT" };
            rows(&mut session, ending);
        }
        assert_eq!(
            rows(&mut session, "SELECT * FROM by_insert ORDER BY id"),
            rows(&mut session, "SELECT * FROM by_copy ORDER BY id"),
            "round {round}"
        );
        assert_eq!(
            session.check_indexes().map(|check| check.indexes),
            Ok(if round % 2 == 1 { 4 } else { 0 })
        );
    }
}

fn csv_layout() -> Layout {
    csv()
}

#[test]
fn hostile_copy_statements_and_data_never_panic() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE TABLE t (id integer PRIMARY KEY, name text, at date, n numeric)",
    );
    let fragments = [
        "COPY",
        "t",
        "(",
        ")",
        "id",
        ",",
        "name",
        "FROM",
        "TO",
        "STDIN",
        "STDOUT",
        "WITH",
        "FORMAT",
        "csv",
        "text",
        "binary",
        "DELIMITER",
        "','",
        "E'\\t'",
        "NULL",
        "''",
        "HEADER",
        "match",
        "QUOTE",
        "ESCAPE",
        "'\"'",
        "FORCE_QUOTE",
        "*",
        "FORCE",
        "NOT",
        "FORCE_NULL",
        "DEFAULT",
        "'\\D'",
        "ENCODING",
        "'utf8'",
        "SELECT 1",
        ";",
        "PROGRAM",
        "'f'",
        "AS",
        "true",
        "-1",
        "WHERE",
        "FREEZE",
        "ON_ERROR",
        "ignore",
        "LOG_VERBOSITY",
        "verbose",
        "'latin1'",
        "'utf16'",
        "a > 1",
    ];
    fuzz(&mut session, &fragments);
    let mut rng = Rng(0xabad_1dea);
    let alphabet = b"0123456789\t\n\r\\N.,\"x-:\xff ";
    for round in 0..3_000 {
        let length = rng.below(60);
        let data: Vec<u8> = (0..length)
            .map(|_| alphabet[rng.below(alphabet.len())])
            .collect();
        let sql = match round % 4 {
            0 => "COPY t FROM STDIN",
            1 => "COPY t FROM STDIN (FORMAT csv)",
            2 => "COPY t (name, id) FROM STDIN (FORMAT csv, HEADER match, DEFAULT 'x')",
            _ => "COPY t FROM STDIN (DELIMITER ',', NULL '')",
        };
        let mut client = Client::sending(&data, 1 + rng.below(8));
        let _ = session.execute_with_copy(sql, &mut |_| true, &mut client);
        if session.in_block() {
            let _ = run(&mut session, "ROLLBACK");
        }
        let text = String::from_utf8_lossy(&data);
        let _ = session.execute_script(
            &format!("COPY t FROM STDIN;\n{text}\n\\.\nSELECT 1;"),
            &mut |_| true,
            &mut Client::default(),
        );
    }
    assert!(copy_out(&mut session, "COPY t TO STDOUT").is_ok());
}

#[test]
fn copy_reads_a_catalog_relation_and_never_writes_one() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(&mut session, "CREATE SCHEMA shop");
    let shown = copy_out(
        &mut session,
        "COPY pg_catalog.pg_namespace (nspname, nspowner) TO STDOUT (FORMAT csv)",
    )
    .unwrap();
    assert!(shown.contains("pg_catalog,10\n"), "{shown}");
    assert!(shown.contains("shop,10\n"), "{shown}");
    assert!(shown.ends_with("COPY 5"), "{shown}");
    let error = copy_in(&mut session, "COPY pg_namespace FROM STDIN", "x\n").unwrap_err();
    assert_eq!(
        (error.state(), error.message()),
        (
            SqlState::INSUFFICIENT_PRIVILEGE,
            "permission denied for table pg_namespace"
        )
    );
}

fn copy_in_bytes(
    session: &mut Session<'_, MemoryStore>,
    sql: &str,
    data: &[u8],
    chunk: usize,
) -> SqlResult<QueryResult> {
    let mut client = Client::sending(data, chunk);
    let mut last = None;
    session.execute_with_copy(
        sql,
        &mut |result: QueryResult| {
            last = Some(result);
            true
        },
        &mut client,
    )?;
    Ok(last.unwrap_or_else(|| QueryResult::command("")))
}

fn copy_out_bytes(session: &mut Session<'_, MemoryStore>, sql: &str) -> SqlResult<Vec<u8>> {
    let mut client = Client::default();
    session.execute_with_copy(sql, &mut |_| true, &mut client)?;
    assert!(client.finished_out, "{sql}");
    Ok(client.output)
}

fn binary_file(rows: &[&[Option<&[u8]>]]) -> Vec<u8> {
    let mut out = b"PGCOPY\n\xff\r\n\0".to_vec();
    out.extend_from_slice(&[0; 8]);
    for row in rows {
        out.extend_from_slice(&(row.len() as i16).to_be_bytes());
        for field in *row {
            match field {
                None => out.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(bytes) => {
                    out.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                    out.extend_from_slice(bytes);
                }
            }
        }
    }
    out
}

fn with_trailer(mut file: Vec<u8>) -> Vec<u8> {
    file.extend_from_slice(&(-1i16).to_be_bytes());
    file
}

#[test]
fn copy_to_stdout_in_the_binary_format_writes_postgresqls_file_layout() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE TABLE pair (a integer PRIMARY KEY, b text); INSERT INTO pair VALUES (1, 'x'), (2, NULL)",
    );
    let one = 1i32.to_be_bytes();
    let two = 2i32.to_be_bytes();
    let expected = with_trailer(binary_file(&[
        &[Some(&one[..]), Some(b"x")],
        &[Some(&two[..]), None],
    ]));
    assert_eq!(
        copy_out_bytes(&mut session, "COPY pair TO STDOUT (FORMAT binary)").unwrap(),
        expected
    );
    assert_eq!(
        copy_out_bytes(&mut session, "COPY BINARY pair TO STDOUT").unwrap(),
        expected
    );
    assert_eq!(
        copy_out_bytes(
            &mut session,
            "COPY (SELECT a, b FROM pair ORDER BY a) TO STDOUT WITH (FORMAT binary)"
        )
        .unwrap(),
        expected
    );
    let mut client = Client::default();
    session
        .execute_with_copy("COPY pair (b) TO STDOUT (FORMAT binary)", &mut |_| true, &mut client)
        .unwrap();
    assert_eq!(
        client.started_out,
        Some(CopyStart {
            format: CopyFormat::Binary,
            columns: 1
        })
    );
    rows(&mut session, "DELETE FROM pair");
    assert_eq!(
        copy_out_bytes(&mut session, "COPY pair TO STDOUT (FORMAT binary)").unwrap(),
        with_trailer(binary_file(&[]))
    );
}

#[test]
fn binary_copy_round_trips_every_type_through_any_pieces() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    typed_table(&mut session);
    rows(
        &mut session,
        "INSERT INTO typed VALUES \
         (1, 'ada', 1.5, '2024-01-02 03:04:05', true, '\\x00ff', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '{\"b\": 1}'), \
         (2, 'bob', NULL, NULL, false, '\\x', NULL, '[]'), \
         (3, 'é 😀', -1234.5, '1999-12-31 23:59:59.999999', NULL, NULL, NULL, NULL)",
    );
    let file = copy_out_bytes(&mut session, "COPY typed TO STDOUT (FORMAT binary)").unwrap();
    let before = rows(&mut session, "SELECT * FROM typed ORDER BY id");
    for chunk in [1, 3, 64, 1 << 20] {
        rows(&mut session, "DELETE FROM typed");
        let loaded =
            copy_in_bytes(&mut session, "COPY typed FROM STDIN (FORMAT binary)", &file, chunk)
                .unwrap();
        assert_eq!(loaded.tag, "COPY 3", "{chunk}");
        assert_eq!(rows(&mut session, "SELECT * FROM typed ORDER BY id"), before);
    }
    rows(
        &mut session,
        "CREATE TABLE listed (id integer PRIMARY KEY, tags text[], nums integer[]); \
         INSERT INTO listed VALUES (1, ARRAY['a', 'b c', NULL], ARRAY[1, 2]), (2, '{}', NULL)",
    );
    let file = copy_out_bytes(&mut session, "COPY listed TO STDOUT (FORMAT binary)").unwrap();
    let before = rows(&mut session, "SELECT * FROM listed ORDER BY id");
    rows(&mut session, "DELETE FROM listed");
    assert_eq!(
        copy_in_bytes(&mut session, "COPY listed FROM STDIN (FORMAT binary)", &file, 5)
            .unwrap()
            .tag,
        "COPY 2"
    );
    assert_eq!(rows(&mut session, "SELECT * FROM listed ORDER BY id"), before);
}

#[test]
fn binary_data_that_breaks_the_format_fails_with_postgresqls_messages() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE TABLE pair (a integer PRIMARY KEY, b text)",
    );
    let one = 1i32.to_be_bytes();
    let good = binary_file(&[&[Some(&one[..]), Some(b"x")]]);
    let mut oids = good.clone();
    oids[12] = 1;
    let mut critical = good.clone();
    critical[11] = 0x80;
    let mut after_trailer = with_trailer(good.clone());
    after_trailer.push(0);
    let cases: [(Vec<u8>, &str, &str, &str); 8] = [
        (
            b"PGCOPY\n".to_vec(),
            "22P04",
            "COPY file signature not recognized",
            "COPY pair, line 0",
        ),
        (
            oids,
            "22P04",
            "invalid COPY file header (WITH OIDS)",
            "COPY pair, line 0",
        ),
        (
            critical,
            "22P04",
            "unrecognized critical flags in COPY file header",
            "COPY pair, line 0",
        ),
        (
            binary_file(&[&[Some(&one[..])]]),
            "22P04",
            "row field count is 1, expected 2",
            "COPY pair, line 1",
        ),
        (
            good[..good.len() - 1].to_vec(),
            "22P04",
            "unexpected EOF in COPY data",
            "COPY pair, line 1",
        ),
        (
            after_trailer,
            "22P04",
            "received copy data after EOF marker",
            "COPY pair, line 2",
        ),
        (
            binary_file(&[&[Some(&[0, 1][..]), Some(b"x")]]),
            "22P03",
            "incorrect binary data format",
            "COPY pair, line 1, column a",
        ),
        (
            binary_file(&[&[Some(&one[..]), Some(b"\xff")]]),
            "22021",
            "invalid byte sequence for encoding \"UTF8\"",
            "COPY pair, line 1, column b",
        ),
    ];
    for (data, code, message, context) in cases {
        let error =
            copy_in_bytes(&mut session, "COPY pair FROM STDIN (FORMAT binary)", &data, 3)
                .unwrap_err();
        assert_eq!(error.state().code(), code, "{message}: {error}");
        assert_eq!(error.message(), message);
        assert_eq!(error.context(), Some(context), "{message}");
    }
    assert_eq!(rows(&mut session, "SELECT count(*) FROM pair"), "count\n0\nSELECT 1");
    let mut extended = b"PGCOPY\n\xff\r\n\0".to_vec();
    extended.extend_from_slice(&0u32.to_be_bytes());
    extended.extend_from_slice(&3u32.to_be_bytes());
    extended.extend_from_slice(b"ext");
    extended.extend_from_slice(&good[19..]);
    assert_eq!(
        copy_in_bytes(&mut session, "COPY pair FROM STDIN (FORMAT binary)", &extended, 2)
            .unwrap()
            .tag,
        "COPY 1"
    );
    let failed = script(
        &mut session,
        "COPY pair FROM STDIN (FORMAT binary);\n\\.\nSELECT 1;",
    )
    .unwrap_err();
    assert_eq!(failed.state().code(), "0A000");
    rows(
        &mut session,
        "CREATE TABLE spans (id integer PRIMARY KEY, span int4range)",
    );
    assert_eq!(
        failure(copy_out(&mut session, "COPY spans TO STDOUT (FORMAT binary)"))
            .state()
            .code(),
        "0A000"
    );
}

#[test]
fn random_binary_data_never_panics_and_loads_nothing_when_it_fails() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE TABLE t (id integer PRIMARY KEY, name text, at date, n numeric, tags text[])",
    );
    let mut rng = Rng(0x00b1_4a27);
    let header = binary_file(&[]);
    for round in 0..3_000 {
        let length = rng.below(80);
        let mut data: Vec<u8> = if round % 2 == 0 { header.clone() } else { Vec::new() };
        data.extend((0..length).map(|_| match rng.below(4) {
            0 => 0,
            1 => 0xff,
            2 => 5,
            _ => rng.below(256) as u8,
        }));
        let mut client = Client::sending(&data, 1 + rng.below(9));
        let _ = session.execute_with_copy(
            "COPY t FROM STDIN (FORMAT binary)",
            &mut |_| true,
            &mut client,
        );
    }
    assert!(copy_out(&mut session, "COPY t TO STDOUT").is_ok());
}

fn notices_of(result: &QueryResult) -> Vec<&str> {
    result.notices.iter().map(String::as_str).collect()
}

#[test]
fn on_error_ignore_skips_rows_whose_values_do_not_convert_and_says_so() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE TABLE kept (a integer PRIMARY KEY, b varchar(3))",
    );
    let data = b"1\tok\nx\tbad\n3\ttoolong\n4\tfine\n";
    let verbose = copy_in_bytes(
        &mut session,
        "COPY kept FROM STDIN (ON_ERROR ignore, LOG_VERBOSITY verbose)",
        data,
        4,
    )
    .unwrap();
    assert_eq!(verbose.tag, "COPY 2");
    assert_eq!(
        notices_of(&verbose),
        [
            "NOTICE:  skipping row due to data type incompatibility at line 2 for column \"a\": \"x\"",
            "NOTICE:  skipping row due to data type incompatibility at line 3 for column \"b\": \"toolong\"",
            "NOTICE:  2 rows were skipped due to data type incompatibility",
        ]
    );
    assert_eq!(
        rows(&mut session, "SELECT * FROM kept ORDER BY a"),
        "a|b\n1|ok\n4|fine\nSELECT 2"
    );
    rows(&mut session, "DELETE FROM kept");
    let plain = copy_in_bytes(
        &mut session,
        "COPY kept FROM STDIN (FORMAT csv, ON_ERROR ignore)",
        b"1,a\n2,b\nz,c\n",
        3,
    )
    .unwrap();
    assert_eq!(plain.tag, "COPY 2");
    assert_eq!(
        notices_of(&plain),
        ["NOTICE:  1 row was skipped due to data type incompatibility"]
    );
    rows(&mut session, "DELETE FROM kept");
    let silent = copy_in_bytes(
        &mut session,
        "COPY kept FROM STDIN (ON_ERROR ignore, LOG_VERBOSITY silent)",
        b"q\tx\n",
        3,
    )
    .unwrap();
    assert_eq!(silent.tag, "COPY 0");
    assert!(silent.notices.is_empty());
    for (data, code) in [
        (&b"5\ta\textra\n"[..], "22P04"),
        (&b"5\ta\n5\tb\n"[..], "23505"),
        (&b"\\N\ta\n"[..], "23502"),
        (&b"6\t\xff\n"[..], "22021"),
    ] {
        let error = copy_in_bytes(
            &mut session,
            "COPY kept FROM STDIN (ON_ERROR ignore)",
            data,
            2,
        )
        .unwrap_err();
        assert_eq!(error.state().code(), code);
    }
    assert_eq!(rows(&mut session, "SELECT count(*) FROM kept"), "count\n0\nSELECT 1");
}

#[test]
fn copy_from_where_loads_only_the_rows_its_condition_keeps() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(
        &mut session,
        "CREATE TABLE picked (id integer PRIMARY KEY, n integer, label text DEFAULT 'd')",
    );
    let loaded = copy_in(
        &mut session,
        "COPY picked (id, n) FROM STDIN WHERE n > 1 AND label = 'd'",
        "1\t1\n2\t5\n3\t\\N\n4\t7\n",
    )
    .unwrap();
    assert_eq!(loaded, "COPY 2");
    assert_eq!(
        rows(&mut session, "SELECT * FROM picked ORDER BY id"),
        "id|n|label\n2|5|d\n4|7|d\nSELECT 2"
    );
    assert_eq!(
        copy_in(
            &mut session,
            "COPY picked FROM STDIN (FORMAT csv, HEADER) WHERE picked.id % 2 = 1",
            "id,n,label\n5,1,a\n6,1,b\n7,1,c\n",
        )
        .unwrap(),
        "COPY 2"
    );
    assert_eq!(
        rows(&mut session, "SELECT id FROM picked WHERE id > 4 ORDER BY id"),
        "id\n5\n7\nSELECT 2"
    );
    let shown = script(
        &mut session,
        "COPY picked FROM STDIN WHERE label <> 'skip';\n8\t1\tskip\n9\t1\tkeep\n\\.\nSELECT count(*) FROM picked WHERE id > 7;",
    )
    .unwrap();
    assert_eq!(shown, ["COPY 1", "count\n1\nSELECT 1"]);
}

#[test]
fn encodings_other_than_utf8_are_converted_in_and_out() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    rows(&mut session, "CREATE TABLE words (id integer PRIMARY KEY, w text)");
    assert_eq!(
        copy_in_bytes(
            &mut session,
            "COPY words FROM STDIN (ENCODING 'LATIN1')",
            b"1\tcaf\xe9\n",
            1,
        )
        .unwrap()
        .tag,
        "COPY 1"
    );
    assert_eq!(
        copy_in_bytes(
            &mut session,
            "COPY words FROM STDIN (FORMAT csv, ENCODING 'WIN1252')",
            b"2,\x93quoted\x94 \x80\n",
            2,
        )
        .unwrap()
        .tag,
        "COPY 1"
    );
    let utf16: Vec<u8> = [0xFEFFu16]
        .into_iter()
        .chain("3\tnaïve 😀\n".encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    assert_eq!(
        copy_in_bytes(&mut session, "COPY words FROM STDIN (ENCODING 'UTF16')", &utf16, 3)
            .unwrap()
            .tag,
        "COPY 1"
    );
    assert_eq!(
        rows(&mut session, "SELECT * FROM words ORDER BY id"),
        "id|w\n1|café\n2|\u{201c}quoted\u{201d} €\n3|naïve 😀\nSELECT 3"
    );
    assert_eq!(
        copy_out_bytes(
            &mut session,
            "COPY (SELECT w FROM words WHERE id < 3 ORDER BY id) TO STDOUT (ENCODING 'WIN1252')"
        )
        .unwrap(),
        b"caf\xe9\n\x93quoted\x94 \x80\n"
    );
    let untranslatable = copy_out_bytes(&mut session, "COPY words TO STDOUT (ENCODING 'LATIN1')")
        .unwrap_err();
    assert_eq!(untranslatable.state().code(), "22P05");
    assert_eq!(
        failure(copy_in_bytes(
            &mut session,
            "COPY words FROM STDIN (ENCODING 'WIN1252')",
            b"9\t\x81\n",
            1,
        )
        .map(|result| result.tag))
        .state()
        .code(),
        "22P05"
    );
    let shown = script(
        &mut session,
        "COPY words FROM STDIN (ENCODING 'LATIN1');\n10\tdéjà\n\\.\nSELECT w FROM words WHERE id = 10;",
    )
    .unwrap();
    assert_eq!(shown, ["COPY 1", "w\ndéjà\nSELECT 1"]);
}
