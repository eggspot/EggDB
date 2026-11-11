use crate::LuaErrorKind;
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
        Err(error) => {
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
    }
}
