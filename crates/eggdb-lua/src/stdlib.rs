use crate::host::Host;
use crate::interpreter::{Interpreter, Unwind, raise};
use crate::value::{Function, TableRef, Value};

type Outcome = Result<Vec<Value>, Unwind>;

fn argument(arguments: &[Value], at: usize) -> Value {
    arguments.get(at).cloned().unwrap_or(Value::Nil)
}

pub(crate) fn number_argument(arguments: &[Value], at: usize, function: &str, line: u32) -> Result<f64, Unwind> {
    match argument(arguments, at).to_number() {
        Some(number) => Ok(number),
        None => raise(line, format!("bad argument #{} to '{function}' (number expected)", at + 1)),
    }
}

pub(crate) fn table_argument(arguments: &[Value], at: usize, function: &str, line: u32) -> Result<TableRef, Unwind> {
    match argument(arguments, at) {
        Value::Table(table) => Ok(table),
        _ => raise(line, format!("bad argument #{} to '{function}' (table expected)", at + 1)),
    }
}

fn print(_: &mut Interpreter, _: &mut dyn Host, _: Vec<Value>, _: u32) -> Outcome {
    Ok(Vec::new())
}

fn type_of(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    if arguments.is_empty() {
        return raise(line, "bad argument #1 to 'type' (value expected)");
    }
    Ok(vec![Value::string(arguments[0].type_name())])
}

fn tostring(interpreter: &mut Interpreter, host: &mut dyn Host, arguments: Vec<Value>, _: u32) -> Outcome {
    let text = interpreter.text_of(&argument(&arguments, 0), host)?;
    Ok(vec![Value::String(text.into())])
}

fn tonumber(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let value = argument(&arguments, 0);
    let base = argument(&arguments, 1);
    if matches!(base, Value::Nil) {
        return Ok(vec![value.to_number().map_or(Value::Nil, Value::Number)]);
    }
    let Some(base) = base.to_number().filter(|base| (2.0..=36.0).contains(base)) else {
        return raise(line, "bad argument #2 to 'tonumber' (base out of range)");
    };
    let Some(bytes) = value.to_bytes() else {
        return Ok(vec![Value::Nil]);
    };
    let text = String::from_utf8_lossy(&bytes).trim().to_lowercase();
    Ok(vec![
        i64::from_str_radix(&text, base as u32)
            .map_or(Value::Nil, |number| Value::Number(number as f64)),
    ])
}

fn next(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let table = table_argument(&arguments, 0, "next", line)?;
    match table.0.borrow().next(&argument(&arguments, 1)) {
        Ok(Some((key, value))) => Ok(vec![key, value]),
        Ok(None) => Ok(vec![Value::Nil]),
        Err(message) => raise(line, message),
    }
}

fn pairs(interpreter: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let table = table_argument(&arguments, 0, "pairs", line)?;
    let next = interpreter.globals.get(&Value::string("next"));
    Ok(vec![next, Value::Table(table), Value::Nil])
}

fn ipairs_step(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let table = table_argument(&arguments, 0, "ipairs", line)?;
    let index = number_argument(&arguments, 1, "ipairs", line)? + 1.0;
    match table.get(&Value::Number(index)) {
        Value::Nil => Ok(vec![Value::Nil]),
        value => Ok(vec![Value::Number(index), value]),
    }
}

fn ipairs(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let table = table_argument(&arguments, 0, "ipairs", line)?;
    Ok(vec![
        Value::Function(Function::Native(ipairs_step, "ipairs_step")),
        Value::Table(table),
        Value::Number(0.0),
    ])
}

fn select(_: &mut Interpreter, _: &mut dyn Host, mut arguments: Vec<Value>, line: u32) -> Outcome {
    if arguments.is_empty() {
        return raise(line, "bad argument #1 to 'select' (number expected)");
    }
    let rest = arguments.split_off(1);
    if matches!(&arguments[0], Value::String(bytes) if &**bytes == b"#") {
        return Ok(vec![Value::Number(rest.len() as f64)]);
    }
    let index = number_argument(&arguments, 0, "select", line)?;
    if index < 1.0 {
        return raise(line, "bad argument #1 to 'select' (index out of range)");
    }
    Ok(rest.into_iter().skip(index as usize - 1).collect())
}

pub(crate) fn unpack(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let table = table_argument(&arguments, 0, "unpack", line)?;
    let from = argument(&arguments, 1).to_number().unwrap_or(1.0) as i64;
    let to = argument(&arguments, 2)
        .to_number()
        .map_or(table.length() as i64, |to| to as i64);
    if to - from >= 1_000_000 {
        return raise(line, "too many results to unpack");
    }
    Ok((from..=to).map(|at| table.get(&Value::Number(at as f64))).collect())
}

fn error(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let value = argument(&arguments, 0);
    let level = argument(&arguments, 1).to_number().unwrap_or(1.0);
    let value = match value {
        Value::String(bytes) if level > 0.0 => {
            let mut text = format!("user_script:{line}: ").into_bytes();
            text.extend_from_slice(&bytes);
            Value::String(text.into())
        }
        other => other,
    };
    Err(Unwind::Error(value, line))
}

