use super::framing::{Framer, FramingError};
use super::{MAX_SYSLOG_MESSAGE_BYTES, Message, SyslogCollector, parse, syslog_messages_dropped};
use crate::tests::MemoryStore;
use eggdb_platform::{DatagramSocket, Network, OsNetwork};
use eggdb_sql::{Session, Value};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const RECEIVED_2026_11_04: i64 = 1_793_793_600_000_000;
const RFC5424_SU: &[u8] = b"<34>1 2003-10-11T22:14:15.003Z mymachine.example.com su - ID47 - \xEF\xBB\xBF'su root' failed for lonvick on /dev/pts/8";
const RFC5424_DONUTS: &[u8] = b"<165>1 2003-08-24T05:14:15.000003-07:00 192.0.2.1 myproc 8710 - - %% It's time to make the do-nuts.";
const RFC5424_EVENT: &[u8] = b"<165>1 2003-10-11T22:14:15.003Z mymachine.example.com evntslog - ID47 [exampleSDID@32473 iut=\"3\" eventSource=\"Application\" eventID=\"1011\"] \xEF\xBB\xBFAn application event log entry...";
const RFC5424_TWO_ELEMENTS: &[u8] = b"<165>1 2003-10-11T22:14:15.003Z mymachine.example.com evntslog - ID47 [exampleSDID@32473 iut=\"3\" eventSource=\"Application\" eventID=\"1011\"][examplePriority@32473 class=\"high\"]";
const RFC3164_SU: &[u8] =
    b"<34>Oct 11 22:14:15 mymachine su: 'su root' failed for lonvick on /dev/pts/8";

fn owned(text: &str) -> Option<String> {
    Some(text.to_owned())
}

#[test]
fn rfc5424_examples_read_every_header_field() {
    let su = parse(RFC5424_SU, RECEIVED_2026_11_04);
    assert_eq!(
        su,
        Message {
            facility: 4,
            severity: 2,
            timestamp_micros: Some(1_065_910_455_003_000),
            hostname: owned("mymachine.example.com"),
            app_name: owned("su"),
            procid: None,
            msgid: owned("ID47"),
            structured_data: None,
            message: "'su root' failed for lonvick on /dev/pts/8".to_owned(),
        }
    );
    let donuts = parse(RFC5424_DONUTS, RECEIVED_2026_11_04);
    assert_eq!((donuts.facility, donuts.severity), (20, 5));
    assert_eq!(donuts.timestamp_micros, Some(1_061_727_255_000_003));
    assert_eq!(donuts.hostname, owned("192.0.2.1"));
    assert_eq!(donuts.procid, owned("8710"));
    assert_eq!(donuts.msgid, None);
    assert_eq!(donuts.message, "%% It's time to make the do-nuts.");
    let event = parse(RFC5424_EVENT, RECEIVED_2026_11_04);
    assert_eq!(
        event.structured_data,
        owned("[exampleSDID@32473 iut=\"3\" eventSource=\"Application\" eventID=\"1011\"]")
    );
    assert_eq!(event.message, "An application event log entry...");
    let two = parse(RFC5424_TWO_ELEMENTS, RECEIVED_2026_11_04);
    assert_eq!(
        two.structured_data,
        owned(
            "[exampleSDID@32473 iut=\"3\" eventSource=\"Application\" eventID=\"1011\"][examplePriority@32473 class=\"high\"]"
        )
    );
    assert_eq!(two.message, "");
}

#[test]
fn rfc5424_values_keep_escaped_brackets_and_quotes_and_fractions_scale() {
    let escaped = parse(
        b"<14>1 1985-04-12T23:20:50.52Z host app - - [id@1 a=\"x\\]y\\\"z\" b=\"\"] after\n",
        RECEIVED_2026_11_04,
    );
    assert_eq!(
        escaped.structured_data,
        owned("[id@1 a=\"x\\]y\\\"z\" b=\"\"]")
    );
    assert_eq!(escaped.message, "after");
    assert_eq!(escaped.timestamp_micros, Some(482_196_050_520_000));
    let leap_day = parse(
        b"<14>1 2024-02-29T01:02:03Z - - - - - x",
        RECEIVED_2026_11_04,
    );
    assert_eq!(leap_day.timestamp_micros, Some(1_709_168_523_000_000));
    assert_eq!(leap_day.hostname, None);
}

