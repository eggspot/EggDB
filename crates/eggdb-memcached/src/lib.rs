//! EggDB's server for memcached's text protocol, the basic meta commands (D194) and the binary
//! protocol (D398), written from memcached's published protocol descriptions, storing items in an
//! EggDB keyspace (each command one transaction), so memcached clients connect unchanged.

mod binary;
mod commands;
mod item;
mod lru;
mod meta;
mod server;

pub use commands::{Arithmetic, Counters, Mode, Shared, Stored};
pub use item::{Item, expiry_of};
pub use lru::{Lru, item_bytes};
pub use server::{ServerConfig, serve, serve_stream};

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::MemoryKeyspace;
    use eggdb_platform::connection_limits::MemoryBudget;
    use std::sync::atomic::AtomicU64;

    fn converse(shared: &Shared<'_>, input: &[u8]) -> String {
        converse_within(shared, &MemoryBudget::default(), input)
    }

    fn converse_within(shared: &Shared<'_>, budget: &MemoryBudget, input: &[u8]) -> String {
        let mut out = Vec::new();
        serve_stream(shared, budget, input, &mut out).unwrap();
        String::from_utf8(out).unwrap().replace("\r\n", "|")
    }

    #[test]
    fn the_text_protocol_stores_reads_and_counts() {
        let keyspace = MemoryKeyspace::default();
        let shared = Shared {
            keyspace: &keyspace,
            next_cas: AtomicU64::new(1),
            counters: Default::default(),
            lru: Default::default(),
        };
        let answer = converse(
            &shared,
            b"set k 5 0 5\r\nhello\r\nget k nope\r\nadd k 0 0 1\r\nx\r\nappend k 0 0 1\r\n!\r\ngets k\r\nincr n 1\r\nset n 0 0 2\r\n41\r\nincr n 1\r\ndecr n 100\r\ndelete k\r\ndelete k\r\nset q 0 0 1 noreply\r\nz\r\nget q\r\nbogus\r\nversion\r\nquit\r\nget q\r\n",
        );
        assert_eq!(
            answer,
            "STORED|VALUE k 5 5|hello|END|NOT_STORED|STORED|VALUE k 5 6 3|hello!|END|NOT_FOUND|STORED|42|0|DELETED|NOT_FOUND|VALUE q 0 1|z|END|ERROR|VERSION 1.6.0 EggDB|"
        );
    }

    #[test]
    fn cas_touch_and_the_meta_commands_follow_the_protocol() {
        let keyspace = MemoryKeyspace::default();
        let shared = Shared {
            keyspace: &keyspace,
            next_cas: AtomicU64::new(100),
            counters: Default::default(),
            lru: Default::default(),
        };
        let answer = converse(
            &shared,
            b"set k 0 0 1\r\na\r\ncas k 0 0 1 999\r\nb\r\ncas k 0 0 1 100\r\nc\r\nget k\r\ntouch k 100\r\ntouch nope 1\r\nms m 2 F3\r\nhi\r\nmg m v f\r\nmg nope v\r\nmd m\r\nmn\r\n",
        );
        assert_eq!(
            answer,
            "STORED|EXISTS|STORED|VALUE k 0 1|c|END|TOUCHED|NOT_FOUND|HD|VA 2 f3|hi|EN|HD|MN|"
        );
    }

    #[test]
    fn values_and_replies_past_the_memory_budget_get_out_of_memory_and_the_client_goes_on() {
        let keyspace = MemoryKeyspace::default();
        let shared = Shared {
            keyspace: &keyspace,
            next_cas: AtomicU64::new(1),
            counters: Default::default(),
            lru: Default::default(),
        };
        let budget = MemoryBudget::new(256 * 1024);
        let value = "v".repeat(200 * 1024);
        let mut input = Vec::new();
        for key in ["a", "b"] {
            input.extend(format!("set {key} 0 0 {}\r\n{value}\r\n", value.len()).into_bytes());
        }
        input.extend(b"get a\r\nget a b\r\n".to_vec());
        input.extend(format!("set c 0 0 300000\r\n{}\r\n", "c".repeat(300_000)).into_bytes());
        input.extend(b"version\r\n".to_vec());
        let answer = converse_within(&shared, &budget, &input);
        let expected = format!(
            "STORED|STORED|VALUE a 0 {}|{value}|END|SERVER_ERROR out of memory|SERVER_ERROR out of memory storing object|VERSION 1.6.0 EggDB|",
            value.len()
        );
        assert!(
            answer == expected,
            "{}",
            &answer[answer.len().saturating_sub(200)..]
        );
        assert_eq!(budget.used_bytes(), 0);
        assert_eq!(converse(&shared, b"get c\r\n"), "END|");
    }
    #[test]
    fn a_multi_get_of_many_items_past_the_memory_budget_stops_before_copying_them_all() {
        let keyspace = MemoryKeyspace::default();
        let shared = Shared {
            keyspace: &keyspace,
            next_cas: AtomicU64::new(1),
            counters: Default::default(),
            lru: Default::default(),
        };
        let budget = MemoryBudget::new(256 * 1024);
        let value = "v".repeat(60 * 1024);
        let keys: Vec<String> = (0..12).map(|at| format!("k{at}")).collect();
        let mut input = Vec::new();
        for key in &keys {
            input.extend(format!("set {key} 0 0 {}\r\n{value}\r\n", value.len()).into_bytes());
        }
        input.extend(format!("get {}\r\n", keys.join(" ")).into_bytes());
        input.extend(format!("gat 100 {}\r\n", keys.join(" ")).into_bytes());
        input.extend(b"get k0\r\nversion\r\n".to_vec());
        let answer = converse_within(&shared, &budget, &input);
        let expected = format!(
            "{}SERVER_ERROR out of memory|SERVER_ERROR out of memory|VALUE k0 0 {}|{value}|END|VERSION 1.6.0 EggDB|",
            "STORED|".repeat(keys.len()),
            value.len()
        );
        assert!(
            answer == expected,
            "{}",
            &answer[answer.len().saturating_sub(200)..]
        );
        assert_eq!(budget.used_bytes(), 0);
        assert_eq!(
            converse(&shared, b"mg k0 t\r\n"),
            "HD t-1|",
            "a refused gat touches nothing"
        );
    }

    fn binary_request(opcode: u8, extras: &[u8], key: &[u8], value: &[u8], opaque: u32) -> Vec<u8> {
        let mut out = vec![0x80, opcode];
        out.extend_from_slice(&(key.len() as u16).to_be_bytes());
        out.push(extras.len() as u8);
        out.push(0);
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&((extras.len() + key.len() + value.len()) as u32).to_be_bytes());
        out.extend_from_slice(&opaque.to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(extras);
        out.extend_from_slice(key);
        out.extend_from_slice(value);
        out
    }

    fn binary_responses(bytes: &[u8]) -> Vec<(u8, u16, u32, Vec<u8>)> {
        let mut found = Vec::new();
        let mut at = 0;
        while at + 24 <= bytes.len() {
            let header = &bytes[at..at + 24];
            let opcode = header[1];
            let status = u16::from_be_bytes([header[6], header[7]]);
            let body = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
            let opaque = u32::from_be_bytes([header[12], header[13], header[14], header[15]]);
            let key = u16::from_be_bytes([header[2], header[3]]) as usize;
            let extras = header[4] as usize;
            let value = bytes[at + 24 + extras + key..at + 24 + body].to_vec();
            found.push((opcode, status, opaque, value));
            at += 24 + body;
        }
        found
    }

    #[test]
    fn the_binary_protocol_stores_reads_counts_and_stays_quiet_when_asked() {
        let keyspace = MemoryKeyspace::default();
        let shared = Shared {
            keyspace: &keyspace,
            next_cas: AtomicU64::new(1),
            counters: Default::default(),
            lru: Default::default(),
        };
        let mut set_extras = 7u32.to_be_bytes().to_vec();
        set_extras.extend_from_slice(&0u32.to_be_bytes());
        let mut counter_extras = 5u64.to_be_bytes().to_vec();
        counter_extras.extend_from_slice(&10u64.to_be_bytes());
        counter_extras.extend_from_slice(&0u32.to_be_bytes());
        let mut input = Vec::new();
        input.extend(binary_request(0x01, &set_extras, b"k", b"hello", 1));
        input.extend(binary_request(0x00, &[], b"k", b"", 2));
        input.extend(binary_request(0x09, &[], b"missing", b"", 3));
        input.extend(binary_request(0x00, &[], b"missing", b"", 4));
        input.extend(binary_request(0x05, &counter_extras, b"n", b"", 5));
        input.extend(binary_request(0x05, &counter_extras, b"n", b"", 6));
        input.extend(binary_request(0x11, &set_extras, b"q", b"z", 7));
        input.extend(binary_request(0x0a, &[], b"", b"", 8));
        input.extend(binary_request(0x07, &[], b"", b"", 9));
        let mut out = Vec::new();
        serve_stream(&shared, &MemoryBudget::default(), input.as_slice(), &mut out).unwrap();
        let responses = binary_responses(&out);
        let opaques: Vec<u32> = responses.iter().map(|(_, _, opaque, _)| *opaque).collect();
        assert_eq!(opaques, vec![1, 2, 4, 5, 6, 8, 9]);
        assert_eq!(responses[1].3, b"hello".to_vec());
        assert_eq!(responses[2].1, 0x0001);
        assert_eq!(responses[3].3, 10u64.to_be_bytes().to_vec());
        assert_eq!(responses[4].3, 15u64.to_be_bytes().to_vec());
    }

    #[test]
    fn items_past_the_memory_limit_evict_the_least_recently_used() {
        let keyspace = MemoryKeyspace::default();
        let value = "v".repeat(1000);
        let room = 3 * item_bytes(b"k0", &Item {
            data: value.as_bytes().to_vec(),
            flags: 0,
            cas: 0,
            expires_at_ms: None,
        }
        .encode());
        let shared = Shared {
            keyspace: &keyspace,
            next_cas: AtomicU64::new(1),
            counters: Default::default(),
            lru: Lru::with_limit(room),
        };
        let mut input = Vec::new();
        for key in ["k0", "k1", "k2"] {
            input.extend(format!("set {key} 0 0 {}\r\n{value}\r\n", value.len()).into_bytes());
        }
        input.extend(b"get k0\r\n".to_vec());
        input.extend(format!("set k3 0 0 {}\r\n{value}\r\n", value.len()).into_bytes());
        converse(&shared, &input);
        assert_eq!(converse(&shared, b"get k1\r\n"), "END|");
        assert!(converse(&shared, b"get k0\r\n").starts_with("VALUE k0 "));
        assert!(converse(&shared, b"get k3\r\n").starts_with("VALUE k3 "));
        assert_eq!(shared.lru.evictions(), 1);
        let stats = converse(&shared, b"stats\r\n");
        assert!(stats.contains("STAT evictions 1|"), "{stats}");
        assert!(stats.contains(&format!("STAT limit_maxbytes {room}|")), "{stats}");
        assert!(stats.contains("STAT curr_items 3|"), "{stats}");
    }
}