fn assert(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    if argument(&arguments, 0).truthy() {
        return Ok(arguments);
    }
    match argument(&arguments, 1) {
        Value::Nil => raise(line, "assertion failed!"),
        message => Err(Unwind::Error(message, line)),
    }
}

fn pcall(interpreter: &mut Interpreter, host: &mut dyn Host, mut arguments: Vec<Value>, line: u32) -> Outcome {
    if arguments.is_empty() {
        return raise(line, "bad argument #1 to 'pcall' (value expected)");
    }
    let rest = arguments.split_off(1);
    match interpreter.call(&arguments[0], rest, host, line) {
        Ok(mut values) => {
            values.insert(0, Value::Boolean(true));
            Ok(values)
        }
        Err(Unwind::Error(value, _)) => Ok(vec![Value::Boolean(false), value]),
        Err(kill) => Err(kill),
    }
}

fn rawget(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let table = table_argument(&arguments, 0, "rawget", line)?;
    Ok(vec![table.get(&argument(&arguments, 1))])
}

fn rawset(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let table = table_argument(&arguments, 0, "rawset", line)?;
    if let Err(message) = table.set(argument(&arguments, 1), argument(&arguments, 2)) {
        return raise(line, message);
    }
    Ok(vec![Value::Table(table)])
}

fn rawequal(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, _: u32) -> Outcome {
    Ok(vec![Value::Boolean(argument(&arguments, 0) == argument(&arguments, 1))])
}

fn setmetatable(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let table = table_argument(&arguments, 0, "setmetatable", line)?;
    let metatable = match argument(&arguments, 1) {
        Value::Table(metatable) => Some(metatable),
        Value::Nil => None,
        _ => return raise(line, "bad argument #2 to 'setmetatable' (nil or table expected)"),
    };
    table.0.borrow_mut().metatable = metatable;
    Ok(vec![Value::Table(table)])
}

fn getmetatable(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, _: u32) -> Outcome {
    Ok(vec![match argument(&arguments, 0) {
        Value::Table(table) => table.0.borrow().metatable.clone().map_or(Value::Nil, Value::Table),
        _ => Value::Nil,
    }])
}

fn table_insert(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let table = table_argument(&arguments, 0, "insert", line)?;
    let length = table.length();
    let result = match arguments.len() {
        2 => table.set(Value::Number((length + 1) as f64), argument(&arguments, 1)),
        3 => {
            let position = number_argument(&arguments, 1, "insert", line)? as usize;
            if position < 1 || position > length + 1 {
                return raise(line, "bad argument #2 to 'insert' (position out of bounds)");
            }
            for at in (position..=length).rev() {
                let moved = table.get(&Value::Number(at as f64));
                let _ = table.set(Value::Number((at + 1) as f64), moved);
            }
            table.set(Value::Number(position as f64), argument(&arguments, 2))
        }
        _ => return raise(line, "wrong number of arguments to 'insert'"),
    };
    result.map(|()| Vec::new()).or_else(|message| raise(line, message))
}

fn table_remove(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let table = table_argument(&arguments, 0, "remove", line)?;
    let length = table.length();
    if length == 0 {
        return Ok(vec![Value::Nil]);
    }
    let position = match argument(&arguments, 1) {
        Value::Nil => length,
        other => other.to_number().unwrap_or(length as f64) as usize,
    };
    let removed = table.get(&Value::Number(position as f64));
    for at in position..length {
        let moved = table.get(&Value::Number((at + 1) as f64));
        let _ = table.set(Value::Number(at as f64), moved);
    }
    let _ = table.set(Value::Number(length as f64), Value::Nil);
    Ok(vec![removed])
}

fn table_concat(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let table = table_argument(&arguments, 0, "concat", line)?;
    let separator = argument(&arguments, 1).to_bytes().unwrap_or_default();
    let from = argument(&arguments, 2).to_number().unwrap_or(1.0) as usize;
    let to = argument(&arguments, 3).to_number().map_or(table.length(), |to| to as usize);
    let mut out = Vec::new();
    for at in from..=to {
        if at > from {
            out.extend_from_slice(&separator);
        }
        match table.get(&Value::Number(at as f64)).to_bytes() {
            Some(bytes) => out.extend_from_slice(&bytes),
            None => return raise(line, format!("invalid value (at index {at}) in table for 'concat'")),
        }
    }
    Ok(vec![Value::String(out.into())])
}

fn table_getn(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let table = table_argument(&arguments, 0, "getn", line)?;
    Ok(vec![Value::Number(table.length() as f64)])
}

