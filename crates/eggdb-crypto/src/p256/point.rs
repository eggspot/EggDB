use super::arithmetic::FieldElement;

const CURVE_B: [u8; 32] = [
    0x5a, 0xc6, 0x35, 0xd8, 0xaa, 0x3a, 0x93, 0xe7, 0xb3, 0xeb, 0xbd, 0x55, 0x76, 0x98, 0x86, 0xbc,
    0x65, 0x1d, 0x06, 0xb0, 0xcc, 0x53, 0xb0, 0xf6, 0x3b, 0xce, 0x3c, 0x3e, 0x27, 0xd2, 0x60, 0x4b,
];
pub(super) const GENERATOR_X: [u8; 32] = [
    0x6b, 0x17, 0xd1, 0xf2, 0xe1, 0x2c, 0x42, 0x47, 0xf8, 0xbc, 0xe6, 0xe5, 0x63, 0xa4, 0x40, 0xf2,
    0x77, 0x03, 0x7d, 0x81, 0x2d, 0xeb, 0x33, 0xa0, 0xf4, 0xa1, 0x39, 0x45, 0xd8, 0x98, 0xc2, 0x96,
];
pub(super) const GENERATOR_Y: [u8; 32] = [
    0x4f, 0xe3, 0x42, 0xe2, 0xfe, 0x1a, 0x7f, 0x9b, 0x8e, 0xe7, 0xeb, 0x4a, 0x7c, 0x0f, 0x9e, 0x16,
    0x2b, 0xce, 0x33, 0x57, 0x6b, 0x31, 0x5e, 0xce, 0xcb, 0xb6, 0x40, 0x68, 0x37, 0xbf, 0x51, 0xf5,
];

const WINDOW_BITS: u32 = 4;
const WINDOW_ENTRIES: usize = 1 << WINDOW_BITS;

fn curve_b() -> FieldElement {
    FieldElement::from_be_bytes(&CURVE_B).unwrap_or(FieldElement::ZERO)
}

pub(super) fn curve_right_side(x: &FieldElement) -> FieldElement {
    let three_x = x.double().add(x);
    x.square().multiply(x).subtract(&three_x).add(&curve_b())
}

pub(super) fn is_on_curve(x: &FieldElement, y: &FieldElement) -> bool {
    y.square().equals(&curve_right_side(x))
}

#[derive(Clone, Copy)]
pub(super) struct ProjectivePoint {
    x: FieldElement,
    y: FieldElement,
    z: FieldElement,
}

impl ProjectivePoint {
    pub(super) fn identity() -> Self {
        Self {
            x: FieldElement::ZERO,
            y: FieldElement::one(),
            z: FieldElement::ZERO,
        }
    }

    pub(super) fn from_affine(x: FieldElement, y: FieldElement) -> Self {
        Self {
            x,
            y,
            z: FieldElement::one(),
        }
    }

    pub(super) fn generator() -> Self {
        match (
            FieldElement::from_be_bytes(&GENERATOR_X),
            FieldElement::from_be_bytes(&GENERATOR_Y),
        ) {
            (Some(x), Some(y)) => Self::from_affine(x, y),
            _ => Self::identity(),
        }
    }

    pub(super) fn add(&self, other: &Self) -> Self {
        let b = curve_b();
        let xx = self.x.multiply(&other.x);
        let yy = self.y.multiply(&other.y);
        let zz = self.z.multiply(&other.z);
        let xy_pairs = self
            .x
            .add(&self.y)
            .multiply(&other.x.add(&other.y))
            .subtract(&xx.add(&yy));
        let yz_pairs = self
            .y
            .add(&self.z)
            .multiply(&other.y.add(&other.z))
            .subtract(&yy.add(&zz));
        let xz_pairs = self
            .x
            .add(&self.z)
            .multiply(&other.x.add(&other.z))
            .subtract(&xx.add(&zz));
        let bzz_part = xz_pairs.subtract(&b.multiply(&zz));
        let bzz3_part = bzz_part.double().add(&bzz_part);
        let yy_minus_bzz3 = yy.subtract(&bzz3_part);
        let yy_plus_bzz3 = yy.add(&bzz3_part);
        let zz3 = zz.double().add(&zz);
        let bxz_part = b.multiply(&xz_pairs).subtract(&zz3.add(&xx));
        let bxz3_part = bxz_part.double().add(&bxz_part);
        let xx3_minus_zz3 = xx.double().add(&xx).subtract(&zz3);
        Self {
            x: yy_plus_bzz3
                .multiply(&xy_pairs)
                .subtract(&yz_pairs.multiply(&bxz3_part)),
            y: yy_plus_bzz3
                .multiply(&yy_minus_bzz3)
                .add(&xx3_minus_zz3.multiply(&bxz3_part)),
            z: yy_minus_bzz3
                .multiply(&yz_pairs)
                .add(&xy_pairs.multiply(&xx3_minus_zz3)),
        }
    }

    pub(super) fn double(&self) -> Self {
        self.add(self)
    }

    fn select(bit: u64, if_one: &Self, if_zero: &Self) -> Self {
        Self {
            x: FieldElement::select(bit, &if_one.x, &if_zero.x),
            y: FieldElement::select(bit, &if_one.y, &if_zero.y),
            z: FieldElement::select(bit, &if_one.z, &if_zero.z),
        }
    }

    fn lookup(table: &[Self; WINDOW_ENTRIES], index: u8) -> Self {
        let mut selected = Self::identity();
        for (position, entry) in (0u64..).zip(table) {
            let difference = position ^ u64::from(index);
            let equal = ((difference | difference.wrapping_neg()) >> 63) ^ 1;
            selected = Self::select(equal, entry, &selected);
        }
        selected
    }

    pub(super) fn multiply(&self, scalar: &[u8; 32]) -> Self {
        let mut table = [Self::identity(); WINDOW_ENTRIES];
        let mut multiple = Self::identity();
        for entry in table.iter_mut().skip(1) {
            multiple = multiple.add(self);
            *entry = multiple;
        }
        let mut result = Self::identity();
        for &byte in scalar {
            for window in [byte >> WINDOW_BITS, byte & 0x0f] {
                for _ in 0..WINDOW_BITS {
                    result = result.double();
                }
                result = result.add(&Self::lookup(&table, window));
            }
        }
        result
    }

    pub(super) fn to_affine(self) -> Option<(FieldElement, FieldElement)> {
        if self.z.is_zero() {
            return None;
        }
        let z_inverse = self.z.invert();
        Some((self.x.multiply(&z_inverse), self.y.multiply(&z_inverse)))
    }
}
