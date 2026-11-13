use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;
use std::cell::RefCell;

pub(crate) const FUNCTION: &str = "eggdb_tablesample";
const NO_SEED: i64 = i64::MIN;

thread_local! {
    static COUNTERS: RefCell<Vec<(i64, u64)>> = const { RefCell::new(Vec::new()) };
    static UNSEEDED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Forgets the row counters of `REPEATABLE` samples, as a statement starts.
pub(crate) fn reset() {
    COUNTERS.with(|counters| counters.borrow_mut().clear());
}

fn mixed(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    value ^ (value >> 33)
}

fn next_draw(seed: i64) -> u64 {
    if seed == NO_SEED {
        let nanos = eggdb_platform_core::time::SystemTime::now()
            .duration_since(eggdb_platform_core::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos() as u64);
        let counter = UNSEEDED.with(|held| {
            held.set(held.get().wrapping_add(1));
            held.get()
        });
        return mixed(nanos ^ counter.rotate_left(32));
    }
    let counter = COUNTERS.with(|counters| {
        let mut counters = counters.borrow_mut();
        match counters.iter_mut().find(|(known, _)| *known == seed) {
            Some(entry) => {
                entry.1 += 1;
                entry.1
            }
            None => {
                counters.push((seed, 1));
                1
            }
        }
    });
    mixed((seed as u64).rotate_left(17) ^ mixed(counter))
}

/// `eggdb_tablesample(percent, seed)`: 1 when the row being read belongs to a `TABLESAMPLE`
/// of `percent` percent; with a seed (`REPEATABLE`) the n-th row of a scan gets the same answer
/// in every run over the same rows, without one each run draws anew.
pub(crate) fn function(values: &[Value]) -> SqlResult<Value> {
    let percent = values.first().and_then(Value::as_float).ok_or_else(|| {
        SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "TABLESAMPLE needs a percentage")
    })?;
    if !(0.0..=100.0).contains(&percent) {
        return Err(SqlError::new(
            SqlState::INVALID_TABLESAMPLE_ARGUMENT,
            "sample percentage must be between 0 and 100",
        ));
    }
    let seed = match values.get(1) {
        None | Some(Value::Null) => NO_SEED,
        Some(value) => value
            .as_float()
            .map(|seed| seed as i64)
            .ok_or_else(|| SqlError::new(SqlState::INVALID_TABLESAMPLE_ARGUMENT, "TABLESAMPLE REPEATABLE needs a number"))?,
    };
    let draw = next_draw(seed) as f64 / u64::MAX as f64;
    Ok(Value::Integer(i32::from(draw * 100.0 < percent)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeatable_samples_repeat_and_bounds_hold() {
        let run = || -> Vec<Value> {
            reset();
            (0..20).map(|_| function(&[Value::Double(50.0), Value::Integer(7)]).unwrap()).collect()
        };
        assert_eq!(run(), run());
        reset();
        assert_eq!(function(&[Value::Double(0.0), Value::Integer(1)]).unwrap(), Value::Integer(0));
        assert_eq!(function(&[Value::Double(100.0), Value::Integer(1)]).unwrap(), Value::Integer(1));
        assert!(function(&[Value::Double(101.0)]).is_err());
    }
}
