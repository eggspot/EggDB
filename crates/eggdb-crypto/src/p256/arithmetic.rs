type Limbs = [u64; 4];

const ONE: Limbs = [1, 0, 0, 0];
const FIELD_SQUARE_ROOT_EXPONENT: Limbs = [
    0,
    0x0000_0000_4000_0000,
    0x4000_0000_0000_0000,
    0x3fff_ffff_c000_0000,
];

#[derive(Clone, Copy)]
struct Modulus {
    value: Limbs,
    negated_inverse: u64,
    r_squared: Limbs,
}

const FIELD: Modulus = Modulus {
    value: [
        0xffff_ffff_ffff_ffff,
        0x0000_0000_ffff_ffff,
        0,
        0xffff_ffff_0000_0001,
    ],
    negated_inverse: 1,
    r_squared: [
        3,
        0xffff_fffb_ffff_ffff,
        0xffff_ffff_ffff_fffe,
        0x0000_0004_ffff_fffd,
    ],
};

const ORDER: Modulus = Modulus {
    value: [
        0xf3b9_cac2_fc63_2551,
        0xbce6_faad_a717_9e84,
        0xffff_ffff_ffff_ffff,
        0xffff_ffff_0000_0000,
    ],
    negated_inverse: 0xccd1_c8aa_ee00_bc4f,
    r_squared: [
        0x8324_4c95_be79_eea2,
        0x4699_799c_49bd_6fa6,
        0x2845_b239_2b6b_ec59,
        0x66e1_2d94_f3d9_5620,
    ],
};

fn add_with_carry(a: u64, b: u64, carry: u64) -> (u64, u64) {
    let wide = u128::from(a) + u128::from(b) + u128::from(carry);
    (wide as u64, (wide >> 64) as u64)
}

fn subtract_with_borrow(a: u64, b: u64, borrow: u64) -> (u64, u64) {
    let wide = u128::from(a).wrapping_sub(u128::from(b) + u128::from(borrow));
    (wide as u64, (wide >> 127) as u64)
}

fn multiply_add(accumulator: u64, a: u64, b: u64, carry: u64) -> (u64, u64) {
    let wide = u128::from(accumulator) + u128::from(a) * u128::from(b) + u128::from(carry);
    (wide as u64, (wide >> 64) as u64)
}

fn add_limbs(a: &Limbs, b: &Limbs) -> (Limbs, u64) {
    let mut sum = [0u64; 4];
    let mut carry = 0;
    for ((sum, &a), &b) in sum.iter_mut().zip(a).zip(b) {
        (*sum, carry) = add_with_carry(a, b, carry);
    }
    (sum, carry)
}

fn subtract_limbs(a: &Limbs, b: &Limbs) -> (Limbs, u64) {
    let mut difference = [0u64; 4];
    let mut borrow = 0;
    for ((difference, &a), &b) in difference.iter_mut().zip(a).zip(b) {
        (*difference, borrow) = subtract_with_borrow(a, b, borrow);
    }
    (difference, borrow)
}

fn mask_of(bit: u64) -> u64 {
    0u64.wrapping_sub(bit & 1)
}

fn select_limbs(bit: u64, if_one: &Limbs, if_zero: &Limbs) -> Limbs {
    let mask = mask_of(bit);
    let mut selected = [0u64; 4];
    for ((selected, &one), &zero) in selected.iter_mut().zip(if_one).zip(if_zero) {
        *selected = (one & mask) | (zero & !mask);
    }
    selected
}

fn is_zero_limbs(a: &Limbs) -> u64 {
    let any = a.iter().fold(0u64, |any, limb| any | limb);
    ((any | any.wrapping_neg()) >> 63) ^ 1
}

fn limbs_from_be_bytes(bytes: &[u8; 32]) -> Limbs {
    let mut limbs = [0u64; 4];
    for (limb, chunk) in limbs.iter_mut().zip(bytes.as_chunks::<8>().0.iter().rev()) {
        *limb = u64::from_be_bytes(*chunk);
    }
    limbs
}

fn limbs_to_be_bytes(limbs: &Limbs) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for (chunk, limb) in bytes.as_chunks_mut::<8>().0.iter_mut().rev().zip(limbs) {
        *chunk = limb.to_be_bytes();
    }
    bytes
}

