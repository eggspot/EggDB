use super::*;

struct Harness {
    keyspace: MemoryKeyspace,
    client: Client,
    reply_room_bytes: usize,
}

impl Harness {
    fn new() -> Self {
        Self {
            keyspace: MemoryKeyspace::default(),
            client: Client {
                authenticated: true,
                ..Client::default()
            },
            reply_room_bytes: usize::MAX,
        }
    }

    fn run(&mut self, arguments: &[&[u8]]) -> Reply {
        let arguments: Vec<Vec<u8>> = arguments.iter().map(|word| word.to_vec()).collect();
        let context = Context {
            keyspace: &self.keyspace,
            password_sha256: None,
            now_ms: 1_000_000,
            reply_room_bytes: self.reply_room_bytes,
            blocked_clients: 0,
        };
        execute(&context, &mut self.client, &arguments)
    }

    fn encoded(&mut self, arguments: &[&[u8]]) -> String {
        let mut out = Vec::new();
        self.run(arguments).encode(&mut out, self.client.resp3);
        String::from_utf8_lossy(&out).replace("\r\n", "|")
    }

    fn text(&mut self, line: &str) -> String {
        let words: Vec<&[u8]> = line.split(' ').map(str::as_bytes).collect();
        self.encoded(&words)
    }

    fn transacted(&mut self, line: &str) -> String {
        let arguments: Vec<Vec<u8>> = line
            .split(' ')
            .map(|word| word.as_bytes().to_vec())
            .collect();
        let context = Context {
            keyspace: &self.keyspace,
            password_sha256: None,
            now_ms: 1_000_000,
            reply_room_bytes: self.reply_room_bytes,
            blocked_clients: 0,
        };
        let mut out = Vec::new();
        execute_with_transactions(&context, &mut self.client, &arguments)
            .encode(&mut out, self.client.resp3);
        String::from_utf8_lossy(&out).replace("\r\n", "|")
    }

    fn value(&mut self, key: &str) -> Vec<u8> {
        match self.run(&[b"GET", key.as_bytes()]) {
            Reply::Bulk(Some(bytes)) => bytes,
            other => panic!("GET {key} answered {other:?}"),
        }
    }
}

#[test]
fn setbit_and_getbit_follow_the_command_pages_examples() {
    let mut redis = Harness::new();
    assert_eq!(redis.text("SETBIT mykey 7 1"), ":0|");
    assert_eq!(redis.text("SETBIT mykey 7 0"), ":1|");
    assert_eq!(redis.value("mykey"), b"\x00");
    assert_eq!(redis.text("SETBIT mykey 7 1"), ":0|");
    assert_eq!(redis.text("GETBIT mykey 0"), ":0|");
    assert_eq!(redis.text("GETBIT mykey 7"), ":1|");
    assert_eq!(redis.text("GETBIT mykey 100"), ":0|");
    assert_eq!(redis.text("SETBIT grown 17 1"), ":0|");
    assert_eq!(redis.value("grown"), b"\x00\x00\x40");
    assert_eq!(redis.text("GETBIT nokey 3"), ":0|");
}

#[test]
fn bit_commands_refuse_bad_offsets_values_and_types_as_redis_does() {
    let mut redis = Harness::new();
    let offset = "-ERR bit offset is not an integer or out of range|";
    assert_eq!(redis.text("SETBIT k 4294967296 1"), offset);
    assert_eq!(redis.text("SETBIT k -1 1"), offset);
    assert_eq!(redis.text("SETBIT k x 1"), offset);
    assert_eq!(redis.text("GETBIT k 4294967296"), offset);
    assert_eq!(
        redis.text("SETBIT k 0 2"),
        "-ERR bit is not an integer or out of range|"
    );
    assert_eq!(
        redis.text("BITPOS k 2"),
        "-ERR The bit argument must be 1 or 0.|"
    );
    assert_eq!(redis.text("BITCOUNT k 0"), "-ERR syntax error|");
    assert_eq!(redis.text("BITCOUNT k 0 1 WORD"), "-ERR syntax error|");
    assert_eq!(
        redis.text("BITCOUNT k a 1"),
        "-ERR value is not an integer or out of range|"
    );
    assert_eq!(redis.text("RPUSH list a"), ":1|");
    let wrong = "-WRONGTYPE Operation against a key holding the wrong kind of value|";
    assert_eq!(redis.text("SETBIT list 0 1"), wrong);
    assert_eq!(redis.text("BITCOUNT list"), wrong);
    assert_eq!(redis.text("BITFIELD list GET u8 0"), wrong);
    assert_eq!(redis.text("BITOP AND dest list"), wrong);
}

#[test]
fn setbit_far_past_the_end_stops_at_the_servers_memory_budget() {
    let mut redis = Harness::new();
    redis.reply_room_bytes = 1 << 20;
    assert_eq!(
        redis.text("SETBIT huge 4294967295 1"),
        "-OOM command not allowed: the value would pass the server's memory budget|"
    );
    assert_eq!(redis.text("EXISTS huge"), ":0|");
    assert!(
        redis
            .text("BITFIELD huge SET u8 4294967000 1")
            .starts_with("-OOM ")
    );
    assert_eq!(redis.text("SETBIT small 8000000 1"), ":0|");
    assert_eq!(redis.text("STRLEN small"), ":1000001|");
}

