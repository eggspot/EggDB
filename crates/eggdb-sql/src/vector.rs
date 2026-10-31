use crate::aggregate::{Accumulator, RowFeed, Sum};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

const BATCH_ROWS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BatchOperation {
    Count,
    Sum,
    Min,
    Max,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BatchCall {
    pub(crate) slot: Option<usize>,
    pub(crate) operation: BatchOperation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Width {
    Small,
    Normal,
    Big,
}

#[derive(Debug)]
struct Lane {
    batch: Vec<i64>,
    rows: i64,
    values: i64,
    width: Option<Width>,
    wide: bool,
    total: i128,
    least: i64,
    greatest: i64,
}

impl Lane {
    fn new() -> Self {
        Self {
            batch: Vec::with_capacity(BATCH_ROWS),
            rows: 0,
            values: 0,
            width: None,
            wide: false,
            total: 0,
            least: i64::MAX,
            greatest: i64::MIN,
        }
    }

    fn flush(&mut self, operation: BatchOperation) {
        if self.batch.is_empty() {
            return;
        }
        self.values += self.batch.len() as i64;
        match operation {
            BatchOperation::Count => {}
            BatchOperation::Sum => self.total += sum(&self.batch),
            BatchOperation::Min => self.least = self.least.min(least(&self.batch)),
            BatchOperation::Max => self.greatest = self.greatest.max(greatest(&self.batch)),
        }
        self.batch.clear();
    }

    fn take(&mut self, value: &Value) -> SqlResult<()> {
        let (number, width) = match value {
            Value::Null => return Ok(()),
            Value::SmallInt(number) => (i64::from(*number), Width::Small),
            Value::Integer(number) => (i64::from(*number), Width::Normal),
            Value::BigInt(number) => (*number, Width::Big),
            _ => {
                return Err(SqlError::new(
                    SqlState::INTERNAL_ERROR,
                    "a batched aggregate met a value that is not an integer",
                ));
            }
        };
        self.wide |= width == Width::Big;
        self.width = Some(width);
        self.batch.push(number);
        Ok(())
    }

    fn accumulator(mut self, call: &BatchCall) -> Accumulator {
        self.flush(call.operation);
        let extreme = |number: i64| match self.width {
            Some(Width::Small) => Value::SmallInt(number as i16),
            Some(Width::Normal) => Value::Integer(number as i32),
            Some(Width::Big) => Value::BigInt(number),
            None => Value::Null,
        };
        match call.operation {
            BatchOperation::Count if call.slot.is_none() => Accumulator::Count(self.rows),
            BatchOperation::Count => Accumulator::Count(self.values),
            BatchOperation::Sum if self.values == 0 => Accumulator::Sum(Sum::Empty),
            BatchOperation::Sum => Accumulator::Sum(Sum::Integer {
                total: self.total,
                wide: self.wide,
            }),
            BatchOperation::Min if self.values == 0 => Accumulator::Extreme(Value::Null),
            BatchOperation::Min => Accumulator::Extreme(extreme(self.least)),
            BatchOperation::Max if self.values == 0 => Accumulator::Extreme(Value::Null),
            BatchOperation::Max => Accumulator::Extreme(extreme(self.greatest)),
        }
    }
}

fn sum(values: &[i64]) -> i128 {
    let mut high = [0i64; 8];
    let mut low = [0i64; 8];
    let mut chunks = values.chunks_exact(8);
    for chunk in &mut chunks {
        for lane in 0..8 {
            high[lane] += chunk[lane] >> 32;
            low[lane] += chunk[lane] & 0xffff_ffff;
        }
    }
    let mut total: i128 = chunks.remainder().iter().map(|&value| i128::from(value)).sum();
    for lane in 0..8 {
        total += (i128::from(high[lane]) << 32) + i128::from(low[lane]);
    }
    total
}

fn least(values: &[i64]) -> i64 {
    values.iter().copied().fold(i64::MAX, i64::min)
}

fn greatest(values: &[i64]) -> i64 {
    values.iter().copied().fold(i64::MIN, i64::max)
}

pub(crate) fn run_batched(calls: &[BatchCall], feed: &mut RowFeed<'_>) -> SqlResult<Vec<Accumulator>> {
    let mut lanes: Vec<Lane> = calls.iter().map(|_| Lane::new()).collect();
    feed(&mut |row| {
        for (call, lane) in calls.iter().zip(lanes.iter_mut()) {
            lane.rows += 1;
            if let Some(slot) = call.slot {
                lane.take(row.get(slot).unwrap_or(&Value::Null))?;
                if lane.batch.len() >= BATCH_ROWS {
                    lane.flush(call.operation);
                }
            }
        }
        Ok(true)
    })?;
    Ok(lanes
        .into_iter()
        .zip(calls)
        .map(|(lane, call)| lane.accumulator(call))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lane_kernels_agree_with_plain_loops_at_the_extremes() {
        let values: Vec<i64> = (0..10_000i64)
            .map(|index| match index % 5 {
                0 => i64::MAX,
                1 => i64::MIN,
                2 => -index,
                3 => index * 1_000_003,
                _ => 0,
            })
            .collect();
        let plain: i128 = values.iter().map(|&value| i128::from(value)).sum();
        assert_eq!(sum(&values), plain);
        assert_eq!(sum(&values[..7]), values[..7].iter().map(|&v| i128::from(v)).sum::<i128>());
        assert_eq!(least(&values), i64::MIN);
        assert_eq!(greatest(&values), i64::MAX);
        assert_eq!(sum(&[]), 0);
    }
}
