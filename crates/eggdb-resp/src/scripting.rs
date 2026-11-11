use crate::commands::{Client, Context, execute, integer};
use crate::keyspace::{KeysRead, KeysWrite, Keyspace};
use crate::protocol::{Reply, format_double};
use crate::transactions::OpenTransaction;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Mutex;

pub(crate) const STEP_BUDGET: u64 = 100_000_000;
const SHA1_HEX_LENGTH: usize = 40;
const NOT_FROM_SCRIPTS: &[&str] = &[
    "eval", "evalsha", "eval_ro", "evalsha_ro", "script", "function", "fcall", "fcall_ro", "multi", "exec", "discard", "watch",
    "unwatch", "subscribe", "psubscribe", "ssubscribe", "unsubscribe", "punsubscribe",
    "sunsubscribe", "monitor", "auth", "hello", "quit", "reset", "client", "config",
];
const NOT_ALLOWED: &str = "ERR This Redis command is not allowed from script";
const READ_ONLY_WRITE: &str = "ERR Write commands are not allowed from read-only scripts.";
const NO_SCRIPT: &str = "NOSCRIPT No matching script. Please use EVAL.";

/// A server's scripting state, shared by its connections: the scripts it has seen, by the
/// hex SHA-1 of their source, for `EVALSHA` and `SCRIPT EXISTS`, and the function libraries
/// `FUNCTION LOAD` registered. Empty after a restart.
#[derive(Debug, Default)]
pub struct Scripts {
    scripts: Mutex<BTreeMap<String, Vec<u8>>>,
    pub(crate) libraries: Mutex<BTreeMap<String, crate::functions::Library>>,
}

impl Scripts {
    /// Keeps `source` and returns its hex SHA-1.
    pub fn load(&self, source: &[u8]) -> String {
        let sha = eggdb_lua::sha1_hex(source);
        if let Ok(mut scripts) = self.scripts.lock() {
            scripts.entry(sha.clone()).or_insert_with(|| source.to_vec());
        }
        sha
    }

    /// The source of the script whose hex SHA-1 is `sha`, in either case.
    #[must_use]
    pub fn source(&self, sha: &[u8]) -> Option<Vec<u8>> {
        let sha = String::from_utf8_lossy(sha).to_ascii_lowercase();
        self.scripts.lock().ok()?.get(&sha).cloned()
    }

    /// Forgets every script.
    pub fn flush(&self) {
        if let Ok(mut scripts) = self.scripts.lock() {
            scripts.clear();
        }
    }
}

struct ReadOnlyTransaction<'t> {
    db: u32,
    keys: &'t dyn KeysRead,
}

impl Keyspace for ReadOnlyTransaction<'_> {
    fn read(
        &self,
        db: u32,
        body: &mut dyn FnMut(&dyn KeysRead) -> Result<(), String>,
    ) -> Result<(), String> {
        if db != self.db {
            return Err("ERR a script uses one database".to_owned());
        }
        body(self.keys)
    }

    fn write(
        &self,
        _db: u32,
        _body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>,
    ) -> Result<(), String> {
        Err(READ_ONLY_WRITE.to_owned())
    }
}

fn script_reply(reply: Reply) -> eggdb_lua::Reply {
    let array = |items: Vec<Reply>| eggdb_lua::Reply::Array(Some(items.into_iter().map(script_reply).collect()));
    let flat = |pairs: Vec<(Reply, Reply)>| {
        array(pairs.into_iter().flat_map(|(left, right)| [left, right]).collect())
    };
    match reply {
        Reply::Status(text) => eggdb_lua::Reply::Status(text.into_bytes()),
        Reply::Error(text) => eggdb_lua::Reply::Error(text.into_bytes()),
        Reply::Integer(number) => eggdb_lua::Reply::Integer(number),
        Reply::Bulk(bytes) => eggdb_lua::Reply::Bulk(bytes),
        Reply::Array(None) => eggdb_lua::Reply::Array(None),
        Reply::Array(Some(items)) | Reply::Set(items) | Reply::Push(items) => array(items),
        Reply::Double(number) => eggdb_lua::Reply::Bulk(Some(format_double(number).into_bytes())),
        Reply::DoubleWithText(_, text) => eggdb_lua::Reply::Bulk(Some(text)),
        Reply::Map(pairs) | Reply::Pairs(pairs) => flat(pairs),
        Reply::PairArray(pairs) => array(
            pairs
                .into_iter()
                .map(|(left, right)| Reply::Array(Some(vec![left, right])))
                .collect(),
        ),
    }
}

