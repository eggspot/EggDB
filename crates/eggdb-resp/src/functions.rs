use crate::commands::{Client, Context, glob};
use crate::protocol::Reply;
use crate::scripting::{STEP_BUDGET, Scripts, run_in_transaction, split_keys};
use eggdb_lua::RegisteredFunction;

const ENGINE: &str = "LUA";
const NO_WRITES: &str = "no-writes";
const SAVED_LIBRARIES: &[u8] = b"function-libraries";
const PAYLOAD_MAGIC: &[u8] = b"EGGDBFN\x01";
const MOST_PAYLOAD_LIBRARIES: usize = 100_000;

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The bytes `FUNCTION DUMP` answers and the server keeps: every library's code, with a magic
/// prefix and a checksum so `FUNCTION RESTORE` refuses damaged or foreign payloads.
fn payload(libraries: &[&Library]) -> Vec<u8> {
    let mut out = PAYLOAD_MAGIC.to_vec();
    out.extend_from_slice(&u32::try_from(libraries.len()).unwrap_or(u32::MAX).to_le_bytes());
    for library in libraries {
        out.extend_from_slice(&u32::try_from(library.code.len()).unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(&library.code);
    }
    let sum = checksum(&out);
    out.extend_from_slice(&sum.to_le_bytes());
    out
}

fn codes_of(bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let damaged = || "ERR DUMP payload version or checksum are wrong".to_owned();
    let body_end = bytes.len().checked_sub(8).ok_or_else(damaged)?;
    let (body, sum) = bytes.split_at(body_end);
    let mut word = [0u8; 8];
    word.copy_from_slice(sum);
    if !body.starts_with(PAYLOAD_MAGIC) || checksum(body) != u64::from_le_bytes(word) {
        return Err(damaged());
    }
    let mut at = PAYLOAD_MAGIC.len();
    let mut take = |count: usize| -> Result<&[u8], String> {
        let end = at.checked_add(count).filter(|end| *end <= body.len()).ok_or_else(damaged)?;
        let taken = &body[at..end];
        at = end;
        Ok(taken)
    };
    let length = |bytes: &[u8]| {
        let mut word = [0u8; 4];
        word.copy_from_slice(bytes);
        u32::from_le_bytes(word) as usize
    };
    let count = length(take(4)?);
    if count > MOST_PAYLOAD_LIBRARIES {
        return Err(damaged());
    }
    let mut codes = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let size = length(take(4)?);
        codes.push(take(size)?.to_vec());
    }
    if at != body.len() {
        return Err(damaged());
    }
    Ok(codes)
}

fn save(context: &Context<'_>, scripts: &Scripts) -> Result<(), String> {
    let libraries = scripts
        .libraries
        .lock()
        .map_err(|_| "ERR the function libraries are unavailable".to_owned())?;
    if libraries.is_empty() {
        return context.keyspace.write_record(SAVED_LIBRARIES, None);
    }
    let listed: Vec<&Library> = libraries.values().collect();
    context.keyspace.write_record(SAVED_LIBRARIES, Some(&payload(&listed)))
}

/// Loads the libraries a server kept in `keyspace` into `scripts`, at start; a library that no
/// longer loads is skipped and named in the returned messages.
pub(crate) fn restore_saved(scripts: &Scripts, keyspace: &dyn crate::Keyspace) -> Vec<String> {
    let saved = match keyspace.read_record(SAVED_LIBRARIES) {
        Ok(Some(saved)) => saved,
        Ok(None) => return Vec::new(),
        Err(error) => return vec![format!("the saved function libraries could not be read: {error}")],
    };
    let codes = match codes_of(&saved) {
        Ok(codes) => codes,
        Err(error) => return vec![format!("the saved function libraries are damaged: {error}")],
    };
    codes
        .iter()
        .filter_map(|code| load(scripts, code, true).err())
        .map(|error| format!("a saved function library did not load: {error}"))
        .collect()
}

