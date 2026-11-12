use crate::x25519::{Element, add, from_bytes, invert, multiply, square, subtract, to_bytes};

/// The bytes of an Ed25519 public key.
pub const ED25519_PUBLIC_KEY_BYTES: usize = 32;
/// The bytes of an Ed25519 signature.
pub const ED25519_SIGNATURE_BYTES: usize = 64;
/// The bytes of an Ed25519 private key's seed.
pub const ED25519_SEED_BYTES: usize = 32;

const ORDER: [u64; 4] = [0x5812_631a_5cf5_d3ed, 0x14de_f9de_a2f7_9cd6, 0, 0x1000_0000_0000_0000];
const BASE_POINT: [u8; 32] = [
    0x58, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
];

fn small(value: u64) -> Element {
    [value, 0, 0, 0, 0]
}

fn negate(a: &Element) -> Element {
    subtract(&small(0), a)
}

fn power(base: &Element, exponent: &[u8; 32]) -> Element {
    let mut result = small(1);
    for byte in exponent.iter().rev() {
        for bit in (0..8).rev() {
            result = square(&result);
            if (byte >> bit) & 1 == 1 {
                result = multiply(&result, base);
            }
        }
    }
    result
}

fn exponent_below_power_of_two(top: u8, low: u8) -> [u8; 32] {
    let mut exponent = [0xffu8; 32];
    exponent[0] = low;
    exponent[31] = top;
    exponent
}

struct Curve {
    d: Element,
    twice_d: Element,
    root_of_minus_one: Element,
}

impl Curve {
    fn new() -> Self {
        let d = negate(&multiply(&small(121_665), &invert(&small(121_666))));
        Self {
            twice_d: add(&d, &d),
            d,
            root_of_minus_one: power(&small(2), &exponent_below_power_of_two(0x1f, 0xfb)),
        }
    }
}

#[derive(Clone, Copy)]
struct Point {
    x: Element,
    y: Element,
    z: Element,
    t: Element,
}

impl Point {
    const IDENTITY: Self = Self {
        x: [0; 5],
        y: [1, 0, 0, 0, 0],
        z: [1, 0, 0, 0, 0],
        t: [0; 5],
    };

    fn plus(&self, other: &Self, curve: &Curve) -> Self {
        let a = multiply(&subtract(&self.y, &self.x), &subtract(&other.y, &other.x));
        let b = multiply(&add(&self.y, &self.x), &add(&other.y, &other.x));
        let c = multiply(&multiply(&self.t, &curve.twice_d), &other.t);
        let zz = multiply(&self.z, &other.z);
        let d = add(&zz, &zz);
        let e = subtract(&b, &a);
        let f = subtract(&d, &c);
        let g = add(&d, &c);
        let h = add(&b, &a);
        Self {
            x: multiply(&e, &f),
            y: multiply(&g, &h),
            t: multiply(&e, &h),
            z: multiply(&f, &g),
        }
    }

    fn times(&self, scalar: &[u8; 32], curve: &Curve) -> Self {
        let mut result = Self::IDENTITY;
        for byte in scalar.iter().rev() {
            for bit in (0..8).rev() {
                result = result.plus(&result, curve);
                if (byte >> bit) & 1 == 1 {
                    result = result.plus(self, curve);
                }
            }
        }
        result
    }

    fn negated(&self) -> Self {
        Self {
            x: negate(&self.x),
            t: negate(&self.t),
            ..*self
        }
    }

    fn encode(&self) -> [u8; 32] {
        let inverse = invert(&self.z);
        let mut bytes = to_bytes(&multiply(&self.y, &inverse));
        let x = to_bytes(&multiply(&self.x, &inverse));
        bytes[31] |= (x[0] & 1) << 7;
        bytes
    }

    fn decode(encoded: &[u8; 32], curve: &Curve) -> Option<Self> {
        let sign = encoded[31] >> 7;
        let mut y_bytes = *encoded;
        y_bytes[31] &= 0x7f;
        let y = from_bytes(&y_bytes);
        if to_bytes(&y) != y_bytes {
            return None;
        }
        let yy = square(&y);
        let u = subtract(&yy, &small(1));
        let v = add(&multiply(&curve.d, &yy), &small(1));
        let v3 = multiply(&square(&v), &v);
        let v7 = multiply(&square(&v3), &v);
        let root = power(&multiply(&u, &v7), &exponent_below_power_of_two(0x0f, 0xfd));
        let mut x = multiply(&multiply(&u, &v3), &root);
        let check = to_bytes(&multiply(&v, &square(&x)));
        if check != to_bytes(&u) {
            if check != to_bytes(&negate(&u)) {
                return None;
            }
            x = multiply(&x, &curve.root_of_minus_one);
        }
        let x_bytes = to_bytes(&x);
        if x_bytes == [0; 32] && sign == 1 {
            return None;
        }
        if x_bytes[0] & 1 != sign {
            x = negate(&x);
        }
        Some(Self {
            x,
            y,
            z: small(1),
            t: multiply(&x, &y),
        })
    }
}

