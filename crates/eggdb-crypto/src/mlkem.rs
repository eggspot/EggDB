use crate::constant_time::constant_time_eq;
use crate::sha3::{Keccak, sha3_256, sha3_512, shake256};

/// The bytes of an ML-KEM-768 encapsulation key.
pub const MLKEM768_ENCAPSULATION_KEY_BYTES: usize = 1184;
/// The bytes of an ML-KEM-768 decapsulation key.
pub const MLKEM768_DECAPSULATION_KEY_BYTES: usize = 2400;
/// The bytes of an ML-KEM-768 ciphertext.
pub const MLKEM768_CIPHERTEXT_BYTES: usize = 1088;
/// The bytes of an ML-KEM shared secret.
pub const MLKEM_SHARED_SECRET_BYTES: usize = 32;
/// The random bytes key generation takes: `d` then `z`.
pub const MLKEM_KEY_GENERATION_SEED_BYTES: usize = 64;

const N: usize = 256;
const Q: u32 = 3329;
const K: usize = 3;
const ETA1: usize = 2;
const ETA2: usize = 2;
const DU: u32 = 10;
const DV: u32 = 4;
const POLYNOMIAL_BYTES: usize = 384;
const INVERSE_OF_128: u32 = 3303;

type Polynomial = [u16; N];

/// An encapsulation key that fails FIPS 203's input checks: the wrong length, or a coefficient
/// that is not below q.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MlKemInvalidKey;

impl std::fmt::Display for MlKemInvalidKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the ML-KEM key or ciphertext is malformed")
    }
}

impl std::error::Error for MlKemInvalidKey {}

const fn bit_reversed_7(value: usize) -> usize {
    let mut reversed = 0;
    let mut bit = 0;
    while bit < 7 {
        reversed |= ((value >> bit) & 1) << (6 - bit);
        bit += 1;
    }
    reversed
}

const fn power_mod(base: u32, exponent: usize) -> u32 {
    let mut result: u64 = 1;
    let mut step = 0;
    while step < exponent {
        result = (result * base as u64) % Q as u64;
        step += 1;
    }
    result as u32
}

const ZETAS: [u16; 128] = {
    let mut zetas = [0u16; 128];
    let mut index = 0;
    while index < 128 {
        zetas[index] = power_mod(17, bit_reversed_7(index)) as u16;
        index += 1;
    }
    zetas
};

const GAMMAS: [u16; 128] = {
    let mut gammas = [0u16; 128];
    let mut index = 0;
    while index < 128 {
        gammas[index] = power_mod(17, 2 * bit_reversed_7(index) + 1) as u16;
        index += 1;
    }
    gammas
};

fn reduce(value: u32) -> u16 {
    (value % Q) as u16
}

fn add(left: u16, right: u16) -> u16 {
    reduce(u32::from(left) + u32::from(right))
}

fn subtract(left: u16, right: u16) -> u16 {
    reduce(u32::from(left) + Q - u32::from(right))
}

fn multiply(left: u16, right: u16) -> u16 {
    reduce(u32::from(left) * u32::from(right))
}

fn ntt(polynomial: &mut Polynomial) {
    let mut zeta_at = 1;
    let mut length = 128;
    while length >= 2 {
        let mut start = 0;
        while start < N {
            let zeta = ZETAS[zeta_at];
            zeta_at += 1;
            for j in start..start + length {
                let product = multiply(zeta, polynomial[j + length]);
                polynomial[j + length] = subtract(polynomial[j], product);
                polynomial[j] = add(polynomial[j], product);
            }
            start += 2 * length;
        }
        length /= 2;
    }
}

fn inverse_ntt(polynomial: &mut Polynomial) {
    let mut zeta_at = 127;
    let mut length = 2;
    while length <= 128 {
        let mut start = 0;
        while start < N {
            let zeta = ZETAS[zeta_at];
            zeta_at -= 1;
            for j in start..start + length {
                let kept = polynomial[j];
                polynomial[j] = add(kept, polynomial[j + length]);
                polynomial[j + length] = multiply(zeta, subtract(polynomial[j + length], kept));
            }
            start += 2 * length;
        }
        length *= 2;
    }
    for coefficient in polynomial.iter_mut() {
        *coefficient = multiply(*coefficient, INVERSE_OF_128 as u16);
    }
}

