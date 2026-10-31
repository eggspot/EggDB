use super::*;

struct Harness {
    keyspace: MemoryKeyspace,
    client: Client,
    now_ms: u64,
    password: Option<[u8; 32]>,
}

impl Harness {
    fn new() -> Self {
        Self {
            keyspace: MemoryKeyspace::default(),
            client: Client {
                db: 0,
                resp3: false,
                authenticated: true,
                name: None,
            },
            now_ms: 1_000_000,
            password: None,
        }
    }

    fn run(&mut self, line: &str) -> Reply {
        let arguments: Vec<Vec<u8>> = line.split(' ').map(|word| word.as_bytes().to_vec()).collect();
        let context = Context {
            keyspace: &self.keyspace,
            password_sha256: self.password.as_ref(),
            now_ms: self.now_ms,
        };
        execute(&context, &mut self.client, &arguments)
    }

    fn text(&mut self, line: &str) -> String {
        let mut out = Vec::new();
        self.run(line).encode(&mut out, self.client.resp3);
        String::from_utf8(out).unwrap().replace("\r\n", "|")
    }
}

#[test]
fn strings_counters_and_expiry_follow_the_documented_semantics() {
    let mut redis = Harness::new();
    assert_eq!(redis.text("SET k v"), "+OK|");
    assert_eq!(redis.text("GET k"), "$1|v|");
    assert_eq!(redis.text("SET k w NX"), "$-1|");
    assert_eq!(redis.text("SET k w XX GET"), "$1|v|");
    assert_eq!(redis.text("INCR n"), ":1|");
    assert_eq!(redis.text("INCRBY n 41"), ":42|");
    assert_eq!(redis.text("INCR k"), "-ERR value is not an integer or out of range|");
    assert_eq!(redis.text("INCRBYFLOAT f 2.5"), "$3|2.5|");
    assert_eq!(redis.text("APPEND k xyz"), ":4|");
    assert_eq!(redis.text("GETRANGE k 1 -1"), "$3|xyz|");
    assert_eq!(redis.text("MSET a 1 b 2"), "+OK|");
    assert_eq!(redis.text("MGET a b nope"), "*3|$1|1|$1|2|$-1|");
    assert_eq!(redis.text("SET t v EX 10"), "+OK|");
    assert_eq!(redis.text("TTL t"), ":10|");
    assert_eq!(redis.text("PTTL nope"), ":-2|");
    assert_eq!(redis.text("TTL k"), ":-1|");
    redis.now_ms += 10_000;
    assert_eq!(redis.text("GET t"), "$-1|");
    assert_eq!(redis.text("EXISTS t k"), ":1|");
    assert_eq!(redis.text("DEL k a nope"), ":2|");
    assert_eq!(redis.text("TYPE b"), "+string|");
    assert_eq!(redis.text("KEYS *"), "*3|$1|b|$1|f|$1|n|");
    assert_eq!(redis.text("RENAME b c"), "+OK|");
    assert_eq!(redis.text("DBSIZE"), ":3|");
    assert_eq!(redis.text("BOGUS"), "-ERR unknown command 'bogus'|");
    assert_eq!(redis.text("GET"), "-ERR wrong number of arguments for 'get' command|");
}

#[test]
fn hashes_lists_sets_and_sorted_sets_behave_as_documented() {
    let mut redis = Harness::new();
    assert_eq!(redis.text("HSET h f1 a f2 b"), ":2|");
    assert_eq!(redis.text("HGET h f2"), "$1|b|");
    assert_eq!(redis.text("HINCRBY h n 5"), ":5|");
    assert_eq!(redis.text("HDEL h f1 nope"), ":1|");
    assert_eq!(redis.text("HGETALL h"), "*4|$2|f2|$1|b|$1|n|$1|5|");
    assert_eq!(redis.text("GET h"), "-WRONGTYPE Operation against a key holding the wrong kind of value|");
    assert_eq!(redis.text("RPUSH l a b c"), ":3|");
    assert_eq!(redis.text("LPUSH l z"), ":4|");
    assert_eq!(redis.text("LRANGE l 0 -1"), "*4|$1|z|$1|a|$1|b|$1|c|");
    assert_eq!(redis.text("LPOP l"), "$1|z|");
    assert_eq!(redis.text("RPOP l 2"), "*2|$1|c|$1|b|");
    assert_eq!(redis.text("LLEN l"), ":1|");
    assert_eq!(redis.text("LPOP l"), "$1|a|");
    assert_eq!(redis.text("EXISTS l"), ":0|");
    assert_eq!(redis.text("SADD s x y x"), ":2|");
    assert_eq!(redis.text("SADD t y z"), ":2|");
    assert_eq!(redis.text("SINTER s t"), "*1|$1|y|");
    assert_eq!(redis.text("SUNION s t"), "*3|$1|x|$1|y|$1|z|");
    assert_eq!(redis.text("SISMEMBER s x"), ":1|");
    assert_eq!(redis.text("ZADD z 2 b 1 a 3 c"), ":3|");
    assert_eq!(redis.text("ZRANGE z 0 -1 WITHSCORES"), "*6|$1|a|$1|1|$1|b|$1|2|$1|c|$1|3|");
    assert_eq!(redis.text("ZINCRBY z 10 a"), "$2|11|");
    assert_eq!(redis.text("ZRANK z a"), ":2|");
    assert_eq!(redis.text("ZRANGEBYSCORE z (2 +inf"), "*2|$1|c|$1|a|");
    assert_eq!(redis.text("ZCOUNT z -inf 3"), ":2|");
    assert_eq!(redis.text("ZSCORE z nope"), "$-1|");
}

#[test]
fn auth_hello_and_select_guard_the_connection() {
    let mut redis = Harness::new();
    redis.password = Some(eggdb_crypto::sha256(b"secret"));
    redis.client.authenticated = false;
    assert_eq!(redis.text("GET k"), "-NOAUTH Authentication required.|");
    assert_eq!(redis.text("AUTH wrong"), "-WRONGPASS invalid username-password pair or user is disabled.|");
    assert_eq!(redis.text("AUTH secret"), "+OK|");
    assert!(redis.text("HELLO 3").starts_with("%7|"));
    assert_eq!(redis.text("HGETALL nope"), "%0|");
    assert_eq!(redis.text("SELECT 16"), "-ERR DB index is out of range|");
    assert_eq!(redis.text("SET k one"), "+OK|");
    assert_eq!(redis.text("SELECT 1"), "+OK|");
    assert_eq!(redis.text("GET k"), "_|");
    assert_eq!(redis.text("PING"), "+PONG|");
}

#[test]
fn a_server_answers_a_pipelined_client_over_tcp() {
    use std::io::{Read, Write};
    let keyspace = MemoryKeyspace::default();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let config = ServerConfig {
        password_sha256: None,
        max_connections: 8,
    };
    std::thread::scope(|scope| {
        scope.spawn(|| {
            serve(&keyspace, &listener, &config, &|| stopped.load(std::sync::atomic::Ordering::Acquire), &|_| {})
                .unwrap();
        });
        let mut client = std::net::TcpStream::connect(address).unwrap();
        client
            .write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$2\r\nhi\r\nGET k\r\nQUIT\r\n")
            .unwrap();
        let mut answer = String::new();
        client.read_to_string(&mut answer).unwrap();
        assert_eq!(answer, "+OK\r\n$2\r\nhi\r\n+OK\r\n");
        stopped.store(true, std::sync::atomic::Ordering::Release);
    });
}
