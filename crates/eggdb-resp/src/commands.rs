use crate::keyspace::{KeysRead, KeysWrite, Keyspace};
use crate::protocol::{Reply, format_double};
use crate::value::{Data, Entry, SortedSet};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const DATABASES: u32 = 16;
pub(crate) const WRONG_TYPE: &str = "WRONGTYPE Operation against a key holding the wrong kind of value";
pub(crate) const NOT_INTEGER: &str = "ERR value is not an integer or out of range";
const NOT_FLOAT: &str = "ERR value is not a valid float";
pub(crate) const SYNTAX: &str = "ERR syntax error";

/// One connection's state: its database, protocol version and login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    /// The selected database, 0 to 15.
    pub db: u32,
    /// Whether the client asked for RESP3 with `HELLO 3`.
    pub resp3: bool,
    /// Whether it has authenticated (or no password is set).
    pub authenticated: bool,
    /// The name it gave with `CLIENT SETNAME`.
    pub name: Option<Vec<u8>>,
}

/// What a connection needs from the server to run its commands.
pub struct Context<'k> {
    /// Where the keys live.
    pub keyspace: &'k dyn Keyspace,
    /// The SHA-256 of the password `AUTH` checks, when one is set.
    pub password_sha256: Option<&'k [u8; 32]>,
    /// The time now, in milliseconds since 1970.
    pub now_ms: u64,
}

impl std::fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").field("now_ms", &self.now_ms).finish_non_exhaustive()
    }
}

fn wrong_arguments(name: &str) -> Reply {
    Reply::Error(format!("ERR wrong number of arguments for '{name}' command"))
}

pub(crate) fn integer(bytes: &[u8]) -> Result<i64, String> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| NOT_INTEGER.to_owned())
}

pub(crate) fn float(bytes: &[u8]) -> Result<f64, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| NOT_FLOAT.to_owned())?;
    let number = match text.to_ascii_lowercase().as_str() {
        "inf" | "+inf" => f64::INFINITY,
        "-inf" => f64::NEG_INFINITY,
        other => other.parse().map_err(|_| NOT_FLOAT.to_owned())?,
    };
    if number.is_nan() {
        return Err(NOT_FLOAT.to_owned());
    }
    Ok(number)
}

pub(crate) fn load(keys: &dyn KeysRead, key: &[u8], now: u64) -> Result<Option<Entry>, String> {
    match keys.get(key)? {
        None => Ok(None),
        Some(bytes) => {
            let entry = Entry::decode(&bytes).ok_or_else(|| "ERR the stored value is damaged".to_owned())?;
            Ok((!entry.expired(now)).then_some(entry))
        }
    }
}

pub(crate) fn save(keys: &mut dyn KeysWrite, key: &[u8], entry: &Entry) -> Result<(), String> {
    if entry.data.is_empty_aggregate() {
        keys.delete(key)?;
        return Ok(());
    }
    keys.put(key, &entry.encode())
}

pub(crate) fn reading(
    context: &Context<'_>,
    db: u32,
    body: impl FnOnce(&dyn KeysRead) -> Result<Reply, String>,
) -> Reply {
    let mut body = Some(body);
    let mut reply = None;
    let outcome = context.keyspace.read(db, &mut |keys| {
        if let Some(body) = body.take() {
            reply = Some(body(keys)?);
        }
        Ok(())
    });
    match (outcome, reply) {
        (Err(error), _) => Reply::Error(error),
        (Ok(()), Some(reply)) => reply,
        (Ok(()), None) => Reply::Error("ERR the command did not run".to_owned()),
    }
}

pub(crate) fn writing(
    context: &Context<'_>,
    db: u32,
    body: impl FnOnce(&mut dyn KeysWrite) -> Result<Reply, String>,
) -> Reply {
    let mut body = Some(body);
    let mut reply = None;
    let outcome = context.keyspace.write(db, &mut |keys| {
        if let Some(body) = body.take() {
            reply = Some(body(keys)?);
        }
        Ok(())
    });
    match (outcome, reply) {
        (Err(error), _) => Reply::Error(error),
        (Ok(()), Some(reply)) => reply,
        (Ok(()), None) => Reply::Error("ERR the command did not run".to_owned()),
    }
}

fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let (mut star, mut star_text) = (None, 0);
    while t < text.len() {
        if p < pattern.len() {
            match pattern[p] {
                b'*' => {
                    star = Some(p);
                    star_text = t;
                    p += 1;
                    continue;
                }
                b'?' => {
                    p += 1;
                    t += 1;
                    continue;
                }
                b'[' => {
                    if let Some((matched, next)) = class(&pattern[p..], text[t]) {
                        if matched {
                            p += next;
                            t += 1;
                            continue;
                        }
                    } else if text[t] == b'[' {
                        p += 1;
                        t += 1;
                        continue;
                    }
                }
                b'\\' if p + 1 < pattern.len() => {
                    if pattern[p + 1] == text[t] {
                        p += 2;
                        t += 1;
                        continue;
                    }
                }
                byte if byte == text[t] => {
                    p += 1;
                    t += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some(at) => {
                p = at + 1;
                star_text += 1;
                t = star_text;
            }
            None => return false,
        }
    }
    pattern[p..].iter().all(|byte| *byte == b'*')
}

fn class(pattern: &[u8], byte: u8) -> Option<(bool, usize)> {
    let mut at = 1;
    let negated = pattern.get(at) == Some(&b'^');
    if negated {
        at += 1;
    }
    let mut matched = false;
    while at < pattern.len() && pattern[at] != b']' {
        let low = if pattern[at] == b'\\' && at + 1 < pattern.len() {
            at += 1;
            pattern[at]
        } else {
            pattern[at]
        };
        if pattern.get(at + 1) == Some(&b'-') && pattern.get(at + 2).is_some_and(|high| *high != b']') {
            let high = pattern[at + 2];
            if (low.min(high)..=low.max(high)).contains(&byte) {
                matched = true;
            }
            at += 3;
        } else {
            if low == byte {
                matched = true;
            }
            at += 1;
        }
    }
    (at < pattern.len()).then_some((matched != negated, at + 1))
}

fn live_keys(keys: &dyn KeysRead, now: u64) -> Result<Vec<Vec<u8>>, String> {
    let mut live = Vec::new();
    for key in keys.keys()? {
        if load(keys, &key, now)?.is_some() {
            live.push(key);
        }
    }
    Ok(live)
}

fn as_string(entry: Option<&Entry>) -> Result<Option<&Vec<u8>>, String> {
    match entry.map(|entry| &entry.data) {
        None => Ok(None),
        Some(Data::String(bytes)) => Ok(Some(bytes)),
        Some(_) => Err(WRONG_TYPE.to_owned()),
    }
}

pub(crate) fn range_of(len: usize, start: i64, stop: i64) -> Option<(usize, usize)> {
    let len = len as i64;
    let start = if start < 0 { (len + start).max(0) } else { start };
    let stop = if stop < 0 { len + stop } else { stop.min(len - 1) };
    (start <= stop && start < len).then_some((start as usize, stop as usize))
}

/// Runs one command for `client` and returns its reply.
pub fn execute(context: &Context<'_>, client: &mut Client, arguments: &[Vec<u8>]) -> Reply {
    let Some(first) = arguments.first() else {
        return Reply::Error("ERR empty command".to_owned());
    };
    let name = String::from_utf8_lossy(first).to_ascii_lowercase();
    let args = &arguments[1..];
    if !client.authenticated && !matches!(name.as_str(), "auth" | "hello" | "quit" | "ping") {
        return Reply::Error("NOAUTH Authentication required.".to_owned());
    }
    match connection_command(context, client, &name, args) {
        Some(reply) => reply,
        None => key_command(context, client.db, &name, args),
    }
}

fn check_password(context: &Context<'_>, given: &[u8]) -> bool {
    context
        .password_sha256
        .is_some_and(|expected| eggdb_crypto::constant_time_eq(expected, &eggdb_crypto::sha256(given)))
}

fn connection_command(context: &Context<'_>, client: &mut Client, name: &str, args: &[Vec<u8>]) -> Option<Reply> {
    Some(match name {
        "ping" => match args {
            [] => Reply::Status("PONG".to_owned()),
            [message] => Reply::bulk(message.clone()),
            _ => wrong_arguments(name),
        },
        "echo" => match args {
            [message] => Reply::bulk(message.clone()),
            _ => wrong_arguments(name),
        },
        "quit" => Reply::ok(),
        "auth" => {
            let password = match args {
                [password] => password,
                [_, password] => password,
                _ => return Some(wrong_arguments(name)),
            };
            if context.password_sha256.is_none() {
                return Some(Reply::Error(
                    "ERR AUTH <password> called without any password configured for the default user.".to_owned(),
                ));
            }
            if check_password(context, password) {
                client.authenticated = true;
                Reply::ok()
            } else {
                Reply::Error("WRONGPASS invalid username-password pair or user is disabled.".to_owned())
            }
        }
        "hello" => {
            let mut rest = args;
            if let Some((version, tail)) = rest.split_first() {
                match integer(version) {
                    Ok(2) => client.resp3 = false,
                    Ok(3) => client.resp3 = true,
                    _ => return Some(Reply::Error("NOPROTO unsupported protocol version".to_owned())),
                }
                rest = tail;
            }
            while let Some((option, tail)) = rest.split_first() {
                match String::from_utf8_lossy(option).to_ascii_lowercase().as_str() {
                    "auth" if tail.len() >= 2 => {
                        if !check_password(context, &tail[1]) {
                            return Some(Reply::Error(
                                "WRONGPASS invalid username-password pair or user is disabled.".to_owned(),
                            ));
                        }
                        client.authenticated = true;
                        rest = &tail[2..];
                    }
                    "setname" if !tail.is_empty() => {
                        client.name = Some(tail[0].clone());
                        rest = &tail[1..];
                    }
                    _ => return Some(Reply::Error(SYNTAX.to_owned())),
                }
            }
            if !client.authenticated {
                return Some(Reply::Error("NOAUTH HELLO must be called with the client already authenticated, otherwise the HELLO <proto> AUTH <user> <pass> option can be used to authenticate the client and select the RESP protocol version at the same time".to_owned()));
            }
            Reply::Map(vec![
                (Reply::bulk("server"), Reply::bulk("redis")),
                (Reply::bulk("version"), Reply::bulk("7.2.0")),
                (Reply::bulk("proto"), Reply::Integer(if client.resp3 { 3 } else { 2 })),
                (Reply::bulk("id"), Reply::Integer(1)),
                (Reply::bulk("mode"), Reply::bulk("standalone")),
                (Reply::bulk("role"), Reply::bulk("master")),
                (Reply::bulk("modules"), Reply::Array(Some(Vec::new()))),
            ])
        }
        "select" => match args {
            [index] => match integer(index) {
                Ok(index) if (0..i64::from(DATABASES)).contains(&index) => {
                    client.db = index as u32;
                    Reply::ok()
                }
                Ok(_) => Reply::Error("ERR DB index is out of range".to_owned()),
                Err(error) => Reply::Error(error),
            },
            _ => wrong_arguments(name),
        },
        "client" => match args.first().map(|word| String::from_utf8_lossy(word).to_ascii_lowercase()) {
            Some(word) if word == "setname" && args.len() == 2 => {
                client.name = Some(args[1].clone());
                Reply::ok()
            }
            Some(word) if word == "getname" => Reply::Bulk(client.name.clone()),
            Some(word) if word == "setinfo" => Reply::ok(),
            Some(word) if word == "id" => Reply::Integer(1),
            _ => Reply::Error("ERR unknown subcommand for 'client'".to_owned()),
        },
        "command" => Reply::Array(Some(Vec::new())),
        "info" => Reply::bulk(
            "# Server\r\nredis_version:7.2.0\r\nredis_mode:standalone\r\nos:EggDB\r\n# Keyspace\r\n".to_owned(),
        ),
        _ => return None,
    })
}

fn key_command(context: &Context<'_>, db: u32, name: &str, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    match (name, args.len()) {
        ("get", 1) => reading(context, db, |keys| {
            Ok(Reply::Bulk(as_string(load(keys, &args[0], now)?.as_ref())?.cloned()))
        }),
        ("set", count) if count >= 2 => set(context, db, args),
        ("setnx", 2) => writing(context, db, |keys| {
            if load(keys, &args[0], now)?.is_some() {
                return Ok(Reply::Integer(0));
            }
            save(keys, &args[0], &Entry { data: Data::String(args[1].clone()), expires_at_ms: None })?;
            Ok(Reply::Integer(1))
        }),
        ("setex" | "psetex", 3) => {
            let amount = match integer(&args[1]) {
                Ok(amount) if amount > 0 => amount as u64,
                _ => return Reply::Error(format!("ERR invalid expire time in '{name}' command")),
            };
            let millis = if name == "setex" { amount.saturating_mul(1000) } else { amount };
            writing(context, db, |keys| {
                save(keys, &args[0], &Entry {
                    data: Data::String(args[2].clone()),
                    expires_at_ms: Some(now.saturating_add(millis)),
                })?;
                Ok(Reply::ok())
            })
        }
        ("mget", count) if count >= 1 => reading(context, db, |keys| {
            let mut values = Vec::with_capacity(args.len());
            for key in args {
                let value = match load(keys, key, now)? {
                    Some(Entry { data: Data::String(bytes), .. }) => Some(bytes),
                    _ => None,
                };
                values.push(Reply::Bulk(value));
            }
            Ok(Reply::Array(Some(values)))
        }),
        ("mset", count) if count >= 2 && count % 2 == 0 => writing(context, db, |keys| {
            for pair in args.chunks(2) {
                save(keys, &pair[0], &Entry { data: Data::String(pair[1].clone()), expires_at_ms: None })?;
            }
            Ok(Reply::ok())
        }),
        ("incr" | "decr", 1) => increment(context, db, &args[0], if name == "incr" { 1 } else { -1 }),
        ("incrby" | "decrby", 2) => match integer(&args[1]) {
            Ok(by) => increment(context, db, &args[0], if name == "incrby" { by } else { by.saturating_neg() }),
            Err(error) => Reply::Error(error),
        },
        ("incrbyfloat", 2) => match float(&args[1]) {
            Ok(by) => writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let current = match as_string(entry.as_ref())? {
                    None => 0.0,
                    Some(bytes) => float(bytes)?,
                };
                let next = current + by;
                if !next.is_finite() {
                    return Err("ERR increment would produce NaN or Infinity".to_owned());
                }
                let text = format_double(next);
                save(keys, &args[0], &Entry {
                    data: Data::String(text.clone().into_bytes()),
                    expires_at_ms: entry.and_then(|entry| entry.expires_at_ms),
                })?;
                Ok(Reply::bulk(text))
            }),
            Err(error) => Reply::Error(error),
        },
        ("append", 2) => writing(context, db, |keys| {
            let entry = load(keys, &args[0], now)?;
            let mut value = as_string(entry.as_ref())?.cloned().unwrap_or_default();
            value.extend_from_slice(&args[1]);
            let length = value.len();
            save(keys, &args[0], &Entry {
                data: Data::String(value),
                expires_at_ms: entry.and_then(|entry| entry.expires_at_ms),
            })?;
            Ok(Reply::Integer(length as i64))
        }),
        ("strlen", 1) => reading(context, db, |keys| {
            Ok(Reply::Integer(as_string(load(keys, &args[0], now)?.as_ref())?.map_or(0, Vec::len) as i64))
        }),
        ("getrange", 3) => {
            let (Ok(start), Ok(stop)) = (integer(&args[1]), integer(&args[2])) else {
                return Reply::Error(NOT_INTEGER.to_owned());
            };
            reading(context, db, |keys| {
                let value = as_string(load(keys, &args[0], now)?.as_ref())?.cloned().unwrap_or_default();
                Ok(Reply::bulk(match range_of(value.len(), start, stop) {
                    Some((from, to)) => value[from..=to].to_vec(),
                    None => Vec::new(),
                }))
            })
        }
        ("getset" | "getdel", count) if (name == "getset" && count == 2) || (name == "getdel" && count == 1) => {
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let old = as_string(entry.as_ref())?.cloned();
                if name == "getset" {
                    save(keys, &args[0], &Entry { data: Data::String(args[1].clone()), expires_at_ms: None })?;
                } else if old.is_some() {
                    keys.delete(&args[0])?;
                }
                Ok(Reply::Bulk(old))
            })
        }
        ("del" | "unlink", count) if count >= 1 => writing(context, db, |keys| {
            let mut removed = 0;
            for key in args {
                if load(keys, key, now)?.is_some() {
                    keys.delete(key)?;
                    removed += 1;
                }
            }
            Ok(Reply::Integer(removed))
        }),
        ("exists", count) if count >= 1 => reading(context, db, |keys| {
            let mut found = 0;
            for key in args {
                if load(keys, key, now)?.is_some() {
                    found += 1;
                }
            }
            Ok(Reply::Integer(found))
        }),
        ("type", 1) => reading(context, db, |keys| {
            Ok(Reply::Status(load(keys, &args[0], now)?.map_or("none", |entry| entry.data.type_name()).to_owned()))
        }),
        ("expire" | "pexpire" | "expireat" | "pexpireat", 2) => {
            let Ok(amount) = integer(&args[1]) else {
                return Reply::Error(NOT_INTEGER.to_owned());
            };
            let at = match name {
                "expire" => (now as i64).saturating_add(amount.saturating_mul(1000)),
                "pexpire" => (now as i64).saturating_add(amount),
                "expireat" => amount.saturating_mul(1000),
                _ => amount,
            };
            writing(context, db, |keys| {
                let Some(mut entry) = load(keys, &args[0], now)? else {
                    return Ok(Reply::Integer(0));
                };
                if at <= now as i64 {
                    keys.delete(&args[0])?;
                    return Ok(Reply::Integer(1));
                }
                entry.expires_at_ms = Some(at as u64);
                save(keys, &args[0], &entry)?;
                Ok(Reply::Integer(1))
            })
        }
        ("ttl" | "pttl", 1) => reading(context, db, |keys| {
            Ok(Reply::Integer(match load(keys, &args[0], now)? {
                None => -2,
                Some(Entry { expires_at_ms: None, .. }) => -1,
                Some(Entry { expires_at_ms: Some(at), .. }) => {
                    let left = at.saturating_sub(now);
                    if name == "ttl" { (left.div_ceil(1000)) as i64 } else { left as i64 }
                }
            }))
        }),
        ("persist", 1) => writing(context, db, |keys| {
            let Some(mut entry) = load(keys, &args[0], now)? else {
                return Ok(Reply::Integer(0));
            };
            if entry.expires_at_ms.take().is_none() {
                return Ok(Reply::Integer(0));
            }
            save(keys, &args[0], &entry)?;
            Ok(Reply::Integer(1))
        }),
        ("rename" | "renamenx", 2) => writing(context, db, |keys| {
            let Some(entry) = load(keys, &args[0], now)? else {
                return Err("ERR no such key".to_owned());
            };
            if name == "renamenx" && load(keys, &args[1], now)?.is_some() {
                return Ok(Reply::Integer(0));
            }
            keys.delete(&args[0])?;
            save(keys, &args[1], &entry)?;
            Ok(if name == "rename" { Reply::ok() } else { Reply::Integer(1) })
        }),
        ("keys", 1) => reading(context, db, |keys| {
            let matched = live_keys(keys, now)?
                .into_iter()
                .filter(|key| glob(&args[0], key))
                .map(Reply::bulk)
                .collect();
            Ok(Reply::Array(Some(matched)))
        }),
        ("scan", count) if count >= 1 => scan(context, db, args),
        ("dbsize", 0) => reading(context, db, |keys| Ok(Reply::Integer(live_keys(keys, now)?.len() as i64))),
        ("flushdb", _) => writing(context, db, |keys| {
            keys.clear()?;
            Ok(Reply::ok())
        }),
        ("flushall", _) => {
            for index in 0..DATABASES {
                let reply = writing(context, index, |keys| {
                    keys.clear()?;
                    Ok(Reply::ok())
                });
                if matches!(reply, Reply::Error(_)) {
                    return reply;
                }
            }
            Reply::ok()
        }
        _ => crate::aggregates::execute(context, db, name, args)
            .unwrap_or_else(|| {
                if crate::aggregates::is_known(name) || is_known(name) {
                    wrong_arguments(name)
                } else {
                    Reply::Error(format!("ERR unknown command '{name}'"))
                }
            }),
    }
}