fn resp_reply(reply: eggdb_lua::Reply) -> Reply {
    match reply {
        eggdb_lua::Reply::Status(text) => Reply::Status(String::from_utf8_lossy(&text).into_owned()),
        eggdb_lua::Reply::Error(text) => Reply::Error(String::from_utf8_lossy(&text).into_owned()),
        eggdb_lua::Reply::Integer(number) => Reply::Integer(number),
        eggdb_lua::Reply::Bulk(bytes) => Reply::Bulk(bytes),
        eggdb_lua::Reply::Array(items) => Reply::Array(items.map(|items| items.into_iter().map(resp_reply).collect())),
        eggdb_lua::Reply::Double(number) => Reply::Double(number),
        eggdb_lua::Reply::Boolean(flag) => Reply::Integer(i64::from(flag)),
    }
}

struct ScriptHost<'c, 'k> {
    context: &'c Context<'k>,
    client: &'c mut Client,
}

impl eggdb_lua::Host for ScriptHost<'_, '_> {
    fn command(&mut self, arguments: &[Vec<u8>]) -> eggdb_lua::Reply {
        let name = arguments
            .first()
            .map(|first| String::from_utf8_lossy(first).to_ascii_lowercase())
            .unwrap_or_default();
        if NOT_FROM_SCRIPTS.contains(&name.as_str()) {
            return eggdb_lua::Reply::Error(NOT_ALLOWED.as_bytes().to_vec());
        }
        script_reply(execute(self.context, self.client, arguments))
    }
}

struct Shebang {
    read_only: bool,
}

fn shebang(source: &[u8]) -> Result<(Shebang, Vec<u8>), Reply> {
    if !source.starts_with(b"#!") {
        return Ok((Shebang { read_only: false }, source.to_vec()));
    }
    let end = source.iter().position(|byte| *byte == b'\n').unwrap_or(source.len());
    let line = String::from_utf8_lossy(&source[2..end]).into_owned();
    let mut words = line.split_whitespace();
    if words.next() != Some("lua") {
        return Err(Reply::Error(format!("ERR Could not find engine '{}'", line.trim())));
    }
    let mut read_only = false;
    for word in words {
        let Some(flags) = word.strip_prefix("flags=") else {
            return Err(Reply::Error(format!("ERR Unknown lua shebang option: {word}")));
        };
        for flag in flags.split(',').filter(|flag| !flag.is_empty()) {
            match flag {
                "no-writes" => read_only = true,
                "allow-oom" | "allow-stale" | "no-cluster" | "allow-cross-slot-keys" => {}
                other => return Err(Reply::Error(format!("ERR Unexpected flag in script shebang: {other}"))),
            }
        }
    }
    Ok((Shebang { read_only }, source[end..].to_vec()))
}

pub(crate) type ScriptBody<'b> = &'b dyn Fn(&[Vec<u8>], &[Vec<u8>], &mut dyn eggdb_lua::Host) -> eggdb_lua::Reply;

pub(crate) fn split_keys(args: &[Vec<u8>]) -> Result<(&[Vec<u8>], &[Vec<u8>]), Reply> {
    let Some((count, rest)) = args.split_first() else {
        return Err(Reply::Error("ERR wrong number of arguments".to_owned()));
    };
    let count = match integer(count) {
        Ok(count) if count < 0 => return Err(Reply::Error("ERR Number of keys can't be negative".to_owned())),
        Ok(count) if count as usize > rest.len() => {
            return Err(Reply::Error("ERR Number of keys can't be greater than number of args".to_owned()));
        }
        Ok(count) => count as usize,
        Err(error) => return Err(Reply::Error(error)),
    };
    Ok(rest.split_at(count))
}

