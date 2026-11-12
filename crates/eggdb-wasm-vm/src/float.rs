use crate::run::Trap;

const F32_SIGN: u32 = 0x8000_0000;
const F64_SIGN: u64 = 0x8000_0000_0000_0000;

fn pop(stack: &mut Vec<u64>) -> Result<u64, Trap> {
    stack.pop().ok_or(Trap::Invalid("an empty stack"))
}

fn f32_of(bits: u64) -> f32 {
    f32::from_bits(bits as u32)
}

fn f64_of(bits: u64) -> f64 {
    f64::from_bits(bits)
}

fn from_f32(value: f32) -> u64 {
    u64::from(value.to_bits())
}

fn from_f64(value: f64) -> u64 {
    value.to_bits()
}

fn from_i32(value: i32) -> u64 {
    u64::from(value as u32)
}

fn minimum_f32(left: f32, right: f32) -> f32 {
    if left.is_nan() || right.is_nan() {
        return f32::NAN;
    }
    if left == right {
        return if left.is_sign_negative() { left } else { right };
    }
    if left < right { left } else { right }
}

fn maximum_f32(left: f32, right: f32) -> f32 {
    if left.is_nan() || right.is_nan() {
        return f32::NAN;
    }
    if left == right {
        return if left.is_sign_positive() { left } else { right };
    }
    if left > right { left } else { right }
}

fn minimum_f64(left: f64, right: f64) -> f64 {
    if left.is_nan() || right.is_nan() {
        return f64::NAN;
    }
    if left == right {
        return if left.is_sign_negative() { left } else { right };
    }
    if left < right { left } else { right }
}

fn maximum_f64(left: f64, right: f64) -> f64 {
    if left.is_nan() || right.is_nan() {
        return f64::NAN;
    }
    if left == right {
        return if left.is_sign_positive() { left } else { right };
    }
    if left > right { left } else { right }
}

fn truncated(value: f64, lowest: f64, past_highest: f64) -> Result<f64, Trap> {
    if value.is_nan() {
        return Err(Trap::InvalidConversion);
    }
    let whole = value.trunc();
    if whole < lowest || whole >= past_highest {
        return Err(Trap::IntegerOverflow);
    }
    Ok(whole)
}

const I32_LOWEST: f64 = -2_147_483_648.0;
const I32_PAST_HIGHEST: f64 = 2_147_483_648.0;
const U32_PAST_HIGHEST: f64 = 4_294_967_296.0;
const I64_LOWEST: f64 = -9_223_372_036_854_775_808.0;
const I64_PAST_HIGHEST: f64 = 9_223_372_036_854_775_808.0;
const U64_PAST_HIGHEST: f64 = 18_446_744_073_709_551_616.0;

fn trapping_truncation(opcode: u8, value: f64) -> Result<u64, Trap> {
    Ok(match opcode {
        0xa8 | 0xaa => from_i32(truncated(value, I32_LOWEST, I32_PAST_HIGHEST)? as i32),
        0xa9 | 0xab => u64::from(truncated(value, 0.0, U32_PAST_HIGHEST)? as u32),
        0xae | 0xb0 => truncated(value, I64_LOWEST, I64_PAST_HIGHEST)? as i64 as u64,
        _ => truncated(value, 0.0, U64_PAST_HIGHEST)? as u64,
    })
}

/// Runs a saturating float-to-integer conversion (`0xfc` 0 to 7).
pub(crate) fn saturating(operation: u32, stack: &mut Vec<u64>) -> Result<(), Trap> {
    let bits = pop(stack)?;
    let source_is_f32 = matches!(operation, 0 | 1 | 4 | 5);
    let value = if source_is_f32 {
        f64::from(f32_of(bits))
    } else {
        f64_of(bits)
    };
    let converted = match operation {
        0..=3 => {
            if matches!(operation, 0 | 2) {
                from_i32(value as i32)
            } else {
                u64::from(value as u32)
            }
        }
        4..=7 => {
            if matches!(operation, 4 | 6) {
                value as i64 as u64
            } else {
                value as u64
            }
        }
        _ => return Err(Trap::Invalid("an unknown saturating conversion")),
    };
    stack.push(converted);
    Ok(())
}