fn limbs(bytes: &[u8; 32]) -> [u64; 4] {
    let mut out = [0u64; 4];
    for (limb, chunk) in out.iter_mut().zip(bytes.chunks_exact(8)) {
        let mut word = [0u8; 8];
        word.copy_from_slice(chunk);
        *limb = u64::from_le_bytes(word);
    }
    out
}

fn at_least_order(value: &[u64; 4]) -> bool {
    for at in (0..4).rev() {
        if value[at] != ORDER[at] {
            return value[at] > ORDER[at];
        }
    }
    true
}

fn minus_order(value: &mut [u64; 4]) {
    let mut borrow = 0u64;
    for (limb, order) in value.iter_mut().zip(ORDER) {
        let (step, first) = limb.overflowing_sub(order);
        let (step, second) = step.overflowing_sub(borrow);
        *limb = step;
        borrow = u64::from(first || second);
    }
}

fn reduced(bytes: &[u8]) -> [u8; 32] {
    let mut remainder = [0u64; 4];
    for byte in bytes.iter().rev() {
        for bit in (0..8).rev() {
            let mut carry = u64::from((byte >> bit) & 1);
            for limb in &mut remainder {
                let next = *limb >> 63;
                *limb = (*limb << 1) | carry;
                carry = next;
            }
            if at_least_order(&remainder) {
                minus_order(&mut remainder);
            }
        }
    }
    let mut out = [0u8; 32];
    for (chunk, limb) in out.chunks_exact_mut(8).zip(remainder) {
        chunk.copy_from_slice(&limb.to_le_bytes());
    }
    out
}

fn product_reduced(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let (left, right) = (limbs(left), limbs(right));
    let mut wide = [0u128; 8];
    for (i, a) in left.iter().enumerate() {
        let mut carry = 0u128;
        for (j, b) in right.iter().enumerate() {
            let sum = wide[i + j] + u128::from(*a) * u128::from(*b) + carry;
            wide[i + j] = sum & u128::from(u64::MAX);
            carry = sum >> 64;
        }
        wide[i + 4] += carry;
    }
    let mut bytes = [0u8; 64];
    for (chunk, limb) in bytes.chunks_exact_mut(8).zip(wide) {
        chunk.copy_from_slice(&(limb as u64).to_le_bytes());
    }
    reduced(&bytes)
}

fn sum_reduced(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut bytes = [0u8; 33];
    let mut carry = 0u16;
    for at in 0..32 {
        let sum = u16::from(left[at]) + u16::from(right[at]) + carry;
        bytes[at] = sum as u8;
        carry = sum >> 8;
    }
    bytes[32] = carry as u8;
    reduced(&bytes)
}

struct ExpandedKey {
    scalar: [u8; 32],
    prefix: [u8; 32],
    public: [u8; ED25519_PUBLIC_KEY_BYTES],
}

fn expanded(seed: &[u8; ED25519_SEED_BYTES], curve: &Curve) -> Option<ExpandedKey> {
    let mut hashed = crate::sha512::Sha512::new();
    hashed.update(seed);
    let digest = hashed.finish();
    let mut scalar = [0u8; 32];
    scalar.copy_from_slice(&digest[..32]);
    scalar[0] &= 248;
    scalar[31] &= 127;
    scalar[31] |= 64;
    let mut prefix = [0u8; 32];
    prefix.copy_from_slice(&digest[32..64]);
    let base = Point::decode(&BASE_POINT, curve)?;
    let public = base.times(&scalar, curve).encode();
    Some(ExpandedKey { scalar, prefix, public })
}

/// The Ed25519 public key of the private key `seed` (RFC 8032).
pub fn ed25519_public_key(seed: &[u8; ED25519_SEED_BYTES]) -> [u8; ED25519_PUBLIC_KEY_BYTES] {
    let curve = Curve::new();
    expanded(seed, &curve).map_or([0u8; ED25519_PUBLIC_KEY_BYTES], |key| key.public)
}