pub(crate) fn run_in_transaction(
    context: &Context<'_>,
    client: &mut Client,
    keys: &[Vec<u8>],
    argv: &[Vec<u8>],
    read_only: bool,
    script: ScriptBody<'_>,
) -> Reply {
    let db = client.db;
    let mut reply = None;
    let body_of = |keyspace: &dyn Keyspace, client: &mut Client| {
        let inner = Context {
            keyspace,
            password_sha256: context.password_sha256,
            now_ms: context.now_ms,
            reply_room_bytes: context.reply_room_bytes,
            blocked_clients: context.blocked_clients,
            scripts: context.scripts,
        };
        let mut host = ScriptHost { context: &inner, client };
        resp_reply(script(keys, argv, &mut host))
    };
    let outcome = if read_only {
        context.keyspace.read(db, &mut |keys| {
            let open = ReadOnlyTransaction { db, keys };
            reply = Some(body_of(&open, client));
            Ok(())
        })
    } else {
        context.keyspace.write(db, &mut |keys| {
            let open = OpenTransaction {
                db,
                keys: RefCell::new(keys),
            };
            reply = Some(body_of(&open, client));
            Ok(())
        })
    };
    client.db = db;
    match outcome {
        Err(error) => Reply::Error(error),
        Ok(()) => reply.unwrap_or(Reply::Bulk(None)),
    }
}

fn run(context: &Context<'_>, client: &mut Client, source: &[u8], args: &[Vec<u8>], read_only: bool) -> Reply {
    let (keys, argv) = match split_keys(args) {
        Ok(split) => split,
        Err(reply) => return reply,
    };
    let (flags, body) = match shebang(source) {
        Ok(parsed) => parsed,
        Err(reply) => return reply,
    };
    let script = |keys: &[Vec<u8>], argv: &[Vec<u8>], host: &mut dyn eggdb_lua::Host| {
        eggdb_lua::run_script(&body, keys, argv, host, STEP_BUDGET)
    };
    run_in_transaction(context, client, keys, argv, read_only || flags.read_only, &script)
}

fn script_subcommand(context: &Context<'_>, args: &[Vec<u8>]) -> Reply {
    let Some((subcommand, rest)) = args.split_first() else {
        return Reply::Error("ERR wrong number of arguments for 'script' command".to_owned());
    };
    match String::from_utf8_lossy(subcommand).to_ascii_lowercase().as_str() {
        "load" => match rest {
            [source] => match context.scripts {
                Some(scripts) => Reply::bulk(scripts.load(source).into_bytes()),
                None => Reply::bulk(eggdb_lua::sha1_hex(source).into_bytes()),
            },
            _ => Reply::Error("ERR wrong number of arguments for 'script|load' command".to_owned()),
        },
        "exists" if !rest.is_empty() => Reply::Array(Some(
            rest.iter()
                .map(|sha| {
                    let known = context.scripts.is_some_and(|scripts| scripts.source(sha).is_some());
                    Reply::Integer(i64::from(known))
                })
                .collect(),
        )),
        "flush" => match rest {
            [] => {
                if let Some(scripts) = context.scripts {
                    scripts.flush();
                }
                Reply::ok()
            }
            [mode] if matches!(String::from_utf8_lossy(mode).to_ascii_lowercase().as_str(), "sync" | "async") => {
                if let Some(scripts) = context.scripts {
                    scripts.flush();
                }
                Reply::ok()
            }
            _ => Reply::Error("ERR SCRIPT FLUSH only support SYNC|ASYNC option".to_owned()),
        },
        "kill" => Reply::Error("NOTBUSY No scripts in execution right now.".to_owned()),
        other => Reply::Error(format!(
            "ERR unknown subcommand '{other}'. Try SCRIPT HELP."
        )),
    }
}

/// Runs `EVAL`, `EVALSHA`, their `_RO` forms and `SCRIPT`; `None` for any other command.
pub(crate) fn command(context: &Context<'_>, client: &mut Client, name: &str, args: &[Vec<u8>]) -> Option<Reply> {
    Some(match name {
        "eval" | "eval_ro" => {
            let Some((source, rest)) = args.split_first() else {
                return Some(Reply::Error(format!("ERR wrong number of arguments for '{name}' command")));
            };
            if let Some(scripts) = context.scripts {
                scripts.load(source);
            }
            run(context, client, source, rest, name == "eval_ro")
        }
        "evalsha" | "evalsha_ro" => {
            let Some((sha, rest)) = args.split_first() else {
                return Some(Reply::Error(format!("ERR wrong number of arguments for '{name}' command")));
            };
            if sha.len() != SHA1_HEX_LENGTH {
                return Some(Reply::Error(NO_SCRIPT.to_owned()));
            }
            match context.scripts.and_then(|scripts| scripts.source(sha)) {
                Some(source) => run(context, client, &source, rest, name == "evalsha_ro"),
                None => Reply::Error(NO_SCRIPT.to_owned()),
            }
        }
        "script" => script_subcommand(context, args),
        _ => return None,
    })
}
