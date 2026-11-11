use crate::{LuaError, LuaErrorKind};
use crate::host::{Host, Reply};
use crate::interpreter::{Interpreter, Native, Unwind, raise};
use crate::value::{TableRef, Value, number_text};

type Outcome = Result<Vec<Value>, Unwind>;

const MAX_REPLY_DEPTH: usize = 1000;
const LOG_LEVELS: [(&str, f64); 4] = [("LOG_DEBUG", 0.0), ("LOG_VERBOSE", 1.0), ("LOG_NOTICE", 2.0), ("LOG_WARNING", 3.0)];

fn single_field(name: &str, text: Vec<u8>) -> Value {
    let table = TableRef::new();
    let _ = table.set(Value::string(name), Value::String(text.into()));
    Value::Table(table)
}

fn value_of_reply(reply: Reply) -> Value {
    match reply {
        Reply::Status(text) => single_field("ok", text),
        Reply::Error(text) => single_field("err", text),
        Reply::Integer(number) => Value::Number(number as f64),
        Reply::Bulk(Some(bytes)) => Value::String(bytes.into()),
        Reply::Bulk(None) | Reply::Array(None) => Value::Boolean(false),
        Reply::Array(Some(items)) => {
            let table = TableRef::new();
            for (index, item) in items.into_iter().enumerate() {
                let _ = table.set(Value::Number((index + 1) as f64), value_of_reply(item));
            }
            Value::Table(table)
        }
        Reply::Double(number) => Value::String(number_text(number).into_bytes().into()),
        Reply::Boolean(flag) => Value::Number(if flag { 1.0 } else { 0.0 }),
    }
}

/// The RESP reply of a value a script returns, by Redis's Lua-to-RESP2 rules: numbers are
/// truncated to integers, `true` is 1, `false` and `nil` are the nil bulk string, a table
/// with `err` or `ok` is an error or a status, and any other table is an array up to its first
/// nil.
pub fn reply_of_value(value: &Value) -> Reply {
    reply_at_depth(value, 0)
}

fn reply_at_depth(value: &Value, depth: usize) -> Reply {
    match value {
        Value::Nil | Value::Boolean(false) | Value::Function(_) => Reply::Bulk(None),
        Value::Boolean(true) => Reply::Integer(1),
        Value::Number(number) => Reply::Integer(if number.is_finite() { *number as i64 } else { 0 }),
        Value::String(bytes) => Reply::Bulk(Some(bytes.to_vec())),
        Value::Table(_) if depth >= MAX_REPLY_DEPTH => Reply::Error(b"ERR reached lua stack limit".to_vec()),
        Value::Table(table) => {
            if let Value::String(text) = table.get(&Value::string("err")) {
                return Reply::Error(text.to_vec());
            }
            if let Value::String(text) = table.get(&Value::string("ok")) {
                return Reply::Status(text.to_vec());
            }
            let mut items = Vec::new();
            let mut index = 1.0;
            loop {
                let item = table.get(&Value::Number(index));
                if matches!(item, Value::Nil) {
                    break;
                }
                items.push(reply_at_depth(&item, depth + 1));
                index += 1.0;
            }
            Reply::Array(Some(items))
        }
    }
}

fn command_arguments(arguments: &[Value], line: u32) -> Result<Vec<Vec<u8>>, Unwind> {
    if arguments.is_empty() {
        return raise(line, "Please specify at least one argument for this redis lib call");
    }
    let mut out = Vec::with_capacity(arguments.len());
    for argument in arguments {
        match argument {
            Value::String(bytes) => out.push(bytes.to_vec()),
            Value::Number(_) => out.push(argument.to_bytes().unwrap_or_default()),
            _ => return raise(line, "Lua redis lib command arguments must be strings or integers"),
        }
    }
    Ok(out)
}

fn call(_: &mut Interpreter, host: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let command = command_arguments(&arguments, line)?;
    match host.command(&command) {
        Reply::Error(text) => Err(Unwind::Error(single_field("err", text), line)),
        reply => Ok(vec![value_of_reply(reply)]),
    }
}

fn pcall(_: &mut Interpreter, host: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    match command_arguments(&arguments, line) {
        Ok(command) => Ok(vec![value_of_reply(host.command(&command))]),
        Err(Unwind::Error(Value::String(text), _)) => Ok(vec![single_field("err", text.to_vec())]),
        Err(other) => Err(other),
    }
}

fn text_argument(arguments: &[Value], function: &str, line: u32) -> Result<Vec<u8>, Unwind> {
    match arguments.first().and_then(Value::to_bytes) {
        Some(text) => Ok(text),
        None => raise(line, format!("wrong number or type of arguments to '{function}'")),
    }
}

fn error_reply(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let text = text_argument(&arguments, "error_reply", line)?;
    let text = if text.first() == Some(&b'-') { text[1..].to_vec() } else { text };
    Ok(vec![single_field("err", text)])
}

