//! The pieces of the workloads that need no server: the seeded random numbers, YCSB's Zipfian
//! key choice, the operation mixes, TPC-C's non-uniform random and its transaction mix, and the
//! latency summary.

/// A seeded xorshift generator.
#[derive(Debug, Clone)]
pub(crate) struct Random(u64);

impl Random {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub(crate) fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit.max(1)
    }

    pub(crate) fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub(crate) fn between(&mut self, low: u64, high: u64) -> u64 {
        low + self.below(high.saturating_sub(low) + 1)
    }
}

/// YCSB's Zipfian choice of records (Gray et al.'s method, constant 0.99), scrambled by a hash so
/// the popular records spread over the key space.
#[derive(Debug, Clone)]
pub(crate) struct Zipfian {
    items: u64,
    theta: f64,
    alpha: f64,
    zeta: f64,
    eta: f64,
}

const ZIPFIAN_CONSTANT: f64 = 0.99;

fn zeta(items: u64, theta: f64) -> f64 {
    (1..=items).map(|item| 1.0 / (item as f64).powf(theta)).sum()
}

impl Zipfian {
    pub(crate) fn new(items: u64) -> Self {
        let items = items.max(2);
        let theta = ZIPFIAN_CONSTANT;
        let zeta_all = zeta(items, theta);
        let zeta_two = zeta(2, theta);
        Self {
            items,
            theta,
            alpha: 1.0 / (1.0 - theta),
            zeta: zeta_all,
            eta: (1.0 - (2.0 / items as f64).powf(1.0 - theta)) / (1.0 - zeta_two / zeta_all),
        }
    }

    pub(crate) fn next(&self, random: &mut Random) -> u64 {
        let u = random.unit();
        let uz = u * self.zeta;
        let rank = if uz < 1.0 {
            0
        } else if uz < 1.0 + 0.5f64.powf(self.theta) {
            1
        } else {
            ((self.items as f64) * (self.eta * u - self.eta + 1.0).powf(self.alpha)) as u64
        };
        let mut hash = rank.min(self.items - 1).wrapping_add(0x9e37_79b9_7f4a_7c15);
        hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (hash ^ (hash >> 31)) % self.items
    }
}

/// A YCSB core workload's share of reads, out of 100; the rest are updates.
pub(crate) fn read_percent(workload: &str) -> Option<u64> {
    match workload {
        "ycsb-a" => Some(50),
        "ycsb-b" => Some(95),
        "ycsb-c" => Some(100),
        _ => None,
    }
}

/// TPC-C's NURand(A, x, y) with the run's constant `c`.
pub(crate) fn nurand(random: &mut Random, a: u64, c: u64, x: u64, y: u64) -> u64 {
    ((random.between(0, a) | random.between(x, y)) + c) % (y - x + 1) + x
}

/// TPC-C's transactions, in the shares of its mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TpccTransaction {
    NewOrder,
    Payment,
    OrderStatus,
    Delivery,
    StockLevel,
}

/// The next transaction of the mix: 45 % New-Order, 43 % Payment, 4 % each of the others.
pub(crate) fn tpcc_mix(random: &mut Random) -> TpccTransaction {
    match random.below(100) {
        0..45 => TpccTransaction::NewOrder,
        45..88 => TpccTransaction::Payment,
        88..92 => TpccTransaction::OrderStatus,
        92..96 => TpccTransaction::Delivery,
        _ => TpccTransaction::StockLevel,
    }
}

/// Latencies in microseconds, summarised.
#[derive(Debug, Clone, Default)]
pub(crate) struct Latencies {
    samples: Vec<u64>,
}

impl Latencies {
    pub(crate) fn record(&mut self, micros: u64) {
        self.samples.push(micros);
    }

    pub(crate) fn merge(&mut self, other: Latencies) {
        self.samples.extend(other.samples);
    }

    pub(crate) fn count(&self) -> usize {
        self.samples.len()
    }

    pub(crate) fn percentile(&self, percent: f64) -> u64 {
        if self.samples.is_empty() {
            return 0;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let at = ((sorted.len() - 1) as f64 * percent / 100.0).round() as usize;
        sorted[at.min(sorted.len() - 1)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zipfian_keys_stay_in_range_and_favour_a_few() {
        let zipfian = Zipfian::new(1_000);
        let mut random = Random::new(7);
        let mut counts = vec![0u32; 1_000];
        for _ in 0..50_000 {
            counts[zipfian.next(&mut random) as usize] += 1;
        }
        counts.sort_unstable_by(|a, b| b.cmp(a));
        let top: u32 = counts[..10].iter().sum();
        assert!(top > 50_000 / 10, "the ten most popular keys got {top} of 50000");
    }

    #[test]
    fn the_tpcc_mix_and_nurand_follow_the_specification() {
        let mut random = Random::new(3);
        let mut new_orders = 0;
        for _ in 0..10_000 {
            if tpcc_mix(&mut random) == TpccTransaction::NewOrder {
                new_orders += 1;
            }
            let customer = nurand(&mut random, 1023, 259, 1, 3000);
            assert!((1..=3000).contains(&customer));
        }
        assert!((4_000..5_000).contains(&new_orders), "{new_orders}");
        assert_eq!(read_percent("ycsb-b"), Some(95));
        assert_eq!(read_percent("tpcc"), None);
        let mut latencies = Latencies::default();
        for micros in 1..=100 {
            latencies.record(micros);
        }
        assert_eq!(latencies.percentile(50.0), 51);
        assert_eq!(latencies.percentile(99.0), 99);
    }
}
