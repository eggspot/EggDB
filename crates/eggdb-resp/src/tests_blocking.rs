use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const SETTLE: Duration = Duration::from_millis(150);
const WRONG_TYPE_REPLY: &str =
    "-WRONGTYPE Operation against a key holding the wrong kind of value|";

struct Harness {
    keyspace: MemoryKeyspace,
    client: Client,
}

impl Harness {
    fn new() -> Self {
        Self {
            keyspace: MemoryKeyspace::default(),
            client: Client {
                authenticated: true,
                ..Client::default()
            },
        }
    }

    fn arguments(line: &str) -> Vec<Vec<u8>> {
        line.split(' ')
            .map(|word| word.as_bytes().to_vec())
            .collect()
    }

    fn text(&mut self, line: &str) -> String {
        let context = Context {
            keyspace: &self.keyspace,
            password_sha256: None,
            now_ms: 1_000_000,
            reply_room_bytes: usize::MAX,
        };
        let mut out = Vec::new();
        execute(&context, &mut self.client, &Self::arguments(line))
            .encode(&mut out, self.client.resp3);
        String::from_utf8_lossy(&out).replace("\r\n", "|")
    }

    fn transacted(&mut self, line: &str) -> String {
        let context = Context {
            keyspace: &self.keyspace,
            password_sha256: None,
            now_ms: 1_000_000,
            reply_room_bytes: usize::MAX,
        };
        let mut out = Vec::new();
        execute_with_transactions(&context, &mut self.client, &Self::arguments(line))
            .encode(&mut out, self.client.resp3);
        String::from_utf8_lossy(&out).replace("\r\n", "|")
    }
}

struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn test_config() -> ServerConfig {
    ServerConfig {
        password_sha256: None,
        max_connections: 16,
        memory_budget: std::sync::Arc::default(),
    }
}

fn with_server(test: impl FnOnce(SocketAddr)) {
    with_server_on(&MemoryKeyspace::default(), test_config(), test);
}

fn with_server_on(
    keyspace: &(dyn Keyspace + Sync),
    config: ServerConfig,
    test: impl FnOnce(SocketAddr),
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stopped = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let _stop = StopOnDrop(&stopped);
        scope.spawn(|| {
            serve(
                keyspace,
                &listener,
                &config,
                &|| stopped.load(Ordering::Acquire),
                &|_| {},
            )
            .unwrap()
        });
        test(address);
    });
}

struct RespClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl RespClient {
    fn connect(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Self {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        }
    }

    fn send(&mut self, line: &str) {
        let words: Vec<&str> = line.split(' ').collect();
        let mut request = format!("*{}\r\n", words.len());
        for word in words {
            request.push_str(&format!("${}\r\n{word}\r\n", word.len()));
        }
        self.writer.write_all(request.as_bytes()).unwrap();
    }

    fn reply(&mut self) -> String {
        let mut text = String::new();
        self.read_reply_into(&mut text);
        text
    }

    fn call(&mut self, line: &str) -> String {
        self.send(line);
        self.reply()
    }

    fn read_reply_into(&mut self, text: &mut String) {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let line = line.trim_end_matches("\r\n").to_owned();
        assert!(!line.is_empty(), "the server closed the connection");
        text.push_str(&line);
        text.push('|');
        let (kind, rest) = line.split_at(1);
        let length: i64 = rest.parse().unwrap_or(0);
        match kind {
            "$" if length >= 0 => {
                let length = usize::try_from(length).unwrap();
                let mut bulk = vec![0u8; length + 2];
                self.reader.read_exact(&mut bulk).unwrap();
                text.push_str(&String::from_utf8_lossy(&bulk[..length]));
                text.push('|');
            }
            "*" | "~" | ">" => {
                for _ in 0..length.max(0) {
                    self.read_reply_into(text);
                }
            }
            "%" => {
                for _ in 0..length.max(0) * 2 {
                    self.read_reply_into(text);
                }
            }
            _ => {}
        }
    }
}

