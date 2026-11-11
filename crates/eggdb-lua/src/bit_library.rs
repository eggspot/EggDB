use crate::host::Host;
use crate::interpreter::{Interpreter, Native, Unwind};
use crate::stdlib::number_argument;
use crate::value::{TableRef, Value};

type Outcome = Result<Vec<Value>, Unwind>;

fn to_bits(number: f64) -> u32 {
    if !number.is_finite() {
        return 0;
    }
    number.round_ties_even().rem_euclid(4_294_967_296.0) as u32
}

fn bits(arguments: &[Value], at: usize, function: &str, line: u32) -> Result<u32, Unwind> {
    number_argument(arguments, at, function, line).map(to_bits)
}

fn result(value: u32) -> Outcome {
    Ok(vec![Value::Number(f64::from(value as i32))])
}

fn tobit(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    result(bits(&arguments, 0, "tobit", line)?)
}

fn bnot(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    result(!bits(&arguments, 0, "bnot", line)?)
}

fn fold(arguments: &[Value], function: &str, line: u32, operation: fn(u32, u32) -> u32) -> Outcome {
    let mut value = bits(arguments, 0, function, line)?;
    for at in 1..arguments.len() {
        value = operation(value, bits(arguments, at, function, line)?);
    }
    result(value)
}

fn band(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    fold(&arguments, "band", line, |left, right| left & right)
}

fn bor(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    fold(&arguments, "bor", line, |left, right| left | right)
}

fn bxor(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    fold(&arguments, "bxor", line, |left, right| left ^ right)
}

fn shift(arguments: &[Value], function: &str, line: u32, operation: fn(u32, u32) -> u32) -> Outcome {
    let value = bits(arguments, 0, function, line)?;
    let count = bits(arguments, 1, function, line)? & 31;
    result(operation(value, count))
}

fn lshift(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    shift(&arguments, "lshift", line, |value, count| value << count)
}

fn rshift(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    shift(&arguments, "rshift", line, |value, count| value >> count)
}

fn arshift(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    shift(&arguments, "arshift", line, |value, count| ((value as i32) >> count) as u32)
}

fn rol(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    shift(&arguments, "rol", line, u32::rotate_left)
}

fn ror(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    shift(&arguments, "ror", line, u32::rotate_right)
}

fn bswap(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    result(bits(&arguments, 0, "bswap", line)?.swap_bytes())
}

fn tohex(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let value = bits(&arguments, 0, "tohex", line)?;
    let digits = match arguments.get(1) {
        None | Some(Value::Nil) => 8,
        Some(_) => to_bits(number_argument(&arguments, 1, "tohex", line)?) as i32,
    };
    let upper = digits < 0;
    let count = digits.unsigned_abs().clamp(1, 8) as usize;
    let mask = if count == 8 { u32::MAX } else { (1u32 << (count * 4)) - 1 };
    let text = if upper {
        format!("{:0count$X}", value & mask)
    } else {
        format!("{:0count$x}", value & mask)
    };
    Ok(vec![Value::string(text)])
}

/// Installs the `bit` library (LuaBitOp, as Redis ships it): 32-bit operations on numbers.
pub(crate) fn install(interpreter: &mut Interpreter) {
    let library = TableRef::new();
    for (name, native) in [
        ("tobit", tobit as Native),
        ("bnot", bnot),
        ("band", band),
        ("bor", bor),
        ("bxor", bxor),
        ("lshift", lshift),
        ("rshift", rshift),
        ("arshift", arshift),
        ("rol", rol),
        ("ror", ror),
        ("bswap", bswap),
        ("tohex", tohex),
    ] {
        interpreter.register(&library, name, native);
    }
    interpreter.set_global("bit", Value::Table(library));
}