#[test]
fn a_broken_rfc5424_header_is_kept_as_an_rfc3164_message() {
    for broken in [
        "<14>1 2023-02-29T01:02:03Z host app - - - not a leap year",
        "<14>1 2003-10-11T22:14:15.0000001Z host app - - - seven fraction digits",
        "<14>1 2003-10-11 22:14:15Z host app - - - no T",
        "<14>1 2003-10-11T22:14:15Z host app - - [unclosed a=\"b] c",
        "<14>1 2003-10-11T22:14:15Z host app - - -no space",
        "<14>1 2003-10-11T22:14:15+24:00 host app - - - bad offset",
    ] {
        let message = parse(broken.as_bytes(), RECEIVED_2026_11_04);
        assert_eq!((message.facility, message.severity), (1, 6), "{broken}");
        assert_eq!(message.timestamp_micros, None, "{broken}");
        assert_eq!(
            message.message,
            broken.get(4..).unwrap_or_default(),
            "{broken}"
        );
    }
}

#[test]
fn rfc3164_examples_read_the_timestamp_host_tag_and_process() {
    let su = parse(RFC3164_SU, RECEIVED_2026_11_04);
    assert_eq!(
        su,
        Message {
            facility: 4,
            severity: 2,
            timestamp_micros: Some(1_791_756_855_000_000),
            hostname: owned("mymachine"),
            app_name: owned("su"),
            procid: None,
            msgid: None,
            structured_data: None,
            message: "'su root' failed for lonvick on /dev/pts/8".to_owned(),
        }
    );
    let bfg = parse(
        b"<13>Feb  5 17:32:18 10.0.0.99 Use the BFG!",
        RECEIVED_2026_11_04,
    );
    assert_eq!(bfg.timestamp_micros, Some(1_770_312_738_000_000));
    assert_eq!(bfg.hostname, owned("10.0.0.99"));
    assert_eq!(bfg.app_name, None);
    assert_eq!(bfg.message, "Use the BFG!");
    let daemon = parse(
        b"<30>Nov  4 11:59:00 db1 sshd[4242]: Accepted publickey for root\r\n",
        RECEIVED_2026_11_04,
    );
    assert_eq!((daemon.facility, daemon.severity), (3, 6));
    assert_eq!(daemon.app_name, owned("sshd"));
    assert_eq!(daemon.procid, owned("4242"));
    assert_eq!(daemon.message, "Accepted publickey for root");
    let no_host = parse(b"<13>Oct 11 22:14:15 su: hi", RECEIVED_2026_11_04);
    assert_eq!((no_host.hostname, no_host.app_name), (None, owned("su")));
}

#[test]
fn rfc3164_without_a_priority_is_user_notice_and_a_bad_timestamp_stays_in_the_text() {
    let bare = parse(b"Use the BFG!", RECEIVED_2026_11_04);
    assert_eq!((bare.facility, bare.severity), (1, 5));
    assert_eq!(bare.message, "Use the BFG!");
    let odd = parse(
        b"<0>1990 Oct 22 10:52:01 TZ-6 scapegoat.dmz.example.org 10.1.2.3 sched[0]: That's All Folks!",
        RECEIVED_2026_11_04,
    );
    assert_eq!((odd.facility, odd.severity), (0, 0));
    assert_eq!(odd.timestamp_micros, None);
    assert_eq!(
        odd.message,
        "1990 Oct 22 10:52:01 TZ-6 scapegoat.dmz.example.org 10.1.2.3 sched[0]: That's All Folks!"
    );
    for bad_priority in ["<192>x", "<01>x", "<>x", "<1234>x", "<12"] {
        let message = parse(bad_priority.as_bytes(), RECEIVED_2026_11_04);
        assert_eq!(
            (message.facility, message.severity),
            (1, 5),
            "{bad_priority}"
        );
        assert_eq!(message.message, bad_priority);
    }
}

#[test]
fn an_rfc3164_timestamp_ahead_of_the_clock_belongs_to_last_year() {
    let received_2026_01_02 = 1_767_312_000_000_000;
    let late = parse(b"<13>Dec 31 23:59:59 host app: x", received_2026_01_02);
    assert_eq!(late.timestamp_micros, Some(1_767_225_599_000_000));
    let leap_day_in_a_common_year = parse(b"<13>Feb 29 00:00:00 host app: x", RECEIVED_2026_11_04);
    assert_eq!(leap_day_in_a_common_year.timestamp_micros, None);
}