#[test]
fn lmove_and_rpoplpush_move_one_element_between_lists() {
    let mut redis = Harness::new();
    assert_eq!(redis.text("RPUSH list a b c"), ":3|");
    assert_eq!(redis.text("LMOVE list other RIGHT LEFT"), "$1|c|");
    assert_eq!(redis.text("RPOPLPUSH list other"), "$1|b|");
    assert_eq!(redis.text("LRANGE other 0 -1"), "*2|$1|b|$1|c|");
    assert_eq!(redis.text("RPUSH list z"), ":2|");
    assert_eq!(redis.text("LMOVE list list LEFT RIGHT"), "$1|a|");
    assert_eq!(redis.text("LRANGE list 0 -1"), "*2|$1|z|$1|a|");
    assert_eq!(redis.text("LMOVE missing other LEFT LEFT"), "$-1|");
    assert_eq!(redis.text("LMOVE list other UP LEFT"), "-ERR syntax error|");
    assert_eq!(redis.text("SET text value"), "+OK|");
    assert_eq!(redis.text("LMOVE list text LEFT LEFT"), WRONG_TYPE_REPLY);
    assert_eq!(redis.text("LMOVE text other LEFT LEFT"), WRONG_TYPE_REPLY);
    assert_eq!(redis.text("LMOVE missing text LEFT LEFT"), "$-1|");
    assert_eq!(redis.text("LLEN list"), ":2|");
    assert_eq!(
        redis.text("LMOVE list other"),
        "-ERR wrong number of arguments for 'lmove' command|"
    );
}

#[test]
fn lmpop_pops_from_the_first_list_that_has_elements() {
    let mut redis = Harness::new();
    assert_eq!(redis.text("RPUSH many 1 2 3"), ":3|");
    assert_eq!(
        redis.text("LMPOP 2 missing many LEFT COUNT 2"),
        "*2|$4|many|*2|$1|1|$1|2|"
    );
    assert_eq!(
        redis.text("LMPOP 1 many RIGHT COUNT 10"),
        "*2|$4|many|*1|$1|3|"
    );
    assert_eq!(redis.text("LMPOP 1 many RIGHT"), "*-1|");
    assert_eq!(redis.text("SET text value"), "+OK|");
    assert_eq!(redis.text("RPUSH many 4"), ":1|");
    assert_eq!(redis.text("LMPOP 2 text many LEFT"), WRONG_TYPE_REPLY);
    assert_eq!(
        redis.text("LMPOP 0 many LEFT"),
        "-ERR numkeys should be greater than 0|"
    );
    assert_eq!(
        redis.text("LMPOP x many LEFT"),
        "-ERR numkeys should be greater than 0|"
    );
    assert_eq!(
        redis.text("LMPOP 1 many LEFT COUNT 0"),
        "-ERR count should be greater than 0|"
    );
    assert_eq!(redis.text("LMPOP 3 many LEFT"), "-ERR syntax error|");
    assert_eq!(redis.text("LMPOP 1 many MIDDLE"), "-ERR syntax error|");
    assert_eq!(
        redis.text("LMPOP 1 many LEFT COUNT 1 COUNT 1"),
        "-ERR syntax error|"
    );
    assert_eq!(redis.text("LLEN many"), ":1|");
}

