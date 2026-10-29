//! Hostile input for the TDS server (D9, D35): a seeded fuzzer that mutates valid PRELOGIN,
//! LOGIN7, SQL batch, RPC and transaction manager messages and feeds them to every parser,
//! which must answer with an error or a value and never panic; and the same mutations sent over
//! real sockets before and after login, with oversized lengths, packets shorter than their
//! header and stalled clients, after which the server must still serve a well-behaved client.
//! `EGGDB_FUZZ_CASES` and `EGGDB_FUZZ_SEED` run longer campaigns.

mod common;

use common::{
    Client, LoginOptions, TestServer, batch_body, call_by_id, call_by_name, decimal_parameter,
    errors, int_parameter, login7_body, prelogin_body, rows, rpc_body, text_parameter,
    transaction_body,
};
use eggdb_tdswire::messages::{
    parse_login7, parse_prelogin, parse_sql_batch, parse_transaction_manager_request,
};
use eggdb_tdswire::rpc::parse_rpc_request;
use std::time::Duration;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound.max(1) as u64) as usize
    }
}

fn setting(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut bytes = seed.to_vec();
    for _ in 0..1 + rng.below(6) {
        match rng.below(6) {
            0 if !bytes.is_empty() => {
                let at = rng.below(bytes.len());
                bytes[at] = rng.next() as u8;
            }
            1 if !bytes.is_empty() => {
                let at = rng.below(bytes.len());
                bytes[at] = [0, 0xFF, 0x7F, 0x80][rng.below(4)];
            }
            2 => {
                let at = rng.below(bytes.len() + 1);
                bytes.truncate(at);
            }
            3 => {
                let at = rng.below(bytes.len() + 1);
                bytes.insert(at, rng.next() as u8);
            }
            4 if bytes.len() >= 2 => {
                let at = rng.below(bytes.len() - 1);
                bytes[at..at + 2].copy_from_slice(&[0xFF, 0xFF]);
            }
            _ => bytes.extend(std::iter::repeat_n(rng.next() as u8, rng.below(64))),
        }
    }
    bytes
}

fn seeds() -> Vec<Vec<u8>> {
    vec![
        prelogin_body(0x00),
        login7_body(&LoginOptions::default()),
        batch_body("SELECT TOP (2) [a] FROM dbo.t WHERE b = N'x' -- c\nSELECT @@VERSION"),
        batch_body("BEGIN TRAN INSERT INTO t VALUES (1) COMMIT SET NOCOUNT ON USE shop"),
        batch_body("CREATE TABLE [t] ([id] INT IDENTITY(1,1) PRIMARY KEY, n NVARCHAR(max))"),
        rpc_body(&[
            call_by_id(
                10,
                &[
                    text_parameter("", Some("SELECT @p1, @p2 WHERE @p3 > 0"), false),
                    text_parameter("", Some("@p1 int, @p2 nvarchar(40), @p3 decimal(5,2)"), false),
                    int_parameter("@p1", Some(-7), true),
                    text_parameter("@p2", None, false),
                    decimal_parameter("@p3", 1250, 5, 2),
                ],
            ),
            call_by_id(12, &[int_parameter("", Some(1), false)]),
        ]),
        rpc_body(&[
            call_by_id(
                13,
                &[
                    int_parameter("", None, true),
                    text_parameter("", Some("@a int"), false),
                    text_parameter("", Some("SELECT @a"), false),
                    int_parameter("", Some(3), false),
                ],
            ),
            call_by_name("sp_unprepare", &[int_parameter("", Some(1), false)]),
        ]),
        transaction_body(7, &[1, b't', 0, 1, 2, 0]),
    ]
}

#[test]
fn mutated_messages_never_panic_a_parser() {
    let cases = setting("EGGDB_FUZZ_CASES", 20_000);
    let mut rng = Rng(setting("EGGDB_FUZZ_SEED", 0x05EE_D7D5));
    let seeds = seeds();
    for _ in 0..cases {
        let seed = &seeds[rng.below(seeds.len())];
        let bytes = mutate(&mut rng, seed);
        let _ = parse_prelogin(&bytes);
        if let Ok(login) = parse_login7(&bytes) {
            let _ = login
                .password
                .as_ref()
                .map(|password| password.bytes().len());
        }
        let _ = parse_sql_batch(&bytes);
        let _ = parse_rpc_request(&bytes);
        let _ = parse_transaction_manager_request(&bytes);
    }
}

