use crate::format::{Spec, float, integer, parse_spec};
use crate::host::Host;
use crate::interpreter::{Interpreter, Native, Unwind, raise};
use crate::pattern::{Capture, Found, anchored, find, is_plain, match_at};
use crate::stdlib::{number_argument, table_argument};
use crate::value::{Function, TableRef, Value};

type Outcome = Result<Vec<Value>, Unwind>;

const MAX_STRING_BYTES: usize = 512 * 1024 * 1024;

fn bytes_argument(arguments: &[Value], at: usize, function: &str, line: u32) -> Result<Vec<u8>, Unwind> {
    match arguments.get(at).and_then(Value::to_bytes) {
        Some(bytes) => Ok(bytes),
        None => raise(line, format!("bad argument #{} to '{function}' (string expected)", at + 1)),
    }
}

fn optional_number(arguments: &[Value], at: usize, default: f64, function: &str, line: u32) -> Result<f64, Unwind> {
    match arguments.get(at) {
        None | Some(Value::Nil) => Ok(default),
        Some(_) => number_argument(arguments, at, function, line),
    }
}

fn relative(position: f64, length: usize) -> i64 {
    let position = position as i64;
    if position < 0 { length as i64 + position + 1 } else { position }
}

fn bytes(text: impl Into<Vec<u8>>) -> Value {
    Value::String(text.into().into())
}

fn capture_value(subject: &[u8], capture: Capture) -> Value {
    match capture {
        Capture::Text(from, to) => bytes(&subject[from..to]),
        Capture::Position(position) => Value::Number(position as f64),
    }
}

fn captures_of(subject: &[u8], found: &Found) -> Vec<Value> {
    found.captures.iter().map(|capture| capture_value(subject, *capture)).collect()
}

fn len(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    Ok(vec![Value::Number(bytes_argument(&arguments, 0, "len", line)?.len() as f64)])
}

fn sub(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let text = bytes_argument(&arguments, 0, "sub", line)?;
    let length = text.len();
    let start = relative(optional_number(&arguments, 1, 1.0, "sub", line)?, length).max(1);
    let end = relative(optional_number(&arguments, 2, -1.0, "sub", line)?, length).min(length as i64);
    if start > end {
        return Ok(vec![bytes(Vec::new())]);
    }
    Ok(vec![bytes(&text[start as usize - 1..end as usize])])
}

fn upper(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    Ok(vec![bytes(bytes_argument(&arguments, 0, "upper", line)?.to_ascii_uppercase())])
}

fn lower(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    Ok(vec![bytes(bytes_argument(&arguments, 0, "lower", line)?.to_ascii_lowercase())])
}

fn reverse(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let mut text = bytes_argument(&arguments, 0, "reverse", line)?;
    text.reverse();
    Ok(vec![bytes(text)])
}

fn rep(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let text = bytes_argument(&arguments, 0, "rep", line)?;
    let count = number_argument(&arguments, 1, "rep", line)?;
    if count <= 0.0 {
        return Ok(vec![bytes(Vec::new())]);
    }
    let count = count as usize;
    if text.len().saturating_mul(count) > MAX_STRING_BYTES {
        return raise(line, "resulting string too large");
    }
    Ok(vec![bytes(text.repeat(count))])
}

fn byte(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let text = bytes_argument(&arguments, 0, "byte", line)?;
    let length = text.len();
    let start = relative(optional_number(&arguments, 1, 1.0, "byte", line)?, length);
    let end = relative(optional_number(&arguments, 2, start as f64, "byte", line)?, length);
    let start = start.max(1);
    let end = end.min(length as i64);
    if start > end {
        return Ok(Vec::new());
    }
    Ok(text[start as usize - 1..end as usize]
        .iter()
        .map(|byte| Value::Number(f64::from(*byte)))
        .collect())
}

fn char(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let mut out = Vec::with_capacity(arguments.len());
    for at in 0..arguments.len() {
        let code = number_argument(&arguments, at, "char", line)?;
        if !(0.0..256.0).contains(&code) {
            return raise(line, format!("bad argument #{} to 'char' (invalid value)", at + 1));
        }
        out.push(code as u8);
    }
    Ok(vec![bytes(out)])
}

fn plain_position(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from);
    }
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| at + from)
}