fn restore(scripts: &Scripts, bytes: &[u8], policy: &str) -> Result<(), String> {
    let codes = codes_of(bytes)?;
    let mut incoming = Vec::with_capacity(codes.len());
    for code in &codes {
        let (name, _) = parse_metadata(code)?;
        incoming.push(name);
    }
    match policy {
        "flush" => {
            if let Ok(mut libraries) = scripts.libraries.lock() {
                libraries.clear();
            }
        }
        "append" => {
            let libraries = scripts
                .libraries
                .lock()
                .map_err(|_| "ERR the function libraries are unavailable".to_owned())?;
            if let Some(taken) = incoming.iter().find(|name| libraries.contains_key(*name)) {
                return Err(format!("ERR Library {taken} already exists"));
            }
        }
        _ => {}
    }
    for code in &codes {
        load(scripts, code, true)?;
    }
    Ok(())
}

/// A function library `FUNCTION LOAD` registered: its name, its whole code (with the `#!lua`
/// line), the code the interpreter runs, and the functions it registers.
#[derive(Debug, Clone)]
pub(crate) struct Library {
    name: String,
    code: Vec<u8>,
    body: Vec<u8>,
    functions: Vec<RegisteredFunction>,
}

impl Library {
    fn function(&self, name: &str) -> Option<&RegisteredFunction> {
        self.functions.iter().find(|function| function.name == name)
    }
}

fn parse_metadata(code: &[u8]) -> Result<(String, Vec<u8>), String> {
    if !code.starts_with(b"#!") {
        return Err("ERR Missing library metadata".to_owned());
    }
    let end = code.iter().position(|byte| *byte == b'\n').unwrap_or(code.len());
    let line = String::from_utf8_lossy(&code[2..end]).into_owned();
    let mut words = line.split_whitespace();
    match words.next() {
        Some(engine) if engine.eq_ignore_ascii_case("lua") => {}
        Some(engine) => return Err(format!("ERR Engine '{engine}' not found")),
        None => return Err("ERR Missing library metadata".to_owned()),
    }
    let mut name = None;
    for word in words {
        match word.split_once('=') {
            Some(("name", value)) => name = Some(value.to_owned()),
            _ => return Err(format!("ERR Invalid metadata value given: {word}")),
        }
    }
    let name = name.ok_or("ERR Library name was not given")?;
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        return Err("ERR Library names can only contain letters, numbers, or underscores(_) and must be at least one character long".to_owned());
    }
    Ok((name, code[end..].to_vec()))
}

fn load(scripts: &Scripts, code: &[u8], replace: bool) -> Result<String, String> {
    let (name, body) = parse_metadata(code)?;
    let functions = eggdb_lua::load_library(&body, STEP_BUDGET)?;
    let mut libraries = scripts
        .libraries
        .lock()
        .map_err(|_| "ERR the function libraries are unavailable".to_owned())?;
    if libraries.contains_key(&name) && !replace {
        return Err(format!("ERR Library '{name}' already exists"));
    }
    for function in &functions {
        let taken = libraries
            .values()
            .any(|library| library.name != name && library.function(&function.name).is_some());
        if taken {
            return Err(format!("ERR Function {} already exists", function.name));
        }
    }
    libraries.insert(
        name.clone(),
        Library {
            name: name.clone(),
            code: code.to_vec(),
            body,
            functions,
        },
    );
    Ok(name)
}

fn bulk(text: &str) -> Reply {
    Reply::bulk(text.as_bytes().to_vec())
}

fn listed(library: &Library, with_code: bool) -> Reply {
    let functions = library
        .functions
        .iter()
        .map(|function| {
            Reply::Map(vec![
                (bulk("name"), bulk(&function.name)),
                (
                    bulk("description"),
                    function.description.as_deref().map_or(Reply::Bulk(None), bulk),
                ),
                (
                    bulk("flags"),
                    Reply::Set(function.flags.iter().map(|flag| bulk(flag)).collect()),
                ),
            ])
        })
        .collect();
    let mut fields = vec![
        (bulk("library_name"), bulk(&library.name)),
        (bulk("engine"), bulk(ENGINE)),
        (bulk("functions"), Reply::Array(Some(functions))),
    ];
    if with_code {
        fields.push((bulk("library_code"), Reply::bulk(library.code.clone())));
    }
    Reply::Map(fields)
}

