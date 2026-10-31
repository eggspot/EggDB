use crate::encoding::encode_key_value;
use crate::types::Value;

const PRECISION: u32 = 12;
const REGISTERS: usize = 1 << PRECISION;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

#[derive(Debug, Clone)]
pub(crate) struct Sketch {
    registers: Box<[u8]>,
    key: Vec<u8>,
}

fn mixed(mut hash: u64) -> u64 {
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

impl Sketch {
    pub(crate) const BYTES: usize = REGISTERS;

    pub(crate) fn new() -> Self {
        Self {
            registers: vec![0u8; REGISTERS].into_boxed_slice(),
            key: Vec::new(),
        }
    }

    pub(crate) fn add(&mut self, value: &Value) {
        if matches!(value, Value::Null) {
            return;
        }
        self.key.clear();
        encode_key_value(&mut self.key, value);
        let hash = mixed(
            self.key
                .iter()
                .fold(FNV_OFFSET, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)),
        );
        let register = (hash >> (64 - PRECISION)) as usize;
        let rest = (hash << PRECISION) | (1 << (PRECISION - 1));
        let rank = rest.leading_zeros() as u8 + 1;
        if rank > self.registers[register] {
            self.registers[register] = rank;
        }
    }

    pub(crate) fn estimate(&self) -> i64 {
        let count = REGISTERS as f64;
        let alpha = 0.7213 / (1.0 + 1.079 / count);
        let sum: f64 = self
            .registers
            .iter()
            .map(|&rank| 2f64.powi(-i32::from(rank)))
            .sum();
        let raw = alpha * count * count / sum;
        let empty = self.registers.iter().filter(|&&rank| rank == 0).count();
        let estimate = if raw <= 2.5 * count && empty > 0 {
            count * (count / empty as f64).ln()
        } else {
            raw
        };
        estimate.round() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_stay_within_a_few_percent_of_the_true_count() {
        for distinct in [0u32, 1, 10, 1000, 50_000, 300_000] {
            let mut sketch = Sketch::new();
            for _ in 0..2 {
                for value in 0..distinct {
                    sketch.add(&Value::Integer(value as i32));
                }
            }
            sketch.add(&Value::Null);
            let estimate = sketch.estimate() as f64;
            let truth = f64::from(distinct);
            let error = if truth == 0.0 { estimate } else { (estimate - truth).abs() / truth };
            assert!(error < 0.05, "{distinct} distinct values estimated as {estimate}");
        }
    }
}