#[test]
fn bitop_stops_at_the_servers_memory_budget_before_it_copies_every_source() {
    let mut redis = Harness::new();
    let big = vec![0xA5_u8; 300_000];
    redis.run(&[b"SET", b"big", &big]);
    redis.run(&[b"SET", b"over", &vec![1_u8; (1 << 20) + 1]]);
    redis.reply_room_bytes = 1 << 20;
    let over_budget = "-OOM command not allowed: the value would pass the server's memory budget|";
    assert_eq!(redis.text("BITOP OR d over"), over_budget);
    assert_eq!(redis.text("EXISTS d"), ":0|");
    let repeated = format!("BITOP OR d{}", " big".repeat(100_000));
    let started = std::time::Instant::now();
    assert_eq!(redis.text(&repeated), over_budget);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(redis.text("EXISTS d"), ":0|");
    assert_eq!(redis.text("BITOP OR d big big big"), ":300000|");
    assert_eq!(redis.value("d"), big);
}

#[test]
fn bitop_over_many_missing_keys_costs_nothing_per_byte() {
    let mut redis = Harness::new();
    redis.run(&[b"SET", b"big", &vec![0xFF_u8; 1 << 20]]);
    let missing: String = (0..100_000).map(|index| format!(" m{index}")).collect();
    let started = std::time::Instant::now();
    assert_eq!(
        redis.text(&format!("BITOP AND d big{missing}")),
        format!(":{}|", 1 << 20)
    );
    assert_eq!(
        redis.text(&format!("BITOP ONE d big{missing}")),
        format!(":{}|", 1 << 20)
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(redis.text("BITCOUNT d"), format!(":{}|", 8 << 20));
    assert_eq!(
        redis.text(&format!("BITOP AND d big{missing}")),
        format!(":{}|", 1 << 20)
    );
    assert_eq!(redis.text("BITCOUNT d"), ":0|");
}

#[test]
fn bitcount_counts_byte_and_bit_ranges_like_the_command_page() {
    let mut redis = Harness::new();
    assert_eq!(redis.text("SET mykey foobar"), "+OK|");
    assert_eq!(redis.text("BITCOUNT mykey"), ":26|");
    assert_eq!(redis.text("BITCOUNT mykey 0 0"), ":4|");
    assert_eq!(redis.text("BITCOUNT mykey 1 1"), ":6|");
    assert_eq!(redis.text("BITCOUNT mykey 1 1 BYTE"), ":6|");
    assert_eq!(redis.text("BITCOUNT mykey 5 30 BIT"), ":17|");
    assert_eq!(redis.text("BITCOUNT mykey -1 -1"), ":4|");
    assert_eq!(redis.text("BITCOUNT mykey -1 -2"), ":0|");
    assert_eq!(redis.text("BITCOUNT mykey 0 100"), ":26|");
    assert_eq!(redis.text("BITCOUNT nokey"), ":0|");
}

#[test]
fn bitpos_finds_set_and_clear_bits_like_the_command_page() {
    let mut redis = Harness::new();
    redis.run(&[b"SET", b"mykey", b"\xff\xf0\x00"]);
    assert_eq!(redis.text("BITPOS mykey 0"), ":12|");
    redis.run(&[b"SET", b"mykey", b"\x00\xff\xf0"]);
    assert_eq!(redis.text("BITPOS mykey 1 0"), ":8|");
    assert_eq!(redis.text("BITPOS mykey 1 2"), ":16|");
    assert_eq!(redis.text("BITPOS mykey 1 2 -1 BYTE"), ":16|");
    assert_eq!(redis.text("BITPOS mykey 1 7 15 BIT"), ":8|");
    redis.run(&[b"SET", b"mykey", b"\x00\x00\x00"]);
    assert_eq!(redis.text("BITPOS mykey 1"), ":-1|");
    assert_eq!(redis.text("BITPOS mykey 1 7 -3 BIT"), ":-1|");
    redis.run(&[b"SET", b"ones", b"\xff\xff"]);
    assert_eq!(redis.text("BITPOS ones 0"), ":16|");
    assert_eq!(redis.text("BITPOS ones 0 0 -1"), ":-1|");
    assert_eq!(redis.text("BITPOS nokey 0"), ":0|");
    assert_eq!(redis.text("BITPOS nokey 1"), ":-1|");
}

#[test]
fn bitop_combines_strings_like_the_command_page() {
    let mut redis = Harness::new();
    assert_eq!(redis.text("SET key1 foobar"), "+OK|");
    assert_eq!(redis.text("SET key2 abcdef"), "+OK|");
    assert_eq!(redis.text("BITOP AND dest key1 key2"), ":6|");
    assert_eq!(redis.value("dest"), b"`bc`ab");
    redis.run(&[b"SET", b"x", b"\xf0"]);
    redis.run(&[b"SET", b"y1", b"\x3c"]);
    redis.run(&[b"SET", b"y2", b"\x0f\x01"]);
    assert_eq!(redis.text("BITOP OR d x y1 y2"), ":2|");
    assert_eq!(redis.value("d"), b"\xff\x01");
    assert_eq!(redis.text("BITOP XOR d x y1 y2"), ":2|");
    assert_eq!(redis.value("d"), b"\xc3\x01");
    assert_eq!(redis.text("BITOP NOT d x"), ":1|");
    assert_eq!(redis.value("d"), b"\x0f");
    assert_eq!(redis.text("BITOP DIFF d x y1 y2"), ":2|");
    assert_eq!(redis.value("d"), b"\xc0\x00");
    assert_eq!(redis.text("BITOP DIFF1 d x y1 y2"), ":2|");
    assert_eq!(redis.value("d"), b"\x0f\x01");
    assert_eq!(redis.text("BITOP ANDOR d x y1 y2"), ":2|");
    assert_eq!(redis.value("d"), b"\x30\x00");
    assert_eq!(redis.text("BITOP ONE d x y1 y2"), ":2|");
    assert_eq!(redis.value("d"), b"\xc3\x01");
    assert_eq!(
        redis.text("BITOP NOT d x y1"),
        "-ERR BITOP NOT must be called with a single source key.|"
    );
    assert_eq!(
        redis.text("BITOP DIFF d x"),
        "-ERR BITOP DIFF must be called with at least two source keys.|"
    );
    assert_eq!(redis.text("BITOP NAND d x y1"), "-ERR syntax error|");
    assert_eq!(redis.text("BITOP OR d nokey1 nokey2"), ":0|");
    assert_eq!(redis.text("EXISTS d"), ":0|");
}

#[test]
fn bitfield_reads_writes_and_overflows_like_the_command_page() {
    let mut redis = Harness::new();
    assert_eq!(
        redis.text("BITFIELD mykey INCRBY i5 100 1 GET u4 0"),
        "*2|:1|:0|"
    );
    let overflow = "BITFIELD mykey incrby u2 100 1 OVERFLOW SAT incrby u2 102 1";
    assert_eq!(redis.text(overflow), "*2|:1|:1|");
    assert_eq!(redis.text(overflow), "*2|:2|:2|");
    assert_eq!(redis.text(overflow), "*2|:3|:3|");
    assert_eq!(redis.text(overflow), "*2|:0|:3|");
    assert_eq!(
        redis.text("BITFIELD mykey OVERFLOW FAIL incrby u2 102 1"),
        "*1|$-1|"
    );
    assert_eq!(
        redis.text("BITFIELD k SET i8 #0 100 SET i8 #1 200 GET i8 #1 GET u8 8"),
        "*4|:0|:0|:-56|:200|"
    );
    assert_eq!(
        redis.text("BITFIELD u OVERFLOW SAT SET u8 0 -1 GET u8 0"),
        "*2|:0|:255|"
    );
    assert_eq!(
        redis.text("BITFIELD w SET i64 0 -1 GET u63 1"),
        "*2|:0|:9223372036854775807|"
    );
    assert_eq!(
        redis.text("BITFIELD w SET i64 0 9223372036854775807 INCRBY i64 0 1"),
        "*2|:-1|:-9223372036854775808|"
    );
    assert_eq!(
        redis.text("BITFIELD s OVERFLOW SAT INCRBY i8 0 -1000 INCRBY u8 8 1000"),
        "*2|:-128|:255|"
    );
    assert_eq!(
        redis.text("BITFIELD nokey GET u8 0 GET i4 100"),
        "*2|:0|:0|"
    );
    assert_eq!(redis.text("EXISTS nokey"), ":0|");
    assert_eq!(redis.text("SET hello Hello_World"), "+OK|");
    assert_eq!(redis.text("BITFIELD_RO hello GET i8 16"), "*1|:108|");
    assert_eq!(
        redis.text("BITFIELD_RO hello SET i8 0 1"),
        "-ERR BITFIELD_RO only supports the GET subcommand|"
    );
    let bad_type = "-ERR Invalid bitfield type. Use something like i16 u8. Note that u64 is not supported but i64 is.|";
    assert_eq!(redis.text("BITFIELD k GET u64 0"), bad_type);
    assert_eq!(redis.text("BITFIELD k GET i65 0"), bad_type);
    assert_eq!(redis.text("BITFIELD k GET x8 0"), bad_type);
    assert_eq!(
        redis.text("BITFIELD k GET u8 -1"),
        "-ERR bit offset is not an integer or out of range|"
    );
    assert_eq!(
        redis.text("BITFIELD k OVERFLOW LOUD"),
        "-ERR Invalid OVERFLOW type specified|"
    );
    assert_eq!(redis.text("BITFIELD k FROB u8 0"), "-ERR syntax error|");
    assert_eq!(redis.text("BITFIELD k GET u8"), "-ERR syntax error|");
}

#[test]
fn pfadd_pfcount_and_pfmerge_follow_the_command_pages_examples() {
    let mut redis = Harness::new();
    assert_eq!(redis.text("PFADD hll a b c d e f g"), ":1|");
    assert_eq!(redis.text("PFCOUNT hll"), ":7|");

    assert_eq!(redis.text("PFADD hll2 foo bar zap"), ":1|");
    assert_eq!(redis.text("PFADD hll2 zap zap zap"), ":0|");
    assert_eq!(redis.text("PFADD hll2 foo bar"), ":0|");
    assert_eq!(redis.text("PFCOUNT hll2"), ":3|");
    assert_eq!(redis.text("PFADD some-other-hll 1 2 3"), ":1|");
    assert_eq!(redis.text("PFCOUNT hll2 some-other-hll"), ":6|");

    assert_eq!(redis.text("PFADD hll3 foo bar zap a"), ":1|");
    assert_eq!(redis.text("PFADD hll4 a b c foo"), ":1|");
    assert_eq!(redis.text("PFMERGE hll5 hll3 hll4"), "+OK|");
    assert_eq!(redis.text("PFCOUNT hll5"), ":6|");

    assert_eq!(redis.text("PFADD empty"), ":1|");
    assert_eq!(redis.text("PFADD empty"), ":0|");
    assert_eq!(redis.text("PFCOUNT empty"), ":0|");
    assert_eq!(redis.text("PFCOUNT nokey"), ":0|");
    assert_eq!(redis.text("PFCOUNT nokey hll"), ":7|");
    assert_eq!(redis.text("TYPE hll"), "+string|");
}

#[test]
fn a_hyperloglog_is_the_documented_hyll_string_and_caches_its_count() {
    let mut redis = Harness::new();
    assert_eq!(redis.text("PFADD hll a b c d e f g"), ":1|");
    let sketch = redis.value("hll");
    assert_eq!(&sketch[..5], b"HYLL\x01");
    assert_eq!(&sketch[5..8], b"\x00\x00\x00");
    assert_eq!(sketch[15] & 0x80, 0x80);
    assert!(sketch.len() < 3000);
    assert_eq!(redis.text("PFCOUNT hll"), ":7|");
    let cached = redis.value("hll");
    assert_eq!(&cached[8..16], &7u64.to_le_bytes());
    assert_eq!(&cached[16..], &sketch[16..]);

    redis.run(&[b"SET", b"copy", &cached]);
    assert_eq!(redis.text("PFCOUNT copy"), ":7|");
}

#[test]
fn many_elements_turn_dense_and_count_within_the_standard_error() {
    let mut redis = Harness::new();
    let total = 20_000;
    for batch in 0..20 {
        let mut words = vec!["PFADD".to_owned(), "big".to_owned()];
        words.extend((batch * 1000..(batch + 1) * 1000).map(|index| format!("elem:{index}")));
        assert!(redis.text(&words.join(" ")).starts_with(':'));
    }
    let sketch = redis.value("big");
    assert_eq!(sketch.len(), 16 + 12288);
    assert_eq!(&sketch[..5], b"HYLL\x00");
    let Reply::Integer(count) = redis.run(&[b"PFCOUNT", b"big"]) else {
        panic!("PFCOUNT did not answer an integer");
    };
    let error = (count - total).abs() as f64 / total as f64;
    assert!(error < 3.0 * 0.0081, "{count} is {error} off {total}");
    assert_eq!(count, 19_603);
    assert_eq!(redis.text("PFMERGE copy big hll"), "+OK|");
    assert_eq!(redis.text("PFCOUNT copy"), ":19603|");
    assert_eq!(&redis.value("copy")[..5], b"HYLL\x00");
}

#[test]
fn hyperloglog_commands_refuse_other_values() {
    let mut redis = Harness::new();
    assert_eq!(redis.text("SET plain notasketch"), "+OK|");
    let not_a_sketch = "-WRONGTYPE Key is not a valid HyperLogLog string value.|";
    assert_eq!(redis.text("PFADD plain a"), not_a_sketch);
    assert_eq!(redis.text("PFCOUNT plain"), not_a_sketch);
    assert_eq!(redis.text("PFMERGE plain"), not_a_sketch);
    assert_eq!(redis.text("RPUSH list a"), ":1|");
    assert_eq!(
        redis.text("PFCOUNT list"),
        "-WRONGTYPE Operation against a key holding the wrong kind of value|"
    );
    redis.run(&[
        b"SET",
        b"short",
        b"HYLL\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
    ]);
    assert_eq!(
        redis.text("PFCOUNT short"),
        "-INVALIDOBJ Corrupted HLL object detected|"
    );
    redis.run(&[
        b"SET",
        b"dense",
        b"HYLL\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
    ]);
    assert_eq!(redis.text("PFADD dense a"), not_a_sketch);
    assert_eq!(
        redis.text("PFCOUNT"),
        "-ERR wrong number of arguments for 'pfcount' command|"
    );
}

fn sicily(redis: &mut Harness) {
    assert_eq!(
        redis.text("GEOADD Sicily 13.361389 38.115556 Palermo 15.087269 37.502669 Catania"),
        ":2|"
    );
}

const PALERMO: &str = "*2|$20|13.36138933897018433|$20|38.11555639549629859|";
const CATANIA: &str = "*2|$20|15.08726745843887329|$20|37.50266842333162032|";

#[test]
fn geoadd_geodist_geopos_and_geohash_follow_the_command_pages_examples() {
    let mut redis = Harness::new();
    sicily(&mut redis);
    assert_eq!(
        redis.text("GEODIST Sicily Palermo Catania"),
        "$11|166274.1516|"
    );
    assert_eq!(
        redis.text("GEODIST Sicily Palermo Catania km"),
        "$8|166.2742|"
    );
    assert_eq!(
        redis.text("GEODIST Sicily Palermo Catania mi"),
        "$8|103.3182|"
    );
    assert_eq!(redis.text("GEODIST Sicily Foo Bar"), "$-1|");
    assert_eq!(
        redis.text("GEOPOS Sicily Palermo Catania NonExisting"),
        format!("*3|{PALERMO}{CATANIA}*-1|")
    );
    assert_eq!(
        redis.text("GEOHASH Sicily Palermo Catania"),
        "*2|$11|sqc8b49rny0|$11|sqdtr74hyu0|"
    );
    assert_eq!(
        redis.text("ZRANGE Sicily 0 -1 WITHSCORES"),
        "*4|$7|Palermo|$16|3479099956230698|$7|Catania|$16|3479447370796909|"
    );
    assert_eq!(redis.text("TYPE Sicily"), "+zset|");
    assert_eq!(
        redis.text("GEOADD Sicily XX CH 13.361389 38.115556 Palermo 15 37 Nowhere"),
        ":0|"
    );
    assert_eq!(redis.text("GEOADD Sicily NX 13.5 38 Palermo"), ":0|");
    assert_eq!(redis.text("GEOADD Sicily CH 13.5 38 Palermo"), ":1|");
    assert_eq!(redis.text("ZCARD Sicily"), ":2|");
    assert_eq!(redis.text("GEOPOS nokey a"), "*1|*-1|");
}

#[test]
fn georadius_and_georadiusbymember_follow_the_command_pages_examples() {
    let mut redis = Harness::new();
    sicily(&mut redis);
    assert_eq!(
        redis.text("GEORADIUS Sicily 15 37 200 km WITHDIST"),
        "*2|*2|$7|Palermo|$8|190.4424|*2|$7|Catania|$7|56.4413|"
    );
    assert_eq!(
        redis.text("GEORADIUS Sicily 15 37 200 km WITHCOORD"),
        format!("*2|*2|$7|Palermo|{PALERMO}*2|$7|Catania|{CATANIA}")
    );
    assert_eq!(
        redis.text("GEORADIUS Sicily 15 37 200 km WITHDIST WITHCOORD"),
        format!("*2|*3|$7|Palermo|$8|190.4424|{PALERMO}*3|$7|Catania|$7|56.4413|{CATANIA}")
    );
    assert_eq!(
        redis.text("GEORADIUS Sicily 15 37 100 km"),
        "*1|$7|Catania|"
    );
    assert_eq!(
        redis.text("GEORADIUS_RO Sicily 15 37 200 km ASC"),
        "*2|$7|Catania|$7|Palermo|"
    );
    assert_eq!(
        redis.text("GEORADIUS Sicily 15 37 200 km COUNT 1"),
        "*1|$7|Catania|"
    );
    assert_eq!(
        redis.text("GEORADIUS Sicily 15 37 200 km STORE near"),
        ":2|"
    );
    assert_eq!(redis.text("ZCARD near"), ":2|");
    assert_eq!(redis.text("GEORADIUS Sicily 15 37 1 km STORE near"), ":0|");
    assert_eq!(redis.text("EXISTS near"), ":0|");
    assert_eq!(
        redis.text("GEORADIUS_RO Sicily 15 37 200 km STORE near"),
        "-ERR syntax error|"
    );
    assert_eq!(
        redis.text("GEORADIUS Sicily 15 37 200 km WITHDIST STORE near"),
        "-ERR STORE option in GEORADIUS is not compatible with WITHDIST, WITHHASH and WITHCOORD options|"
    );

    let mut redis = Harness::new();
    assert_eq!(
        redis.text("GEOADD Sicily 13.583333 37.316667 Agrigento"),
        ":1|"
    );
    sicily(&mut redis);
    assert_eq!(
        redis.text("GEORADIUSBYMEMBER Sicily Agrigento 100 km"),
        "*2|$9|Agrigento|$7|Palermo|"
    );
    assert_eq!(
        redis.text("GEORADIUSBYMEMBER_RO Sicily Agrigento 100 km DESC"),
        "*2|$7|Palermo|$9|Agrigento|"
    );
    assert_eq!(
        redis.text("GEORADIUSBYMEMBER Sicily Nobody 100 km"),
        "-ERR could not decode requested zset member|"
    );
}

#[test]
fn geosearch_and_geosearchstore_follow_the_command_pages_examples() {
    let mut redis = Harness::new();
    sicily(&mut redis);
    assert_eq!(
        redis.text("GEOADD Sicily 12.758489 38.788135 edge1 17.241510 38.788135 edge2"),
        ":2|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMLONLAT 15 37 BYRADIUS 200 km ASC"),
        "*2|$7|Catania|$7|Palermo|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMLONLAT 15 37 BYBOX 400 400 km ASC WITHCOORD WITHDIST"),
        format!(
            "*4|*3|$7|Catania|$7|56.4413|{CATANIA}*3|$7|Palermo|$8|190.4424|{PALERMO}\
             *3|$5|edge2|$8|279.7403|*2|$20|17.24151045083999634|$20|38.78813451624225195|\
             *3|$5|edge1|$8|279.7405|*2|$19|12.7584877610206604|$20|38.78813451624225195|"
        )
    );
    assert_eq!(
        redis.text("GEOSEARCHSTORE key1 Sicily FROMLONLAT 15 37 BYBOX 400 400 km ASC COUNT 3"),
        ":3|"
    );
    assert_eq!(
        redis.text(
            "GEOSEARCH key1 FROMLONLAT 15 37 BYBOX 400 400 km ASC WITHCOORD WITHDIST WITHHASH"
        ),
        format!(
            "*3|*4|$7|Catania|$7|56.4413|:3479447370796909|{CATANIA}\
             *4|$7|Palermo|$8|190.4424|:3479099956230698|{PALERMO}\
             *4|$5|edge2|$8|279.7403|:3481342659049484|*2|$20|17.24151045083999634|$20|38.78813451624225195|"
        )
    );
    assert_eq!(
        redis.text(
            "GEOSEARCHSTORE key2 Sicily FROMLONLAT 15 37 BYBOX 400 400 km ASC COUNT 3 STOREDIST"
        ),
        ":3|"
    );
    assert_eq!(
        redis.text("ZRANGE key2 0 -1 WITHSCORES"),
        "*6|$7|Catania|$16|56.4412578701582|$7|Palermo|$18|190.44242984775784|$5|edge2|$17|279.7403417843143|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMMEMBER Palermo BYRADIUS 100 km"),
        "*2|$7|Palermo|$5|edge1|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMLONLAT 15 37 BYBOX 400 400 km COUNT 1 ANY"),
        "*1|$7|Palermo|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMLONLAT 15 37 BYBOX 400 400 km COUNT 1"),
        "*1|$7|Catania|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMLONLAT 15 37 BYRADIUS 200 km DESC"),
        "*2|$7|Palermo|$7|Catania|"
    );
    assert_eq!(
        redis.text("GEOSEARCH nokey FROMMEMBER a BYRADIUS 1 km"),
        "*0|"
    );
    assert_eq!(
        redis.text("GEOSEARCHSTORE key1 nokey FROMLONLAT 15 37 BYRADIUS 1 km"),
        ":0|"
    );
    assert_eq!(redis.text("EXISTS key1"), ":0|");
}

