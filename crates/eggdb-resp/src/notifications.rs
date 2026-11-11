use std::sync::atomic::{AtomicU32, Ordering};

const KEYSPACE: u32 = 1;
const KEYEVENT: u32 = 1 << 1;
const GENERIC: u32 = 1 << 2;
const STRING: u32 = 1 << 3;
const LIST: u32 = 1 << 4;
const SET: u32 = 1 << 5;
const HASH: u32 = 1 << 6;
const ZSET: u32 = 1 << 7;
const EXPIRED: u32 = 1 << 8;
const EVICTED: u32 = 1 << 9;
const STREAM: u32 = 1 << 10;
const KEY_MISS: u32 = 1 << 11;
const NEW: u32 = 1 << 12;
const MODULE: u32 = 1 << 13;
const ALL: u32 = GENERIC | STRING | LIST | SET | HASH | ZSET | EXPIRED | EVICTED | STREAM | MODULE;
const CLASSES: [(char, u32); 12] = [
    ('g', GENERIC),
    ('$', STRING),
    ('l', LIST),
    ('s', SET),
    ('h', HASH),
    ('z', ZSET),
    ('x', EXPIRED),
    ('e', EVICTED),
    ('t', STREAM),
    ('m', KEY_MISS),
    ('n', NEW),
    ('d', MODULE),
];

/// The name of the `CONFIG` parameter that turns keyspace notifications on.
pub(crate) const PARAMETER: &str = "notify-keyspace-events";

/// Parses `notify-keyspace-events` flags such as `KEA` or `Kx`.
pub(crate) fn parse(text: &[u8]) -> Result<u32, String> {
    let mut flags = 0;
    for byte in text {
        flags |= match byte {
            b'A' => ALL,
            b'K' => KEYSPACE,
            b'E' => KEYEVENT,
            other => CLASSES
                .iter()
                .find(|(class, _)| *class as u8 == *other)
                .map(|(_, flag)| *flag)
                .ok_or_else(|| "ERR Invalid event class character. Use 'Ag$lshzxeKEtmdn'.".to_owned())?,
        };
    }
    Ok(flags)
}

/// The flags as `CONFIG GET` shows them, `A` standing for every class it covers.
pub(crate) fn format(flags: u32) -> String {
    let mut text = String::new();
    if flags & ALL == ALL {
        text.push('A');
    }
    for (class, flag) in CLASSES {
        if flags & flag != 0 && (flags & ALL != ALL || flag & ALL == 0) {
            text.push(class);
        }
    }
    if flags & KEYSPACE != 0 {
        text.push('K');
    }
    if flags & KEYEVENT != 0 {
        text.push('E');
    }
    text
}

fn lower(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_ascii_lowercase()
}

fn has_word(args: &[Vec<u8>], word: &str) -> bool {
    args.iter().any(|arg| arg.eq_ignore_ascii_case(word.as_bytes()))
}

