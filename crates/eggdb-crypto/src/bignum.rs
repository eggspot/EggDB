const LIMB_BITS: usize = 64;
const LIMB_BYTES: usize = 8;
const WINDOW_BITS: usize = 4;
const WINDOW_ENTRIES: usize = 1 << WINDOW_BITS;
const WINDOW_MASK: u64 = (1 << WINDOW_BITS) - 1;

pub(crate) fn add_with_carry(a: u64, b: u64, carry: u64) -> (u64, u64) {
    let wide = u128::from(a) + u128::from(b) + u128::from(carry);
    (wide as u64, (wide >> 64) as u64)
}

pub(crate) fn subtract_with_borrow(a: u64, b: u64, borrow: u64) -> (u64, u64) {
    let wide = u128::from(a).wrapping_sub(u128::from(b) + u128::from(borrow));
    (wide as u64, (wide >> 127) as u64)
}

pub(crate) fn multiply_add(accumulator: u64, a: u64, b: u64, carry: u64) -> (u64, u64) {
    let wide = u128::from(accumulator) + u128::from(a) * u128::from(b) + u128::from(carry);
    (wide as u64, (wide >> 64) as u64)
}

pub(crate) fn mask_of(bit: u64) -> u64 {
    0u64.wrapping_sub(bit & 1)
}

pub(crate) fn equal_bit(a: u64, b: u64) -> u64 {
    let difference = a ^ b;
    ((difference | difference.wrapping_neg()) >> 63) ^ 1
}

fn subtract_in_place(value: &mut [u64], subtrahend: &[u64]) -> u64 {
    let mut borrow = 0;
    for (limb, &other) in value.iter_mut().zip(subtrahend) {
        (*limb, borrow) = subtract_with_borrow(*limb, other, borrow);
    }
    borrow
}

fn select_in_place(bit: u64, destination: &mut [u64], source: &[u64]) {
    let mask = mask_of(bit);
    for (destination, &source) in destination.iter_mut().zip(source) {
        *destination = (source & mask) | (*destination & !mask);
    }
}

/// Reads a big-endian unsigned integer into `limb_count` little-endian 64-bit limbs, or `None`
/// when it does not fit. Leading zero bytes are allowed.
pub(crate) fn limbs_from_be_bytes(bytes: &[u8], limb_count: usize) -> Option<Vec<u64>> {
    let first_nonzero = bytes
        .iter()
        .position(|&byte| byte != 0)
        .unwrap_or(bytes.len());
    let significant = &bytes[first_nonzero..];
    if significant.len() > limb_count * LIMB_BYTES {
        return None;
    }
    let mut limbs = vec![0u64; limb_count];
    for (index, &byte) in significant.iter().rev().enumerate() {
        limbs[index / LIMB_BYTES] |= u64::from(byte) << (8 * (index % LIMB_BYTES));
    }
    Some(limbs)
}

/// Writes limbs as a big-endian unsigned integer of exactly `length` bytes, dropping any bytes
/// above that length.
pub(crate) fn limbs_to_be_bytes(limbs: &[u64], length: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; length];
    for (index, byte) in bytes.iter_mut().rev().enumerate() {
        if let Some(limb) = limbs.get(index / LIMB_BYTES) {
            *byte = (limb >> (8 * (index % LIMB_BYTES))) as u8;
        }
    }
    bytes
}

fn bit_length(limbs: &[u64]) -> usize {
    limbs
        .iter()
        .rposition(|&limb| limb != 0)
        .map_or(0, |top| top * LIMB_BITS + (LIMB_BITS - limbs[top].leading_zeros() as usize))
}

/// An odd modulus prepared for Montgomery multiplication with R = 2^(64 * limbs). Values in
/// Montgomery form are `limb_count()` limbs below the modulus. Multiplication and the secret
/// exponentiation take the same steps and touch the same memory whatever the values.
#[derive(Clone)]
pub(crate) struct MontgomeryModulus {
    limbs: Vec<u64>,
    negated_inverse: u64,
    r_squared: Vec<u64>,
    bits: usize,
}

