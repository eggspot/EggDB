//! `eggdb serve`'s statement statistics, slow-statement log and `EXPLAIN (ANALYZE, BUFFERS)`
//! (D286), driven over the PostgreSQL protocol: `pg_stat_statements` counts each statement
//! shape with its literals as placeholders, `--log-slow-statements 0` logs every statement as
//! one line on stderr without its values, and `BUFFERS` prints the page cache's hits and reads.

mod common;

use common::TempPath;
use eggdb_pgwire::client::{PgClient, parse_postgres_url_with_password};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};

const PASSWORD: &str = "s3cret";

struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(file: &TempPath, extra: &[&str]) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .arg("serve")
        .arg(file.path())
        .args(["--create", "--listen", "0", "--database", "stats"])
        .args(extra)
        .env("EGGDB_PASSWORD", PASSWORD)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    let port = first
        .split("\"port\":")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .and_then(|port| port.parse().ok())
        .unwrap_or_else(|| panic!("no port in {first}"));
    Server { child, port }
}

fn connect(server: &Server) -> PgClient {
    let options = parse_postgres_url_with_password(&format!(
        "postgres://eggdb:{PASSWORD}@127.0.0.1:{}/stats?sslmode=disable",
        server.port
    ))
    .unwrap();
    PgClient::connect(&options).unwrap()
}

fn rows(client: &mut PgClient, sql: &str) -> Vec<Vec<String>> {
    client
        .simple_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
        .into_iter()
        .flat_map(|result| result.rows)
        .map(|row| row.into_iter().map(Option::unwrap_or_default).collect())
        .collect()
}

fn first_column(client: &mut PgClient, sql: &str) -> Vec<String> {
    rows(client, sql)
        .into_iter()
        .filter_map(|row| row.into_iter().next())
        .collect()
}

fn stderr_after_stop(mut server: Server) -> String {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &server.child.id().to_string()])
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = server.child.kill();
    }
    let _ = server.child.wait();
    let mut stderr = String::new();
    if let Some(mut pipe) = server.child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    stderr
}

fn with_accounts(client: &mut PgClient) {
    client
        .simple_query(
            "CREATE TABLE accounts (id integer PRIMARY KEY, secret text);
             INSERT INTO accounts VALUES (1, 'hunter2');
             INSERT INTO accounts VALUES (2, 'swordfish')",
        )
        .unwrap();
}

#[test]
fn pg_stat_statements_counts_each_shape_without_its_literals_and_resets() {
    let file = TempPath::new("statement-statistics");
    let server = serve(&file, &[]);
    let mut client = connect(&server);
    with_accounts(&mut client);
    for id in [1, 1, 2] {
        rows(
            &mut client,
            &format!("SELECT secret FROM accounts WHERE id = {id}"),
        );
    }
    assert_eq!(
        rows(
            &mut client,
            "SELECT query, calls, rows, min_exec_time <= max_exec_time, total_exec_time >= 0 \
             FROM pg_stat_statements WHERE query LIKE 'SELECT secret FROM accounts%'"
        ),
        vec![vec![
            "SELECT secret FROM accounts WHERE id = $1".to_owned(),
            "3".to_owned(),
            "3".to_owned(),
            "t".to_owned(),
            "t".to_owned(),
        ]]
    );
    assert_eq!(
        first_column(
            &mut client,
            "SELECT query FROM pg_stat_statements WHERE query LIKE 'INSERT INTO accounts%'"
        ),
        vec!["INSERT INTO accounts VALUES ($1, $2)".to_owned()]
    );
    assert_eq!(
        first_column(
            &mut client,
            "SELECT count(*) FROM pg_stat_statements WHERE query LIKE '%hunter2%' OR query LIKE '%swordfish%'"
        ),
        vec!["0".to_owned()]
    );
    assert_eq!(
        first_column(&mut client, "SELECT dealloc FROM pg_stat_statements_info"),
        vec!["0".to_owned()]
    );
    rows(&mut client, "SELECT pg_stat_statements_reset()");
    assert_eq!(
        first_column(&mut client, "SELECT query FROM pg_stat_statements"),
        vec!["SELECT pg_stat_statements_reset()".to_owned()]
    );
    client.terminate();
}

#[test]
fn statement_statistics_zero_keeps_none() {
    let file = TempPath::new("statement-statistics-off");
    let server = serve(&file, &["--statement-statistics", "0"]);
    let mut client = connect(&server);
    rows(&mut client, "SELECT 1");
    assert_eq!(
        first_column(&mut client, "SELECT count(*) FROM pg_stat_statements"),
        vec!["0".to_owned()]
    );
    client.terminate();
}

#[test]
fn a_zero_threshold_logs_every_statement_as_one_line_without_its_values() {
    let file = TempPath::new("slow-statements");
    let server = serve(&file, &["--log-slow-statements", "0"]);
    let mut client = connect(&server);
    with_accounts(&mut client);
    rows(&mut client, "SELECT secret FROM accounts WHERE id = 1");
    client.terminate();
    let stderr = stderr_after_stop(server);
    let line = stderr
        .lines()
        .find(|line| line.contains("query=\"SELECT secret FROM accounts WHERE id = $1\""))
        .unwrap_or_else(|| panic!("no slow-statement line in {stderr}"));
    assert!(line.contains("slow statement duration_ms="), "{line}");
    assert!(
        line.contains(" database=stats user=eggdb protocol=postgresql rows=1 "),
        "{line}"
    );
    assert!(
        line.starts_with("20"),
        "the line starts with its UTC time: {line}"
    );
    assert!(
        stderr.contains("query=\"INSERT INTO accounts VALUES ($1, $2)\""),
        "{stderr}"
    );
    assert!(
        !stderr.contains("hunter2") && !stderr.contains("swordfish"),
        "{stderr}"
    );
}

#[test]
fn explain_analyze_buffers_prints_the_page_hits_and_reads_under_the_top_node() {
    let file = TempPath::new("explain-buffers");
    let server = serve(&file, &[]);
    let mut client = connect(&server);
    with_accounts(&mut client);
    let plan = first_column(
        &mut client,
        "EXPLAIN (ANALYZE, BUFFERS) SELECT * FROM accounts",
    );
    assert!(
        plan[0].starts_with("Seq Scan on accounts") && plan[0].ends_with("(actual rows=2 loops=1)"),
        "{plan:?}"
    );
    let buffers = &plan[1];
    let counts = buffers
        .strip_prefix("  Buffers: shared ")
        .unwrap_or_else(|| panic!("{plan:?}"));
    assert!(
        counts.split(' ').all(|count| {
            let (name, number) = count.split_once('=').unwrap_or_default();
            matches!(name, "hit" | "read") && number.parse::<u64>().is_ok_and(|n| n > 0)
        }),
        "{plan:?}"
    );
    assert!(
        plan[plan.len() - 2].starts_with("Planning Time: ")
            && plan[plan.len() - 1].starts_with("Execution Time: "),
        "{plan:?}"
    );
    let without = first_column(&mut client, "EXPLAIN (ANALYZE) SELECT * FROM accounts");
    assert!(
        without.iter().all(|line| !line.contains("Buffers:")),
        "{without:?}"
    );
    let off = first_column(
        &mut client,
        "EXPLAIN (ANALYZE, BUFFERS off) SELECT * FROM accounts",
    );
    assert!(off.iter().all(|line| !line.contains("Buffers:")), "{off:?}");
    client.terminate();
}