#[test]
fn geo_commands_refuse_bad_input_with_redis_errors() {
    let mut redis = Harness::new();
    sicily(&mut redis);
    assert_eq!(
        redis.text("GEOADD Sicily 181 0 nowhere"),
        "-ERR invalid longitude,latitude pair 181.000000,0.000000|"
    );
    assert_eq!(
        redis.text("GEOADD Sicily 0 85.06 nowhere"),
        "-ERR invalid longitude,latitude pair 0.000000,85.060000|"
    );
    assert_eq!(
        redis.text("GEOADD Sicily x 0 nowhere"),
        "-ERR value is not a valid float|"
    );
    assert_eq!(
        redis.text("GEOADD Sicily NX XX 1 1 a"),
        "-ERR syntax error|"
    );
    assert_eq!(redis.text("GEOADD Sicily 1 1 a 2"), "-ERR syntax error|");
    assert_eq!(
        redis.text("GEODIST Sicily Palermo Catania yards"),
        "-ERR unsupported unit provided. please use M, KM, FT, MI|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily BYRADIUS 1 km WITHDIST WITHHASH"),
        "-ERR exactly one of FROMMEMBER or FROMLONLAT can be specified for GEOSEARCH|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMLONLAT 15 37 WITHDIST WITHHASH"),
        "-ERR exactly one of BYRADIUS and BYBOX can be specified for GEOSEARCH|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMLONLAT 15 37 FROMMEMBER Palermo BYRADIUS 1 km"),
        "-ERR FROMMEMBER and FROMLONLAT cannot both be specified for GEOSEARCH|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMLONLAT 15 37 BYRADIUS 1 km BYBOX 1 1 km"),
        "-ERR BYRADIUS and BYBOX cannot both be specified for GEOSEARCH|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMLONLAT 15 37 BYRADIUS 1 km ANY"),
        "-ERR the ANY argument requires COUNT argument|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMLONLAT 15 37 BYRADIUS 1 km COUNT 0"),
        "-ERR COUNT must be > 0|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMMEMBER Nobody BYRADIUS 1 km"),
        "-ERR could not decode requested zset member|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMLONLAT 15 37 BYRADIUS -1 km"),
        "-ERR radius cannot be negative|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMLONLAT 15 37 BYBOX 1 -1 km"),
        "-ERR height or width cannot be negative|"
    );
    assert_eq!(
        redis.text("GEORADIUS Sicily 15 37 far km"),
        "-ERR need numeric radius|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily FROMLONLAT 15 37 BYRADIUS 1 km STORE x"),
        "-ERR syntax error|"
    );
    assert_eq!(
        redis.text("GEOSEARCHSTORE d Sicily FROMLONLAT 15 37 BYRADIUS 200 km WITHDIST"),
        "-ERR GEOSEARCHSTORE is not compatible with WITHDIST, WITHHASH and WITHCOORD options|"
    );
    assert_eq!(redis.text("SET plain v"), "+OK|");
    assert_eq!(
        redis.text("GEOADD plain 1 1 a"),
        "-WRONGTYPE Operation against a key holding the wrong kind of value|"
    );
    assert_eq!(
        redis.text("GEOSEARCH plain FROMLONLAT 1 1 BYRADIUS 1 km"),
        "-WRONGTYPE Operation against a key holding the wrong kind of value|"
    );
}