/// Runs a floating-point comparison, arithmetic or conversion instruction: `0x5b`..`0x66`,
/// `0x8b`..`0xa6`, `0xa8`..`0xab` and `0xae`..`0xbf`.
pub(crate) fn apply(opcode: u8, stack: &mut Vec<u64>) -> Result<(), Trap> {
    match opcode {
        0x5b..=0x60 => {
            let right = f32_of(pop(stack)?);
            let left = f32_of(pop(stack)?);
            let truth = match opcode {
                0x5b => left == right,
                0x5c => left != right,
                0x5d => left < right,
                0x5e => left > right,
                0x5f => left <= right,
                _ => left >= right,
            };
            stack.push(u64::from(truth));
        }
        0x61..=0x66 => {
            let right = f64_of(pop(stack)?);
            let left = f64_of(pop(stack)?);
            let truth = match opcode {
                0x61 => left == right,
                0x62 => left != right,
                0x63 => left < right,
                0x64 => left > right,
                0x65 => left <= right,
                _ => left >= right,
            };
            stack.push(u64::from(truth));
        }
        0x8b..=0x91 => {
            let bits = pop(stack)? as u32;
            let value = f32::from_bits(bits);
            let answer = match opcode {
                0x8b => bits & !F32_SIGN,
                0x8c => bits ^ F32_SIGN,
                0x8d => value.ceil().to_bits(),
                0x8e => value.floor().to_bits(),
                0x8f => value.trunc().to_bits(),
                0x90 => value.round_ties_even().to_bits(),
                _ => value.sqrt().to_bits(),
            };
            stack.push(u64::from(answer));
        }
        0x92..=0x98 => {
            let right_bits = pop(stack)? as u32;
            let left_bits = pop(stack)? as u32;
            let (left, right) = (f32::from_bits(left_bits), f32::from_bits(right_bits));
            let answer = match opcode {
                0x92 => (left + right).to_bits(),
                0x93 => (left - right).to_bits(),
                0x94 => (left * right).to_bits(),
                0x95 => (left / right).to_bits(),
                0x96 => minimum_f32(left, right).to_bits(),
                0x97 => maximum_f32(left, right).to_bits(),
                _ => (left_bits & !F32_SIGN) | (right_bits & F32_SIGN),
            };
            stack.push(u64::from(answer));
        }
        0x99..=0x9f => {
            let bits = pop(stack)?;
            let value = f64::from_bits(bits);
            stack.push(match opcode {
                0x99 => bits & !F64_SIGN,
                0x9a => bits ^ F64_SIGN,
                0x9b => value.ceil().to_bits(),
                0x9c => value.floor().to_bits(),
                0x9d => value.trunc().to_bits(),
                0x9e => value.round_ties_even().to_bits(),
                _ => value.sqrt().to_bits(),
            });
        }
        0xa0..=0xa6 => {
            let right_bits = pop(stack)?;
            let left_bits = pop(stack)?;
            let (left, right) = (f64::from_bits(left_bits), f64::from_bits(right_bits));
            stack.push(match opcode {
                0xa0 => (left + right).to_bits(),
                0xa1 => (left - right).to_bits(),
                0xa2 => (left * right).to_bits(),
                0xa3 => (left / right).to_bits(),
                0xa4 => minimum_f64(left, right).to_bits(),
                0xa5 => maximum_f64(left, right).to_bits(),
                _ => (left_bits & !F64_SIGN) | (right_bits & F64_SIGN),
            });
        }
        0xa8 | 0xa9 | 0xae | 0xaf => {
            let value = f64::from(f32_of(pop(stack)?));
            stack.push(trapping_truncation(opcode, value)?);
        }
        0xaa | 0xab | 0xb0 | 0xb1 => {
            let value = f64_of(pop(stack)?);
            stack.push(trapping_truncation(opcode, value)?);
        }
        0xb2 => {
            let value = pop(stack)? as u32 as i32;
            stack.push(from_f32(value as f32));
        }
        0xb3 => {
            let value = pop(stack)? as u32;
            stack.push(from_f32(value as f32));
        }
        0xb4 => {
            let value = pop(stack)? as i64;
            stack.push(from_f32(value as f32));
        }
        0xb5 => {
            let value = pop(stack)?;
            stack.push(from_f32(value as f32));
        }
        0xb6 => {
            let value = f64_of(pop(stack)?);
            stack.push(from_f32(value as f32));
        }
        0xb7 => {
            let value = pop(stack)? as u32 as i32;
            stack.push(from_f64(f64::from(value)));
        }
        0xb8 => {
            let value = pop(stack)? as u32;
            stack.push(from_f64(f64::from(value)));
        }
        0xb9 => {
            let value = pop(stack)? as i64;
            stack.push(from_f64(value as f64));
        }
        0xba => {
            let value = pop(stack)?;
            stack.push(from_f64(value as f64));
        }
        0xbb => {
            let value = f32_of(pop(stack)?);
            stack.push(from_f64(f64::from(value)));
        }
        0xbc => {
            let bits = pop(stack)?;
            stack.push(u64::from(bits as u32));
        }
        0xbd..=0xbf => {
            let bits = pop(stack)?;
            stack.push(if opcode == 0xbe {
                u64::from(bits as u32)
            } else {
                bits
            });
        }
        other => return Err(Trap::Unsupported(other)),
    }
    Ok(())
}
