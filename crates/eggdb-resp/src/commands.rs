use crate::keyspace::{KeysRead, KeysWrite, Keyspace};
use crate::protocol::{Reply, format_double};
use crate::value::{Data, Entry};

const DATABASES: u32 = 16;
pub(crate) const WRONG_TYPE: &str =
    "WRONGTYPE Operation against a key holding the wrong kind of value";
pub(crate) const NOT_INTEGER: &str = "ERR value is not an integer or out of range";
const NOT_FLOAT: &str = "ERR value is not a valid float";
pub(crate) const SYNTAX: &str = "ERR syntax error";
pub(crate) const DAMAGED_VALUE: &str = "ERR the stored value is damaged";

/// One connection's state: its database, protocol version, login, and a transaction it is
/// queueing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Client {
    /// The selected database, 0 to 15.
    pub db: u32,
    /// Whether the client asked for RESP3 with `HELLO 3`.
    pub resp3: bool,
    /// Whether it has authenticated (or no password is set).
    pub authenticated: bool,
    /// The name it gave with `CLIENT SETNAME`.
    pub name: Option<Vec<u8>>,
    /// The commands queued since `MULTI`, or `None` outside a transaction.
    pub queued: Option<Vec<Vec<Vec<u8>>>>,
    /// Whether a queued command was refused, so `EXEC` aborts.
    pub queue_failed: bool,
    /// The keys `WATCH` saw, with their database and stored bytes then.
    pub watched: Vec<(u32, Vec<u8>, Option<Vec<u8>>)>,
}

/// What a connection needs from the server to run its commands.
pub struct Context<'k> {
    /// Where the keys live.
    pub keyspace: &'k dyn Keyspace,
    /// The SHA-256 of the password `AUTH` checks, when one is set.
    pub password_sha256: Option<&'k [u8; 32]>,
    /// The time now, in milliseconds since 1970.
    pub now_ms: u64,
    /// The most bytes one reply may take: a command that reads many keys stops with `-OOM`
    /// before it copies past it. `usize::MAX` for no limit.
    pub reply_room_bytes: usize,
    /// How many clients wait in a blocking command now, which `INFO` reports as
    /// `blocked_clients`. 0 where nothing can block (a replay, `EXEC`, an embedded caller).
    pub blocked_clients: usize,
    /// The server's scripting state (the script cache of `EVALSHA` and `SCRIPT`, and the
    /// libraries of `FUNCTION` and `FCALL`), or `None` where none is kept (a replay, an embedded
    /// caller): `EVAL` still runs, `EVALSHA` finds nothing and functions are unavailable.
    pub scripts: Option<&'k crate::scripting::Scripts>,
}

const REPLY_OVER_BUDGET: &str =
    "OOM command not allowed: the reply would pass the server's memory budget";
const VALUE_OVER_BUDGET: &str =
    "OOM command not allowed: the value would pass the server's memory budget";
const BULK_FRAMING_BYTES: usize = 32;

impl Context<'_> {
    pub(crate) fn check_reply_room(&self, reply_bytes: usize) -> Result<(), String> {
        if reply_bytes > self.reply_room_bytes {
            Err(REPLY_OVER_BUDGET.to_owned())
        } else {
            Ok(())
        }
    }

    pub(crate) fn check_value_room(&self, value_bytes: usize) -> Result<(), String> {
        if value_bytes > self.reply_room_bytes {
            Err(VALUE_OVER_BUDGET.to_owned())
        } else {
            Ok(())
        }
    }
}

pub(crate) fn bulk_reply_bytes(bytes: &[u8]) -> usize {
    bytes.len().saturating_add(BULK_FRAMING_BYTES)
}

impl std::fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("now_ms", &self.now_ms)
            .finish_non_exhaustive()
    }
}