fn list(scripts: &Scripts, args: &[Vec<u8>]) -> Reply {
    let mut with_code = false;
    let mut pattern: Option<&[u8]> = None;
    let mut at = 0;
    while at < args.len() {
        match String::from_utf8_lossy(&args[at]).to_ascii_lowercase().as_str() {
            "withcode" => with_code = true,
            "libraryname" if at + 1 < args.len() => {
                at += 1;
                pattern = Some(&args[at]);
            }
            _ => return Reply::Error("ERR Unknown argument".to_owned()),
        }
        at += 1;
    }
    let Ok(libraries) = scripts.libraries.lock() else {
        return Reply::Error("ERR the function libraries are unavailable".to_owned());
    };
    Reply::Array(Some(
        libraries
            .values()
            .filter(|library| pattern.is_none_or(|pattern| glob(pattern, library.name.as_bytes())))
            .map(|library| listed(library, with_code))
            .collect(),
    ))
}

fn stats(scripts: &Scripts) -> Reply {
    let (libraries, functions) = scripts.libraries.lock().map_or((0, 0), |libraries| {
        let functions = libraries.values().map(|library| library.functions.len()).sum::<usize>();
        (libraries.len(), functions)
    });
    Reply::Map(vec![
        (bulk("running_script"), Reply::Bulk(None)),
        (
            bulk("engines"),
            Reply::Map(vec![(
                bulk(ENGINE),
                Reply::Map(vec![
                    (bulk("libraries_count"), Reply::Integer(libraries as i64)),
                    (bulk("functions_count"), Reply::Integer(functions as i64)),
                ]),
            )]),
        ),
    ])
}

fn flush_mode_valid(rest: &[Vec<u8>]) -> bool {
    match rest {
        [] => true,
        [mode] => matches!(String::from_utf8_lossy(mode).to_ascii_lowercase().as_str(), "sync" | "async"),
        _ => false,
    }
}

fn saved(context: &Context<'_>, scripts: &Scripts, reply: Reply) -> Reply {
    if matches!(reply, Reply::Error(_)) {
        return reply;
    }
    match save(context, scripts) {
        Ok(()) => reply,
        Err(error) => Reply::Error(format!("ERR the function libraries could not be kept: {error}")),
    }
}

fn function_subcommand(context: &Context<'_>, scripts: &Scripts, args: &[Vec<u8>]) -> Reply {
    let Some((subcommand, rest)) = args.split_first() else {
        return Reply::Error("ERR wrong number of arguments for 'function' command".to_owned());
    };
    match String::from_utf8_lossy(subcommand).to_ascii_lowercase().as_str() {
        "load" => {
            let (replace, code) = match rest {
                [code] => (false, code),
                [flag, code] if flag.eq_ignore_ascii_case(b"replace") => (true, code),
                _ => return Reply::Error("ERR wrong number of arguments for 'function|load' command".to_owned()),
            };
            let reply = match load(scripts, code, replace) {
                Ok(name) => bulk(&name),
                Err(error) => Reply::Error(error),
            };
            saved(context, scripts, reply)
        }
        "delete" => match rest {
            [name] => {
                let name = String::from_utf8_lossy(name).into_owned();
                let reply = match scripts.libraries.lock() {
                    Ok(mut libraries) if libraries.remove(&name).is_some() => Reply::ok(),
                    _ => Reply::Error("ERR Library not found".to_owned()),
                };
                saved(context, scripts, reply)
            }
            _ => Reply::Error("ERR wrong number of arguments for 'function|delete' command".to_owned()),
        },
        "flush" if flush_mode_valid(rest) => {
            if let Ok(mut libraries) = scripts.libraries.lock() {
                libraries.clear();
            }
            saved(context, scripts, Reply::ok())
        }
        "dump" if rest.is_empty() => match scripts.libraries.lock() {
            Ok(libraries) => {
                let listed: Vec<&Library> = libraries.values().collect();
                Reply::bulk(payload(&listed))
            }
            Err(_) => Reply::Error("ERR the function libraries are unavailable".to_owned()),
        },
        "restore" => {
            let (bytes, policy) = match rest {
                [bytes] => (bytes, "append".to_owned()),
                [bytes, policy] => (bytes, String::from_utf8_lossy(policy).to_ascii_lowercase()),
                _ => {
                    return Reply::Error(
                        "ERR wrong number of arguments for 'function|restore' command".to_owned(),
                    );
                }
            };
            if !matches!(policy.as_str(), "append" | "replace" | "flush") {
                return Reply::Error("ERR Wrong restore policy given, value should be either FLUSH, APPEND or REPLACE.".to_owned());
            }
            let reply = match restore(scripts, bytes, &policy) {
                Ok(()) => Reply::ok(),
                Err(error) => Reply::Error(error),
            };
            saved(context, scripts, reply)
        }
        "flush" => Reply::Error("ERR FUNCTION FLUSH only supports SYNC|ASYNC option".to_owned()),
        "list" => list(scripts, rest),
        "stats" => stats(scripts),
        "kill" => Reply::Error("NOTBUSY No scripts in execution right now.".to_owned()),
        other => Reply::Error(format!("ERR unknown subcommand '{other}'. Try FUNCTION HELP.")),
    }
}

