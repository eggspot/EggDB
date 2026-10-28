//! Hostile input for the PostgreSQL protocol (D9, D35): a seeded fuzzer that mutates valid
//! messages and feeds them to every parser and to the SCRAM exchange, which must answer with an
//! error or a value and never panic; and the same mutations sent over real sockets, before and
//! after authentication, after which the server must still serve a well-behaved client.
//! `EGGDB_FUZZ_CASES` and `EGGDB_FUZZ_SEED` run longer campaigns.

mod common;

use common::{Client, DATABASE, TestServer, USER, cstring, tags};
use eggdb_pgwire::messages::{parse_message, parse_sasl_initial_response, parse_startup};
use eggdb_pgwire::{ScramExchange, ScramVerifier};
use std::time::{Duration, Instant};

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

fn seeds() -> Vec<(u8, Vec<u8>)> {
    let mut bind = cstring("p");
    bind.extend(cstring("s"));
    bind.extend_from_slice(&1i16.to_be_bytes());
    bind.extend_from_slice(&1i16.to_be_bytes());
    bind.extend_from_slice(&2i16.to_be_bytes());
    bind.extend_from_slice(&4i32.to_be_bytes());
    bind.extend_from_slice(&7i32.to_be_bytes());
    bind.extend_from_slice(&(-1i32).to_be_bytes());
    bind.extend_from_slice(&1i16.to_be_bytes());
    bind.extend_from_slice(&1i16.to_be_bytes());
    let mut parse = cstring("s");
    parse.extend(cstring("SELECT $1, $2"));
    parse.extend_from_slice(&2i16.to_be_bytes());
    parse.extend_from_slice(&23u32.to_be_bytes());
    parse.extend_from_slice(&25u32.to_be_bytes());
    let mut execute = cstring("p");
    execute.extend_from_slice(&10i32.to_be_bytes());
    let mut sasl = cstring("SCRAM-SHA-256");
    sasl.extend_from_slice(&20i32.to_be_bytes());
    sasl.extend_from_slice(b"n,,n=,r=abcdefghijkl");
    vec![
        (b'Q', cstring("SELECT 1; SELECT 'x'")),
        (b'P', parse),
        (b'B', bind),
        (b'D', b"Ss\0".to_vec()),
        (b'D', b"Pp\0".to_vec()),
        (b'E', execute),
        (b'C', b"Ss\0".to_vec()),
        (b'S', Vec::new()),
        (b'H', Vec::new()),
        (b'p', sasl),
        (
            b'Q',
            cstring("CREATE TABLE IF NOT EXISTS h (a integer, b text)"),
        ),
        (b'Q', cstring("COPY h FROM STDIN")),
        (b'Q', cstring("COPY h TO STDOUT (FORMAT csv, HEADER)")),
        (b'd', b"1\tx\n2\t\\N\n".to_vec()),
        (b'c', Vec::new()),
        (b'f', cstring("gave up")),
    ]
}

fn startup_seed() -> Vec<u8> {
    let mut body = 196_608u32.to_be_bytes().to_vec();
    for text in ["user", USER, "database", DATABASE, "options", "-c x=y"] {
        body.extend(cstring(text));
    }
    body.push(0);
    body
}

fn mutate(rng: &mut Rng, bytes: &mut Vec<u8>) {
    for _ in 0..=rng.below(3) {
        match rng.below(7) {
            0 if !bytes.is_empty() => {
                let at = rng.below(bytes.len());
                bytes[at] ^= 1 << rng.below(8);
            }
            1 => bytes.truncate(rng.below(bytes.len() + 1)),
            2 => {
                let at = rng.below(bytes.len() + 1);
                let noise: Vec<u8> = (0..rng.below(16)).map(|_| rng.next() as u8).collect();
                bytes.splice(at..at, noise);
            }
            3 if bytes.len() >= 4 => {
                let at = rng.below(bytes.len() - 3);
                let value = [0u32, 1, 0x7fff_ffff, 0xffff_ffff, 0x8000_0000, 65_535][rng.below(6)];
                bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
            }
            4 if bytes.len() >= 2 => {
                let at = rng.below(bytes.len() - 1);
                let value = [0u16, 0xffff, 0x7fff, 0x8000, 1, 2][rng.below(6)];
                bytes[at..at + 2].copy_from_slice(&value.to_be_bytes());
            }
            5 if !bytes.is_empty() => {
                let at = rng.below(bytes.len());
                bytes.remove(at);
            }
            _ => {
                let length = rng.below(64);
                *bytes = (0..length).map(|_| rng.next() as u8).collect();
            }
        }
    }
}