fn event_for(command: &str, args: &[Vec<u8>], key: &[u8]) -> Option<(u32, &'static str)> {
    let first = args.get(1).map(Vec::as_slice);
    let second = args.get(2).map(Vec::as_slice);
    let from_right = |position: usize| args.get(position).is_none_or(|word| word.eq_ignore_ascii_case(b"right"));
    Some(match command {
        "set" | "setnx" | "setex" | "psetex" | "getset" | "mset" | "msetnx" | "bitop" => (STRING, "set"),
        "append" => (STRING, "append"),
        "incr" | "incrby" | "decr" | "decrby" => (STRING, "incrby"),
        "incrbyfloat" => (STRING, "incrbyfloat"),
        "setrange" => (STRING, "setrange"),
        "setbit" | "bitfield" => (STRING, "setbit"),
        "pfadd" | "pfmerge" => (STRING, "pfadd"),
        "del" | "unlink" | "getdel" => (GENERIC, "del"),
        "expire" | "pexpire" | "expireat" | "pexpireat" | "getex" => (GENERIC, "expire"),
        "persist" => (GENERIC, "persist"),
        "rename" | "renamenx" if first == Some(key) => (GENERIC, "rename_from"),
        "rename" | "renamenx" => (GENERIC, "rename_to"),
        "copy" => (GENERIC, "copy_to"),
        "restore" => (GENERIC, "restore"),
        "lpush" | "lpushx" => (LIST, "lpush"),
        "rpush" | "rpushx" => (LIST, "rpush"),
        "lpop" | "blpop" => (LIST, "lpop"),
        "rpop" | "brpop" => (LIST, "rpop"),
        "lmpop" | "blmpop" if has_word(args, "right") => (LIST, "rpop"),
        "lmpop" | "blmpop" => (LIST, "lpop"),
        "rpoplpush" | "brpoplpush" if first == Some(key) => (LIST, "rpop"),
        "rpoplpush" | "brpoplpush" => (LIST, "lpush"),
        "lmove" | "blmove" if first == Some(key) && from_right(3) => (LIST, "rpop"),
        "lmove" | "blmove" if first == Some(key) => (LIST, "lpop"),
        "lmove" | "blmove" if from_right(4) => (LIST, "rpush"),
        "lmove" | "blmove" => (LIST, "lpush"),
        "linsert" => (LIST, "linsert"),
        "lset" => (LIST, "lset"),
        "ltrim" => (LIST, "ltrim"),
        "lrem" => (LIST, "lrem"),
        "sort" => (LIST, "sortstore"),
        "sadd" => (SET, "sadd"),
        "srem" => (SET, "srem"),
        "spop" => (SET, "spop"),
        "smove" if first == Some(key) && second != Some(key) => (SET, "srem"),
        "smove" => (SET, "sadd"),
        "sinterstore" => (SET, "sinterstore"),
        "sunionstore" => (SET, "sunionstore"),
        "sdiffstore" => (SET, "sdiffstore"),
        "hset" | "hmset" | "hsetnx" => (HASH, "hset"),
        "hdel" => (HASH, "hdel"),
        "hincrby" => (HASH, "hincrby"),
        "hincrbyfloat" => (HASH, "hincrbyfloat"),
        "zadd" if has_word(args, "incr") => (ZSET, "zincr"),
        "zadd" | "geoadd" => (ZSET, "zadd"),
        "zincrby" => (ZSET, "zincr"),
        "zrem" => (ZSET, "zrem"),
        "zpopmin" | "bzpopmin" => (ZSET, "zpopmin"),
        "zpopmax" | "bzpopmax" => (ZSET, "zpopmax"),
        "zmpop" | "bzmpop" if has_word(args, "max") => (ZSET, "zpopmax"),
        "zmpop" | "bzmpop" => (ZSET, "zpopmin"),
        "zremrangebyscore" => (ZSET, "zremrangebyscore"),
        "zremrangebyrank" => (ZSET, "zremrangebyrank"),
        "zremrangebylex" => (ZSET, "zremrangebylex"),
        "zunionstore" => (ZSET, "zunionstore"),
        "zinterstore" => (ZSET, "zinterstore"),
        "zdiffstore" => (ZSET, "zdiffstore"),
        "zrangestore" => (ZSET, "zrangestore"),
        "geosearchstore" | "georadius" | "georadiusbymember" => (ZSET, "georadiusstore"),
        "xadd" => (STREAM, "xadd"),
        "xtrim" => (STREAM, "xtrim"),
        "xdel" => (STREAM, "xdel"),
        "xsetid" => (STREAM, "xsetid"),
        "xgroup" => match first.map(lower).as_deref() {
            Some("create") => (STREAM, "xgroup-create"),
            Some("destroy") => (STREAM, "xgroup-destroy"),
            Some("createconsumer") => (STREAM, "xgroup-createconsumer"),
            Some("delconsumer") => (STREAM, "xgroup-delconsumer"),
            Some("setid") => (STREAM, "xgroup-setid"),
            _ => return None,
        },
        _ => return None,
    })
}