fn call(context: &Context<'_>, client: &mut Client, scripts: &Scripts, args: &[Vec<u8>], read_only_call: bool) -> Reply {
    let Some((name, rest)) = args.split_first() else {
        return Reply::Error("ERR wrong number of arguments".to_owned());
    };
    let name = String::from_utf8_lossy(name).into_owned();
    let found = scripts.libraries.lock().ok().and_then(|libraries| {
        libraries.values().find_map(|library| {
            library
                .function(&name)
                .map(|function| (library.body.clone(), function.flags.iter().any(|flag| flag == NO_WRITES)))
        })
    });
    let Some((body, no_writes)) = found else {
        return Reply::Error("ERR Function not found".to_owned());
    };
    if read_only_call && !no_writes {
        return Reply::Error("ERR Can not execute a script with write flag using *_ro command.".to_owned());
    }
    let (keys, argv) = match split_keys(rest) {
        Ok(split) => split,
        Err(reply) => return reply,
    };
    let script = |keys: &[Vec<u8>], argv: &[Vec<u8>], host: &mut dyn eggdb_lua::Host| {
        eggdb_lua::call_function(&body, &name, keys, argv, host, STEP_BUDGET)
    };
    run_in_transaction(context, client, keys, argv, no_writes, &script)
}

/// Runs `FUNCTION`, `FCALL` and `FCALL_RO`; `None` for any other command.
pub(crate) fn command(context: &Context<'_>, client: &mut Client, name: &str, args: &[Vec<u8>]) -> Option<Reply> {
    if !matches!(name, "function" | "fcall" | "fcall_ro") {
        return None;
    }
    let Some(scripts) = context.scripts else {
        return Some(Reply::Error("ERR functions are not available here".to_owned()));
    };
    Some(match name {
        "function" => function_subcommand(context, scripts, args),
        _ => call(context, client, scripts, args, name == "fcall_ro"),
    })
}

#[cfg(test)]
mod tests {
    use super::{Library, codes_of, payload};

    fn library(name: &str) -> Library {
        Library {
            name: name.to_owned(),
            code: format!("#!lua name={name}\nredis.register_function('{name}_f', function() return 1 end)").into_bytes(),
            body: Vec::new(),
            functions: Vec::new(),
        }
    }

    #[test]
    fn a_dump_payload_round_trips_and_damage_is_refused() {
        let first = library("one");
        let second = library("two");
        let bytes = payload(&[&first, &second]);
        assert_eq!(codes_of(&bytes).unwrap(), vec![first.code.clone(), second.code.clone()]);
        let mut flipped = bytes.clone();
        flipped[12] ^= 1;
        assert!(codes_of(&flipped).is_err());
        assert!(codes_of(&bytes[..bytes.len() - 1]).is_err());
        assert!(codes_of(b"short").is_err());
    }
}

