use super::*;
use crate::content_coding::decode_request_body;
use eggdb_content::crc32;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::AtomicBool;

const PLAIN_TEXT: &[u8] = b"hello hello hello, gzip and deflate in EggDB\n";
const ZLIB_TEXT: [u8; 44] = [
    0x78, 0xda, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x3a, 0x0a, 0xe9, 0x55, 0x99,
    0x05, 0x0a, 0x89, 0x79, 0x29, 0x0a, 0x29, 0xa9, 0x69, 0x39, 0x89, 0x25, 0xa9, 0x0a, 0x99, 0x79,
    0x0a, 0xae, 0xe9, 0xe9, 0x2e, 0x4e, 0x5c, 0x00, 0x76, 0xbc, 0x0f, 0x85,
];

fn stored_deflate(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let blocks: Vec<&[u8]> = if data.is_empty() {
        vec![data]
    } else {
        data.chunks(0xffff).collect()
    };
    let last = blocks.len() - 1;
    for (index, block) in blocks.into_iter().enumerate() {
        let length = block.len() as u16;
        out.push(u8::from(index == last));
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&(!length).to_le_bytes());
        out.extend_from_slice(block);
    }
    out
}

fn gzip_member(deflated: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut member = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    member.extend_from_slice(deflated);
    member.extend_from_slice(&crc32(plain).to_le_bytes());
    member.extend_from_slice(&(plain.len() as u32).to_le_bytes());
    member
}

fn gzip(plain: &[u8]) -> Vec<u8> {
    gzip_member(&stored_deflate(plain), plain)
}

struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn bit(&mut self, set: bool) {
        if self.bits % 8 == 0 {
            self.bytes.push(0);
        }
        if set {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << (self.bits % 8);
        }
        self.bits += 1;
    }

    fn value(&mut self, value: u32, count: u32) {
        for at in 0..count {
            self.bit((value >> at) & 1 == 1);
        }
    }

    fn huffman_code(&mut self, code: u32, length: u32) {
        for at in (0..length).rev() {
            self.bit((code >> at) & 1 == 1);
        }
    }
}

fn zeros_deflate(copies: usize) -> Vec<u8> {
    let mut out = BitWriter {
        bytes: Vec::new(),
        bits: 0,
    };
    out.value(1, 1);
    out.value(1, 2);
    out.huffman_code(0x30, 8);
    for _ in 0..copies {
        out.huffman_code(0b1100_0101, 8);
        out.huffman_code(0, 5);
    }
    out.huffman_code(0, 7);
    out.bytes
}

fn zeros_gzip(copies: usize) -> Vec<u8> {
    gzip_member(&zeros_deflate(copies), &vec![0u8; 1 + 258 * copies])
}

fn echo(request: &Request) -> Response {
    let seen = |name: &str| request.header(name).unwrap_or("none").to_owned();
    Response::ok("application/octet-stream", request.body.clone())
        .with_header("X-Seen-Content-Encoding", &seen("content-encoding"))
        .with_header("X-Seen-Content-Length", &seen("content-length"))
        .with_header("X-Seen-Transfer-Encoding", &seen("transfer-encoding"))
}

fn with_server(limits: &Limits<'_>, test: impl FnOnce(SocketAddr)) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stopped = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            serve_with_limits(
                &listener,
                &echo,
                &|| stopped.load(Ordering::Acquire),
                limits,
            )
            .unwrap();
        });
        test(address);
        stopped.store(true, Ordering::Release);
    });
}

struct Answer {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.split("\r\n").skip(1).find_map(|line| {
            let (field, value) = line.split_once(':')?;
            field.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }
}

fn post(address: SocketAddr, path: &str, headers: &str, body: &[u8]) -> Answer {
    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
        body.len()
    );
    let _ = client.write_all(head.as_bytes());
    let _ = client.write_all(body);
    let mut answer = Vec::new();
    let _ = client.read_to_end(&mut answer);
    let head_end = answer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap()
        + 4;
    let head = String::from_utf8_lossy(&answer[..head_end]).into_owned();
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Answer {
        status,
        head,
        body: answer[head_end..].to_vec(),
    }
}

