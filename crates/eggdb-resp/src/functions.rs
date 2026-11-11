use crate::commands::{Client, Context, glob};
use crate::protocol::Reply;
use crate::scripting::{STEP_BUDGET, Scripts, run_in_transaction, split_keys};
use eggdb_lua::RegisteredFunction;

const ENGINE: &str = "LUA";
const NO_WRITES: &str = "no-writes";

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

fn function_subcommand(scripts: &Scripts, args: &[Vec<u8>]) -> Reply {
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
            match load(scripts, code, replace) {
                Ok(name) => bulk(&name),
                Err(error) => Reply::Error(error),
            }
        }
        "delete" => match rest {
            [name] => {
                let name = String::from_utf8_lossy(name).into_owned();
                match scripts.libraries.lock() {
                    Ok(mut libraries) if libraries.remove(&name).is_some() => Reply::ok(),
                    _ => Reply::Error("ERR Library not found".to_owned()),
                }
            }
            _ => Reply::Error("ERR wrong number of arguments for 'function|delete' command".to_owned()),
        },
        "flush" if flush_mode_valid(rest) => {
            if let Ok(mut libraries) = scripts.libraries.lock() {
                libraries.clear();
            }
            Reply::ok()
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
        "function" => function_subcommand(scripts, args),
        _ => call(context, client, scripts, args, name == "fcall_ro"),
    })
}