#[test]
fn zpopmin_zpopmax_and_zmpop_reply_as_each_protocol_documents() {
    let mut redis = Harness::new();
    assert_eq!(redis.text("ZADD scores 1 a 2 b 3 c 4 d 5 e"), ":5|");
    assert_eq!(redis.text("ZPOPMIN scores"), "*2|$1|a|$1|1|");
    assert_eq!(redis.text("ZPOPMAX scores 2"), "*4|$1|e|$1|5|$1|d|$1|4|");
    assert_eq!(redis.text("ZPOPMIN missing"), "*0|");
    assert_eq!(redis.text("ZPOPMIN scores 0"), "*0|");
    assert_eq!(
        redis.text("ZPOPMIN scores -1"),
        "-ERR value is out of range, must be positive|"
    );
    assert_eq!(
        redis.text("ZPOPMIN scores x"),
        "-ERR value is not an integer or out of range|"
    );
    assert_eq!(redis.text("ZPOPMIN scores 1 2"), "-ERR syntax error|");
    assert_eq!(
        redis.text("ZMPOP 2 missing scores MAX"),
        "*2|$6|scores|*1|*2|$1|c|$1|3|"
    );
    assert_eq!(
        redis.text("ZMPOP 1 scores MIN COUNT 9"),
        "*2|$6|scores|*1|*2|$1|b|$1|2|"
    );
    assert_eq!(redis.text("ZMPOP 1 scores MIN"), "*-1|");
    assert_eq!(redis.text("ZMPOP 1 scores LOW"), "-ERR syntax error|");
    assert_eq!(
        redis.text("ZPOPMIN"),
        "-ERR wrong number of arguments for 'zpopmin' command|"
    );

    redis.client.resp3 = true;
    assert_eq!(redis.text("ZADD scores 1 a 2 b 3 c"), ":3|");
    assert_eq!(redis.text("ZPOPMIN scores"), "*2|$1|a|,1|");
    assert_eq!(redis.text("ZPOPMIN scores 5"), "*2|*2|$1|b|,2|*2|$1|c|,3|");
    assert_eq!(redis.text("ZPOPMIN missing 2"), "*0|");
    assert_eq!(redis.text("ZMPOP 1 missing MIN"), "_|");
    assert_eq!(redis.text("LMPOP 1 missing LEFT"), "_|");
}

#[test]
fn blocking_commands_outside_a_connection_answer_at_once() {
    let mut redis = Harness::new();
    assert_eq!(redis.text("BLPOP missing 0"), "*-1|");
    assert_eq!(redis.text("RPUSH queue v w"), ":2|");
    assert_eq!(redis.text("BLPOP missing queue 0"), "*2|$5|queue|$1|v|");
    assert_eq!(redis.text("BRPOP queue 0"), "*2|$5|queue|$1|w|");
    assert_eq!(redis.text("BLMOVE missing other LEFT LEFT 0"), "$-1|");
    assert_eq!(redis.text("BRPOPLPUSH missing other 0"), "$-1|");
    assert_eq!(redis.text("ZADD scores 1 a 2 b 3 c"), ":3|");
    assert_eq!(
        redis.text("BZPOPMAX missing scores 0"),
        "*3|$6|scores|$1|c|$1|3|"
    );
    assert_eq!(redis.text("BZPOPMIN scores 1.5"), "*3|$6|scores|$1|a|$1|1|");
    assert_eq!(
        redis.text("BZMPOP 0 1 scores MIN"),
        "*2|$6|scores|*1|*2|$1|b|$1|2|"
    );
    assert_eq!(redis.text("BZPOPMIN scores 0"), "*-1|");
    assert_eq!(redis.text("BLMPOP 0 1 missing LEFT"), "*-1|");
    assert_eq!(redis.text("XREAD BLOCK 0 STREAMS events $"), "*-1|");
    assert_eq!(redis.text("SET text value"), "+OK|");
    assert_eq!(redis.text("BLPOP text 0"), WRONG_TYPE_REPLY);
    assert_eq!(redis.text("BZPOPMIN text 0"), WRONG_TYPE_REPLY);
    assert_eq!(redis.text("XREAD BLOCK 0 STREAMS text $"), WRONG_TYPE_REPLY);
}

#[test]
fn blocking_commands_refuse_bad_timeouts_and_arguments_with_redis_errors() {
    let mut redis = Harness::new();
    for (line, error) in [
        (
            "BLPOP queue abc",
            "-ERR timeout is not a float or out of range|",
        ),
        ("BLPOP queue -1", "-ERR timeout is negative|"),
        ("BLPOP queue inf", "-ERR timeout is out of range|"),
        (
            "BLPOP queue",
            "-ERR wrong number of arguments for 'blpop' command|",
        ),
        (
            "BRPOPLPUSH a b",
            "-ERR wrong number of arguments for 'brpoplpush' command|",
        ),
        ("BLMOVE a b UP LEFT 0", "-ERR syntax error|"),
        (
            "BLMPOP 0 0 queue LEFT",
            "-ERR numkeys should be greater than 0|",
        ),
        (
            "BZMPOP 0 1 scores MIN COUNT -1",
            "-ERR count should be greater than 0|",
        ),
        (
            "BZMPOP x 1 scores MIN",
            "-ERR timeout is not a float or out of range|",
        ),
        (
            "XREAD BLOCK abc STREAMS events $",
            "-ERR timeout is not an integer or out of range|",
        ),
        (
            "XREAD BLOCK -1 STREAMS events $",
            "-ERR timeout is negative|",
        ),
        (
            "XREAD BLOCK 0 STREAMS events",
            "-ERR Unbalanced 'xread' list of streams: for each stream key an ID or '$' must be specified.|",
        ),
    ] {
        assert_eq!(redis.text(line), error, "{line}");
    }
}