/// Keyspace notifications (`notify-keyspace-events`): which classes are on, and the messages a
/// command's writes publish.
#[derive(Debug, Default)]
pub(crate) struct Notifications {
    flags: AtomicU32,
}

impl Notifications {
    pub(crate) fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    pub(crate) fn set(&self, text: &[u8]) -> Result<(), String> {
        self.flags.store(parse(text)?, Ordering::Relaxed);
        Ok(())
    }

    /// The `(channel, message)` pairs to publish for `arguments`, a command whose writes put
    /// `written` keys and deleted `deleted` keys, each with its database.
    pub(crate) fn events(
        &self,
        arguments: &[Vec<u8>],
        written: &[(u32, Vec<u8>)],
        deleted: &[(u32, Vec<u8>)],
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let flags = self.flags();
        if flags & (KEYSPACE | KEYEVENT) == 0 || written.is_empty() && deleted.is_empty() {
            return Vec::new();
        }
        let command = arguments.first().map(|word| lower(word)).unwrap_or_default();
        let mut seen: Vec<&(u32, Vec<u8>)> = Vec::new();
        let mut fired = Vec::new();
        let mut fire = |db: u32, key: &[u8], class: u32, event: &str| {
            if flags & class == 0 {
                return;
            }
            if flags & KEYSPACE != 0 {
                let mut channel = format!("__keyspace@{db}__:").into_bytes();
                channel.extend_from_slice(key);
                fired.push((channel, event.as_bytes().to_vec()));
            }
            if flags & KEYEVENT != 0 {
                fired.push((format!("__keyevent@{db}__:{event}").into_bytes(), key.to_vec()));
            }
        };
        for entry in written.iter().chain(deleted) {
            if seen.contains(&entry) {
                continue;
            }
            seen.push(entry);
            let (db, key) = entry;
            if let Some((class, event)) = event_for(&command, arguments, key) {
                fire(*db, key, class, event);
            }
        }
        let renames = matches!(command.as_str(), "rename" | "renamenx");
        for (db, key) in deleted {
            let already = matches!(event_for(&command, arguments, key), Some((_, "del")));
            if !renames && !already {
                fire(*db, key, GENERIC, "del");
            }
        }
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<Vec<u8>> {
        line.split(' ').map(|word| word.as_bytes().to_vec()).collect()
    }

    #[test]
    fn flags_parse_and_print_as_redis_shows_them() {
        assert_eq!(format(parse(b"KEA").unwrap()), "AKE");
        assert_eq!(format(parse(b"Elg").unwrap()), "glE");
        assert_eq!(format(parse(b"").unwrap()), "");
        assert!(parse(b"Kq").is_err());
    }

    #[test]
    fn writes_publish_keyspace_and_keyevent_messages_of_the_enabled_classes() {
        let notifications = Notifications::default();
        notifications.set(b"KEs").unwrap();
        let fired = notifications.events(&words("LPOP queue"), &[], &[(0, b"queue".to_vec())]);
        assert!(fired.is_empty());
        notifications.set(b"KEA").unwrap();
        let fired = notifications.events(&words("LPOP queue"), &[(0, b"queue".to_vec())], &[(0, b"queue".to_vec())]);
        assert_eq!(
            fired,
            vec![
                (b"__keyspace@0__:queue".to_vec(), b"lpop".to_vec()),
                (b"__keyevent@0__:lpop".to_vec(), b"queue".to_vec()),
                (b"__keyspace@0__:queue".to_vec(), b"del".to_vec()),
                (b"__keyevent@0__:del".to_vec(), b"queue".to_vec()),
            ]
        );
        notifications.set(b"Eg").unwrap();
        let fired = notifications.events(
            &words("RENAME old new"),
            &[(2, b"new".to_vec())],
            &[(2, b"old".to_vec())],
        );
        assert_eq!(
            fired,
            vec![
                (b"__keyevent@2__:rename_to".to_vec(), b"new".to_vec()),
                (b"__keyevent@2__:rename_from".to_vec(), b"old".to_vec()),
            ]
        );
    }
}
