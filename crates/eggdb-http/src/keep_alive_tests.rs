use super::*;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

struct Answer {
    head: String,
    body: Vec<u8>,
}

impl Answer {
    fn status(&self) -> u16 {
        self.head
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0)
    }

    fn header(&self, name: &str) -> Option<String> {
        self.head.split("\r\n").skip(1).find_map(|line| {
            let (field, value) = line.split_once(':')?;
            field
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn read_answer(reader: &mut BufReader<TcpStream>, head_only: bool) -> Answer {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0, "closed: {head}");
        head.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    let mut answer = Answer {
        head,
        body: Vec::new(),
    };
    if head_only {
        return answer;
    }
    if answer
        .header("transfer-encoding")
        .is_some_and(|coding| coding == "chunked")
    {
        loop {
            let mut size_line = String::new();
            reader.read_line(&mut size_line).unwrap();
            let size = usize::from_str_radix(size_line.trim_end(), 16).unwrap();
            let mut chunk = vec![0u8; size + 2];
            reader.read_exact(&mut chunk).unwrap();
            assert_eq!(&chunk[size..], b"\r\n");
            if size == 0 {
                break;
            }
            answer.body.extend_from_slice(&chunk[..size]);
        }
    } else if let Some(length) = answer.header("content-length") {
        let mut body = vec![0u8; length.parse().unwrap()];
        reader.read_exact(&mut body).unwrap();
        answer.body = body;
    } else {
        reader.read_to_end(&mut answer.body).unwrap();
    }
    answer
}

fn closed(reader: &mut BufReader<TcpStream>) -> bool {
    let mut rest = Vec::new();
    matches!(reader.read_to_end(&mut rest), Ok(0))
}

fn connect(address: SocketAddr) -> (TcpStream, BufReader<TcpStream>) {
    let client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let reader = BufReader::new(client.try_clone().unwrap());
    (client, reader)
}

fn streamed_rows(rows: usize) -> Reply<'static> {
    StreamedResponse::new(200, "application/x-ndjson", move |out| {
        for row in 0..rows {
            writeln!(out, "{{\"row\":{row}}}")?;
        }
        Ok(())
    })
    .with_header("Content-Length", "1")
    .into()
}

fn handler(request: &Request) -> Reply<'static> {
    match request.path.as_str() {
        "/rows" => streamed_rows(5_000),
        "/held" => StreamedResponse::new(200, "text/plain", |out| out.write_all(b"held"))
            .with_held_bytes(1_000_000)
            .into(),
        "/large" => Response::ok("text/plain", vec![b'x'; 200_000]).into(),
        _ => Response::ok(
            "text/plain",
            format!(
                "{} {} {}",
                request.method,
                request.path,
                String::from_utf8_lossy(&request.body)
            ),
        )
        .into(),
    }
}

fn with_server(limits: &Limits<'_>, test: impl FnOnce(SocketAddr)) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stopped = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            serve_replies_with_limits(
                &listener,
                &handler,
                &|| stopped.load(Ordering::Acquire),
                limits,
            )
            .unwrap();
        });
        test(address);
        stopped.store(true, Ordering::Release);
    });
}

fn default_limits(budget: &MemoryBudget) -> Limits<'_> {
    Limits::new(MAX_BODY_BYTES, budget)
}