fn is_known(name: &str) -> bool {
    matches!(
        name,
        "get" | "set" | "setnx" | "setex" | "psetex" | "mget" | "mset" | "incr" | "decr" | "incrby"
            | "decrby" | "incrbyfloat" | "append" | "strlen" | "getrange" | "getset" | "getdel"
            | "del" | "unlink" | "exists" | "type" | "expire" | "pexpire" | "expireat"
            | "pexpireat" | "ttl" | "pttl" | "persist" | "rename" | "renamenx" | "keys" | "scan"
            | "dbsize"
    )
}

fn increment(context: &Context<'_>, db: u32, key: &[u8], by: i64) -> Reply {
    let now = context.now_ms;
    writing(context, db, |keys| {
        let entry = load(keys, key, now)?;
        let current = match as_string(entry.as_ref())? {
            None => 0,
            Some(bytes) => integer(bytes)?,
        };
        let next = current
            .checked_add(by)
            .ok_or_else(|| "ERR increment or decrement would overflow".to_owned())?;
        save(keys, key, &Entry {
            data: Data::String(next.to_string().into_bytes()),
            expires_at_ms: entry.and_then(|entry| entry.expires_at_ms),
        })?;
        Ok(Reply::Integer(next))
    })
}

fn set(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let mut expires: Option<u64> = None;
    let (mut only_new, mut only_existing, mut keep_ttl, mut get) = (false, false, false, false);
    let mut options = args[2..].iter();
    while let Some(option) = options.next() {
        let word = String::from_utf8_lossy(option).to_ascii_lowercase();
        match word.as_str() {
            "nx" => only_new = true,
            "xx" => only_existing = true,
            "keepttl" => keep_ttl = true,
            "get" => get = true,
            "ex" | "px" | "exat" | "pxat" => {
                let Some(Ok(amount)) = options.next().map(|value| integer(value)) else {
                    return Reply::Error(SYNTAX.to_owned());
                };
                if amount <= 0 {
                    return Reply::Error("ERR invalid expire time in 'set' command".to_owned());
                }
                let amount = amount as u64;
                expires = Some(match word.as_str() {
                    "ex" => now.saturating_add(amount.saturating_mul(1000)),
                    "px" => now.saturating_add(amount),
                    "exat" => amount.saturating_mul(1000),
                    _ => amount,
                });
            }
            _ => return Reply::Error(SYNTAX.to_owned()),
        }
    }
    if only_new && only_existing {
        return Reply::Error(SYNTAX.to_owned());
    }
    writing(context, db, |keys| {
        let existing = load(keys, &args[0], now)?;
        let old = if get { as_string(existing.as_ref())?.cloned() } else { None };
        if (only_new && existing.is_some()) || (only_existing && existing.is_none()) {
            return Ok(if get { Reply::Bulk(old) } else { Reply::null() });
        }
        let expires_at_ms = if keep_ttl {
            existing.and_then(|entry| entry.expires_at_ms)
        } else {
            expires
        };
        save(keys, &args[0], &Entry { data: Data::String(args[1].clone()), expires_at_ms })?;
        Ok(if get { Reply::Bulk(old) } else { Reply::ok() })
    })
}