fn wrong_arguments(name: &str) -> Reply {
    Reply::Error(format!(
        "ERR wrong number of arguments for '{name}' command"
    ))
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
            let entry = Entry::decode(&bytes).ok_or_else(|| DAMAGED_VALUE.to_owned())?;
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

pub(crate) fn glob(pattern: &[u8], text: &[u8]) -> bool {
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
        if pattern.get(at + 1) == Some(&b'-')
            && pattern.get(at + 2).is_some_and(|high| *high != b']')
        {
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
    let start = if start < 0 {
        (len + start).max(0)
    } else {
        start
    };
    let stop = if stop < 0 {
        len + stop
    } else {
        stop.min(len - 1)
    };
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
    if let Some(reply) = crate::scripting::command(context, client, &name, args) {
        return reply;
    }
    if let Some(reply) = crate::functions::command(context, client, &name, args) {
        return reply;
    }
    match connection_command(context, client, &name, args) {
        Some(reply) => reply,
        None => key_command(context, client.db, &name, args),
    }
}

fn check_password(context: &Context<'_>, given: &[u8]) -> bool {
    context.password_sha256.is_some_and(|expected| {
        eggdb_crypto::constant_time_eq(expected, &eggdb_crypto::sha256(given))
    })
}

fn connection_command(
    context: &Context<'_>,
    client: &mut Client,
    name: &str,
    args: &[Vec<u8>],
) -> Option<Reply> {
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
                Reply::Error(
                    "WRONGPASS invalid username-password pair or user is disabled.".to_owned(),
                )
            }
        }
        "hello" => {
            let mut rest = args;
            if let Some((version, tail)) = rest.split_first() {
                match integer(version) {
                    Ok(2) => client.resp3 = false,
                    Ok(3) => client.resp3 = true,
                    _ => {
                        return Some(Reply::Error(
                            "NOPROTO unsupported protocol version".to_owned(),
                        ));
                    }
                }
                rest = tail;
            }
            while let Some((option, tail)) = rest.split_first() {
                match String::from_utf8_lossy(option)
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "auth" if tail.len() >= 2 => {
                        if !check_password(context, &tail[1]) {
                            return Some(Reply::Error(
                                "WRONGPASS invalid username-password pair or user is disabled."
                                    .to_owned(),
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
                (
                    Reply::bulk("proto"),
                    Reply::Integer(if client.resp3 { 3 } else { 2 }),
                ),
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
        "client" => match args
            .first()
            .map(|word| String::from_utf8_lossy(word).to_ascii_lowercase())
        {
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
        "info" => info(context, args),
        _ => return None,
    })
}

fn keyspace_counts(context: &Context<'_>, db: u32) -> Option<(usize, usize)> {
    let now = context.now_ms;
    let mut counts = None;
    let outcome = context.keyspace.read(db, &mut |keys| {
        let live = live_keys(keys, now)?;
        let mut expiring = 0usize;
        for key in &live {
            if load(keys, key, now)?.is_some_and(|entry| entry.expires_at_ms.is_some()) {
                expiring += 1;
            }
        }
        counts = Some((live.len(), expiring));
        Ok(())
    });
    outcome.ok().and(counts)
}

const INFO_SECTIONS: [&str; 8] = [
    "server",
    "clients",
    "memory",
    "persistence",
    "stats",
    "replication",
    "cpu",
    "keyspace",
];

fn info_section(context: &Context<'_>, section: &str) -> String {
    match section {
        "server" => "# Server\r\nredis_version:7.2.0\r\nredis_mode:standalone\r\nos:EggDB\r\narch_bits:64\r\nprocess_id:1\r\ntcp_port:6379\r\n".to_owned(),
        "clients" => format!(
            "# Clients\r\nblocked_clients:{}\r\nconnected_clients:1\r\nmaxclients:10000\r\n",
            context.blocked_clients
        ),
        "memory" => "# Memory\r\nused_memory:0\r\nmaxmemory:0\r\nmaxmemory_policy:noeviction\r\nmem_allocator:eggdb\r\n".to_owned(),
        "persistence" => "# Persistence\r\nloading:0\r\nrdb_changes_since_last_save:0\r\naof_enabled:0\r\n".to_owned(),
        "stats" => "# Stats\r\ntotal_connections_received:0\r\ntotal_commands_processed:0\r\nkeyspace_hits:0\r\nkeyspace_misses:0\r\n".to_owned(),
        "replication" => "# Replication\r\nrole:master\r\nconnected_slaves:0\r\n".to_owned(),
        "cpu" => "# CPU\r\nused_cpu_sys:0.000000\r\nused_cpu_user:0.000000\r\n".to_owned(),
        "keyspace" => {
            let mut out = "# Keyspace\r\n".to_owned();
            for db in 0..DATABASES {
                if let Some((keys, expires)) = keyspace_counts(context, db)
                    && keys > 0
                {
                    out.push_str(&format!("db{db}:keys={keys},expires={expires},avg_ttl=0\r\n"));
                }
            }
            out
        }
        _ => String::new(),
    }
}

fn info(context: &Context<'_>, args: &[Vec<u8>]) -> Reply {
    let asked: Vec<String> = args
        .iter()
        .map(|word| String::from_utf8_lossy(word).to_ascii_lowercase())
        .collect();
    let every = asked.is_empty()
        || asked
            .iter()
            .any(|word| matches!(word.as_str(), "all" | "default" | "everything"));
    let sections: Vec<&str> = INFO_SECTIONS
        .iter()
        .copied()
        .filter(|section| every || asked.iter().any(|word| word == section))
        .collect();
    let text: Vec<String> = sections
        .iter()
        .map(|section| info_section(context, section))
        .filter(|part| !part.is_empty())
        .collect();
    Reply::bulk(text.join("\r\n"))
}

fn key_command(context: &Context<'_>, db: u32, name: &str, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    match (name, args.len()) {
        ("get", 1) => reading(context, db, |keys| {
            Ok(Reply::Bulk(
                as_string(load(keys, &args[0], now)?.as_ref())?.cloned(),
            ))
        }),
        ("set", count) if count >= 2 => set(context, db, args),
        ("setnx", 2) => writing(context, db, |keys| {
            if load(keys, &args[0], now)?.is_some() {
                return Ok(Reply::Integer(0));
            }
            save(
                keys,
                &args[0],
                &Entry {
                    data: Data::String(args[1].clone()),
                    expires_at_ms: None,
                },
            )?;
            Ok(Reply::Integer(1))
        }),
        ("setex" | "psetex", 3) => {
            let amount = match integer(&args[1]) {
                Ok(amount) if amount > 0 => amount as u64,
                _ => return Reply::Error(format!("ERR invalid expire time in '{name}' command")),
            };
            let millis = if name == "setex" {
                amount.saturating_mul(1000)
            } else {
                amount
            };
            writing(context, db, |keys| {
                save(
                    keys,
                    &args[0],
                    &Entry {
                        data: Data::String(args[2].clone()),
                        expires_at_ms: Some(now.saturating_add(millis)),
                    },
                )?;
                Ok(Reply::ok())
            })
        }
        ("mget", count) if count >= 1 => reading(context, db, |keys| {
            let mut values = Vec::with_capacity(args.len());
            let mut reply_bytes = 0usize;
            for key in args {
                let value = match load(keys, key, now)? {
                    Some(Entry {
                        data: Data::String(bytes),
                        ..
                    }) => Some(bytes),
                    _ => None,
                };
                reply_bytes = reply_bytes
                    .saturating_add(bulk_reply_bytes(value.as_deref().unwrap_or_default()));
                context.check_reply_room(reply_bytes)?;
                values.push(Reply::Bulk(value));
            }
            Ok(Reply::Array(Some(values)))
        }),
        ("mset", count) if count >= 2 && count % 2 == 0 => writing(context, db, |keys| {
            for pair in args.chunks(2) {
                save(
                    keys,
                    &pair[0],
                    &Entry {
                        data: Data::String(pair[1].clone()),
                        expires_at_ms: None,
                    },
                )?;
            }
            Ok(Reply::ok())
        }),
        ("incr" | "decr", 1) => {
            increment(context, db, &args[0], if name == "incr" { 1 } else { -1 })
        }
        ("incrby" | "decrby", 2) => match integer(&args[1]) {
            Ok(by) => increment(
                context,
                db,
                &args[0],
                if name == "incrby" {
                    by
                } else {
                    by.saturating_neg()
                },
            ),
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
                save(
                    keys,
                    &args[0],
                    &Entry {
                        data: Data::String(text.clone().into_bytes()),
                        expires_at_ms: entry.and_then(|entry| entry.expires_at_ms),
                    },
                )?;
                Ok(Reply::bulk(text))
            }),
            Err(error) => Reply::Error(error),
        },
        ("append", 2) => writing(context, db, |keys| {
            let entry = load(keys, &args[0], now)?;
            let mut value = as_string(entry.as_ref())?.cloned().unwrap_or_default();
            value.extend_from_slice(&args[1]);
            let length = value.len();
            save(
                keys,
                &args[0],
                &Entry {
                    data: Data::String(value),
                    expires_at_ms: entry.and_then(|entry| entry.expires_at_ms),
                },
            )?;
            Ok(Reply::Integer(length as i64))
        }),
        ("strlen", 1) => reading(context, db, |keys| {
            Ok(Reply::Integer(
                as_string(load(keys, &args[0], now)?.as_ref())?.map_or(0, Vec::len) as i64,
            ))
        }),
        ("getrange", 3) => {
            let (Ok(start), Ok(stop)) = (integer(&args[1]), integer(&args[2])) else {
                return Reply::Error(NOT_INTEGER.to_owned());
            };
            reading(context, db, |keys| {
                let value = as_string(load(keys, &args[0], now)?.as_ref())?
                    .cloned()
                    .unwrap_or_default();
                Ok(Reply::bulk(match range_of(value.len(), start, stop) {
                    Some((from, to)) => value[from..=to].to_vec(),
                    None => Vec::new(),
                }))
            })
        }
        ("getset" | "getdel", count)
            if (name == "getset" && count == 2) || (name == "getdel" && count == 1) =>
        {
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let old = as_string(entry.as_ref())?.cloned();
                if name == "getset" {
                    save(
                        keys,
                        &args[0],
                        &Entry {
                            data: Data::String(args[1].clone()),
                            expires_at_ms: None,
                        },
                    )?;
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
        ("object", count) if count >= 1 => object(context, db, args),
        ("debug", count) if count >= 1 => debug(context, db, args),
        ("type", 1) => reading(context, db, |keys| {
            Ok(Reply::Status(
                load(keys, &args[0], now)?
                    .map_or("none", |entry| entry.data.type_name())
                    .to_owned(),
            ))
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
                Some(Entry {
                    expires_at_ms: None,
                    ..
                }) => -1,
                Some(Entry {
                    expires_at_ms: Some(at),
                    ..
                }) => {
                    let left = at.saturating_sub(now);
                    if name == "ttl" {
                        (left.div_ceil(1000)) as i64
                    } else {
                        left as i64
                    }
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
            Ok(if name == "rename" {
                Reply::ok()
            } else {
                Reply::Integer(1)
            })
        }),
        ("keys", 1) => reading(context, db, |keys| {
            let mut matched = Vec::new();
            let mut reply_bytes = 0usize;
            keys.visit_keys(&mut |key| {
                if glob(&args[0], &key) && load(keys, &key, now)?.is_some() {
                    reply_bytes = reply_bytes.saturating_add(bulk_reply_bytes(&key));
                    context.check_reply_room(reply_bytes)?;
                    matched.push(Reply::bulk(key));
                }
                Ok(true)
            })?;
            Ok(Reply::Array(Some(matched)))
        }),
        ("scan", count) if count >= 1 => scan(context, db, args),
        ("dbsize", 0) => reading(context, db, |keys| {
            Ok(Reply::Integer(live_keys(keys, now)?.len() as i64))
        }),
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
            .or_else(|| crate::pops::execute(context, db, name, args))
            .or_else(|| crate::blocking::execute(context, db, name, args))
            .or_else(|| crate::streams::execute(context, db, name, args))
            .or_else(|| crate::stream_groups::execute(context, db, name, args))
            .or_else(|| crate::bitmaps::execute(context, db, name, args))
            .or_else(|| crate::hyperloglog::execute(context, db, name, args))
            .or_else(|| crate::geo::execute(context, db, name, args))
            .or_else(|| crate::more_commands::execute(context, db, name, args))
            .or_else(|| crate::more_commands_ordered::execute(context, db, name, args))
            .unwrap_or_else(|| {
                if crate::aggregates::is_known(name)
                    || crate::pops::is_known(name)
                    || crate::streams::is_known(name)
                    || crate::stream_groups::is_known(name)
                    || crate::bitmaps::is_known(name)
                    || crate::hyperloglog::is_known(name)
                    || crate::geo::is_known(name)
                    || crate::more_commands::is_known(name)
                    || crate::more_commands_ordered::is_known(name)
                    || is_known(name)
                {
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
        "get"
            | "set"
            | "setnx"
            | "setex"
            | "psetex"
            | "mget"
            | "mset"
            | "incr"
            | "decr"
            | "incrby"
            | "decrby"
            | "incrbyfloat"
            | "append"
            | "strlen"
            | "getrange"
            | "getset"
            | "getdel"
            | "del"
            | "unlink"
            | "exists"
            | "type"
            | "object"
            | "debug"
            | "expire"
            | "pexpire"
            | "expireat"
            | "pexpireat"
            | "ttl"
            | "pttl"
            | "persist"
            | "rename"
            | "renamenx"
            | "keys"
            | "scan"
            | "dbsize"
    )
}

const SMALL_ENTRIES: usize = 128;
const SMALL_ELEMENT_BYTES: usize = 64;
const SMALL_INTEGER_SET: usize = 512;
const EMBEDDED_STRING_BYTES: usize = 44;

fn small<'e>(mut elements: impl Iterator<Item = &'e [u8]>, count: usize) -> bool {
    count <= SMALL_ENTRIES && elements.all(|element| element.len() <= SMALL_ELEMENT_BYTES)
}

fn is_integer_text(bytes: &[u8]) -> bool {
    bytes.len() <= 20
        && std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| text.parse::<i64>().ok())
            .is_some_and(|number| number.to_string().as_bytes() == bytes)
}

fn encoding_of(data: &Data) -> &'static str {
    match data {
        Data::String(bytes) if is_integer_text(bytes) => "int",
        Data::String(bytes) if bytes.len() <= EMBEDDED_STRING_BYTES => "embstr",
        Data::String(_) => "raw",
        Data::Hash(map)
            if small(
                map.iter()
                    .flat_map(|(field, value)| [field.as_slice(), value.as_slice()]),
                map.len(),
            ) =>
        {
            "listpack"
        }
        Data::Hash(_) => "hashtable",
        Data::List(list) if small(list.iter().map(Vec::as_slice), list.len()) => "listpack",
        Data::List(_) => "quicklist",
        Data::Set(set)
            if set.len() <= SMALL_INTEGER_SET && set.iter().all(|member| is_integer_text(member)) =>
        {
            "intset"
        }
        Data::Set(set) if small(set.iter().map(Vec::as_slice), set.len()) => "listpack",
        Data::Set(_) => "hashtable",
        Data::SortedSet(set) => {
            let ordered = set.ordered();
            if small(ordered.iter().map(|(_, member)| member.as_slice()), ordered.len()) {
                "listpack"
            } else {
                "skiplist"
            }
        }
        Data::Stream(_) => "stream",
    }
}

fn object(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let subcommand = String::from_utf8_lossy(&args[0]).to_ascii_lowercase();
    if subcommand == "help" && args.len() == 1 {
        return Reply::Array(Some(
            [
                "OBJECT <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
                "ENCODING <key>",
                "    Return the kind of internal representation used in order to store the value",
                "    associated with a <key>.",
                "FREQ <key>",
                "    Return the access frequency index of the <key>. The returned integer is",
                "    proportional to the logarithm of the recent access frequency of the key.",
                "IDLETIME <key>",
                "    Return the idle time of the <key>, that is the approximated number of",
                "    seconds elapsed since the last access to the key.",
                "REFCOUNT <key>",
                "    Return the number of references of the value associated with the specified",
                "    <key>.",
                "HELP",
                "    Print this help.",
            ]
            .iter()
            .map(|line| Reply::Status((*line).to_owned()))
            .collect(),
        ));
    }
    let known = matches!(
        subcommand.as_str(),
        "encoding" | "freq" | "idletime" | "refcount"
    );
    if !known || args.len() != 2 {
        return Reply::Error(format!(
            "ERR unknown subcommand or wrong number of arguments for '{subcommand}'. Try OBJECT HELP."
        ));
    }
    reading(context, db, |keys| {
        let Some(entry) = load(keys, &args[1], now)? else {
            return Ok(Reply::Bulk(None));
        };
        Ok(match subcommand.as_str() {
            "encoding" => Reply::bulk(encoding_of(&entry.data)),
            "refcount" => Reply::Integer(1),
            "idletime" => Reply::Integer(0),
            _ => Reply::Error(
                "ERR An LFU maxmemory policy is not selected, access frequency not tracked. Please note that when switching between policies at runtime LRU and LFU data will take some time to adjust.".to_owned(),
            ),
        })
    })
}

const MOST_DEBUG_SLEEP_SECONDS: f64 = 60.0;

fn serialized_length(data: &Data) -> usize {
    match data {
        Data::String(bytes) => bytes.len(),
        Data::Hash(map) => map.iter().map(|(field, value)| field.len() + value.len()).sum(),
        Data::List(list) => list.iter().map(Vec::len).sum(),
        Data::Set(set) => set.iter().map(Vec::len).sum(),
        Data::SortedSet(set) => set
            .ordered()
            .iter()
            .map(|(_, member)| member.len() + 8)
            .sum(),
        Data::Stream(_) => 0,
    }
}

fn debug(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let subcommand = String::from_utf8_lossy(&args[0]).to_ascii_lowercase();
    match (subcommand.as_str(), args.len()) {
        ("help", 1) => Reply::Array(Some(
            [
                "DEBUG <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
                "OBJECT <key>",
                "    Show low level info about the key and associated value.",
                "SLEEP <seconds>",
                "    Stop the server for <seconds>. Decimals allowed.",
                "SET-ACTIVE-EXPIRE <0|1>",
                "    Setting it to 0 disables expiring keys in background when they are not accessed.",
                "HELP",
                "    Print this help.",
            ]
            .iter()
            .map(|line| Reply::Status((*line).to_owned()))
            .collect(),
        )),
        ("object", 2) => reading(context, db, |keys| {
            let Some(entry) = load(keys, &args[1], now)? else {
                return Ok(Reply::Error("ERR no such key".to_owned()));
            };
            Ok(Reply::Status(format!(
                "Value at:0x0 refcount:1 encoding:{} serializedlength:{} lru:0 lru_seconds_idle:0",
                encoding_of(&entry.data),
                serialized_length(&entry.data)
            )))
        }),
        ("sleep", 2) => {
            let seconds = String::from_utf8_lossy(&args[1])
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0);
            let Some(seconds) = seconds else {
                return Reply::Error("ERR value is not a valid float".to_owned());
            };
            std::thread::sleep(std::time::Duration::from_secs_f64(
                seconds.min(MOST_DEBUG_SLEEP_SECONDS),
            ));
            Reply::ok()
        }
        ("set-active-expire", 2) => Reply::ok(),
        _ => Reply::Error(format!(
            "ERR unknown subcommand or wrong number of arguments for '{subcommand}'. Try DEBUG HELP."
        )),
    }
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
        save(
            keys,
            key,
            &Entry {
                data: Data::String(next.to_string().into_bytes()),
                expires_at_ms: entry.and_then(|entry| entry.expires_at_ms),
            },
        )?;
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
        let old = if get {
            as_string(existing.as_ref())?.cloned()
        } else {
            None
        };
        if (only_new && existing.is_some()) || (only_existing && existing.is_none()) {
            return Ok(if get { Reply::Bulk(old) } else { Reply::null() });
        }
        let expires_at_ms = if keep_ttl {
            existing.and_then(|entry| entry.expires_at_ms)
        } else {
            expires
        };
        save(
            keys,
            &args[0],
            &Entry {
                data: Data::String(args[1].clone()),
                expires_at_ms,
            },
        )?;
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
        match String::from_utf8_lossy(option)
            .to_ascii_lowercase()
            .as_str()
        {
            "match" => pattern = options.next().cloned(),
            "count" => match options.next().map(|value| integer(value)) {
                Some(Ok(value)) if value > 0 => count = value as usize,
                _ => return Reply::Error(SYNTAX.to_owned()),
            },
            "type" => {
                kind = options
                    .next()
                    .map(|value| String::from_utf8_lossy(value).to_ascii_lowercase())
            }
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
            if kind
                .as_ref()
                .is_some_and(|kind| kind != entry.data.type_name())
            {
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