#[test]
fn two_requests_are_answered_on_one_connection() {
    let budget = MemoryBudget::default();
    with_server(&default_limits(&budget), |address| {
        let (mut client, mut reader) = connect(address);
        client
            .write_all(b"GET /first HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let first = read_answer(&mut reader, false);
        assert_eq!(
            (first.status(), first.text()),
            (200, "GET /first ".to_owned())
        );
        assert_eq!(first.header("connection"), None);
        client
            .write_all(b"POST /second HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\nabc")
            .unwrap();
        let second = read_answer(&mut reader, false);
        assert_eq!(second.text(), "POST /second abc");
        client
            .write_all(b"HEAD /third HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let third = read_answer(&mut reader, true);
        assert_eq!(third.header("content-length").as_deref(), Some("12"));
        client
            .write_all(b"GET /fourth HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let fourth = read_answer(&mut reader, false);
        assert_eq!(fourth.text(), "GET /fourth ");
        assert_eq!(fourth.header("connection").as_deref(), Some("close"));
        assert!(closed(&mut reader));
    });
}

#[test]
fn pipelined_requests_are_answered_in_order() {
    let budget = MemoryBudget::default();
    with_server(&default_limits(&budget), |address| {
        let (mut client, mut reader) = connect(address);
        client
            .write_all(
                b"GET /a HTTP/1.1\r\n\r\nPOST /b HTTP/1.1\r\nContent-Length: 2\r\n\r\nhi\r\nPOST /c HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\nGET /d HTTP/1.1\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        let answers: Vec<String> = (0..4)
            .map(|_| read_answer(&mut reader, false).text())
            .collect();
        assert_eq!(answers, ["GET /a ", "POST /b hi", "POST /c abc", "GET /d "]);
        assert!(closed(&mut reader));
    });
}

#[test]
fn chunked_uploads_ignore_extensions_and_discard_trailers() {
    let request = read_request(
        &mut &b"POST /upload HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5;name=value;flag\r\nhello\r\n6 ; x=\"quoted\"\r\n world\r\n000\r\nExpires: never\r\nX-Checksum: 1\r\n\r\n"[..],
    )
    .unwrap();
    assert_eq!(request.body, b"hello world");
    assert_eq!(request.header("expires"), None);
    let mut bytes = b"PUT /k HTTP/1.1\r\nTransfer-Encoding: Chunked\r\n\r\n".to_vec();
    for _ in 0..10 {
        bytes.extend_from_slice(b"2710\r\n");
        bytes.extend_from_slice(&[b'z'; 10_000]);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"0\r\n\r\n");
    let large = read_request(&mut &bytes[..]).unwrap();
    assert_eq!(large.body.len(), 100_000);
    let budget = MemoryBudget::default();
    with_server(&default_limits(&budget), |address| {
        let (mut client, mut reader) = connect(address);
        client
            .write_all(
                b"POST /up HTTP/1.1\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n",
            )
            .unwrap();
        let interim = read_answer(&mut reader, true);
        assert_eq!(interim.status(), 100);
        client.write_all(b"4\r\nwiki\r\n").unwrap();
        client.write_all(b"5\r\npedia\r\n0\r\n\r\n").unwrap();
        assert_eq!(read_answer(&mut reader, false).text(), "POST /up wikipedia");
    });
}

#[test]
fn streamed_responses_are_sent_chunked_and_the_connection_goes_on() {
    let budget = MemoryBudget::new(100_000);
    with_server(&default_limits(&budget), |address| {
        let (mut client, mut reader) = connect(address);
        client.write_all(b"GET /rows HTTP/1.1\r\n\r\n").unwrap();
        let rows = read_answer(&mut reader, false);
        assert_eq!(rows.header("transfer-encoding").as_deref(), Some("chunked"));
        assert_eq!(rows.header("content-length"), None);
        let text = rows.text();
        assert_eq!(text.lines().count(), 5_000);
        assert!(text.starts_with("{\"row\":0}\n{\"row\":1}\n"));
        assert!(text.ends_with("{\"row\":4999}\n"));
        assert!(rows.body.len() > STREAM_CHUNK_BYTES);
        client.write_all(b"HEAD /rows HTTP/1.1\r\n\r\n").unwrap();
        let head = read_answer(&mut reader, true);
        assert_eq!(head.header("transfer-encoding").as_deref(), Some("chunked"));
        client.write_all(b"GET /after HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(read_answer(&mut reader, false).text(), "GET /after ");
        let (mut old, mut old_reader) = connect(address);
        old.write_all(b"GET /rows HTTP/1.0\r\nConnection: keep-alive\r\n\r\n")
            .unwrap();
        let plain = read_answer(&mut old_reader, false);
        assert_eq!(plain.header("transfer-encoding"), None);
        assert_eq!(plain.header("connection").as_deref(), Some("close"));
        assert_eq!(plain.text().lines().count(), 5_000);
    });
    assert_eq!(budget.used_bytes(), 0);
    let collected = streamed_rows(2).into_response();
    assert_eq!(collected.body, b"{\"row\":0}\n{\"row\":1}\n");
}

#[test]
fn http_1_0_keeps_its_connection_only_when_it_asks() {
    let budget = MemoryBudget::default();
    with_server(&default_limits(&budget), |address| {
        let (mut client, mut reader) = connect(address);
        client
            .write_all(b"GET /a HTTP/1.0\r\nConnection: Keep-Alive\r\n\r\n")
            .unwrap();
        let kept = read_answer(&mut reader, false);
        assert_eq!(kept.header("connection").as_deref(), Some("keep-alive"));
        client.write_all(b"GET /b HTTP/1.0\r\n\r\n").unwrap();
        let last = read_answer(&mut reader, false);
        assert_eq!(
            (last.text(), last.header("connection")),
            ("GET /b ".to_owned(), Some("close".to_owned()))
        );
        assert!(closed(&mut reader));
    });
}

#[test]
fn smuggling_attempts_are_refused_and_their_connection_closed() {
    let budget = MemoryBudget::default();
    with_server(&default_limits(&budget), |address| {
        for (attempt, status) in [
            (&b"POST /x HTTP/1.1\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n"[..], 400),
            (b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 4\r\n\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 501),
            (b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked, chunked\r\n\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: identity\r\n\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nTransfer-Encoding:\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\nhiGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nContent-Length: 2, 2\r\n\r\nhiGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nContent-Length: +2\r\n\r\nhiGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nTransfer-Encoding : chunked\r\n\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nX: y\r\n Transfer-Encoding: chunked\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nX: a\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\nabc\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3x\r\nabc\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n-3\r\nabc\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcd\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nbad trailer\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
            (b"GET /x HTTP/1.2.3\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n", 400),
        ] {
            let (mut client, mut reader) = connect(address);
            client.write_all(attempt).unwrap();
            let refused = read_answer(&mut reader, false);
            assert_eq!(
                refused.status(),
                status,
                "{}",
                String::from_utf8_lossy(attempt)
            );
            assert_eq!(refused.header("connection").as_deref(), Some("close"));
            assert!(closed(&mut reader), "{}", String::from_utf8_lossy(attempt));
        }
    });
}

#[test]
fn oversized_chunks_are_refused_and_the_budget_returns_to_zero() {
    for (bytes, expected) in [
        (
            &b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n100001\r\n"[..],
            HttpError::BodyTooLarge,
        ),
        (
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffffff\r\n",
            HttpError::BodyTooLarge,
        ),
        (
            b"POST / HTTP/1.1\r\nContent-Length: 99999999999999999999999\r\n\r\n",
            HttpError::BodyTooLarge,
        ),
    ] {
        assert_eq!(
            read_request(&mut &bytes[..]).map(|request| request.body),
            Err(expected),
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
    let zeros = read_request(
        &mut &b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n00000000000000000000001\r\nx\r\n0\r\n\r\n"[..],
    );
    assert_eq!(zeros.map(|request| request.body), Ok(b"x".to_vec()));
    let mut many = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for _ in 0..30_000 {
        many.extend_from_slice(b"1;padding-padding-padding\r\nx\r\n");
    }
    assert_eq!(
        read_request(&mut &many[..]).map(|request| request.body.len()),
        Err(HttpError::Malformed)
    );
    let budget = MemoryBudget::new(150_000);
    let limits = Limits::new(1 << 20, &budget)
        .answering_over_budget_with(|| Response::text(429, "slow down"));
    with_server(&limits, |address| {
        let (mut client, mut reader) = connect(address);
        let mut upload = b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for _ in 0..4 {
            upload.extend_from_slice(b"10000\r\n");
            upload.extend_from_slice(&[b'u'; 0x10000]);
            upload.extend_from_slice(b"\r\n");
        }
        let _ = client.write_all(&upload);
        let refused = read_answer(&mut reader, false);
        assert_eq!(
            (refused.status(), refused.text()),
            (429, "slow down\n".to_owned())
        );
        assert!(closed(&mut reader));
        let (mut big, mut big_reader) = connect(address);
        let _ = big.write_all(b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n200000\r\n");
        assert_eq!(read_answer(&mut big_reader, false).status(), 413);
        let (mut fitting, mut fitting_reader) = connect(address);
        let mut upload = b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        upload.extend_from_slice(b"186a0\r\n");
        upload.extend_from_slice(&[b'f'; 100_000]);
        upload.extend_from_slice(b"\r\n0\r\n\r\n");
        fitting.write_all(&upload).unwrap();
        let served = read_answer(&mut fitting_reader, false);
        assert_eq!(served.body.len(), "POST /x ".len() + 100_000);
    });
    assert_eq!(budget.used_bytes(), 0);
}

#[test]
fn an_idle_connection_is_closed_and_the_last_allowed_request_says_close() {
    let budget = MemoryBudget::default();
    let limits = default_limits(&budget).with_keep_alive(Duration::from_millis(300), 2);
    with_server(&limits, |address| {
        let (mut client, mut reader) = connect(address);
        client.write_all(b"GET /a HTTP/1.1\r\n\r\n").unwrap();
        read_answer(&mut reader, false);
        let waited = Instant::now();
        assert!(closed(&mut reader));
        assert!(waited.elapsed() < Duration::from_secs(5));
        let (mut client, mut reader) = connect(address);
        client
            .write_all(b"GET /a HTTP/1.1\r\n\r\nGET /b HTTP/1.1\r\n\r\nGET /c HTTP/1.1\r\n\r\n")
            .unwrap();
        assert_eq!(read_answer(&mut reader, false).header("connection"), None);
        let second = read_answer(&mut reader, false);
        assert_eq!(
            (second.text(), second.header("connection")),
            ("GET /b ".to_owned(), Some("close".to_owned()))
        );
        assert!(closed(&mut reader));
        let (_silent, mut silent_reader) = connect(address);
        let waited = Instant::now();
        assert!(closed(&mut silent_reader));
        assert!(waited.elapsed() < Duration::from_secs(8));
    });
}

#[test]
fn a_streamed_body_whose_source_outgrows_the_budget_gets_the_over_budget_answer() {
    let budget = MemoryBudget::new(150_000);
    let limits = Limits::new(1 << 20, &budget)
        .answering_over_budget_with(|| Response::text(429, "slow down"));
    with_server(&limits, |address| {
        let (mut client, mut reader) = connect(address);
        client
            .write_all(b"GET /held HTTP/1.1\r\n\r\nGET /rows HTTP/1.1\r\nConnection: close\r\n\r\n")
            .unwrap();
        let refused = read_answer(&mut reader, false);
        assert_eq!(
            (refused.status(), refused.text()),
            (429, "slow down\n".to_owned())
        );
        let rows = read_answer(&mut reader, false);
        assert_eq!(rows.status(), 200);
        assert!(rows.text().starts_with("{\"row\":0}"));
    });
    assert_eq!(budget.used_bytes(), 0);
    let roomy = MemoryBudget::new(2_000_000);
    with_server(&default_limits(&roomy), |address| {
        let (mut client, mut reader) = connect(address);
        client
            .write_all(b"GET /held HTTP/1.1\r\nConnection: close\r\n\r\n")
            .unwrap();
        let held = read_answer(&mut reader, false);
        assert_eq!((held.status(), held.text()), (200, "held".to_owned()));
    });
    assert_eq!(roomy.used_bytes(), 0);
}

#[test]
fn a_body_trickled_slower_than_the_slowest_rate_is_cut_off() {
    let budget = MemoryBudget::default();
    with_server(&default_limits(&budget), |address| {
        let (mut client, mut reader) = connect(address);
        client
            .write_all(b"POST /slow HTTP/1.1\r\nContent-Length: 1000\r\n\r\n")
            .unwrap();
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                while started.elapsed() < Duration::from_secs(12) {
                    if client.write_all(b"x").is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(400));
                }
            });
            let mut rest = Vec::new();
            let ended = reader.read_to_end(&mut rest);
            let still_open = ended.as_ref().is_err_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
            });
            assert!(!still_open, "{ended:?}");
            assert!(started.elapsed() < Duration::from_secs(9));
        });
    });
}

#[test]
fn hostile_requests_return_errors_and_never_panic() {
    let seeds: [&[u8]; 4] = [
        b"POST /a?b=c HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5;e=1\r\nhello\r\n0\r\nT: v\r\n\r\nGET / HTTP/1.1\r\n\r\n",
        b"PUT /k HTTP/1.1\r\nContent-Length: 5\r\nExpect: 100-continue\r\n\r\nhelloGET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n",
        b"GET / HTTP/1.0\r\nConnection: close, keep-alive\r\n\r\n",
        b"POST / HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\nContent-Length: 0\r\n\r\n0\r\n\r\n",
    ];
    let alphabet = b"\r\n:; 0123456789abcdefABCDEF\t,-\x00\xff";
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..20_000 {
        let seed = seeds[(next() % seeds.len() as u64) as usize];
        let mut bytes = seed.to_vec();
        for _ in 0..(next() % 6) {
            let at = (next() % (bytes.len() as u64 + 1)) as usize;
            let byte = alphabet[(next() % alphabet.len() as u64) as usize];
            match next() % 3 {
                0 if at < bytes.len() => bytes[at] = byte,
                1 if at < bytes.len() => {
                    bytes.remove(at);
                }
                _ => bytes.insert(at, byte),
            }
        }
        let budget = MemoryBudget::new(1 << 20);
        let mut reader = RequestReader::new(PlainSource(&bytes[..]));
        for _ in 0..4 {
            let mut held = budget.empty_reservation();
            if reader.next_request(1 << 16, &mut held, &mut || {}).is_err() {
                break;
            }
        }
        assert_eq!(budget.used_bytes(), 0);
    }
}
