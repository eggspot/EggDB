#[derive(Debug, Clone)]
pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub(crate) fn one_in_million(&mut self, per_million: u32) -> bool {
        self.next_u64() % 1_000_000 < u64::from(per_million)
    }

    pub(crate) fn coin(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}

#[cfg(test)]
mod tests {
    use super::SplitMix64;

    #[test]
    fn matches_the_published_reference_sequence_for_seed_zero() {
        let mut rng = SplitMix64::new(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(rng.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn rates_of_zero_and_a_million_are_never_and_always() {
        let mut rng = SplitMix64::new(7);
        assert!((0..1000).all(|_| !rng.one_in_million(0)));
        assert!((0..1000).all(|_| rng.one_in_million(1_000_000)));
    }
}