/// The Ed25519 signature (RFC 8032, deterministic) of `message` by the private key `seed`.
/// It runs on the same variable-time arithmetic as [`ed25519_verify`], so it suits signing
/// where an attacker cannot time many signatures, such as EggDB's own audit digests, and not
/// signing on request.
pub fn ed25519_sign(seed: &[u8; ED25519_SEED_BYTES], message: &[u8]) -> [u8; ED25519_SIGNATURE_BYTES] {
    let curve = Curve::new();
    let mut signature = [0u8; ED25519_SIGNATURE_BYTES];
    let (Some(key), Some(base)) = (expanded(seed, &curve), Point::decode(&BASE_POINT, &curve)) else {
        return signature;
    };
    let mut nonce = crate::sha512::Sha512::new();
    nonce.update(&key.prefix);
    nonce.update(message);
    let r = reduced(&nonce.finish());
    let encoded_r = base.times(&r, &curve).encode();
    let mut challenge = crate::sha512::Sha512::new();
    challenge.update(&encoded_r);
    challenge.update(&key.public);
    challenge.update(message);
    let k = reduced(&challenge.finish());
    let s = sum_reduced(&r, &product_reduced(&k, &key.scalar));
    signature[..32].copy_from_slice(&encoded_r);
    signature[32..].copy_from_slice(&s);
    signature
}

/// Whether `signature` is a valid Ed25519 signature (RFC 8032, cofactorless) of `message` by
/// `public_key`; a key or signature that does not decode, or a scalar not below the group
/// order, is invalid. Verification only: it handles public data and is not constant-time.
pub fn ed25519_verify(
    public_key: &[u8; ED25519_PUBLIC_KEY_BYTES],
    message: &[u8],
    signature: &[u8; ED25519_SIGNATURE_BYTES],
) -> bool {
    let curve = Curve::new();
    let Some(key) = Point::decode(public_key, &curve) else {
        return false;
    };
    let mut r = [0u8; 32];
    r.copy_from_slice(&signature[..32]);
    let mut s = [0u8; 32];
    s.copy_from_slice(&signature[32..]);
    if at_least_order(&limbs(&s)) {
        return false;
    }
    let Some(base) = Point::decode(&BASE_POINT, &curve) else {
        return false;
    };
    let mut hashed = crate::sha512::Sha512::new();
    hashed.update(&r);
    hashed.update(public_key);
    hashed.update(message);
    let k = reduced(&hashed.finish());
    let check = base.times(&s, &curve).plus(&key.negated().times(&k, &curve), &curve);
    crate::constant_time_eq(&check.encode(), &r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    fn array<const N: usize>(text: &str) -> [u8; N] {
        hex(text).try_into().unwrap()
    }

    #[test]
    fn the_rfc_8032_test_vectors_verify_and_tampering_fails() {
        let key = array::<32>("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        let signature = array::<64>("e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b");
        assert!(ed25519_verify(&key, b"", &signature));
        assert!(!ed25519_verify(&key, b"x", &signature));
        let second_key = array::<32>("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c");
        let second = array::<64>("92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00");
        assert!(ed25519_verify(&second_key, &[0x72], &second));
        let mut bent = second;
        bent[10] ^= 1;
        assert!(!ed25519_verify(&second_key, &[0x72], &bent));
        let mut big_s = second;
        big_s[63] = 0xff;
        assert!(!ed25519_verify(&second_key, &[0x72], &big_s));
    }

    #[test]
    fn the_rfc_8032_test_vectors_sign_and_their_keys_derive() {
        let seed = array::<32>("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
        assert_eq!(
            ed25519_public_key(&seed),
            array::<32>("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
        );
        assert_eq!(
            ed25519_sign(&seed, b""),
            array::<64>("e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b")
        );
        let second_seed = array::<32>("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb");
        let second_key = ed25519_public_key(&second_seed);
        assert_eq!(
            second_key,
            array::<32>("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c")
        );
        let second = ed25519_sign(&second_seed, &[0x72]);
        assert_eq!(
            second,
            array::<64>("92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00")
        );
        let long: Vec<u8> = (0..1000u32).map(|value| value as u8).collect();
        assert!(ed25519_verify(&second_key, &long, &ed25519_sign(&second_seed, &long)));
    }

    #[test]
    fn scalars_reduce_modulo_the_group_order() {
        let mut order = [0u8; 32];
        for (chunk, limb) in order.chunks_exact_mut(8).zip(ORDER) {
            chunk.copy_from_slice(&limb.to_le_bytes());
        }
        assert_eq!(reduced(&order), [0u8; 32]);
        let mut one_more = order;
        one_more[0] += 1;
        let mut one = [0u8; 32];
        one[0] = 1;
        assert_eq!(reduced(&one_more), one);
    }
}