fn multiply_ntts(left: &Polynomial, right: &Polynomial) -> Polynomial {
    let mut product = [0u16; N];
    for (index, gamma) in GAMMAS.iter().enumerate() {
        let (a0, a1) = (left[2 * index], left[2 * index + 1]);
        let (b0, b1) = (right[2 * index], right[2 * index + 1]);
        product[2 * index] = add(multiply(a0, b0), multiply(multiply(a1, b1), *gamma));
        product[2 * index + 1] = add(multiply(a0, b1), multiply(a1, b0));
    }
    product
}

fn add_polynomials(left: &Polynomial, right: &Polynomial) -> Polynomial {
    std::array::from_fn(|index| add(left[index], right[index]))
}

fn byte_encode(polynomial: &Polynomial, bits: u32, out: &mut Vec<u8>) {
    let mut accumulator: u64 = 0;
    let mut held = 0u32;
    for coefficient in polynomial {
        accumulator |= u64::from(*coefficient) << held;
        held += bits;
        while held >= 8 {
            out.push(accumulator as u8);
            accumulator >>= 8;
            held -= 8;
        }
    }
}

fn byte_decode(bytes: &[u8], bits: u32) -> Polynomial {
    let mut polynomial = [0u16; N];
    let mut accumulator: u64 = 0;
    let mut held = 0u32;
    let mut next = bytes.iter();
    let mask = (1u64 << bits) - 1;
    for coefficient in polynomial.iter_mut() {
        while held < bits {
            accumulator |= u64::from(*next.next().unwrap_or(&0)) << held;
            held += 8;
        }
        *coefficient = (accumulator & mask) as u16;
        accumulator >>= bits;
        held -= bits;
    }
    polynomial
}

fn compress(value: u16, bits: u32) -> u16 {
    let scaled = (u64::from(value) << (bits + 1)) + u64::from(Q);
    ((scaled / (2 * u64::from(Q))) & ((1 << bits) - 1)) as u16
}

fn decompress(value: u16, bits: u32) -> u16 {
    ((u32::from(value) * Q + (1 << (bits - 1))) >> bits) as u16
}

fn sample_ntt(rho: &[u8; 32], j: u8, i: u8) -> Polynomial {
    let mut xof = Keccak::shake128();
    xof.absorb(rho);
    xof.absorb(&[j, i]);
    let mut polynomial = [0u16; N];
    let mut filled = 0;
    let mut block = [0u8; 168];
    while filled < N {
        xof.squeeze(&mut block);
        for triple in block.chunks_exact(3) {
            let first = u16::from(triple[0]) | (u16::from(triple[1] & 0x0f) << 8);
            let second = u16::from(triple[1] >> 4) | (u16::from(triple[2]) << 4);
            for candidate in [first, second] {
                if u32::from(candidate) < Q && filled < N {
                    polynomial[filled] = candidate;
                    filled += 1;
                }
            }
        }
    }
    polynomial
}

fn sample_cbd(bytes: &[u8], eta: usize) -> Polynomial {
    let bit = |at: usize| u16::from((bytes[at / 8] >> (at % 8)) & 1);
    std::array::from_fn(|index| {
        let base = 2 * index * eta;
        let x: u16 = (0..eta).map(|j| bit(base + j)).sum();
        let y: u16 = (0..eta).map(|j| bit(base + eta + j)).sum();
        subtract(x, y)
    })
}

fn prf(seed: &[u8; 32], counter: u8, eta: usize) -> Vec<u8> {
    shake256(&[seed, &[counter]], 64 * eta)
}