fn wire_request(words: &[&[u8]]) -> Vec<u8> {
    let mut request = format!("*{}\r\n", words.len()).into_bytes();
    for word in words {
        request.extend_from_slice(format!("${}\r\n", word.len()).as_bytes());
        request.extend_from_slice(word);
        request.extend_from_slice(b"\r\n");
    }
    request
}

#[test]
fn a_redis_cli_session_over_the_wire_gets_resp_replies() {
    let mut stream = Vec::new();
    stream.extend(wire_request(&[b"SETBIT", b"bits", b"9", b"1"]));
    stream.extend(wire_request(&[b"BITCOUNT", b"bits"]));
    stream.extend(wire_request(&[b"BITFIELD", b"bits", b"GET", b"u16", b"0"]));
    stream.extend_from_slice(b"PFADD visitors alice bob carol\r\n");
    stream.extend_from_slice(b"PFCOUNT visitors\r\n");
    stream.extend(wire_request(&[
        b"GEOADD",
        b"Sicily",
        b"13.361389",
        b"38.115556",
        b"Palermo",
        b"15.087269",
        b"37.502669",
        b"Catania",
    ]));
    stream.extend_from_slice(b"GEODIST Sicily Palermo Catania km\r\n");
    stream.extend(wire_request(&[b"HELLO", b"3"]));
    stream.extend_from_slice(b"GEOPOS Sicily Nowhere\r\n");
    let mut reader = RequestReader::new(stream.as_slice());
    let keyspace = MemoryKeyspace::default();
    let mut client = Client {
        authenticated: true,
        ..Client::default()
    };
    let mut answers = Vec::new();
    while let Ok(request) = reader.next_request() {
        let context = Context {
            keyspace: &keyspace,
            password_sha256: None,
            now_ms: 1_000_000,
            reply_room_bytes: usize::MAX,
            blocked_clients: 0,
        };
        let reply = execute(&context, &mut client, &request);
        let mut out = Vec::new();
        reply.encode(&mut out, client.resp3);
        answers.push(String::from_utf8_lossy(&out).into_owned());
    }
    assert_eq!(answers.len(), 9);
    assert_eq!(answers[0], ":0\r\n");
    assert_eq!(answers[1], ":1\r\n");
    assert_eq!(answers[2], "*1\r\n:64\r\n");
    assert_eq!(answers[3], ":1\r\n");
    assert_eq!(answers[4], ":3\r\n");
    assert_eq!(answers[5], ":2\r\n");
    assert_eq!(answers[6], "$8\r\n166.2742\r\n");
    assert_eq!(answers[8], "*1\r\n_\r\n");
}