#[test]
fn parsers_and_scram_never_panic_on_mutated_messages() {
    let cases = setting("EGGDB_FUZZ_CASES", 20_000);
    let seed = setting("EGGDB_FUZZ_SEED", 0x5eed_2026);
    let mut rng = Rng(seed);
    let seeds = seeds();
    let verifier = ScramVerifier::derive(b"pw", b"0123456789abcdef", 4096);
    let started = Instant::now();
    for case in 0..cases {
        let (tag, body) = &seeds[rng.below(seeds.len())];
        let mut tag = *tag;
        let mut body = body.clone();
        mutate(&mut rng, &mut body);
        if rng.below(10) == 0 {
            tag = rng.next() as u8;
        }
        let _ = parse_message(tag, &body);
        let _ = parse_sasl_initial_response(&body);
        let mut startup = startup_seed();
        mutate(&mut rng, &mut startup);
        let _ = parse_startup(&startup);
        let mut exchange = ScramExchange::new(verifier.clone(), &case.to_be_bytes());
        let mut first = b"n,,n=,r=abcdefghijkl".to_vec();
        mutate(&mut rng, &mut first);
        if let Ok(server_first) = exchange.client_first(&first) {
            let nonce = server_first
                .split(',')
                .next()
                .unwrap_or("")
                .trim_start_matches("r=")
                .to_owned();
            let mut last =
                format!("c=biws,r={nonce},p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=")
                    .into_bytes();
            mutate(&mut rng, &mut last);
            assert!(
                exchange.client_final(&last).is_err(),
                "case {case}, seed {seed}"
            );
        }
    }
    assert!(started.elapsed() < Duration::from_secs(120));
}

fn framed(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![tag];
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fn drain(client: &mut Client, limit: Duration) {
    let deadline = Instant::now() + limit;
    if client
        .stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .is_err()
    {
        return;
    }
    let mut buffer = [0u8; 4096];
    while Instant::now() < deadline {
        match std::io::Read::read(&mut client.stream, &mut buffer) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return,
        }
    }
}

#[test]
fn hostile_clients_over_sockets_leave_the_server_serving() {
    let cases = setting("EGGDB_FUZZ_CASES", 20_000) / 250;
    let seed = setting("EGGDB_FUZZ_SEED", 0x5eed_2026);
    let server = TestServer::start_with(16, Duration::from_millis(400));
    let mut rng = Rng(seed ^ 0xface);
    let seeds = seeds();
    for _ in 0..cases {
        if rng.below(2) == 0 {
            let mut client = Client::connect(server.address);
            let mut startup = startup_seed();
            mutate(&mut rng, &mut startup);
            let mut packet = ((startup.len() + 4) as u32).to_be_bytes().to_vec();
            packet.extend(startup);
            mutate(&mut rng, &mut packet);
            client.send_raw(&packet);
            drain(&mut client, Duration::from_secs(2));
        } else {
            let (mut client, _) = Client::login(server.address);
            let mut stream = Vec::new();
            for _ in 0..=rng.below(4) {
                let (tag, body) = &seeds[rng.below(seeds.len())];
                let mut body = body.clone();
                if rng.below(2) == 0 {
                    mutate(&mut rng, &mut body);
                }
                stream.extend(framed(*tag, &body));
            }
            if rng.below(3) == 0 {
                mutate(&mut rng, &mut stream);
            }
            client.send_raw(&stream);
            client.send_raw(&framed(b'S', &[]));
            client.send_raw(&framed(b'X', &[]));
            drain(&mut client, Duration::from_secs(2));
        }
    }
    let (mut client, _) = Client::login(server.address);
    assert_eq!(tags(&client.query("SELECT 1")), "TDCZ");
}

