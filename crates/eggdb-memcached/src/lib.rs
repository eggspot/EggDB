//! EggDB's server for memcached's text protocol and the basic meta commands (D194), written from
//! memcached's published protocol description, storing items in an EggDB keyspace (each command
//! one transaction), so memcached clients connect unchanged.

mod commands;
mod item;
mod server;

pub use commands::{Mode, Shared, Stored};
pub use item::{Item, expiry_of};
pub use server::{ServerConfig, serve, serve_stream};

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_resp::MemoryKeyspace;
    use std::sync::atomic::AtomicU64;

    fn converse(shared: &Shared<'_>, input: &[u8]) -> String {
        let mut out = Vec::new();
        serve_stream(shared, input, &mut out).unwrap();
        String::from_utf8(out).unwrap().replace("\r\n", "|")
    }

    #[test]
    fn the_text_protocol_stores_reads_and_counts() {
        let keyspace = MemoryKeyspace::default();
        let shared = Shared { keyspace: &keyspace, next_cas: AtomicU64::new(1) };
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
        let shared = Shared { keyspace: &keyspace, next_cas: AtomicU64::new(100) };
        let answer = converse(
            &shared,
            b"set k 0 0 1\r\na\r\ncas k 0 0 1 999\r\nb\r\ncas k 0 0 1 100\r\nc\r\nget k\r\ntouch k 100\r\ntouch nope 1\r\nms m 2 F3\r\nhi\r\nmg m v f\r\nmg nope v\r\nmd m\r\nmn\r\n",
        );
        assert_eq!(
            answer,
            "STORED|EXISTS|STORED|VALUE k 0 1|c|END|TOUCHED|NOT_FOUND|HD|VA 2 f3|hi|EN|HD|MN|"
        );
    }
}
