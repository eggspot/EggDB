use crate::commands::{Client, Context, execute};
use crate::protocol::Reply;
use std::net::SocketAddr;

/// Redis Cluster's hash slots.
pub const SLOTS: u16 = 16_384;
const LAST_SLOT: i64 = SLOTS as i64 - 1;
const CLUSTER_BUS_PORT_OFFSET: u16 = 10_000;
const NODE_ID_LENGTH: usize = 40;

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// The hash slot of `key`: CRC16 of its hash tag (the bytes between the first `{` and the next
/// `}`, when there are any) or of the whole key, modulo 16,384.
pub fn key_slot(key: &[u8]) -> u16 {
    let hashed = key
        .iter()
        .position(|&byte| byte == b'{')
        .and_then(|open| {
            let rest = &key[open + 1..];
            let close = rest.iter().position(|&byte| byte == b'}')?;
            (close > 0).then(|| &rest[..close])
        })
        .unwrap_or(key);
    crc16(hashed) % SLOTS
}

/// The node id this server gives itself: forty hex digits drawn from its address, so it stays
/// the same while the server keeps its address.
pub fn node_id(address: SocketAddr) -> String {
    let text = address.to_string();
    let mut id = String::with_capacity(NODE_ID_LENGTH);
    let mut seed = u64::from(crc16(text.as_bytes())) | 0x9e37_79b9_7f4a_7c15;
    while id.len() < NODE_ID_LENGTH {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        id.push_str(&format!("{seed:016x}"));
    }
    id.truncate(NODE_ID_LENGTH);
    id
}

fn error(text: &str) -> Reply {
    Reply::Error(text.to_owned())
}

fn slot_argument(argument: Option<&Vec<u8>>) -> Result<u16, Reply> {
    std::str::from_utf8(argument.map(Vec::as_slice).unwrap_or_default())
        .ok()
        .and_then(|text| text.parse::<i64>().ok())
        .filter(|slot| (0..=LAST_SLOT).contains(slot))
        .map(|slot| slot as u16)
        .ok_or_else(|| error("ERR Invalid or out of range slot"))
}

fn keys_in_slot(context: &Context<'_>, client: &mut Client, slot: u16) -> Vec<Vec<u8>> {
    let listed = execute(context, client, &[b"keys".to_vec(), b"*".to_vec()]);
    match listed {
        Reply::Array(Some(items)) => items
            .into_iter()
            .filter_map(|item| match item {
                Reply::Bulk(Some(key)) => Some(key),
                _ => None,
            })
            .filter(|key| key_slot(key) == slot)
            .collect(),
        _ => Vec::new(),
    }
}

fn node_entry(address: SocketAddr, id: &str) -> Reply {
    Reply::Array(Some(vec![
        Reply::bulk(address.ip().to_string()),
        Reply::Integer(i64::from(address.port())),
        Reply::bulk(id.to_owned()),
    ]))
}

/// `CLUSTER ...` on a server that is a cluster of one node holding every slot: enough for
/// cluster clients to connect, route every key here and never meet `MOVED`.
pub fn command(
    context: &Context<'_>,
    client: &mut Client,
    args: &[Vec<u8>],
    address: SocketAddr,
) -> Reply {
    let id = node_id(address);
    let sub = args
        .first()
        .map(|first| String::from_utf8_lossy(first).to_ascii_lowercase())
        .unwrap_or_default();
    match sub.as_str() {
        "info" => Reply::bulk(
            [
                "cluster_state:ok",
                "cluster_slots_assigned:16384",
                "cluster_slots_ok:16384",
                "cluster_slots_pfail:0",
                "cluster_slots_fail:0",
                "cluster_known_nodes:1",
                "cluster_size:1",
                "cluster_current_epoch:1",
                "cluster_my_epoch:1",
                "",
            ]
            .join("\r\n"),
        ),
        "myid" => Reply::bulk(id),
        "myshardid" => Reply::bulk(node_id(SocketAddr::new(address.ip(), 0))),
        "keyslot" => match args.get(1) {
            Some(key) if args.len() == 2 => Reply::Integer(i64::from(key_slot(key))),
            _ => error("ERR wrong number of arguments for 'cluster|keyslot' command"),
        },
        "countkeysinslot" => match slot_argument(args.get(1)) {
            Ok(slot) => Reply::Integer(keys_in_slot(context, client, slot).len() as i64),
            Err(reply) => reply,
        },
        "getkeysinslot" => {
            let slot = match slot_argument(args.get(1)) {
                Ok(slot) => slot,
                Err(reply) => return reply,
            };
            let Some(count) = args
                .get(2)
                .and_then(|count| std::str::from_utf8(count).ok())
                .and_then(|count| count.parse::<usize>().ok())
            else {
                return error("ERR Invalid number of keys");
            };
            Reply::Array(Some(
                keys_in_slot(context, client, slot)
                    .into_iter()
                    .take(count)
                    .map(Reply::bulk)
                    .collect(),
            ))
        }
        "slots" => Reply::Array(Some(vec![Reply::Array(Some(vec![
            Reply::Integer(0),
            Reply::Integer(LAST_SLOT),
            node_entry(address, &id),
        ]))])),
        "shards" => Reply::Array(Some(vec![Reply::Array(Some(vec![
            Reply::bulk("slots"),
            Reply::Array(Some(vec![Reply::Integer(0), Reply::Integer(LAST_SLOT)])),
            Reply::bulk("nodes"),
            Reply::Array(Some(vec![Reply::Array(Some(vec![
                Reply::bulk("id"),
                Reply::bulk(id),
                Reply::bulk("port"),
                Reply::Integer(i64::from(address.port())),
                Reply::bulk("ip"),
                Reply::bulk(address.ip().to_string()),
                Reply::bulk("endpoint"),
                Reply::bulk(address.ip().to_string()),
                Reply::bulk("role"),
                Reply::bulk("master"),
                Reply::bulk("replication-offset"),
                Reply::Integer(0),
                Reply::bulk("health"),
                Reply::bulk("online"),
            ]))])),
        ]))])),
        "nodes" => Reply::bulk(format!(
            "{id} {}:{}@{} myself,master - 0 0 1 connected 0-{LAST_SLOT}\n",
            address.ip(),
            address.port(),
            address.port().saturating_add(CLUSTER_BUS_PORT_OFFSET)
        )),
        "replicas" | "slaves" => Reply::Array(Some(Vec::new())),
        "" => error("ERR wrong number of arguments for 'cluster' command"),
        other => Reply::Error(format!(
            "ERR unknown subcommand '{other}'. Try CLUSTER HELP."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_slots_follow_redis_cluster_and_hash_tags() {
        assert_eq!(crc16(b"123456789"), 0x31c3);
        assert_eq!(key_slot(b"foo"), 12_182);
        assert_eq!(key_slot(b"{user1000}.following"), key_slot(b"{user1000}.followers"));
        assert_eq!(key_slot(b"{user1000}.following"), key_slot(b"user1000"));
        assert_eq!(key_slot(b"foo{}{bar}"), key_slot(b"foo{}{bar}"));
        assert_ne!(key_slot(b"foo{}{bar}"), key_slot(b"bar"));
    }

    #[test]
    fn the_node_id_is_forty_hex_digits_and_stable() {
        let address: SocketAddr = "127.0.0.1:6379".parse().expect("an address");
        let id = node_id(address);
        assert_eq!(id.len(), 40);
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(id, node_id(address));
    }
}