#[test]
fn gzip_and_deflate_bodies_reach_the_handler_decoded() {
    let budget = MemoryBudget::default();
    let limits = Limits::new(MAX_BODY_BYTES, &budget).decoding_content_encoding();
    let gzipped = gzip(PLAIN_TEXT);
    let raw = stored_deflate(PLAIN_TEXT);
    let two_members = [gzip(b"first "), gzip(b"second")].concat();
    let stacked = gzip(&ZLIB_TEXT);
    with_server(&limits, |address| {
        for (headers, body, expected) in [
            ("Content-Encoding: gzip\r\n", &gzipped[..], PLAIN_TEXT),
            ("Content-Encoding: X-GZIP\r\n", &gzipped[..], PLAIN_TEXT),
            ("Content-Encoding: deflate\r\n", &ZLIB_TEXT[..], PLAIN_TEXT),
            ("Content-Encoding: deflate\r\n", &raw[..], PLAIN_TEXT),
            ("Content-Encoding: identity\r\n", PLAIN_TEXT, PLAIN_TEXT),
            (
                "Content-Encoding: gzip\r\n",
                &two_members[..],
                &b"first second"[..],
            ),
            (
                "Content-Encoding: deflate, gzip\r\n",
                &stacked[..],
                PLAIN_TEXT,
            ),
            (
                "Content-Encoding: deflate\r\nContent-Encoding: identity, gzip\r\n",
                &stacked[..],
                PLAIN_TEXT,
            ),
        ] {
            let answer = post(address, "/echo", headers, body);
            assert_eq!(answer.status, 200, "{headers}");
            assert_eq!(answer.body, expected, "{headers}");
            assert_eq!(answer.header("x-seen-content-encoding"), Some("none"));
            assert_eq!(
                answer.header("x-seen-content-length"),
                Some(expected.len().to_string().as_str())
            );
        }
        let mut client = TcpStream::connect(address).unwrap();
        let mut chunked = b"POST /c HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        chunked.extend_from_slice(format!("{:x}\r\n", gzipped.len()).as_bytes());
        chunked.extend_from_slice(&gzipped);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
        client.write_all(&chunked).unwrap();
        let mut answer = String::new();
        client.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 200 "), "{answer}");
        assert!(
            answer.contains("X-Seen-Transfer-Encoding: none\r\n"),
            "{answer}"
        );
        assert!(answer.ends_with("in EggDB\n"), "{answer}");
    });
    assert_eq!(budget.used_bytes(), 0);
}

#[test]
fn unknown_and_damaged_codings_are_refused() {
    let budget = MemoryBudget::default();
    let limits = Limits::new(MAX_BODY_BYTES, &budget).decoding_content_encoding();
    let gzipped = gzip(PLAIN_TEXT);
    let mut damaged = gzipped.clone();
    let crc_at = damaged.len() - 8;
    damaged[crc_at] ^= 0xff;
    with_server(&limits, |address| {
        for coding in [
            "br",
            "gzip, br",
            "compress",
            "zstd",
            "gzip, gzip, gzip, gzip, gzip",
        ] {
            let answer = post(
                address,
                "/echo",
                &format!("Content-Encoding: {coding}\r\n"),
                &gzipped,
            );
            assert_eq!(answer.status, 415, "{coding}");
            assert_eq!(answer.header("accept-encoding"), Some("gzip, deflate"));
            assert_eq!(answer.header("connection"), Some("close"));
        }
        for (coding, body) in [
            ("gzip", &damaged[..]),
            ("gzip", PLAIN_TEXT),
            ("deflate", PLAIN_TEXT),
            ("deflate, gzip", &gzipped[..]),
        ] {
            let answer = post(
                address,
                "/echo",
                &format!("Content-Encoding: {coding}\r\n"),
                body,
            );
            assert_eq!(answer.status, 400, "{coding}");
        }
    });
    assert_eq!(budget.used_bytes(), 0);
}