fn matrix(rho: &[u8; 32]) -> [[Polynomial; K]; K] {
    std::array::from_fn(|i| std::array::from_fn(|j| sample_ntt(rho, j as u8, i as u8)))
}

fn decode_vector(bytes: &[u8]) -> [Polynomial; K] {
    std::array::from_fn(|i| byte_decode(&bytes[i * POLYNOMIAL_BYTES..(i + 1) * POLYNOMIAL_BYTES], 12))
}

fn pke_key_generation(d: &[u8; 32]) -> (Vec<u8>, Vec<u8>) {
    let expanded = sha3_512(&[d, &[K as u8]]);
    let mut rho = [0u8; 32];
    let mut sigma = [0u8; 32];
    rho.copy_from_slice(&expanded[..32]);
    sigma.copy_from_slice(&expanded[32..]);
    let a = matrix(&rho);
    let mut counter = 0u8;
    let mut secret: [Polynomial; K] = std::array::from_fn(|_| {
        let sampled = sample_cbd(&prf(&sigma, counter, ETA1), ETA1);
        counter += 1;
        sampled
    });
    let mut error: [Polynomial; K] = std::array::from_fn(|_| {
        let sampled = sample_cbd(&prf(&sigma, counter, ETA1), ETA1);
        counter += 1;
        sampled
    });
    for polynomial in secret.iter_mut().chain(error.iter_mut()) {
        ntt(polynomial);
    }
    let mut encapsulation = Vec::with_capacity(MLKEM768_ENCAPSULATION_KEY_BYTES);
    let mut decapsulation = Vec::with_capacity(K * POLYNOMIAL_BYTES);
    for ((row, mut t), secret_row) in a.iter().zip(error).zip(&secret) {
        for (entry, secret_entry) in row.iter().zip(&secret) {
            t = add_polynomials(&t, &multiply_ntts(entry, secret_entry));
        }
        byte_encode(&t, 12, &mut encapsulation);
        byte_encode(secret_row, 12, &mut decapsulation);
    }
    encapsulation.extend_from_slice(&rho);
    (encapsulation, decapsulation)
}

fn pke_encrypt(encapsulation: &[u8], message: &[u8; 32], randomness: &[u8; 32]) -> Vec<u8> {
    let t = decode_vector(&encapsulation[..K * POLYNOMIAL_BYTES]);
    let mut rho = [0u8; 32];
    rho.copy_from_slice(&encapsulation[K * POLYNOMIAL_BYTES..]);
    let a = matrix(&rho);
    let mut counter = 0u8;
    let mut y: [Polynomial; K] = std::array::from_fn(|_| {
        let sampled = sample_cbd(&prf(randomness, counter, ETA1), ETA1);
        counter += 1;
        sampled
    });
    let error_1: [Polynomial; K] = std::array::from_fn(|_| {
        let sampled = sample_cbd(&prf(randomness, counter, ETA2), ETA2);
        counter += 1;
        sampled
    });
    let error_2 = sample_cbd(&prf(randomness, counter, ETA2), ETA2);
    for polynomial in &mut y {
        ntt(polynomial);
    }
    let mut ciphertext = Vec::with_capacity(MLKEM768_CIPHERTEXT_BYTES);
    for (i, error) in error_1.iter().enumerate() {
        let mut u = [0u16; N];
        for (row, y_entry) in a.iter().zip(&y) {
            u = add_polynomials(&u, &multiply_ntts(&row[i], y_entry));
        }
        inverse_ntt(&mut u);
        let u = add_polynomials(&u, error);
        let compressed: Polynomial = std::array::from_fn(|index| compress(u[index], DU));
        byte_encode(&compressed, DU, &mut ciphertext);
    }
    let mut v = [0u16; N];
    for (t_entry, y_entry) in t.iter().zip(&y) {
        v = add_polynomials(&v, &multiply_ntts(t_entry, y_entry));
    }
    inverse_ntt(&mut v);
    let mu: Polynomial = std::array::from_fn(|index| {
        let bit = (message[index / 8] >> (index % 8)) & 1;
        decompress(u16::from(bit), 1)
    });
    let v = add_polynomials(&add_polynomials(&v, &error_2), &mu);
    let compressed: Polynomial = std::array::from_fn(|index| compress(v[index], DV));
    byte_encode(&compressed, DV, &mut ciphertext);
    ciphertext
}