#[test]
fn inside_multi_blocking_commands_run_as_non_blocking() {
    let mut redis = Harness::new();
    assert_eq!(redis.transacted("MULTI"), "+OK|");
    assert_eq!(redis.transacted("BLPOP missing 0"), "+QUEUED|");
    assert_eq!(
        redis.transacted("BLMOVE missing other LEFT LEFT 0"),
        "+QUEUED|"
    );
    assert_eq!(redis.transacted("RPUSH queue v"), "+QUEUED|");
    assert_eq!(redis.transacted("BRPOP queue 0"), "+QUEUED|");
    assert_eq!(redis.transacted("EXEC"), "*4|*-1|$-1|:1|*2|$5|queue|$1|v|");
}

#[test]
fn replaying_an_append_only_file_never_blocks() {
    let mut aof = Vec::new();
    for words in [
        &["RPUSH", "queue", "a"][..],
        &["BLPOP", "queue", "0"],
        &["BLPOP", "queue", "0"],
        &["BZPOPMIN", "scores", "0"],
        &["BLMOVE", "queue", "other", "LEFT", "LEFT", "0"],
        &["XREAD", "BLOCK", "0", "STREAMS", "events", "$"],
    ] {
        aof.extend_from_slice(format!("*{}\r\n", words.len()).as_bytes());
        for word in words {
            aof.extend_from_slice(format!("${}\r\n{word}\r\n", word.len()).as_bytes());
        }
    }
    let keyspace = MemoryKeyspace::default();
    let report = replay_aof(&keyspace, &aof[..], 0).unwrap();
    assert_eq!(report.commands, 6);
    assert_eq!(report.failed, 0, "{:?}", report.first_error);
    let mut redis = Harness::new();
    redis.keyspace = keyspace;
    assert_eq!(redis.text("EXISTS queue other"), ":0|");
}

#[test]
fn blocked_clients_are_served_first_come_first_served() {
    with_server(|address| {
        let mut first = RespClient::connect(address);
        let mut second = RespClient::connect(address);
        let mut third = RespClient::connect(address);
        for client in [&mut first, &mut second, &mut third] {
            client.send("BLPOP queue 0");
            std::thread::sleep(SETTLE);
        }
        let mut pusher = RespClient::connect(address);
        assert_eq!(pusher.call("RPUSH queue a b c"), ":3|");
        assert_eq!(first.reply(), "*2|$5|queue|$1|a|");
        assert_eq!(second.reply(), "*2|$5|queue|$1|b|");
        assert_eq!(third.reply(), "*2|$5|queue|$1|c|");
        assert_eq!(pusher.call("EXISTS queue"), ":0|");
        assert_eq!(first.call("PING"), "+PONG|");
    });
}

#[test]
fn a_blocked_client_that_disconnects_takes_no_element() {
    with_server(|address| {
        let mut gone = RespClient::connect(address);
        gone.send("BLPOP queue 0");
        std::thread::sleep(SETTLE);
        drop(gone);
        std::thread::sleep(SETTLE);
        let mut pusher = RespClient::connect(address);
        assert_eq!(pusher.call("RPUSH queue a"), ":1|");
        std::thread::sleep(SETTLE);
        assert_eq!(pusher.call("LRANGE queue 0 -1"), "*1|$1|a|");
    });
}