impl Modulus {
    fn reduce_once(&self, value: &Limbs, top: u64) -> Limbs {
        let (difference, borrow) = subtract_limbs(value, &self.value);
        let (_, below_modulus) = subtract_with_borrow(top, 0, borrow);
        select_limbs(below_modulus, value, &difference)
    }

    fn is_below(&self, value: &Limbs) -> bool {
        subtract_limbs(value, &self.value).1 == 1
    }

    fn add(&self, a: &Limbs, b: &Limbs) -> Limbs {
        let (sum, carry) = add_limbs(a, b);
        self.reduce_once(&sum, carry)
    }

    fn subtract(&self, a: &Limbs, b: &Limbs) -> Limbs {
        let (difference, borrow) = subtract_limbs(a, b);
        let correction = select_limbs(borrow, &self.value, &[0; 4]);
        add_limbs(&difference, &correction).0
    }

    fn multiply(&self, a: &Limbs, b: &Limbs) -> Limbs {
        let mut t = [0u64; 4];
        let mut top = 0u64;
        for &b_limb in b {
            let mut carry = 0;
            for (t_limb, &a_limb) in t.iter_mut().zip(a) {
                (*t_limb, carry) = multiply_add(*t_limb, a_limb, b_limb, carry);
            }
            let (high, overflow) = add_with_carry(top, carry, 0);
            let m = t[0].wrapping_mul(self.negated_inverse);
            let (_, mut carry) = multiply_add(t[0], m, self.value[0], 0);
            let mut shifted = [0u64; 4];
            for ((shifted, &t_limb), &modulus_limb) in
                shifted.iter_mut().zip(&t[1..]).zip(&self.value[1..])
            {
                (*shifted, carry) = multiply_add(t_limb, m, modulus_limb, carry);
            }
            let (last, overflow_carry) = add_with_carry(high, carry, 0);
            shifted[3] = last;
            t = shifted;
            top = overflow + overflow_carry;
        }
        self.reduce_once(&t, top)
    }

    fn to_montgomery(&self, a: &Limbs) -> Limbs {
        self.multiply(a, &self.r_squared)
    }

    fn from_montgomery(&self, a: &Limbs) -> Limbs {
        self.multiply(a, &ONE)
    }

    fn power(&self, base: &Limbs, public_exponent: &Limbs) -> Limbs {
        let mut result = self.to_montgomery(&ONE);
        for limb in public_exponent.iter().rev() {
            for bit in (0..64).rev() {
                result = self.multiply(&result, &result);
                if (limb >> bit) & 1 == 1 {
                    result = self.multiply(&result, base);
                }
            }
        }
        result
    }

    fn invert(&self, a: &Limbs) -> Limbs {
        let exponent = subtract_limbs(&self.value, &[2, 0, 0, 0]).0;
        self.power(a, &exponent)
    }
}

#[derive(Clone, Copy)]
pub(super) struct FieldElement(Limbs);

impl FieldElement {
    pub(super) const ZERO: Self = Self([0; 4]);

    pub(super) fn one() -> Self {
        Self(FIELD.to_montgomery(&ONE))
    }

    pub(super) fn from_be_bytes(bytes: &[u8; 32]) -> Option<Self> {
        let limbs = limbs_from_be_bytes(bytes);
        FIELD
            .is_below(&limbs)
            .then(|| Self(FIELD.to_montgomery(&limbs)))
    }

    pub(super) fn to_be_bytes(self) -> [u8; 32] {
        limbs_to_be_bytes(&FIELD.from_montgomery(&self.0))
    }

    pub(super) fn add(&self, other: &Self) -> Self {
        Self(FIELD.add(&self.0, &other.0))
    }

    pub(super) fn double(&self) -> Self {
        self.add(self)
    }

    pub(super) fn subtract(&self, other: &Self) -> Self {
        Self(FIELD.subtract(&self.0, &other.0))
    }

    pub(super) fn negate(&self) -> Self {
        Self::ZERO.subtract(self)
    }

    pub(super) fn multiply(&self, other: &Self) -> Self {
        Self(FIELD.multiply(&self.0, &other.0))
    }

    pub(super) fn square(&self) -> Self {
        self.multiply(self)
    }

    pub(super) fn invert(&self) -> Self {
        Self(FIELD.invert(&self.0))
    }

    pub(super) fn square_root(&self) -> Option<Self> {
        let root = Self(FIELD.power(&self.0, &FIELD_SQUARE_ROOT_EXPONENT));
        root.square().equals(self).then_some(root)
    }