fn pke_decrypt(decapsulation: &[u8], ciphertext: &[u8]) -> [u8; 32] {
    let u_bytes = 32 * DU as usize;
    let secret = decode_vector(decapsulation);
    let mut product = [0u16; N];
    for (i, secret_row) in secret.iter().enumerate() {
        let compressed = byte_decode(&ciphertext[i * u_bytes..(i + 1) * u_bytes], DU);
        let mut u: Polynomial = std::array::from_fn(|index| decompress(compressed[index], DU));
        ntt(&mut u);
        product = add_polynomials(&product, &multiply_ntts(secret_row, &u));
    }
    inverse_ntt(&mut product);
    let compressed_v = byte_decode(&ciphertext[K * u_bytes..], DV);
    let mut message = [0u8; 32];
    for index in 0..N {
        let v = decompress(compressed_v[index], DV);
        let w = subtract(v, product[index]);
        message[index / 8] |= (compress(w, 1) as u8) << (index % 8);
    }
    message
}

/// The keys of ML-KEM-768 (FIPS 203) from a 64-byte seed (`d`, then `z`): the encapsulation
/// key to send and the decapsulation key to keep.
pub fn mlkem768_key_pair(seed: &[u8; MLKEM_KEY_GENERATION_SEED_BYTES]) -> (Vec<u8>, Vec<u8>) {
    let mut d = [0u8; 32];
    d.copy_from_slice(&seed[..32]);
    let (encapsulation, pke_decapsulation) = pke_key_generation(&d);
    let mut decapsulation = pke_decapsulation;
    decapsulation.extend_from_slice(&encapsulation);
    decapsulation.extend_from_slice(&sha3_256(&[&encapsulation]));
    decapsulation.extend_from_slice(&seed[32..]);
    (encapsulation, decapsulation)
}

fn encapsulation_key_valid(encapsulation: &[u8]) -> bool {
    encapsulation.len() == MLKEM768_ENCAPSULATION_KEY_BYTES
        && (0..K).all(|i| {
            let bytes = &encapsulation[i * POLYNOMIAL_BYTES..(i + 1) * POLYNOMIAL_BYTES];
            byte_decode(bytes, 12).iter().all(|coefficient| u32::from(*coefficient) < Q)
        })
}

/// Encapsulates to `encapsulation` with the 32 random bytes `message`: the ciphertext to send and
/// the shared secret.
pub fn mlkem768_encapsulate(
    encapsulation: &[u8],
    message: &[u8; 32],
) -> Result<(Vec<u8>, [u8; MLKEM_SHARED_SECRET_BYTES]), MlKemInvalidKey> {
    if !encapsulation_key_valid(encapsulation) {
        return Err(MlKemInvalidKey);
    }
    let expanded = sha3_512(&[message, &sha3_256(&[encapsulation])]);
    let mut shared = [0u8; 32];
    let mut randomness = [0u8; 32];
    shared.copy_from_slice(&expanded[..32]);
    randomness.copy_from_slice(&expanded[32..]);
    Ok((pke_encrypt(encapsulation, message, &randomness), shared))
}