fn status_reply(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    Ok(vec![single_field("ok", text_argument(&arguments, "status_reply", line)?)])
}

fn sha1hex(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let text = text_argument(&arguments, "sha1hex", line)?;
    Ok(vec![Value::string(sha1_hex(&text))])
}

/// The lowercase hex SHA-1 of `source`: a script's name for `EVALSHA`.
pub fn sha1_hex(source: &[u8]) -> String {
    eggdb_crypto::sha1(source).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn log(_: &mut Interpreter, host: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    if arguments.len() < 2 {
        return raise(line, "redis.log() requires two arguments or more.");
    }
    let level = match arguments[0].to_number() {
        Some(level) if (0.0..=3.0).contains(&level) => level as u8,
        _ => return raise(line, "Invalid debug level."),
    };
    let mut message = Vec::new();
    for (index, argument) in arguments[1..].iter().enumerate() {
        if index > 0 {
            message.push(b' ');
        }
        message.extend_from_slice(&argument.to_bytes().unwrap_or_default());
    }
    host.log(level, &message);
    Ok(Vec::new())
}

fn accepted(_: &mut Interpreter, _: &mut dyn Host, _: Vec<Value>, _: u32) -> Outcome {
    Ok(vec![Value::Boolean(true)])
}

fn setresp(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    match arguments.first().and_then(Value::to_number) {
        Some(version) if version == 2.0 || version == 3.0 => Ok(Vec::new()),
        _ => raise(line, "RESP version must be 2 or 3."),
    }
}

fn install(interpreter: &mut Interpreter) -> TableRef {
    let library = TableRef::new();
    for (name, native) in [
        ("call", call as Native),
        ("pcall", pcall),
        ("error_reply", error_reply),
        ("status_reply", status_reply),
        ("sha1hex", sha1hex),
        ("log", log),
        ("setresp", setresp),
        ("replicate_commands", accepted),
        ("set_repl", accepted),
    ] {
        interpreter.register(&library, name, native);
    }
    for (name, level) in LOG_LEVELS {
        let _ = library.set(Value::string(name), Value::Number(level));
    }
    interpreter.set_global("redis", Value::Table(library.clone()));
    library
}

fn byte_table(items: &[Vec<u8>]) -> Value {
    let table = TableRef::new();
    for (index, item) in items.iter().enumerate() {
        let _ = table.set(Value::Number((index + 1) as f64), Value::String(item.clone().into()));
    }
    Value::Table(table)
}

fn first_word_is_code(message: &str) -> bool {
    let word = message.split(' ').next().unwrap_or_default();
    !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_uppercase())
}

fn error_reply_of(error: LuaError) -> Reply {
    let message = match error.kind {
        LuaErrorKind::Syntax => format!(
            "ERR Error compiling script (new function): user_script:{}: {}",
            error.line, error.message
        ),
        LuaErrorKind::Runtime if error.message.starts_with("user_script:") => format!("ERR {}", error.message),
        LuaErrorKind::Runtime if first_word_is_code(&error.message) => error.message,
        LuaErrorKind::Runtime if error.line > 0 => {
            format!("ERR user_script:{}: {}", error.line, error.message)
        }
        LuaErrorKind::Runtime => format!("ERR {}", error.message),
    };
    Reply::Error(message.into_bytes())
}

/// Runs a Redis script: `KEYS` and `ARGV` set from `keys` and `argv`, the `redis` library
/// sending commands to `host`, at most `step_budget` steps, and the first returned value
/// turned into a reply. Errors come back as error replies, in Redis's `user_script:LINE:` form.
pub fn run_script(source: &[u8], keys: &[Vec<u8>], argv: &[Vec<u8>], host: &mut dyn Host, step_budget: u64) -> Reply {
    let mut interpreter = Interpreter::new();
    install(&mut interpreter);
    interpreter.set_global("KEYS", byte_table(keys));
    interpreter.set_global("ARGV", byte_table(argv));
    match interpreter.run(source, host, step_budget) {
        Ok(values) => reply_of_value(values.first().unwrap_or(&Value::Nil)),
        Err(error) => error_reply_of(error),
    }
}

/// A function a library registered with `redis.register_function`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredFunction {
    /// Its name, unique across the server's libraries.
    pub name: String,
    /// The description it was registered with, if any.
    pub description: Option<String>,
    /// Its flags (`no-writes`, `allow-oom`, ...), as registered.
    pub flags: Vec<String>,
}

const FUNCTION_NAME_RULE: &str =
    "Function names can only contain letters, numbers, or underscores(_) and must be at least one character long";

