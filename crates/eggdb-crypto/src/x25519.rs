/// The length of an X25519 private key, public key and shared secret in bytes.
pub const X25519_KEY_BYTES: usize = 32;

const MASK: u64 = (1 << 51) - 1;
const A24: u64 = 121_665;
const BASE_POINT: [u8; X25519_KEY_BYTES] = {
    let mut point = [0u8; X25519_KEY_BYTES];
    point[0] = 9;
    point
};

/// The peer's X25519 public key gave the all-zero shared secret: it is a point of small order,
/// which RFC 7748 section 6.1 and TLS 1.3 say must be refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X25519LowOrderPoint;

impl std::fmt::Display for X25519LowOrderPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the X25519 public key is a point of small order")
    }
}

impl std::error::Error for X25519LowOrderPoint {}

type Element = [u64; 5];

fn load(bytes: &[u8; 32], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

fn from_bytes(bytes: &[u8; 32]) -> Element {
    [
        load(bytes, 0) & MASK,
        (load(bytes, 6) >> 3) & MASK,
        (load(bytes, 12) >> 6) & MASK,
        (load(bytes, 19) >> 1) & MASK,
        (load(bytes, 24) >> 12) & MASK,
    ]
}

fn carry(mut h: Element) -> Element {
    let c = h[0] >> 51;
    h[0] &= MASK;
    h[1] += c;
    let c = h[1] >> 51;
    h[1] &= MASK;
    h[2] += c;
    let c = h[2] >> 51;
    h[2] &= MASK;
    h[3] += c;
    let c = h[3] >> 51;
    h[3] &= MASK;
    h[4] += c;
    let c = h[4] >> 51;
    h[4] &= MASK;
    h[0] += c * 19;
    h
}

fn to_bytes(h: &Element) -> [u8; 32] {
    let mut h = carry(carry(*h));
    let mut q = (h[0] + 19) >> 51;
    q = (h[1] + q) >> 51;
    q = (h[2] + q) >> 51;
    q = (h[3] + q) >> 51;
    q = (h[4] + q) >> 51;
    h[0] += 19 * q;
    let c = h[0] >> 51;
    h[0] &= MASK;
    h[1] += c;
    let c = h[1] >> 51;
    h[1] &= MASK;
    h[2] += c;
    let c = h[2] >> 51;
    h[2] &= MASK;
    h[3] += c;
    let c = h[3] >> 51;
    h[3] &= MASK;
    h[4] += c;
    h[4] &= MASK;
    let words = [
        h[0] | (h[1] << 51),
        (h[1] >> 13) | (h[2] << 38),
        (h[2] >> 26) | (h[3] << 25),
        (h[3] >> 39) | (h[4] << 12),
    ];
    let mut out = [0u8; 32];
    for (bytes, word) in out.as_chunks_mut::<8>().0.iter_mut().zip(words) {
        *bytes = word.to_le_bytes();
    }
    out
}

fn add(a: &Element, b: &Element) -> Element {
    carry([a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3], a[4] + b[4]])
}

fn subtract(a: &Element, b: &Element) -> Element {
    const FOUR_P_LOW: u64 = 0x1f_ffff_ffff_ffb4;
    const FOUR_P_HIGH: u64 = 0x1f_ffff_ffff_fffc;
    carry([
        a[0] + FOUR_P_LOW - b[0],
        a[1] + FOUR_P_HIGH - b[1],
        a[2] + FOUR_P_HIGH - b[2],
        a[3] + FOUR_P_HIGH - b[3],
        a[4] + FOUR_P_HIGH - b[4],
    ])
}

fn multiply(a: &Element, b: &Element) -> Element {
    let [a0, a1, a2, a3, a4] = a.map(u128::from);
    let [b0, b1, b2, b3, b4] = b.map(u128::from);
    let (b1_19, b2_19, b3_19, b4_19) = (b1 * 19, b2 * 19, b3 * 19, b4 * 19);
    let r0 = a0 * b0 + a1 * b4_19 + a2 * b3_19 + a3 * b2_19 + a4 * b1_19;
    let mut r1 = a0 * b1 + a1 * b0 + a2 * b4_19 + a3 * b3_19 + a4 * b2_19;
    let mut r2 = a0 * b2 + a1 * b1 + a2 * b0 + a3 * b4_19 + a4 * b3_19;
    let mut r3 = a0 * b3 + a1 * b2 + a2 * b1 + a3 * b0 + a4 * b4_19;
    let mut r4 = a0 * b4 + a1 * b3 + a2 * b2 + a3 * b1 + a4 * b0;
    let mask = u128::from(MASK);
    r1 += r0 >> 51;
    let h0 = r0 & mask;
    r2 += r1 >> 51;
    let h1 = r1 & mask;
    r3 += r2 >> 51;
    let h2 = r2 & mask;
    r4 += r3 >> 51;
    let h3 = r3 & mask;
    let h0 = h0 + (r4 >> 51) * 19;
    let h4 = r4 & mask;
    let h1 = h1 + (h0 >> 51);
    let h0 = h0 & mask;
    [h0 as u64, h1 as u64, h2 as u64, h3 as u64, h4 as u64]
}