    pub(super) fn is_zero(&self) -> bool {
        is_zero_limbs(&self.0) == 1
    }

    pub(super) fn is_odd(&self) -> bool {
        FIELD.from_montgomery(&self.0)[0] & 1 == 1
    }

    pub(super) fn equals(&self, other: &Self) -> bool {
        is_zero_limbs(&subtract_limbs(&self.0, &other.0).0) == 1
    }

    pub(super) fn select(bit: u64, if_one: &Self, if_zero: &Self) -> Self {
        Self(select_limbs(bit, &if_one.0, &if_zero.0))
    }
}

#[derive(Clone, Copy)]
pub(super) struct Scalar(Limbs);

impl Scalar {
    pub(super) fn from_be_bytes(bytes: &[u8; 32]) -> Option<Self> {
        let limbs = limbs_from_be_bytes(bytes);
        ORDER
            .is_below(&limbs)
            .then(|| Self(ORDER.to_montgomery(&limbs)))
    }

    pub(super) fn from_be_bytes_reduced(bytes: &[u8; 32]) -> Self {
        let reduced = ORDER.reduce_once(&limbs_from_be_bytes(bytes), 0);
        Self(ORDER.to_montgomery(&reduced))
    }

    pub(super) fn from_digest(digest: &[u8]) -> Self {
        let mut bytes = [0u8; 32];
        if digest.len() >= 32 {
            bytes.copy_from_slice(&digest[..32]);
        } else {
            bytes[32 - digest.len()..].copy_from_slice(digest);
        }
        Self::from_be_bytes_reduced(&bytes)
    }

    pub(super) fn to_be_bytes(self) -> [u8; 32] {
        limbs_to_be_bytes(&ORDER.from_montgomery(&self.0))
    }

    pub(super) fn add(&self, other: &Self) -> Self {
        Self(ORDER.add(&self.0, &other.0))
    }

    pub(super) fn multiply(&self, other: &Self) -> Self {
        Self(ORDER.multiply(&self.0, &other.0))
    }

    pub(super) fn invert(&self) -> Self {
        Self(ORDER.invert(&self.0))
    }

    pub(super) fn is_zero(&self) -> bool {
        is_zero_limbs(&self.0) == 1
    }

    pub(super) fn equals(&self, other: &Self) -> bool {
        is_zero_limbs(&subtract_limbs(&self.0, &other.0).0) == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r_squared_by_doubling(modulus: &Modulus) -> Limbs {
        let mut value = subtract_limbs(&[0; 4], &modulus.value).0;
        for _ in 0..256 {
            value = modulus.add(&value, &value);
        }
        value
    }

    #[test]
    fn the_montgomery_constants_match_their_moduli() {
        for modulus in [FIELD, ORDER] {
            assert_eq!(
                modulus.value[0].wrapping_mul(modulus.negated_inverse),
                u64::MAX
            );
            assert_eq!(r_squared_by_doubling(&modulus), modulus.r_squared);
        }
    }

    #[test]
    fn an_element_times_its_inverse_is_one() {
        let mut bytes = [0u8; 32];
        bytes[31] = 7;
        bytes[0] = 0x5a;
        let element = FieldElement::from_be_bytes(&bytes).unwrap();
        assert!(element.multiply(&element.invert()).equals(&FieldElement::one()));
        let scalar = Scalar::from_be_bytes(&bytes).unwrap();
        let one = Scalar::from_be_bytes(&limbs_to_be_bytes(&ONE)).unwrap();
        assert!(scalar.multiply(&scalar.invert()).equals(&one));
    }

    #[test]
    fn values_at_or_above_the_modulus_are_refused() {
        assert!(FieldElement::from_be_bytes(&limbs_to_be_bytes(&FIELD.value)).is_none());
        assert!(Scalar::from_be_bytes(&limbs_to_be_bytes(&ORDER.value)).is_none());
        assert!(Scalar::from_be_bytes(&[0xff; 32]).is_none());
        let wrapped = Scalar::from_be_bytes_reduced(&limbs_to_be_bytes(&ORDER.value));
        assert!(wrapped.is_zero());
    }

    #[test]
    fn a_square_root_squares_back() {
        let mut bytes = [0u8; 32];
        bytes[31] = 2;
        let two = FieldElement::from_be_bytes(&bytes).unwrap();
        let four = two.square();
        let root = four.square_root().unwrap();
        assert!(root.equals(&two) || root.equals(&two.negate()));
    }
}