fn search(arguments: &[Value], function: &str, line: u32, positions: bool) -> Outcome {
    let subject = bytes_argument(arguments, 0, function, line)?;
    let pattern = bytes_argument(arguments, 1, function, line)?;
    let init = relative(optional_number(arguments, 2, 1.0, function, line)?, subject.len()).max(1) as usize;
    if init > subject.len() + 1 {
        return Ok(vec![Value::Nil]);
    }
    let plain = positions && (arguments.get(3).is_some_and(Value::truthy) || is_plain(&pattern));
    if plain {
        return Ok(match plain_position(&subject, &pattern, init - 1) {
            Some(at) => vec![Value::Number((at + 1) as f64), Value::Number((at + pattern.len()) as f64)],
            None => vec![Value::Nil],
        });
    }
    let found = match find(&subject, &pattern, init - 1) {
        Ok(found) => found,
        Err(message) => return raise(line, message),
    };
    Ok(match found {
        None => vec![Value::Nil],
        Some(found) if positions => {
            let mut values = vec![Value::Number((found.start + 1) as f64), Value::Number(found.end as f64)];
            if found.explicit_captures {
                values.extend(captures_of(&subject, &found));
            }
            values
        }
        Some(found) => captures_of(&subject, &found),
    })
}

fn find_function(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    search(&arguments, "find", line, true)
}

fn match_function(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    search(&arguments, "match", line, false)
}

fn gmatch_step(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let state = table_argument(&arguments, 0, "gmatch", line)?;
    let subject = state.get(&Value::Number(1.0)).to_bytes().unwrap_or_default();
    let pattern = state.get(&Value::Number(2.0)).to_bytes().unwrap_or_default();
    let mut position = state.get(&Value::Number(3.0)).to_number().unwrap_or(0.0) as usize;
    while position <= subject.len() {
        let found = match match_at(&subject, &pattern, position) {
            Ok(found) => found,
            Err(message) => return raise(line, message),
        };
        if let Some(found) = found {
            let next = if found.end == found.start { found.end + 1 } else { found.end };
            let _ = state.set(Value::Number(3.0), Value::Number(next as f64));
            return Ok(captures_of(&subject, &found));
        }
        position += 1;
    }
    let _ = state.set(Value::Number(3.0), Value::Number((subject.len() + 1) as f64));
    Ok(vec![Value::Nil])
}

fn gmatch(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let subject = bytes_argument(&arguments, 0, "gmatch", line)?;
    let pattern = bytes_argument(&arguments, 1, "gmatch", line)?;
    let state = TableRef::new();
    let _ = state.set(Value::Number(1.0), bytes(subject));
    let _ = state.set(Value::Number(2.0), bytes(pattern));
    let _ = state.set(Value::Number(3.0), Value::Number(0.0));
    Ok(vec![
        Value::Function(Function::Native(gmatch_step, "gmatch_step")),
        Value::Table(state),
        Value::Nil,
    ])
}

fn expand_replacement(template: &[u8], subject: &[u8], found: &Found, out: &mut Vec<u8>, line: u32) -> Result<(), Unwind> {
    let mut at = 0;
    while at < template.len() {
        let byte = template[at];
        at += 1;
        if byte != b'%' {
            out.push(byte);
            continue;
        }
        let Some(&next) = template.get(at) else {
            return raise(line, "invalid use of '%' in replacement string");
        };
        at += 1;
        if !next.is_ascii_digit() {
            if next != b'%' {
                return raise(line, "invalid use of '%' in replacement string");
            }
            out.push(b'%');
            continue;
        }
        let index = usize::from(next - b'0');
        let capture = if index == 0 || (index == 1 && !found.explicit_captures) {
            Capture::Text(found.start, found.end)
        } else {
            match found.captures.get(index - 1) {
                Some(capture) if found.explicit_captures => *capture,
                _ => return raise(line, format!("invalid capture index %{index} in replacement string")),
            }
        };
        match capture_value(subject, capture).to_bytes() {
            Some(text) => out.extend_from_slice(&text),
            None => return raise(line, "invalid capture value"),
        }
    }
    Ok(())
}