#[test]
fn stopping_the_server_ends_a_client_blocked_without_a_timeout() {
    let keyspace = MemoryKeyspace::default();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stopped = AtomicBool::new(false);
    let config = test_config();
    std::thread::scope(|scope| {
        let _stop = StopOnDrop(&stopped);
        let server = scope.spawn(|| {
            serve(
                &keyspace,
                &listener,
                &config,
                &|| stopped.load(Ordering::Acquire),
                &|_| {},
            )
            .unwrap()
        });
        let mut blocked = RespClient::connect(address);
        blocked.send("BLPOP queue 0");
        std::thread::sleep(SETTLE);
        let asked = Instant::now();
        stopped.store(true, Ordering::Release);
        server.join().unwrap();
        assert!(asked.elapsed() < Duration::from_secs(5));
        let mut rest = Vec::new();
        let _ = blocked.reader.read_to_end(&mut rest);
        assert!(rest.is_empty(), "{rest:?}");
    });
}

#[test]
fn timeouts_answer_the_null_array_in_both_protocols() {
    with_server(|address| {
        let mut client = RespClient::connect(address);
        let asked = Instant::now();
        assert_eq!(client.call("BLPOP queue 0.1"), "*-1|");
        assert!(asked.elapsed() >= Duration::from_millis(100));
        assert_eq!(client.call("BRPOPLPUSH queue other 0.05"), "*-1|");
        assert_eq!(client.call("BLMPOP 0.05 1 queue LEFT"), "*-1|");
        assert_eq!(client.call("XREAD BLOCK 50 STREAMS events $"), "*-1|");
        assert!(client.call("HELLO 3").starts_with("%7|"));
        assert_eq!(client.call("BZPOPMIN scores 0.05"), "_|");
        assert_eq!(client.call("BLMOVE queue other LEFT LEFT 0.05"), "_|");
    });
}

#[test]
fn writes_wake_list_move_sorted_set_and_stream_waiters() {
    with_server(|address| {
        let mut waiter = RespClient::connect(address);
        let mut writer = RespClient::connect(address);

        waiter.send("BLMOVE source target LEFT RIGHT 0");
        std::thread::sleep(SETTLE);
        assert_eq!(writer.call("RPUSH source a b"), ":2|");
        assert_eq!(waiter.reply(), "$1|a|");
        assert_eq!(writer.call("LRANGE target 0 -1"), "*1|$1|a|");

        waiter.send("BZPOPMAX scores 0");
        std::thread::sleep(SETTLE);
        assert_eq!(writer.call("ZADD scores 1 low 2 high"), ":2|");
        assert_eq!(waiter.reply(), "*3|$6|scores|$4|high|$1|2|");

        waiter.send("XREAD BLOCK 0 STREAMS events $");
        std::thread::sleep(SETTLE);
        assert_eq!(writer.call("XADD events 7-1 field value"), "$3|7-1|");
        assert_eq!(
            waiter.reply(),
            "*1|*2|$6|events|*1|*2|$3|7-1|*2|$5|field|$5|value|"
        );

        waiter.send("BZMPOP 0 2 empty scores MIN COUNT 5");
        assert_eq!(waiter.reply(), "*2|$6|scores|*1|*2|$3|low|$1|1|");

        waiter.send("BLMPOP 0 2 empty source RIGHT COUNT 5");
        assert_eq!(waiter.reply(), "*2|$6|source|*1|$1|b|");

        waiter.send("BRPOP first second 0");
        std::thread::sleep(SETTLE);
        assert_eq!(writer.call("LPUSH second x"), ":1|");
        assert_eq!(waiter.reply(), "*2|$6|second|$1|x|");
    });
}

#[test]
fn a_key_retyped_while_waited_on_keeps_the_client_blocked() {
    with_server(|address| {
        let mut waiter = RespClient::connect(address);
        let mut writer = RespClient::connect(address);
        waiter.send("BLPOP queue 0");
        std::thread::sleep(SETTLE);
        assert_eq!(writer.call("ZADD queue 1 member"), ":1|");
        std::thread::sleep(SETTLE);
        assert_eq!(writer.call("DEL queue"), ":1|");
        assert_eq!(writer.call("RPUSH queue item"), ":1|");
        assert_eq!(waiter.reply(), "*2|$5|queue|$4|item|");

        assert_eq!(writer.call("SET target text"), "+OK|");
        waiter.send("BLMOVE source target LEFT LEFT 0");
        std::thread::sleep(SETTLE);
        assert_eq!(writer.call("RPUSH source x"), ":1|");
        assert_eq!(waiter.reply(), WRONG_TYPE_REPLY);
        assert_eq!(writer.call("LLEN source"), ":1|");
    });
}