/// The shared secret `ciphertext` carries for `decapsulation`; a ciphertext that does not
/// re-encrypt to itself gives the implicit-rejection secret, as FIPS 203 requires.
pub fn mlkem768_decapsulate(
    decapsulation: &[u8],
    ciphertext: &[u8],
) -> Result<[u8; MLKEM_SHARED_SECRET_BYTES], MlKemInvalidKey> {
    if decapsulation.len() != MLKEM768_DECAPSULATION_KEY_BYTES
        || ciphertext.len() != MLKEM768_CIPHERTEXT_BYTES
    {
        return Err(MlKemInvalidKey);
    }
    let pke_bytes = K * POLYNOMIAL_BYTES;
    let pke_decapsulation = &decapsulation[..pke_bytes];
    let encapsulation = &decapsulation[pke_bytes..pke_bytes + MLKEM768_ENCAPSULATION_KEY_BYTES];
    let hash = &decapsulation[pke_bytes + MLKEM768_ENCAPSULATION_KEY_BYTES..pke_bytes + MLKEM768_ENCAPSULATION_KEY_BYTES + 32];
    let z = &decapsulation[pke_bytes + MLKEM768_ENCAPSULATION_KEY_BYTES + 32..];
    if sha3_256(&[encapsulation]) != *hash {
        return Err(MlKemInvalidKey);
    }
    let message = pke_decrypt(pke_decapsulation, ciphertext);
    let expanded = sha3_512(&[&message, hash]);
    let mut shared = [0u8; 32];
    let mut randomness = [0u8; 32];
    shared.copy_from_slice(&expanded[..32]);
    randomness.copy_from_slice(&expanded[32..]);
    let rejection = shake256(&[z, ciphertext], 32);
    let reencrypted = pke_encrypt(encapsulation, &message, &randomness);
    if !constant_time_eq(&reencrypted, ciphertext) {
        shared.copy_from_slice(&rejection);
    }
    Ok(shared)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_number_theoretic_transform_round_trips() {
        let original: Polynomial = std::array::from_fn(|index| (index as u16 * 13) % Q as u16);
        let mut transformed = original;
        ntt(&mut transformed);
        inverse_ntt(&mut transformed);
        assert_eq!(transformed, original);
        assert_eq!(ZETAS[0], 1);
        assert_eq!(ZETAS[1], 1729);
        assert_eq!(GAMMAS[0], 17);
    }

    #[test]
    fn compression_and_byte_packing_round_trip() {
        for value in [0u16, 1, 832, 1664, 1665, 2496, 3328] {
            let back = decompress(compress(value, DU), DU);
            let distance = (i32::from(back) - i32::from(value)).rem_euclid(Q as i32);
            assert!(distance.min(Q as i32 - distance) <= 2, "{value} -> {back}");
        }
        let polynomial: Polynomial = std::array::from_fn(|index| (index as u16 * 7) % Q as u16);
        let mut bytes = Vec::new();
        byte_encode(&polynomial, 12, &mut bytes);
        assert_eq!(bytes.len(), POLYNOMIAL_BYTES);
        assert_eq!(byte_decode(&bytes, 12), polynomial);
    }

    #[test]
    fn encapsulation_and_decapsulation_agree_and_tampering_is_rejected() {
        let seed: [u8; 64] = std::array::from_fn(|index| index as u8);
        let (encapsulation, decapsulation) = mlkem768_key_pair(&seed);
        assert_eq!(encapsulation.len(), MLKEM768_ENCAPSULATION_KEY_BYTES);
        assert_eq!(decapsulation.len(), MLKEM768_DECAPSULATION_KEY_BYTES);
        let message = [7u8; 32];
        let (ciphertext, shared) = mlkem768_encapsulate(&encapsulation, &message).unwrap();
        assert_eq!(ciphertext.len(), MLKEM768_CIPHERTEXT_BYTES);
        assert_eq!(mlkem768_decapsulate(&decapsulation, &ciphertext).unwrap(), shared);
        let mut tampered = ciphertext.clone();
        tampered[0] ^= 1;
        assert_ne!(mlkem768_decapsulate(&decapsulation, &tampered).unwrap(), shared);
        let mut bad_key = encapsulation.clone();
        bad_key[0] = 0xff;
        bad_key[1] = 0xff;
        assert_eq!(mlkem768_encapsulate(&bad_key, &message), Err(MlKemInvalidKey));
        assert!(mlkem768_decapsulate(&decapsulation, &ciphertext[1..]).is_err());
    }
}