impl MontgomeryModulus {
    /// Prepares the big-endian `modulus`, or `None` when it is even or below 3.
    pub(crate) fn new(modulus: &[u8]) -> Option<Self> {
        let first_nonzero = modulus.iter().position(|&byte| byte != 0)?;
        let significant = &modulus[first_nonzero..];
        let limb_count = significant.len().div_ceil(LIMB_BYTES);
        let limbs = limbs_from_be_bytes(significant, limb_count)?;
        let lowest = *limbs.first()?;
        let bits = bit_length(&limbs);
        if lowest & 1 == 0 || bits < 2 {
            return None;
        }
        let mut inverse = lowest;
        for _ in 0..6 {
            inverse = inverse.wrapping_mul(2u64.wrapping_sub(lowest.wrapping_mul(inverse)));
        }
        let mut modulus = Self {
            limbs,
            negated_inverse: inverse.wrapping_neg(),
            r_squared: Vec::new(),
            bits,
        };
        let mut r_squared = vec![0u64; limb_count];
        r_squared[0] = 1;
        for _ in 0..2 * LIMB_BITS * limb_count {
            r_squared = modulus.add(&r_squared, &r_squared);
        }
        modulus.r_squared = r_squared;
        Some(modulus)
    }

    pub(crate) fn limb_count(&self) -> usize {
        self.limbs.len()
    }

    pub(crate) fn bits(&self) -> usize {
        self.bits
    }

    pub(crate) fn byte_length(&self) -> usize {
        self.bits.div_ceil(8)
    }

    pub(crate) fn to_be_bytes(&self) -> Vec<u8> {
        limbs_to_be_bytes(&self.limbs, self.byte_length())
    }

    /// Reads a big-endian value that must be below the modulus.
    pub(crate) fn read_below(&self, bytes: &[u8]) -> Option<Vec<u64>> {
        let limbs = limbs_from_be_bytes(bytes, self.limb_count())?;
        let mut difference = limbs.clone();
        let borrow = subtract_in_place(&mut difference, &self.limbs);
        (borrow == 1).then_some(limbs)
    }

    fn reduce_once(&self, value: &mut [u64], top: u64) {
        let mut difference = value.to_vec();
        let borrow = subtract_in_place(&mut difference, &self.limbs);
        let (_, below_modulus) = subtract_with_borrow(top, 0, borrow);
        select_in_place(below_modulus ^ 1, value, &difference);
    }

    fn add(&self, a: &[u64], b: &[u64]) -> Vec<u64> {
        let mut sum = vec![0u64; self.limb_count()];
        let mut carry = 0;
        for ((sum, &a), &b) in sum.iter_mut().zip(a).zip(b) {
            (*sum, carry) = add_with_carry(a, b, carry);
        }
        self.reduce_once(&mut sum, carry);
        sum
    }

    /// The Montgomery product a * b / R modulo the modulus, for a and b below it.
    pub(crate) fn multiply(&self, a: &[u64], b: &[u64]) -> Vec<u64> {
        let count = self.limb_count();
        let mut t = vec![0u64; count + 2];
        for &b_limb in b.iter().take(count) {
            let mut carry = 0;
            for (t_limb, &a_limb) in t.iter_mut().zip(a.iter().take(count)) {
                (*t_limb, carry) = multiply_add(*t_limb, a_limb, b_limb, carry);
            }
            let (sum, overflow) = add_with_carry(t[count], carry, 0);
            t[count] = sum;
            t[count + 1] = overflow;
            let m = t[0].wrapping_mul(self.negated_inverse);
            let (_, mut carry) = multiply_add(t[0], m, self.limbs[0], 0);
            for (index, &modulus_limb) in self.limbs.iter().enumerate().skip(1) {
                (t[index - 1], carry) = multiply_add(t[index], m, modulus_limb, carry);
            }
            let (sum, overflow) = add_with_carry(t[count], carry, 0);
            t[count - 1] = sum;
            t[count] = t[count + 1] + overflow;
        }
        let top = t[count];
        t.truncate(count);
        self.reduce_once(&mut t, top);
        t
    }

    pub(crate) fn to_montgomery(&self, value: &[u64]) -> Vec<u64> {
        self.multiply(value, &self.r_squared)
    }

    pub(crate) fn from_montgomery(&self, value: &[u64]) -> Vec<u64> {
        let mut one = vec![0u64; self.limb_count()];
        one[0] = 1;
        self.multiply(value, &one)
    }

    fn montgomery_one(&self) -> Vec<u64> {
        let mut one = vec![0u64; self.limb_count()];
        one[0] = 1;
        self.to_montgomery(&one)
    }