fn gsub(interpreter: &mut Interpreter, host: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let subject = bytes_argument(&arguments, 0, "gsub", line)?;
    let pattern = bytes_argument(&arguments, 1, "gsub", line)?;
    let replacement = arguments.get(2).cloned().unwrap_or(Value::Nil);
    if !matches!(replacement, Value::String(_) | Value::Number(_) | Value::Table(_) | Value::Function(_)) {
        return raise(line, "bad argument #3 to 'gsub' (string/function/table expected)");
    }
    let limit = optional_number(&arguments, 3, f64::INFINITY, "gsub", line)?;
    let (is_anchored, body) = anchored(&pattern);
    let mut out = Vec::with_capacity(subject.len());
    let mut position = 0;
    let mut count = 0.0;
    while count < limit {
        let found = match match_at(&subject, body, position) {
            Ok(found) => found,
            Err(message) => return raise(line, message),
        };
        if let Some(found) = &found {
            count += 1.0;
            let whole = &subject[found.start..found.end];
            let first = captures_of(&subject, found).into_iter().next().unwrap_or(Value::Nil);
            let looked_up = match &replacement {
                Value::Table(table) => Some(interpreter.index(&Value::Table(table.clone()), &first, host, line)?),
                Value::Function(_) => Some(
                    interpreter
                        .call(&replacement, captures_of(&subject, found), host, line)?
                        .into_iter()
                        .next()
                        .unwrap_or(Value::Nil),
                ),
                template => {
                    let template = template.to_bytes().unwrap_or_default();
                    expand_replacement(&template, &subject, found, &mut out, line)?;
                    None
                }
            };
            match looked_up {
                None => {}
                Some(Value::Nil | Value::Boolean(false)) => out.extend_from_slice(whole),
                Some(value @ (Value::String(_) | Value::Number(_))) => {
                    out.extend_from_slice(&value.to_bytes().unwrap_or_default());
                }
                Some(other) => return raise(line, format!("invalid replacement value (a {})", other.type_name())),
            }
        }
        match found {
            Some(found) if found.end > position => position = found.end,
            _ if position < subject.len() => {
                out.push(subject[position]);
                position += 1;
            }
            _ => break,
        }
        if is_anchored {
            break;
        }
    }
    out.extend_from_slice(&subject[position.min(subject.len())..]);
    Ok(vec![bytes(out), Value::Number(count)])
}

fn quoted(text: &[u8]) -> Vec<u8> {
    let mut out = vec![b'"'];
    for &byte in text {
        match byte {
            b'"' | b'\\' | b'\n' => {
                out.push(b'\\');
                out.push(byte);
            }
            b'\r' => out.extend_from_slice(b"\\r"),
            0 => out.extend_from_slice(b"\\000"),
            other => out.push(other),
        }
    }
    out.push(b'"');
    out
}

fn format(interpreter: &mut Interpreter, host: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let template = bytes_argument(&arguments, 0, "format", line)?;
    let mut out = Vec::with_capacity(template.len());
    let mut next = 1;
    let mut at = 0;
    while at < template.len() {
        let byte = template[at];
        at += 1;
        if byte != b'%' {
            out.push(byte);
            continue;
        }
        if template.get(at) == Some(&b'%') {
            out.push(b'%');
            at += 1;
            continue;
        }
        let (spec, used) = match parse_spec(&template[at..]) {
            Ok(parsed) => parsed,
            Err(message) => return raise(line, message),
        };
        at += used;
        let index = next;
        next += 1;
        if index >= arguments.len() {
            return raise(line, format!("bad argument #{} to 'format' (no value)", index + 1));
        }
        let piece: Vec<u8> = match spec.conversion {
            b'd' | b'i' | b'u' | b'x' | b'X' | b'o' => {
                integer(&spec, number_argument(&arguments, index, "format", line)?).into_bytes()
            }
            b'c' => {
                let code = number_argument(&arguments, index, "format", line)? as i64 as u8;
                pad_bytes(&spec, &[code])
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' => {
                float(&spec, number_argument(&arguments, index, "format", line)?).into_bytes()
            }
            b'q' => quoted(&bytes_argument(&arguments, index, "format", line)?),
            b's' => {
                let mut text = interpreter.text_of(&arguments[index], host)?;
                if let Some(precision) = spec.precision {
                    text.truncate(precision);
                }
                pad_bytes(&spec, &text)
            }
            other => return raise(line, format!("invalid option '%{}' to 'format'", other as char)),
        };
        out.extend_from_slice(&piece);
    }
    Ok(vec![bytes(out)])
}

fn pad_bytes(spec: &Spec, text: &[u8]) -> Vec<u8> {
    if text.len() >= spec.width {
        return text.to_vec();
    }
    let fill = vec![b' '; spec.width - text.len()];
    if spec.left { [text, &fill].concat() } else { [&fill[..], text].concat() }
}

/// Installs the `string` library: the global table and the one string values index into.
pub(crate) fn install(interpreter: &mut Interpreter) {
    let library = interpreter.string_library.clone();
    for (name, native) in [
        ("len", len as Native),
        ("sub", sub),
        ("upper", upper),
        ("lower", lower),
        ("reverse", reverse),
        ("rep", rep),
        ("byte", byte),
        ("char", char),
        ("find", find_function),
        ("match", match_function),
        ("gmatch", gmatch),
        ("gsub", gsub),
        ("format", format),
    ] {
        interpreter.register(&library, name, native);
    }
    interpreter.set_global("string", Value::Table(library));
}
