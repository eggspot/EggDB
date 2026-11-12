pub(crate) const RECORDS: u64 = 100_000;
const ZIPFIAN_CONSTANT: f64 = 0.99;
const LONGEST_SCAN: u64 = 100;
const PER_MILLE: u64 = 1000;

#[derive(Debug, Clone, Copy)]
pub(crate) enum Operation {
    Read,
    Update,
    Insert,
    Scan(u64),
    ReadModifyWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chooser {
    Zipfian,
    Latest,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Mix {
    read: u64,
    update: u64,
    insert: u64,
    scan: u64,
    read_modify_write: u64,
    chooser: Chooser,
}

const fn mix(read: u64, update: u64, insert: u64, scan: u64, read_modify_write: u64) -> Mix {
    Mix {
        read,
        update,
        insert,
        scan,
        read_modify_write,
        chooser: Chooser::Zipfian,
    }
}

pub(crate) const WORKLOAD_A: Mix = mix(500, 500, 0, 0, 0);
pub(crate) const WORKLOAD_B: Mix = mix(950, 50, 0, 0, 0);
pub(crate) const WORKLOAD_C: Mix = mix(1000, 0, 0, 0, 0);
pub(crate) const WORKLOAD_D: Mix = Mix {
    chooser: Chooser::Latest,
    ..mix(950, 0, 50, 0, 0)
};
pub(crate) const WORKLOAD_E: Mix = mix(0, 0, 50, 950, 0);
pub(crate) const WORKLOAD_F: Mix = mix(500, 0, 0, 0, 500);

pub(crate) struct Random(pub(crate) u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mixed = self.0;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        mixed ^ (mixed >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }
}

pub(crate) struct Zipfian {
    items: u64,
    zeta_n: f64,
    alpha: f64,
    eta: f64,
    half_power: f64,
}

fn zeta(items: u64, theta: f64) -> f64 {
    (1..=items).map(|rank| 1.0 / (rank as f64).powf(theta)).sum()
}

impl Zipfian {
    pub(crate) fn new(items: u64) -> Self {
        let theta = ZIPFIAN_CONSTANT;
        let zeta_2 = zeta(2, theta);
        let zeta_n = zeta(items, theta);
        Self {
            items,
            zeta_n,
            alpha: 1.0 / (1.0 - theta),
            eta: (1.0 - (2.0 / items as f64).powf(1.0 - theta)) / (1.0 - zeta_2 / zeta_n),
            half_power: 1.0 + 0.5f64.powf(theta),
        }
    }

    pub(crate) fn next(&self, random: &mut Random) -> u64 {
        let unit = random.unit();
        let scaled = unit * self.zeta_n;
        if scaled < 1.0 {
            return 0;
        }
        if scaled < self.half_power {
            return 1;
        }
        let rank = self.items as f64 * (self.eta * unit - self.eta + 1.0).powf(self.alpha);
        (rank as u64).min(self.items - 1)
    }
}

pub(crate) struct Driver {
    mix: Mix,
    random: Random,
    zipfian: Zipfian,
    inserted: u64,
}

impl Driver {
    pub(crate) fn new(mix: Mix, seed: u64) -> Self {
        Self {
            mix,
            random: Random(seed),
            zipfian: Zipfian::new(RECORDS),
            inserted: RECORDS,
        }
    }

    fn record(&mut self) -> u64 {
        let drawn = self.zipfian.next(&mut self.random);
        match self.mix.chooser {
            Chooser::Zipfian => drawn % self.inserted,
            Chooser::Latest => self.inserted - 1 - drawn.min(self.inserted - 1),
        }
    }

    pub(crate) fn next(&mut self) -> (Operation, u64) {
        let mut roll = self.random.below(PER_MILLE);
        let mix = self.mix;
        for (share, operation) in [
            (mix.read, Operation::Read),
            (mix.update, Operation::Update),
            (mix.insert, Operation::Insert),
            (mix.scan, Operation::Scan(0)),
            (mix.read_modify_write, Operation::ReadModifyWrite),
        ] {
            if roll < share {
                return match operation {
                    Operation::Insert => {
                        let index = self.inserted;
                        self.inserted += 1;
                        (Operation::Insert, index)
                    }
                    Operation::Scan(_) => {
                        let length = 1 + self.random.below(LONGEST_SCAN);
                        (Operation::Scan(length), self.record())
                    }
                    other => (other, self.record()),
                };
            }
            roll -= share;
        }
        (Operation::Read, self.record())
    }
}

#[cfg(test)]
mod tests {
    use super::{Random, Zipfian};

    #[test]
    fn the_zipfian_chooser_favours_the_first_ranks_and_stays_in_range() {
        let zipfian = Zipfian::new(1000);
        let mut random = Random(7);
        let mut first = 0;
        for _ in 0..10_000 {
            let rank = zipfian.next(&mut random);
            assert!(rank < 1000);
            if rank == 0 {
                first += 1;
            }
        }
        assert!(first > 500, "rank 0 came {first} times in 10000");
    }
}