    /// `base` (Montgomery form) to the power `exponent` (plain limbs), in Montgomery form, by a
    /// fixed 4-bit window over every limb of `exponent`: the same squarings, multiplications and
    /// table reads whatever the exponent's value, for private exponents.
    pub(crate) fn power_secret(&self, base: &[u64], exponent: &[u64]) -> Vec<u64> {
        let one = self.montgomery_one();
        let mut table = Vec::with_capacity(WINDOW_ENTRIES);
        let mut power = one.clone();
        table.push(one.clone());
        for _ in 1..WINDOW_ENTRIES {
            power = self.multiply(&power, base);
            table.push(power.clone());
        }
        let mut result = one;
        for &limb in exponent.iter().rev() {
            for window in (0..LIMB_BITS / WINDOW_BITS).rev() {
                for _ in 0..WINDOW_BITS {
                    result = self.multiply(&result, &result);
                }
                let index = (limb >> (window * WINDOW_BITS)) & WINDOW_MASK;
                let mut selected = vec![0u64; self.limb_count()];
                for (position, entry) in (0u64..).zip(&table) {
                    select_in_place(equal_bit(position, index), &mut selected, entry);
                }
                result = self.multiply(&result, &selected);
            }
        }
        result
    }

    /// `base` (Montgomery form) to the power `exponent` (plain limbs), in Montgomery form, by
    /// square and multiply. Its timing depends on the exponent, so it is only for public
    /// exponents.
    pub(crate) fn power_public(&self, base: &[u64], exponent: &[u64]) -> Vec<u64> {
        let mut result = self.montgomery_one();
        for bit in (0..bit_length(exponent)).rev() {
            result = self.multiply(&result, &result);
            if (exponent[bit / LIMB_BITS] >> (bit % LIMB_BITS)) & 1 == 1 {
                result = self.multiply(&result, base);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modulus_of(value: u64) -> MontgomeryModulus {
        MontgomeryModulus::new(&value.to_be_bytes()).unwrap()
    }

    fn power_mod(base: u64, exponent: u64, modulus: u64) -> u64 {
        let mut result = 1u128;
        let mut square = u128::from(base) % u128::from(modulus);
        let mut remaining = exponent;
        while remaining > 0 {
            if remaining & 1 == 1 {
                result = result * square % u128::from(modulus);
            }
            square = square * square % u128::from(modulus);
            remaining >>= 1;
        }
        result as u64
    }

    #[test]
    fn even_and_tiny_moduli_are_refused() {
        assert!(MontgomeryModulus::new(&[]).is_none());
        assert!(MontgomeryModulus::new(&[0, 0]).is_none());
        assert!(MontgomeryModulus::new(&[1]).is_none());
        assert!(MontgomeryModulus::new(&[0x10, 0x00]).is_none());
        assert!(MontgomeryModulus::new(&[3]).is_some());
    }

    #[test]
    fn both_exponentiations_match_plain_arithmetic() {
        let modulus_value = 0xffff_ffff_ffff_ffc5u64;
        let modulus = modulus_of(modulus_value);
        for (base, exponent) in [(2u64, 65_537u64), (0x1234_5678_9abc, 0xdead_beef_cafe), (7, 0)] {
            let base_limbs = modulus.read_below(&base.to_be_bytes()).unwrap();
            let base_form = modulus.to_montgomery(&base_limbs);
            let expected = power_mod(base, exponent, modulus_value);
            for result in [
                modulus.power_secret(&base_form, &[exponent]),
                modulus.power_public(&base_form, &[exponent]),
            ] {
                assert_eq!(modulus.from_montgomery(&result), vec![expected]);
            }
        }
    }

    #[test]
    fn a_multi_limb_square_root_of_one_squares_back_to_one() {
        let mut modulus_bytes = [0xffu8; 32];
        modulus_bytes[31] = 0xed;
        let modulus = MontgomeryModulus::new(&modulus_bytes).unwrap();
        let mut minus_one = modulus_bytes;
        minus_one[31] -= 1;
        let value = modulus.to_montgomery(&modulus.read_below(&minus_one).unwrap());
        let squared = modulus.from_montgomery(&modulus.multiply(&value, &value));
        assert_eq!(squared, vec![1, 0, 0, 0]);
    }

    #[test]
    fn values_at_or_above_the_modulus_are_refused() {
        let modulus = modulus_of(1_000_003);
        assert!(modulus.read_below(&1_000_003u64.to_be_bytes()).is_none());
        assert!(modulus.read_below(&[1; 9]).is_none());
        assert!(modulus.read_below(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 5]).is_some());
    }

    #[test]
    fn bytes_round_trip_through_limbs() {
        let bytes = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let limbs = limbs_from_be_bytes(&bytes, 2).unwrap();
        assert_eq!(limbs, vec![0x0203_0405_0607_0809, 0x01]);
        assert_eq!(limbs_to_be_bytes(&limbs, 9), bytes.to_vec());
        assert_eq!(limbs_to_be_bytes(&limbs, 10)[0], 0);
        assert!(limbs_from_be_bytes(&bytes, 1).is_none());
    }
}