#[test]
fn a_zip_bomb_is_refused_with_413_and_the_budget_bounds_what_decodes() {
    let budget = MemoryBudget::default();
    let limits = Limits::new(MAX_BODY_BYTES, &budget).decoding_content_encoding();
    let bomb = zeros_gzip(40_000);
    assert!(bomb.len() < 70_000);
    let fitting = zeros_gzip(1_000);
    with_server(&limits, |address| {
        let refused = post(address, "/echo", "Content-Encoding: gzip\r\n", &bomb);
        assert_eq!(refused.status, 413);
        let nested = post(
            address,
            "/echo",
            "Content-Encoding: gzip, gzip\r\n",
            &gzip(&bomb),
        );
        assert_eq!(nested.status, 413);
        let served = post(address, "/echo", "Content-Encoding: gzip\r\n", &fitting);
        assert_eq!(served.status, 200);
        assert_eq!(served.body.len(), 258_001);
        assert!(served.body.iter().all(|&byte| byte == 0));
    });
    assert_eq!(budget.used_bytes(), 0);
    let small = MemoryBudget::new(200_000);
    let limits = Limits::new(MAX_BODY_BYTES, &small).decoding_content_encoding();
    with_server(&limits, |address| {
        let refused = post(address, "/echo", "Content-Encoding: gzip\r\n", &fitting);
        assert_eq!(refused.status, 503);
        assert_eq!(refused.header("retry-after"), Some("1"));
    });
    assert_eq!(small.used_bytes(), 0);
}

#[test]
fn bodies_stay_encoded_where_the_listener_does_not_decode() {
    let budget = MemoryBudget::default();
    let gzipped = gzip(PLAIN_TEXT);
    let off = Limits::new(MAX_BODY_BYTES, &budget);
    with_server(&off, |address| {
        let answer = post(address, "/echo", "Content-Encoding: br\r\n", &gzipped);
        assert_eq!(answer.status, 200);
        assert_eq!(answer.body, gzipped);
        assert_eq!(answer.header("x-seen-content-encoding"), Some("br"));
    });
    let some = Limits::new(MAX_BODY_BYTES, &budget)
        .decoding_content_encoding_of(|request| request.path != "/raw");
    with_server(&some, |address| {
        let raw = post(address, "/raw", "Content-Encoding: gzip\r\n", &gzipped);
        assert_eq!(raw.header("x-seen-content-encoding"), Some("gzip"));
        assert_eq!(raw.body, gzipped);
        let decoded = post(address, "/echo", "Content-Encoding: gzip\r\n", &gzipped);
        assert_eq!(decoded.body, PLAIN_TEXT);
    });
}

#[test]
fn mutated_gzip_bodies_decode_or_fail_and_never_panic() {
    let budget = MemoryBudget::new(1 << 22);
    let valid = [gzip(PLAIN_TEXT), zeros_gzip(200)].concat();
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..20_000 {
        let mut body = valid.clone();
        for _ in 0..=(next() % 6) {
            if body.is_empty() {
                break;
            }
            let at = (next() % body.len() as u64) as usize;
            match next() % 4 {
                0 => body[at] ^= 1 << (next() % 8),
                1 => body[at] = next() as u8,
                2 => body.truncate(at),
                _ => body.insert(at, next() as u8),
            }
        }
        let coding = ["gzip", "deflate", "deflate, gzip", "x-gzip"][(next() % 4) as usize];
        let mut request = Request {
            method: "POST".to_owned(),
            path: "/".to_owned(),
            query: None,
            headers: vec![("content-encoding".to_owned(), coding.to_owned())],
            body,
        };
        let mut held = budget.empty_reservation();
        let decoded = decode_request_body(&mut request, 1 << 16, &mut held, &budget);
        if decoded.is_ok() {
            assert!(request.body.len() <= 1 << 16);
            assert_eq!(request.header("content-encoding"), None);
        }
        drop(held);
        assert_eq!(budget.used_bytes(), 0);
    }
}