#[test]
fn geo_commands_check_their_arity_as_redis_does() {
    let mut redis = Harness::new();
    sicily(&mut redis);
    assert_eq!(
        redis.text("GEOSEARCH Sicily BYRADIUS 1 km"),
        "-ERR wrong number of arguments for 'geosearch' command|"
    );
    assert_eq!(
        redis.text("GEOSEARCH Sicily"),
        "-ERR wrong number of arguments for 'geosearch' command|"
    );
    assert_eq!(
        redis.text("GEOSEARCHSTORE d Sicily FROMLONLAT 15 37 BYRADIUS"),
        "-ERR wrong number of arguments for 'geosearchstore' command|"
    );
    assert_eq!(
        redis.text("GEODIST Sicily Palermo"),
        "-ERR wrong number of arguments for 'geodist' command|"
    );
    assert_eq!(
        redis.text("GEODIST Sicily Palermo Catania km extra"),
        "-ERR syntax error|"
    );
    assert_eq!(
        redis.text("GEODIST Sicily Palermo Catania km"),
        "$8|166.2742|"
    );
}

fn sketch_bytes(encoding: u8, stale: bool, body: &[u8]) -> Vec<u8> {
    let mut bytes = b"HYLL".to_vec();
    bytes.extend([encoding, 0, 0, 0]);
    bytes.extend([0; 7]);
    bytes.push(if stale { 0x80 } else { 0 });
    bytes.extend_from_slice(body);
    bytes
}