fn scan(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let Ok(cursor) = integer(&args[0]) else {
        return Reply::Error("ERR invalid cursor".to_owned());
    };
    let (mut pattern, mut count, mut kind) = (None, 10usize, None);
    let mut options = args[1..].iter();
    while let Some(option) = options.next() {
        match String::from_utf8_lossy(option).to_ascii_lowercase().as_str() {
            "match" => pattern = options.next().cloned(),
            "count" => match options.next().map(|value| integer(value)) {
                Some(Ok(value)) if value > 0 => count = value as usize,
                _ => return Reply::Error(SYNTAX.to_owned()),
            },
            "type" => kind = options.next().map(|value| String::from_utf8_lossy(value).to_ascii_lowercase()),
            _ => return Reply::Error(SYNTAX.to_owned()),
        }
    }
    reading(context, db, |keys| {
        let all = keys.keys()?;
        let start = cursor.max(0) as usize;
        let end = (start + count).min(all.len());
        let mut found = Vec::new();
        for key in all.get(start..end).unwrap_or_default() {
            let Some(entry) = load(keys, key, now)? else {
                continue;
            };
            if pattern.as_ref().is_some_and(|pattern| !glob(pattern, key)) {
                continue;
            }
            if kind.as_ref().is_some_and(|kind| kind != entry.data.type_name()) {
                continue;
            }
            found.push(Reply::bulk(key.clone()));
        }
        let next = if end >= all.len() { 0 } else { end };
        Ok(Reply::Array(Some(vec![
            Reply::bulk(next.to_string()),
            Reply::Array(Some(found)),
        ])))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_patterns_match_as_the_documented_rules_say() {
        for (pattern, text, expected) in [
            (&b"h?llo"[..], &b"hello"[..], true),
            (b"h*llo", b"heeeello", true),
            (b"h[ae]llo", b"hallo", true),
            (b"h[^e]llo", b"hello", false),
            (b"h[a-b]llo", b"hbllo", true),
            (b"*", b"", true),
            (b"user:*:name", b"user:42:name", true),
            (b"a\\*b", b"a*b", true),
            (b"a\\*b", b"axb", false),
            (b"[", b"[", true),
        ] {
            assert_eq!(glob(pattern, text), expected, "{pattern:?} {text:?}");
        }
    }
}