fn table_sort(interpreter: &mut Interpreter, host: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let table = table_argument(&arguments, 0, "sort", line)?;
    let comparator = argument(&arguments, 1);
    let length = table.length();
    let mut items: Vec<Value> = (1..=length).map(|at| table.get(&Value::Number(at as f64))).collect();
    let mut failure = None;
    let mut less = |left: &Value, right: &Value| -> bool {
        if failure.is_some() {
            return false;
        }
        let outcome = match &comparator {
            Value::Nil => match (left, right) {
                (Value::Number(a), Value::Number(b)) => Ok(a < b),
                (Value::String(a), Value::String(b)) => Ok(a < b),
                _ => raise(line, format!("attempt to compare {} with {}", left.type_name(), right.type_name())),
            },
            function => interpreter
                .call(function, vec![left.clone(), right.clone()], host, line)
                .map(|values| values.first().is_some_and(Value::truthy)),
        };
        outcome.unwrap_or_else(|error| {
            failure = Some(error);
            false
        })
    };
    for at in 1..items.len() {
        let mut position = at;
        while position > 0 && less(&items[position], &items[position - 1]) {
            items.swap(position, position - 1);
            position -= 1;
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    for (at, item) in items.into_iter().enumerate() {
        let _ = table.set(Value::Number((at + 1) as f64), item);
    }
    Ok(Vec::new())
}

fn math_unary(arguments: &[Value], name: &str, line: u32, operation: fn(f64) -> f64) -> Outcome {
    Ok(vec![Value::Number(operation(number_argument(arguments, 0, name, line)?))])
}

fn math_floor(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    math_unary(&arguments, "floor", line, f64::floor)
}

fn math_ceil(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    math_unary(&arguments, "ceil", line, f64::ceil)
}

fn math_abs(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    math_unary(&arguments, "abs", line, f64::abs)
}

fn math_sqrt(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    math_unary(&arguments, "sqrt", line, f64::sqrt)
}

fn math_log(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    math_unary(&arguments, "log", line, f64::ln)
}

fn math_exp(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    math_unary(&arguments, "exp", line, f64::exp)
}

fn math_fmod(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let left = number_argument(&arguments, 0, "fmod", line)?;
    let right = number_argument(&arguments, 1, "fmod", line)?;
    Ok(vec![Value::Number(left % right)])
}

fn math_pow(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let left = number_argument(&arguments, 0, "pow", line)?;
    let right = number_argument(&arguments, 1, "pow", line)?;
    Ok(vec![Value::Number(left.powf(right))])
}

fn math_extreme(arguments: &[Value], name: &str, line: u32, pick_larger: bool) -> Outcome {
    let mut best = number_argument(arguments, 0, name, line)?;
    for at in 1..arguments.len() {
        let next = number_argument(arguments, at, name, line)?;
        if (pick_larger && next > best) || (!pick_larger && next < best) {
            best = next;
        }
    }
    Ok(vec![Value::Number(best)])
}

fn math_max(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    math_extreme(&arguments, "max", line, true)
}

fn math_min(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    math_extreme(&arguments, "min", line, false)
}

fn math_modf(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let number = number_argument(&arguments, 0, "modf", line)?;
    Ok(vec![Value::Number(number.trunc()), Value::Number(number.fract())])
}

fn library(interpreter: &mut Interpreter, name: &str) -> TableRef {
    let table = TableRef::new();
    interpreter.set_global(name, Value::Table(table.clone()));
    table
}

/// Installs the base library and the `table` and `math` libraries in `interpreter`'s globals.
pub(crate) fn install(interpreter: &mut Interpreter) {
    let globals = interpreter.globals.clone();
    for (name, native) in [
        ("print", print as crate::interpreter::Native),
        ("type", type_of),
        ("tostring", tostring),
        ("tonumber", tonumber),
        ("next", next),
        ("pairs", pairs),
        ("ipairs", ipairs),
        ("select", select),
        ("unpack", unpack),
        ("error", error),
        ("assert", assert),
        ("pcall", pcall),
        ("rawget", rawget),
        ("rawset", rawset),
        ("rawequal", rawequal),
        ("setmetatable", setmetatable),
        ("getmetatable", getmetatable),
    ] {
        interpreter.register(&globals, name, native);
    }
    interpreter.set_global("_G", Value::Table(globals.clone()));
    let table = library(interpreter, "table");
    for (name, native) in [
        ("insert", table_insert as crate::interpreter::Native),
        ("remove", table_remove),
        ("concat", table_concat),
        ("getn", table_getn),
        ("sort", table_sort),
        ("unpack", unpack),
    ] {
        interpreter.register(&table, name, native);
    }
    let math = library(interpreter, "math");
    for (name, native) in [
        ("floor", math_floor as crate::interpreter::Native),
        ("ceil", math_ceil),
        ("abs", math_abs),
        ("sqrt", math_sqrt),
        ("log", math_log),
        ("exp", math_exp),
        ("fmod", math_fmod),
        ("pow", math_pow),
        ("max", math_max),
        ("min", math_min),
        ("modf", math_modf),
    ] {
        interpreter.register(&math, name, native);
    }
    let _ = math.set(Value::string("pi"), Value::Number(std::f64::consts::PI));
    let _ = math.set(Value::string("huge"), Value::Number(f64::INFINITY));
    crate::string_library::install(interpreter);
}