#[test]
fn inside_multi_a_connection_never_blocks() {
    with_server(|address| {
        let mut client = RespClient::connect(address);
        assert_eq!(client.call("MULTI"), "+OK|");
        assert_eq!(client.call("BLPOP queue 0"), "+QUEUED|");
        assert_eq!(client.call("BLMOVE queue other LEFT LEFT 0"), "+QUEUED|");
        assert_eq!(client.call("XREAD BLOCK 0 STREAMS events $"), "+QUEUED|");
        assert_eq!(client.call("EXEC"), "*3|*-1|$-1|*-1|");
    });
}

#[derive(Default)]
struct CountingKeyspace {
    inner: MemoryKeyspace,
    writes: AtomicUsize,
}

impl Keyspace for CountingKeyspace {
    fn read(
        &self,
        db: u32,
        body: &mut dyn FnMut(&dyn KeysRead) -> Result<(), String>,
    ) -> Result<(), String> {
        self.inner.read(db, body)
    }

    fn write(
        &self,
        db: u32,
        body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>,
    ) -> Result<(), String> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.write(db, body)
    }
}

#[test]
fn a_client_blocked_on_empty_keys_opens_no_write_transaction() {
    let keyspace = CountingKeyspace::default();
    with_server_on(&keyspace, test_config(), |address| {
        let mut client = RespClient::connect(address);
        let asked = Instant::now();
        assert_eq!(client.call("BLPOP queue other 1"), "*-1|");
        assert!(asked.elapsed() >= Duration::from_secs(1));
        assert_eq!(client.call("BZPOPMIN scores 0.5"), "*-1|");
        assert_eq!(client.call("BLMOVE queue target LEFT LEFT 0.5"), "*-1|");
        assert_eq!(keyspace.writes.load(Ordering::SeqCst), 0);
        assert_eq!(client.call("RPUSH queue a"), ":1|");
        assert_eq!(client.call("BLPOP queue 1"), "*2|$5|queue|$1|a|");
    });
}

#[test]
fn a_client_that_closes_behind_a_pipelined_blocking_command_takes_no_element() {
    let config = ServerConfig {
        max_connections: 2,
        ..test_config()
    };
    with_server_on(&MemoryKeyspace::default(), config, |address| {
        let mut gone = TcpStream::connect(address).unwrap();
        gone.write_all(b"*3\r\n$5\r\nBLPOP\r\n$5\r\nqueue\r\n$1\r\n0\r\n")
            .unwrap();
        std::thread::sleep(SETTLE);
        gone.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
        drop(gone);
        std::thread::sleep(SETTLE * 4);
        let mut pusher = RespClient::connect(address);
        assert_eq!(pusher.call("LPUSH queue x"), ":1|");
        std::thread::sleep(SETTLE);
        assert_eq!(pusher.call("LLEN queue"), ":1|");
        let mut another = RespClient::connect(address);
        assert_eq!(another.call("PING"), "+PONG|");
    });
}

#[test]
fn stream_reads_stop_with_oom_once_they_pass_the_reply_room() {
    let mut redis = Harness::new();
    let value = "v".repeat(1000);
    for id in 1..=4 {
        assert_eq!(
            redis.text(&format!("XADD events {id}-1 field {value}")),
            format!("${}|{id}-1|", 3)
        );
    }
    let mut run = |line: &str, reply_room_bytes: usize| {
        let context = Context {
            keyspace: &redis.keyspace,
            password_sha256: None,
            now_ms: 1_000_000,
            reply_room_bytes,
        };
        let mut out = Vec::new();
        execute(&context, &mut redis.client, &Harness::arguments(line)).encode(&mut out, false);
        String::from_utf8(out).unwrap()
    };
    assert!(run("XREAD STREAMS events 0", 2500).starts_with("-OOM "));
    assert!(run("XREAD COUNT 2 STREAMS events 0", 2500).starts_with("*1\r\n"));
    assert!(run("XREAD STREAMS events 0", 8000).starts_with("*1\r\n"));
}