#[test]
fn crafted_sketches_are_refused_or_counted_without_overflow() {
    let mut redis = Harness::new();
    let corrupted = "-INVALIDOBJ Corrupted HLL object detected|";
    redis.run(&[b"SET", b"full", &sketch_bytes(1, true, &[0x7F, 0xFF])]);
    assert_eq!(redis.text("PFCOUNT full"), ":0|");
    redis.run(&[b"SET", b"past", &sketch_bytes(1, true, &[0x7F, 0xFF, 0x00])]);
    assert_eq!(redis.text("PFCOUNT past"), corrupted);
    redis.run(&[b"SET", b"cut", &sketch_bytes(1, true, &[0x40])]);
    assert_eq!(redis.text("PFCOUNT cut"), corrupted);
    redis.run(&[b"SET", b"top", &sketch_bytes(0, true, &[0xFF; 12_288])]);
    let largest = format!(":{}|", i64::MAX);
    assert_eq!(redis.text("PFCOUNT top"), largest);
    let counted = redis.value("top");
    assert_eq!(counted[8..16], i64::MAX.to_le_bytes());
    assert_eq!(redis.text("PFCOUNT top"), largest);
    assert_eq!(redis.value("top"), counted);
    assert_eq!(redis.text("PFCOUNT top full"), largest);
}