#[test]
fn hostile_messages_inside_a_copy_leave_the_server_serving() {
    let cases = (setting("EGGDB_FUZZ_CASES", 20_000) / 500).max(20);
    let seed = setting("EGGDB_FUZZ_SEED", 0x5eed_2026);
    let server = TestServer::start_with(16, Duration::from_millis(400));
    let (mut setup, _) = Client::login(server.address);
    assert_eq!(
        tags(&setup.query("CREATE TABLE h (a integer PRIMARY KEY, b text)")),
        "CZ"
    );
    let mut rng = Rng(seed ^ 0xc0b1);
    let starts = [
        "COPY h FROM STDIN",
        "COPY h FROM STDIN (FORMAT csv, HEADER match)",
        "COPY h (b, a) FROM STDIN (DELIMITER ',', NULL '')",
    ];
    let rows = [
        &b"1\tx\n"[..],
        b"2\t\\N\n3\ty",
        b"4,\"q\"\"\"\n",
        b"\\.\n",
        b"a,b\n",
    ];
    for _ in 0..cases {
        let (mut client, _) = Client::login(server.address);
        let mut stream = framed(b'Q', &cstring(starts[rng.below(starts.len())]));
        for _ in 0..=rng.below(6) {
            let (tag, mut body) = match rng.below(6) {
                0 | 1 => (b'd', rows[rng.below(rows.len())].to_vec()),
                2 => (b'c', Vec::new()),
                3 => (b'f', cstring("stop")),
                4 => (b"HSQXP"[rng.below(5)], Vec::new()),
                _ => (rng.next() as u8, Vec::new()),
            };
            if rng.below(3) == 0 {
                mutate(&mut rng, &mut body);
            }
            stream.extend(framed(tag, &body));
        }
        if rng.below(4) == 0 {
            mutate(&mut rng, &mut stream);
        }
        client.send_raw(&stream);
        client.send_raw(&framed(b'c', &[]));
        client.send_raw(&framed(b'S', &[]));
        client.send_raw(&framed(b'X', &[]));
        drain(&mut client, Duration::from_secs(2));
    }
    let (mut client, _) = Client::login(server.address);
    assert_eq!(tags(&client.query("SELECT 1")), "TDCZ");
    let copied = client.query("COPY h TO STDOUT");
    assert_eq!(copied.last().map(|message| message.tag), Some(b'Z'));
    assert_eq!(copied[0].tag, b'H');
}

#[test]
fn oversized_lengths_are_refused_without_allocating_them() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    client.send_raw(&0x7fff_ffffu32.to_be_bytes());
    let refused = client.read().unwrap();
    assert_eq!(refused.code(), "08P01");
    let (mut client, _) = Client::login(server.address);
    let mut header = vec![b'Q'];
    header.extend_from_slice(&0xffff_fff0u32.to_be_bytes());
    client.send_raw(&header);
    let refused = client.read().unwrap();
    assert_eq!(refused.code(), "08P01");
    assert_eq!(client.read(), None);
    let mut client = Client::connect(server.address);
    client.send_startup(196_608, &[("user", USER)]);
    let _ = client.read();
    client.send_raw(&framed(b'p', &vec![b'x'; 64 * 1024]));
    let refused = client.read().unwrap();
    assert_eq!(refused.code(), "08P01");
    let (mut client, _) = Client::login(server.address);
    assert_eq!(tags(&client.query("SELECT 1")), "TDCZ");
}