fn square(a: &Element) -> Element {
    multiply(a, a)
}

fn square_times(a: &Element, times: usize) -> Element {
    let mut result = *a;
    for _ in 0..times {
        result = square(&result);
    }
    result
}

fn invert(z: &Element) -> Element {
    let z2 = square(z);
    let z8 = square_times(&z2, 2);
    let z9 = multiply(&z8, z);
    let z11 = multiply(&z9, &z2);
    let z22 = square(&z11);
    let z_5_0 = multiply(&z22, &z9);
    let z_10_0 = multiply(&square_times(&z_5_0, 5), &z_5_0);
    let z_20_0 = multiply(&square_times(&z_10_0, 10), &z_10_0);
    let z_40_0 = multiply(&square_times(&z_20_0, 20), &z_20_0);
    let z_50_0 = multiply(&square_times(&z_40_0, 10), &z_10_0);
    let z_100_0 = multiply(&square_times(&z_50_0, 50), &z_50_0);
    let z_200_0 = multiply(&square_times(&z_100_0, 100), &z_100_0);
    let z_250_0 = multiply(&square_times(&z_200_0, 50), &z_50_0);
    multiply(&square_times(&z_250_0, 5), &z11)
}

fn conditional_swap(swap: u64, a: &mut Element, b: &mut Element) {
    let mask = 0u64.wrapping_sub(swap);
    for (left, right) in a.iter_mut().zip(b.iter_mut()) {
        let difference = mask & (*left ^ *right);
        *left ^= difference;
        *right ^= difference;
    }
}

/// The X25519 function of RFC 7748: the u-coordinate of `scalar` (clamped as the RFC says)
/// times the point with u-coordinate `u` (its top bit ignored), by a Montgomery ladder whose
/// steps and memory accesses do not depend on the scalar.
pub fn x25519(scalar: &[u8; X25519_KEY_BYTES], u: &[u8; X25519_KEY_BYTES]) -> [u8; X25519_KEY_BYTES] {
    let mut clamped = *scalar;
    clamped[0] &= 248;
    clamped[31] &= 127;
    clamped[31] |= 64;
    let x1 = from_bytes(u);
    let mut x2: Element = [1, 0, 0, 0, 0];
    let mut z2: Element = [0; 5];
    let mut x3 = x1;
    let mut z3: Element = [1, 0, 0, 0, 0];
    let a24: Element = [A24, 0, 0, 0, 0];
    let mut swap = 0u64;
    for bit in (0..255).rev() {
        let k = u64::from((clamped[bit / 8] >> (bit % 8)) & 1);
        swap ^= k;
        conditional_swap(swap, &mut x2, &mut x3);
        conditional_swap(swap, &mut z2, &mut z3);
        swap = k;
        let a = add(&x2, &z2);
        let aa = square(&a);
        let b = subtract(&x2, &z2);
        let bb = square(&b);
        let e = subtract(&aa, &bb);
        let c = add(&x3, &z3);
        let d = subtract(&x3, &z3);
        let da = multiply(&d, &a);
        let cb = multiply(&c, &b);
        x3 = square(&add(&da, &cb));
        z3 = multiply(&x1, &square(&subtract(&da, &cb)));
        x2 = multiply(&aa, &bb);
        z2 = multiply(&e, &add(&aa, &multiply(&a24, &e)));
    }
    conditional_swap(swap, &mut x2, &mut x3);
    conditional_swap(swap, &mut z2, &mut z3);
    to_bytes(&multiply(&x2, &invert(&z2)))
}

/// The X25519 public key of `private_key`: the private key times the base point 9.
pub fn x25519_public_key(private_key: &[u8; X25519_KEY_BYTES]) -> [u8; X25519_KEY_BYTES] {
    x25519(private_key, &BASE_POINT)
}

/// The shared secret of `private_key` and the peer's `public_key`, refused when it is all zero
/// (the peer sent a point of small order). The check reads every byte whatever their values.
pub fn x25519_shared_secret(
    private_key: &[u8; X25519_KEY_BYTES],
    public_key: &[u8; X25519_KEY_BYTES],
) -> Result<[u8; X25519_KEY_BYTES], X25519LowOrderPoint> {
    let secret = x25519(private_key, public_key);
    let any_bit = secret.iter().fold(0u8, |bits, byte| bits | byte);
    if std::hint::black_box(any_bit) == 0 {
        return Err(X25519LowOrderPoint);
    }
    Ok(secret)
}
