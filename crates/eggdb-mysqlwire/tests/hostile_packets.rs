//! Hostile input for the MySQL protocol (D9, D35): a seeded fuzzer that mutates valid payloads
//! and feeds them to every parser and to both password checks, which must answer with an error
//! or a value and never panic; and the same mutations sent over real sockets before and after
//! authentication, with oversized lengths, truncated handshake responses and wrong sequence
//! ids, after which the server must still serve a well-behaved client.
//! `EGGDB_FUZZ_CASES` and `EGGDB_FUZZ_SEED` run longer campaigns.

mod common;

use common::{Client, DATABASE, PASSWORD, Reply, TestServer, USER, handshake_response, texts};
use eggdb_mysqlwire::packets::{parse_command, parse_execute, parse_handshake_response};
use eggdb_mysqlwire::{MySqlCredentials, NONCE_BYTES, caching_sha2_response};
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

fn command_seeds() -> Vec<Vec<u8>> {
    let mut execute = vec![0x17, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0b10, 1, 8, 0, 253, 0, 3, 0];
    execute.extend_from_slice(&5i64.to_le_bytes());
    execute.extend_from_slice(b"\x02hi");
    let mut long = vec![0x18, 1, 0, 0, 0, 0, 0];
    long.extend_from_slice(b"data");
    vec![
        b"\x03SELECT 1; SELECT 'x'".to_vec(),
        b"\x16SELECT ? + 1".to_vec(),
        execute,
        long,
        vec![0x19, 1, 0, 0, 0],
        vec![0x1A, 1, 0, 0, 0],
        vec![0x1B, 0, 0],
        vec![0x0E],
        b"\x02shop".to_vec(),
        b"\x04items\x00%".to_vec(),
    ]
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
                let value =
                    [0u32, 1, 0x7fff_ffff, 0xffff_ffff, 0x8000_0000, 0xff_ffff][rng.below(6)];
                bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
            }
            4 if !bytes.is_empty() => {
                let at = rng.below(bytes.len());
                bytes[at] = [0xFB, 0xFC, 0xFD, 0xFE, 0xFF, 0][rng.below(6)];
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
fn parsers_and_password_checks_never_panic_on_mutated_payloads() {
    let cases = setting("EGGDB_FUZZ_CASES", 20_000);
    let seed = setting("EGGDB_FUZZ_SEED", 0x5eed_2026);
    let mut rng = Rng(seed);
    let seeds = command_seeds();
    let credentials = MySqlCredentials::derive(b"pw");
    let nonce = [7u8; NONCE_BYTES];
    let login = handshake_response(
        USER,
        &caching_sha2_response(b"pw", &nonce),
        DATABASE,
        "caching_sha2_password",
    );
    let started = Instant::now();
    for _ in 0..cases {
        let mut command = seeds[rng.below(seeds.len())].clone();
        mutate(&mut rng, &mut command);
        let _ = parse_command(&command);
        let count = rng.below(70);
        let _ = parse_execute(command.get(5..).unwrap_or(&[]), count, &[], &[]);
        let _ = parse_execute(
            command.get(5..).unwrap_or(&[]),
            count,
            &[(8, false); 3],
            &[true],
        );
        let mut response = login.clone();
        mutate(&mut rng, &mut response);
        if let Ok(parsed) = parse_handshake_response(&response) {
            let _ = credentials.check_caching_sha2(&nonce, &parsed.auth_response);
            let _ = credentials.check_native(&nonce, &parsed.auth_response);
        }
    }
    assert!(started.elapsed() < Duration::from_secs(120));
}

fn framed(sequence: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = (payload.len() as u32).to_le_bytes()[..3].to_vec();
    bytes.push(sequence);
    bytes.extend_from_slice(payload);
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
    let seeds = command_seeds();
    for _ in 0..cases {
        if rng.below(2) == 0 {
            let mut client = Client::connect(server.address);
            let handshake = client.handshake();
            let response = caching_sha2_response(PASSWORD.as_bytes(), &handshake.nonce);
            let mut login = handshake_response(USER, &response, DATABASE, "caching_sha2_password");
            mutate(&mut rng, &mut login);
            let mut packet = framed(1, &login);
            if rng.below(3) == 0 {
                mutate(&mut rng, &mut packet);
            }
            client.send_raw(&packet);
            drain(&mut client, Duration::from_secs(2));
        } else {
            let mut client = Client::login(server.address);
            let mut stream = Vec::new();
            for _ in 0..=rng.below(4) {
                let mut payload = seeds[rng.below(seeds.len())].clone();
                if rng.below(2) == 0 {
                    mutate(&mut rng, &mut payload);
                }
                stream.extend(framed(0, &payload));
            }
            if rng.below(3) == 0 {
                mutate(&mut rng, &mut stream);
            }
            client.send_raw(&stream);
            client.send_raw(&framed(0, &[0x01]));
            drain(&mut client, Duration::from_secs(2));
        }
    }
    let mut client = Client::login(server.address);
    assert_eq!(texts(&client.query("SELECT 1")), vec![vec!["1".to_owned()]]);
}

#[test]
fn oversized_lengths_are_refused_without_allocating_them() {
    let server = TestServer::start(8);
    let mut client = Client::connect(server.address);
    client.handshake();
    client.send_raw(&framed(1, &vec![b'x'; 20 * 1024]));
    let refused = client.read().unwrap();
    assert!(matches!(
        client.parse_reply(refused),
        Reply::Err { code: 1153, .. }
    ));
    let mut client = Client::login(server.address);
    let frame = vec![0u8; 0xFF_FFFF];
    for sequence in 0..4 {
        client.send_raw(&[0xFF, 0xFF, 0xFF, sequence]);
        client.send_raw(&frame);
    }
    client.send_raw(&[0xFF, 0xFF, 0xFF, 4]);
    let refused = client.read().unwrap();
    assert!(matches!(
        client.parse_reply(refused),
        Reply::Err { code: 1153, .. }
    ));
    let mut client = Client::login(server.address);
    assert_eq!(texts(&client.query("SELECT 2")), vec![vec!["2".to_owned()]]);
}

#[test]
fn truncated_handshakes_time_out_and_wrong_sequence_ids_are_refused() {
    let server = TestServer::start_with(8, Duration::from_millis(500));
    let mut slow = Client::connect(server.address);
    slow.handshake();
    slow.send_raw(&[40, 0, 0, 1, 0x00, 0x02]);
    let started = Instant::now();
    let refused = slow.read().unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(matches!(
        slow.parse_reply(refused),
        Reply::Err { code: 1043, .. }
    ));
    let mut client = Client::connect(server.address);
    let handshake = client.handshake();
    let response = caching_sha2_response(PASSWORD.as_bytes(), &handshake.nonce);
    client.send_raw(&framed(
        5,
        &handshake_response(USER, &response, DATABASE, "caching_sha2_password"),
    ));
    let refused = client.read().unwrap();
    assert!(matches!(
        client.parse_reply(refused),
        Reply::Err { code: 1156, .. }
    ));
    let mut client = Client::login(server.address);
    client.send_raw(&framed(3, b"\x03SELECT 1"));
    let refused = client.read().unwrap();
    assert!(matches!(
        client.parse_reply(refused),
        Reply::Err { code: 1156, .. }
    ));
    assert_eq!(client.read(), None);
    let mut client = Client::connect(server.address);
    client.handshake();
    client.send_raw(&framed(1, &[0x00, 0x02, 0x00, 0x00]));
    let refused = client.read().unwrap();
    assert!(matches!(
        client.parse_reply(refused),
        Reply::Err { code: 1043, .. }
    ));
    let mut client = Client::login(server.address);
    assert_eq!(texts(&client.query("SELECT 3")), vec![vec!["3".to_owned()]]);
}