fn noise(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

fn hostile_input(seed: &mut u64) -> Vec<u8> {
    const PIECES: [&[u8]; 14] = [
        b"<",
        b">",
        b"<34>",
        b"1 ",
        b"2003-10-11T22:14:15.003Z ",
        b"Oct 11 22:14:15 ",
        b"[",
        b"]",
        b"\"",
        b"\\",
        b"-",
        b" ",
        b"\xEF\xBB\xBF",
        b"\xFF\0\n",
    ];
    let length = noise(seed) % 24;
    let mut input = Vec::new();
    for _ in 0..length {
        let pick = noise(seed);
        match PIECES.get((pick % 16) as usize) {
            Some(piece) => input.extend_from_slice(piece),
            None => input.push((pick >> 8) as u8),
        }
    }
    input
}

#[test]
fn hostile_messages_and_streams_never_panic() {
    let mut seed = 0x005E_ED0F_5915_1061_u64;
    for _ in 0..20_000 {
        let input = hostile_input(&mut seed);
        let message = parse(&input, RECEIVED_2026_11_04);
        assert!(message.facility <= 23 && message.severity <= 7);
        let mut framer = Framer::new(64);
        for piece in input.chunks(5) {
            framer.push(piece);
            while let Ok(Some(_)) = framer.next_frame() {}
        }
        let _ = framer.finish();
    }
    let huge_digits = "9".repeat(40);
    let mut framer = Framer::new(64);
    framer.push(huge_digits.as_bytes());
    assert_eq!(framer.next_frame(), Err(FramingError::BadLength));
}

fn counted(message: &[u8]) -> Vec<u8> {
    let mut frame = format!("{} ", message.len()).into_bytes();
    frame.extend_from_slice(message);
    frame
}

#[test]
fn tcp_frames_split_by_octet_counts_and_line_ends_at_any_read_boundary() {
    let mut stream = counted(RFC5424_SU);
    stream.extend_from_slice(RFC3164_SU);
    stream.extend_from_slice(b"\r\n");
    stream.extend_from_slice(&counted(RFC5424_EVENT));
    stream.extend_from_slice(b"<13>last line without an end");
    for split in 0..stream.len() {
        let mut framer = Framer::new(MAX_SYSLOG_MESSAGE_BYTES);
        let mut frames = Vec::new();
        let (first, second) = stream.split_at(split);
        for part in [first, second] {
            framer.push(part);
            while let Some(frame) = framer.next_frame().unwrap() {
                frames.push(frame);
            }
        }
        frames.extend(framer.finish());
        assert_eq!(
            frames,
            [
                RFC5424_SU.to_vec(),
                RFC3164_SU.to_vec(),
                RFC5424_EVENT.to_vec(),
                b"<13>last line without an end".to_vec()
            ],
            "split at {split}"
        );
    }
}

#[test]
fn a_line_trickled_one_byte_at_a_time_is_searched_once() {
    let line = vec![b'x'; MAX_SYSLOG_MESSAGE_BYTES - 1];
    let mut stream = Vec::new();
    for _ in 0..20 {
        stream.extend_from_slice(&line);
        stream.push(b'\n');
    }
    let mut framer = Framer::new(MAX_SYSLOG_MESSAGE_BYTES);
    let mut frames = 0;
    let started = std::time::Instant::now();
    for byte in &stream {
        framer.push(std::slice::from_ref(byte));
        while let Some(frame) = framer.next_frame().unwrap() {
            assert_eq!(frame, line);
            frames += 1;
        }
    }
    assert_eq!(frames, 20);
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
}

#[test]
fn tcp_frames_longer_than_the_limit_or_with_a_bad_count_are_refused() {
    let mut framer = Framer::new(8);
    framer.push(b"9 <13>abcdef");
    assert_eq!(framer.next_frame(), Err(FramingError::TooLong));
    let mut framer = Framer::new(8);
    framer.push(b"<13>abcdefgh");
    assert_eq!(framer.next_frame(), Err(FramingError::TooLong));
    let mut framer = Framer::new(8);
    framer.push(b"12x<13>");
    assert_eq!(framer.next_frame(), Err(FramingError::BadLength));
    let mut framer = Framer::new(8);
    framer.push(b"3 <1>");
    assert_eq!(framer.next_frame(), Ok(Some(b"<1>".to_vec())));
    assert_eq!(framer.next_frame(), Ok(None));
}

fn query(store: &MemoryStore, sql: &str) -> Vec<Vec<Option<String>>> {
    let mut session = Session::new(store);
    let mut rows = Vec::new();
    let outcome = session.execute(sql, &mut |result| {
        if result.columns.is_some() {
            rows = result
                .rows
                .iter()
                .map(|row| row.iter().map(Value::to_text).collect())
                .collect();
        }
    });
    if outcome.is_err() {
        return Vec::new();
    }
    rows
}

fn wait_for_rows(store: &MemoryStore, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let total = query(store, "SELECT count(*) FROM syslog");
        let counted = total
            .first()
            .and_then(|row| row.first().cloned().flatten())
            .and_then(|text| text.parse::<usize>().ok());
        if counted == Some(count) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the syslog table never reached {count} rows");
}

struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn syslog_sent_over_udp_and_tcp_is_read_back_through_sql() {
    let store = MemoryStore::default();
    let collector = SyslogCollector::new(&store, "eggdb", "eggdb", 4, None);
    collector.prepare().unwrap();
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let datagrams = OsNetwork::new().bind_datagram(loopback).unwrap();
    let address = datagrams.local_address().unwrap();
    let listener = TcpListener::bind(SocketAddr::new(address.ip(), 0)).unwrap();
    let stream_address = listener.local_addr().unwrap();
    let stopped = AtomicBool::new(false);
    let stop = || stopped.load(Ordering::Acquire);
    std::thread::scope(|scope| {
        let server = scope
            .spawn(|| collector.serve(Some(&datagrams), Some(&listener), &stop, &|_: &str| {}));
        let stop_even_on_a_failure = StopOnDrop(&stopped);
        let sender = UdpSocket::bind(loopback).unwrap();
        sender.send_to(RFC5424_SU, address).unwrap();
        sender.send_to(RFC3164_SU, address).unwrap();
        let oversized = vec![b'x'; MAX_SYSLOG_MESSAGE_BYTES + 1];
        let _ = sender.send_to(&oversized, address);
        sender.send_to(b"", address).unwrap();
        wait_for_rows(&store, 2);
        let mut stream = TcpStream::connect(stream_address).unwrap();
        stream.write_all(&counted(RFC5424_EVENT)).unwrap();
        stream
            .write_all(b"<30>Nov  4 11:59:00 db1 sshd[4242]: Accepted publickey for root\n")
            .unwrap();
        drop(stream);
        wait_for_rows(&store, 4);
        drop(stop_even_on_a_failure);
        server.join().unwrap().unwrap();
    });
    let rows = query(
        &store,
        "SELECT facility, severity, hostname, app_name, procid, msgid, structured_data, message, received_at IS NOT NULL, \"timestamp\" IS NOT NULL FROM syslog ORDER BY app_name, hostname",
    );
    let text = |value: &str| Some(value.to_owned());
    let su_row = |hostname: &str, msgid: Option<String>| {
        vec![
            text("4"),
            text("2"),
            text(hostname),
            text("su"),
            None,
            msgid,
            None,
            text("'su root' failed for lonvick on /dev/pts/8"),
            text("t"),
            text("t"),
        ]
    };
    assert_eq!(
        rows,
        vec![
            vec![
                text("20"),
                text("5"),
                text("mymachine.example.com"),
                text("evntslog"),
                None,
                text("ID47"),
                text("[exampleSDID@32473 iut=\"3\" eventSource=\"Application\" eventID=\"1011\"]"),
                text("An application event log entry..."),
                text("t"),
                text("t"),
            ],
            vec![
                text("3"),
                text("6"),
                text("db1"),
                text("sshd"),
                text("4242"),
                None,
                None,
                text("Accepted publickey for root"),
                text("t"),
                text("t"),
            ],
            su_row("mymachine", None),
            su_row("mymachine.example.com", text("ID47")),
        ]
    );
    let sent_at = query(
        &store,
        "SELECT \"timestamp\" FROM syslog WHERE app_name = 'evntslog'",
    );
    assert_eq!(sent_at, vec![vec![text("2003-10-11 22:14:15.003+00")]]);
}

fn wait_until(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "the condition never held");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_silent_tcp_connection_is_closed_after_the_idle_timeout_and_keeps_its_last_line() {
    let store = MemoryStore::default();
    let idle_timeout = Duration::from_millis(300);
    let collector = SyslogCollector::new(&store, "eggdb", "eggdb", 4, Some(idle_timeout));
    collector.prepare().unwrap();
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let address = listener.local_addr().unwrap();
    let stopped = AtomicBool::new(false);
    let stop = || stopped.load(Ordering::Acquire);
    std::thread::scope(|scope| {
        let server = scope.spawn(|| {
            collector.serve::<eggdb_platform::OsDatagramSocket>(
                None,
                Some(&listener),
                &stop,
                &|_: &str| {},
            )
        });
        let stop_even_on_a_failure = StopOnDrop(&stopped);
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(b"<13>first\n<13>unfinished").unwrap();
        let started = Instant::now();
        let mut byte = [0; 1];
        let closed = matches!(stream.read(&mut byte), Ok(0) | Err(_));
        let waited = started.elapsed();
        assert!(closed);
        assert!(
            waited >= idle_timeout - Duration::from_millis(50) && waited < Duration::from_secs(5),
            "{waited:?}"
        );
        wait_for_rows(&store, 2);
        drop(stop_even_on_a_failure);
        server.join().unwrap().unwrap();
    });
    let messages = query(&store, "SELECT message FROM syslog ORDER BY message");
    assert_eq!(
        messages,
        vec![
            vec![Some("first".to_owned())],
            vec![Some("unfinished".to_owned())]
        ]
    );
    assert_eq!(collector.messages_dropped(), 0);
}

#[test]
fn a_batch_the_table_refuses_is_logged_and_counted_and_later_batches_still_land() {
    let store = MemoryStore::default();
    query(&store, "CREATE TABLE syslog (only_column integer)");
    let collector = SyslogCollector::new(&store, "eggdb", "eggdb", 4, None);
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let address = listener.local_addr().unwrap();
    let logged = Mutex::new(Vec::<String>::new());
    let log = |line: &str| logged.lock().unwrap().push(line.to_owned());
    let dropped_before = syslog_messages_dropped();
    let stopped = AtomicBool::new(false);
    let stop = || stopped.load(Ordering::Acquire);
    std::thread::scope(|scope| {
        let server = scope.spawn(|| {
            collector.serve::<eggdb_platform::OsDatagramSocket>(None, Some(&listener), &stop, &log)
        });
        let stop_even_on_a_failure = StopOnDrop(&stopped);
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(b"<13>one\n<13>two\n<13>three\n").unwrap();
        drop(stream);
        wait_until(|| collector.messages_dropped() == 3);
        query(&store, "DROP TABLE syslog");
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(b"<13>after\n").unwrap();
        drop(stream);
        wait_for_rows(&store, 1);
        drop(stop_even_on_a_failure);
        server.join().unwrap().unwrap();
    });
    assert_eq!(collector.messages_dropped(), 3);
    assert!(syslog_messages_dropped() >= dropped_before + 3);
    let lines = logged.lock().unwrap().clone();
    assert!(!lines.is_empty(), "nothing was logged");
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("dropped ") && line.contains("syslog table refused")),
        "{lines:?}"
    );
}

#[test]
fn datagrams_longer_than_the_limit_are_counted_as_dropped() {
    let store = MemoryStore::default();
    let collector = SyslogCollector::new(&store, "eggdb", "eggdb", 4, None);
    collector.prepare().unwrap();
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let datagrams = OsNetwork::new().bind_datagram(loopback).unwrap();
    let address = datagrams.local_address().unwrap();
    let stopped = AtomicBool::new(false);
    let stop = || stopped.load(Ordering::Acquire);
    std::thread::scope(|scope| {
        let server = scope.spawn(|| collector.serve(Some(&datagrams), None, &stop, &|_: &str| {}));
        let stop_even_on_a_failure = StopOnDrop(&stopped);
        let sender = UdpSocket::bind(loopback).unwrap();
        let oversized = vec![b'x'; MAX_SYSLOG_MESSAGE_BYTES + 1];
        let delivered = sender.send_to(&oversized, address).is_ok();
        sender.send_to(b"<13>fits", address).unwrap();
        wait_for_rows(&store, 1);
        if delivered {
            wait_until(|| collector.messages_dropped() == 1);
        }
        drop(stop_even_on_a_failure);
        server.join().unwrap().unwrap();
    });
    assert!(collector.messages_dropped() <= 1);
}