#[test]
fn writes_keep_or_drop_the_expiry_as_redis_does() {
    let mut redis = Harness::new();
    assert_eq!(redis.text("SETBIT bits 0 1"), ":0|");
    assert_eq!(redis.text("EXPIRE bits 100"), ":1|");
    assert_eq!(redis.text("SETBIT bits 9 1"), ":0|");
    assert_eq!(redis.text("TTL bits"), ":100|");
    assert_eq!(redis.text("PFADD visits a"), ":1|");
    assert_eq!(redis.text("EXPIRE visits 100"), ":1|");
    assert_eq!(redis.text("PFADD visits b"), ":1|");
    assert_eq!(redis.text("TTL visits"), ":100|");
    assert_eq!(redis.text("GEOADD places 1 1 a"), ":1|");
    assert_eq!(redis.text("EXPIRE places 100"), ":1|");
    assert_eq!(redis.text("GEOADD places 1.5 1.5 b"), ":1|");
    assert_eq!(redis.text("TTL places"), ":100|");
    assert_eq!(redis.text("SET combined x EX 100"), "+OK|");
    assert_eq!(redis.text("BITOP OR combined bits"), ":2|");
    assert_eq!(redis.text("TTL combined"), ":-1|");
    assert_eq!(redis.text("SET near x EX 100"), "+OK|");
    assert_eq!(
        redis.text("GEOSEARCHSTORE near places FROMLONLAT 1 1 BYRADIUS 500 km"),
        ":2|"
    );
    assert_eq!(redis.text("TTL near"), ":-1|");
}