#[test]
fn mutated_messages_over_sockets_leave_the_server_serving() {
    let server = TestServer::start_with(64, Duration::from_secs(2));
    let mut rng = Rng(setting("EGGDB_FUZZ_SEED", 0x0BAD_07D5));
    let cases = setting("EGGDB_FUZZ_CASES", 20_000).min(200);
    for case in 0..cases {
        let seeds = seeds();
        let bytes = mutate(&mut rng, &seeds[case as usize % 5]);
        let mut client = Client::connect(server.address);
        match case % 4 {
            0 => client.send(0x12, &bytes),
            1 => {
                client.prelogin(0x02);
                client.send(0x10, &bytes);
            }
            2 => {
                let (mut logged_in, _) =
                    Client::login_with(server.address, &LoginOptions::default());
                let seed = 2 + rng.below(seeds.len() - 2);
                let request = mutate(&mut rng, &seeds[seed]);
                let kind = match seed {
                    5 | 6 => 0x03,
                    7 => 0x0E,
                    _ => 0x01,
                };
                logged_in.send(kind, &request);
                let _ = logged_in.read_message();
                continue;
            }
            _ => {
                client.send_raw(&bytes);
                continue;
            }
        }
        let _ = client.read_message();
    }
    let mut short = Client::connect(server.address);
    short.send_raw(&[0x12, 0x01, 0x00, 0x04, 0, 0, 1, 0]);
    assert!(short.is_closed());
    let mut stalled = Client::connect(server.address);
    stalled.send_raw(&[0x12, 0x01, 0x00]);
    assert!(stalled.is_closed());
    let mut client = Client::login(server.address);
    assert_eq!(
        rows(&client.batch("SELECT 1")),
        vec![vec![Some("1".to_owned())]]
    );
    assert!(errors(&client.batch("SELECT 2")).is_empty());
}

#[test]
fn hostile_arguments_to_sql_servers_functions_get_errors_and_the_server_keeps_serving() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let long_date = format!("SELECT CAST('{}' AS datetime2) AS d", "2".repeat(10_000));
    let wide_binary = format!("SELECT 0x{} AS b", "AB".repeat(500_000));
    let batches = [
        "SELECT DATEADD(year, 2147483647, '20060731') AS d".to_owned(),
        "SELECT DATEADD(nanosecond, 9223372036854775807, SYSDATETIME()) AS d".to_owned(),
        "SELECT DATEADD(microsecond, -9223372036854775807, SYSDATETIME()) AS d".to_owned(),
        "SELECT DATEDIFF(nanosecond, '0001-01-01', '9999-12-31') AS d".to_owned(),
        "SELECT CONVERT(varchar(30), GETDATE(), 65535) AS d".to_owned(),
        "SELECT CONVERT(varchar(30), GETDATE(), 99999) AS d".to_owned(),
        "SELECT CAST('99999999999999999999999999999999999999999' AS money) AS m".to_owned(),
        "SELECT CAST(N'\u{1F600}\u{1F600}\u{1F600}' AS uniqueidentifier) AS g".to_owned(),
        "SELECT CHARINDEX(N'b', N'a\u{1F600}b', -5) AS i".to_owned(),
        "SELECT CAST(1e308 AS real) AS r".to_owned(),
        "SELECT CAST('12/31/9999 23:59:59.997' AS datetime) + 1 AS d".to_owned(),
        long_date,
        wide_binary,
    ];
    for text in &batches {
        let tokens = client.batch(text);
        assert!(!tokens.is_empty(), "{}", &text[..text.len().min(80)]);
    }
    assert_eq!(
        rows(&client.batch("SELECT 1")),
        vec![vec![Some("1".to_owned())]]
    );
}