fn valid_function_name(name: &[u8]) -> bool {
    !name.is_empty() && name.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn register_function(interpreter: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let (name, callback, description, flags) = match arguments.as_slice() {
        [name, callback] => (name.clone(), callback.clone(), Value::Nil, Value::Nil),
        [Value::Table(options)] => (
            options.get(&Value::string("function_name")),
            options.get(&Value::string("callback")),
            options.get(&Value::string("description")),
            options.get(&Value::string("flags")),
        ),
        _ => return raise(line, "wrong number of arguments to redis.register_function"),
    };
    let Some(name) = name.to_bytes().filter(|name| valid_function_name(name)) else {
        return raise(line, FUNCTION_NAME_RULE);
    };
    if !matches!(callback, Value::Function(_)) {
        return raise(line, "callback argument given to redis.register_function must be a function");
    }
    let registered = interpreter.registered_functions.clone();
    let key = Value::String(name.into());
    if !matches!(registered.get(&key), Value::Nil) {
        return raise(line, "Function already exists in the library");
    }
    let entry = TableRef::new();
    let _ = entry.set(Value::string("callback"), callback);
    let _ = entry.set(Value::string("description"), description);
    let _ = entry.set(Value::string("flags"), flags);
    let _ = registered.set(key, Value::Table(entry));
    Ok(Vec::new())
}

fn library_interpreter() -> Interpreter {
    let mut interpreter = Interpreter::new();
    let library = install(&mut interpreter);
    interpreter.register(&library, "register_function", register_function);
    interpreter
}

fn registered_of(interpreter: &Interpreter) -> Result<Vec<RegisteredFunction>, String> {
    let mut functions = Vec::new();
    for (name, entry) in interpreter.registered_functions.pairs() {
        let Value::Table(entry) = entry else { continue };
        let description = entry.get(&Value::string("description")).to_bytes();
        let mut flags = Vec::new();
        match entry.get(&Value::string("flags")) {
            Value::Nil => {}
            Value::Table(list) => {
                for (_, flag) in list.pairs() {
                    let flag = flag.to_bytes().ok_or("ERR flags argument to redis.register_function must be a table representing function flags")?;
                    let flag = String::from_utf8_lossy(&flag).into_owned();
                    if !matches!(flag.as_str(), "no-writes" | "allow-oom" | "allow-stale" | "no-cluster" | "allow-cross-slot-keys") {
                        return Err("ERR unknown flag given".to_owned());
                    }
                    flags.push(flag);
                }
            }
            _ => return Err("ERR flags argument to redis.register_function must be a table representing function flags".to_owned()),
        }
        functions.push(RegisteredFunction {
            name: String::from_utf8_lossy(&name.to_bytes().unwrap_or_default()).into_owned(),
            description: description.map(|text| String::from_utf8_lossy(&text).into_owned()),
            flags,
        });
    }
    Ok(functions)
}

struct LoadingHost;

impl Host for LoadingHost {
    fn command(&mut self, _arguments: &[Vec<u8>]) -> Reply {
        Reply::Error(b"ERR redis.call is not allowed while a library loads".to_vec())
    }
}

/// Runs a function library's code (without its `#!lua` line) to learn the functions it
/// registers, as `FUNCTION LOAD` does. Commands cannot be called while it loads.
pub fn load_library(code: &[u8], step_budget: u64) -> Result<Vec<RegisteredFunction>, String> {
    let mut interpreter = library_interpreter();
    if let Err(error) = interpreter.run(code, &mut LoadingHost, step_budget) {
        let Reply::Error(message) = error_reply_of(error) else {
            return Err("ERR the library failed to load".to_owned());
        };
        return Err(String::from_utf8_lossy(&message).into_owned());
    }
    let functions = registered_of(&interpreter)?;
    if functions.is_empty() {
        return Err("ERR No functions registered".to_owned());
    }
    Ok(functions)
}

/// Calls `function` of a library as `FCALL` does: the library's code runs again in a fresh
/// state, then the function is called with the `keys` and `argv` tables and its first returned
/// value becomes the reply.
pub fn call_function(
    code: &[u8],
    function: &str,
    keys: &[Vec<u8>],
    argv: &[Vec<u8>],
    host: &mut dyn Host,
    step_budget: u64,
) -> Reply {
    let mut interpreter = library_interpreter();
    if let Err(error) = interpreter.run(code, &mut LoadingHost, step_budget) {
        return error_reply_of(error);
    }
    let entry = interpreter.registered_functions.get(&Value::string(function));
    let Value::Table(entry) = entry else {
        return Reply::Error(b"ERR Function not found".to_vec());
    };
    let callback = entry.get(&Value::string("callback"));
    match interpreter.call(&callback, vec![byte_table(keys), byte_table(argv)], host, 0) {
        Ok(values) => reply_of_value(values.first().unwrap_or(&Value::Nil)),
        Err(unwind) => error_reply_of(interpreter.error_of(unwind)),
    }
}