#[test]
fn setbit_and_pfadd_queue_in_multi_and_touch_watched_keys() {
    let mut redis = Harness::new();
    assert_eq!(redis.transacted("MULTI"), "+OK|");
    assert_eq!(redis.transacted("SETBIT bits 3 1"), "+QUEUED|");
    assert_eq!(redis.transacted("PFADD visits a"), "+QUEUED|");
    assert_eq!(redis.transacted("EXEC"), "*2|:0|:1|");
    assert_eq!(redis.text("GETBIT bits 3"), ":1|");
    assert_eq!(redis.text("PFCOUNT visits"), ":1|");
    assert_eq!(redis.transacted("WATCH bits visits"), "+OK|");
    assert_eq!(redis.transacted("SETBIT bits 4 1"), ":0|");
    assert_eq!(redis.transacted("MULTI"), "+OK|");
    assert_eq!(redis.transacted("GETBIT bits 4"), "+QUEUED|");
    assert_eq!(redis.transacted("EXEC"), "*-1|");
    assert_eq!(redis.transacted("WATCH visits"), "+OK|");
    assert_eq!(redis.transacted("PFADD visits b"), ":1|");
    assert_eq!(redis.transacted("MULTI"), "+OK|");
    assert_eq!(redis.transacted("PFCOUNT visits"), "+QUEUED|");
    assert_eq!(redis.transacted("EXEC"), "*-1|");
}
