use crate::aes::{AES_BLOCK_BYTES, Aes, AesKeyLengthError};

const ROUNDS: u8 = 10;
const MAX_MODULUS: u128 = 1 << 120;
const MIN_DOMAIN: u128 = 1_000_000;

/// Why FF1 refused a key, an alphabet size or an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ff1Error {
    /// The AES key is not 16, 24 or 32 bytes long.
    KeyLength(AesKeyLengthError),
    /// The radix is not between 2 and 65536.
    Radix,
    /// The input is shorter than 2 numerals, gives fewer than a million possible values, or is
    /// longer than this implementation's 120-bit halves allow.
    Length,
    /// A numeral is not below the radix.
    Numeral,
    /// The tweak is longer than 2^32 - 1 bytes.
    Tweak,
}

/// FF1 format-preserving encryption (NIST SP 800-38G Rev. 1) over AES: a string of numerals in
/// a given radix encrypts to another string of the same length and radix.
pub struct Ff1 {
    aes: Aes,
    radix: u32,
}

impl Ff1 {
    /// An FF1 cipher for an AES key and a radix from 2 to 65536.
    pub fn new(key: &[u8], radix: u32) -> Result<Self, Ff1Error> {
        if !(2..=65536).contains(&radix) {
            return Err(Ff1Error::Radix);
        }
        Ok(Self { aes: Aes::new(key).map_err(Ff1Error::KeyLength)?, radix })
    }

    /// Encrypts `numerals` under `tweak`.
    pub fn encrypt(&self, numerals: &[u32], tweak: &[u8]) -> Result<Vec<u32>, Ff1Error> {
        self.run(numerals, tweak, true)
    }

    /// Decrypts `numerals` under `tweak`.
    pub fn decrypt(&self, numerals: &[u32], tweak: &[u8]) -> Result<Vec<u32>, Ff1Error> {
        self.run(numerals, tweak, false)
    }

    fn modulus(&self, length: usize) -> Result<u128, Ff1Error> {
        let exponent = u32::try_from(length).map_err(|_| Ff1Error::Length)?;
        u128::from(self.radix).checked_pow(exponent).filter(|modulus| *modulus <= MAX_MODULUS).ok_or(Ff1Error::Length)
    }

    fn number(&self, numerals: &[u32]) -> u128 {
        numerals.iter().fold(0, |number, numeral| number * u128::from(self.radix) + u128::from(*numeral))
    }

    fn numerals(&self, mut number: u128, length: usize) -> Vec<u32> {
        let mut out = vec![0; length];
        for numeral in out.iter_mut().rev() {
            *numeral = u32::try_from(number % u128::from(self.radix)).unwrap_or_default();
            number /= u128::from(self.radix);
        }
        out
    }

    fn cbc_mac(&self, bytes: &[u8]) -> [u8; AES_BLOCK_BYTES] {
        let mut state = [0u8; AES_BLOCK_BYTES];
        for block in bytes.chunks(AES_BLOCK_BYTES) {
            for (byte, input) in state.iter_mut().zip(block) {
                *byte ^= input;
            }
            self.aes.encrypt_block(&mut state);
        }
        state
    }

    fn round_value(&self, prefix: &[u8], round: u8, half: u128, b: usize, d: usize, modulus: u128) -> u128 {
        let mut block = prefix.to_vec();
        block.push(round);
        block.extend_from_slice(&half.to_be_bytes()[AES_BLOCK_BYTES - b..]);
        let r = self.cbc_mac(&block);
        let mut s = r.to_vec();
        let mut counter: u128 = 1;
        while s.len() < d {
            let mut extra = r;
            for (byte, mask) in extra.iter_mut().zip(counter.to_be_bytes()) {
                *byte ^= mask;
            }
            self.aes.encrypt_block(&mut extra);
            s.extend_from_slice(&extra);
            counter += 1;
        }
        s[..d].iter().fold(0u128, |y, byte| (y * 256 + u128::from(*byte)) % modulus)
    }

    fn run(&self, numerals: &[u32], tweak: &[u8], encrypt: bool) -> Result<Vec<u32>, Ff1Error> {
        let n = numerals.len();
        if numerals.iter().any(|numeral| *numeral >= self.radix) {
            return Err(Ff1Error::Numeral);
        }
        let u = n / 2;
        let v = n - u;
        let modulus_u = self.modulus(u)?;
        let modulus_v = self.modulus(v)?;
        if n < 2 || modulus_u.saturating_mul(modulus_v) < MIN_DOMAIN {
            return Err(Ff1Error::Length);
        }
        let t = u32::try_from(tweak.len()).map_err(|_| Ff1Error::Tweak)?;
        let bits = 128 - (modulus_v - 1).leading_zeros() as usize;
        let b = bits.div_ceil(8);
        let d = 4 * b.div_ceil(4) + 4;
        let length = u32::try_from(n).map_err(|_| Ff1Error::Length)?;
        let mut prefix = vec![1, 2, 1];
        prefix.extend_from_slice(&self.radix.to_be_bytes()[1..]);
        prefix.extend_from_slice(&[10, (u % 256) as u8]);
        prefix.extend_from_slice(&length.to_be_bytes());
        prefix.extend_from_slice(&t.to_be_bytes());
        prefix.extend_from_slice(tweak);
        prefix.resize(prefix.len() + (16 - (tweak.len() + b + 1) % 16) % 16, 0);
        let mut a = self.number(&numerals[..u]);
        let mut b_half = self.number(&numerals[u..]);
        for step in 0..ROUNDS {
            let round = if encrypt { step } else { ROUNDS - 1 - step };
            let modulus = if round % 2 == 0 { modulus_u } else { modulus_v };
            if encrypt {
                let y = self.round_value(&prefix, round, b_half, b, d, modulus);
                let c = ((a % modulus) + y) % modulus;
                a = b_half;
                b_half = c;
            } else {
                let y = self.round_value(&prefix, round, a, b, d, modulus);
                let c = ((b_half % modulus) + modulus - y) % modulus;
                b_half = a;
                a = c;
            }
        }
        let mut out = self.numerals(a, u);
        out.extend(self.numerals(b_half, v));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::{Ff1, Ff1Error};

    const KEY_128: &str = "2b7e151628aed2a6abf7158809cf4f3c";
    const KEY_192: &str = "2b7e151628aed2a6abf7158809cf4f3cef4359d8d580aa4f";
    const KEY_256: &str = "2b7e151628aed2a6abf7158809cf4f3cef4359d8d580aa4f7f036d6f04fc6a94";
    const TWEAK_10: &str = "39383736353433323130";
    const TWEAK_36: &str = "3737373770717273373737";

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap()).collect()
    }

    fn numerals(text: &str) -> Vec<u32> {
        text.chars().map(|character| character.to_digit(36).unwrap()).collect()
    }

    fn text(numerals: &[u32]) -> String {
        numerals.iter().map(|numeral| char::from_digit(*numeral, 36).unwrap()).collect()
    }

    #[test]
    fn nist_sp_800_38g_samples_encrypt_and_decrypt() {
        let samples = [
            (KEY_128, 10, "", "0123456789", "2433477484"),
            (KEY_128, 10, TWEAK_10, "0123456789", "6124200773"),
            (KEY_128, 36, TWEAK_36, "0123456789abcdefghi", "a9tv40mll9kdu509eum"),
            (KEY_192, 10, "", "0123456789", "2830668132"),
            (KEY_192, 10, TWEAK_10, "0123456789", "2496655549"),
            (KEY_192, 36, TWEAK_36, "0123456789abcdefghi", "xbj3kv35jrawxv32ysr"),
            (KEY_256, 10, "", "0123456789", "6657667009"),
            (KEY_256, 10, TWEAK_10, "0123456789", "1001623463"),
            (KEY_256, 36, TWEAK_36, "0123456789abcdefghi", "xs8a0azh2avyalyzuwd"),
        ];
        for (key, radix, tweak, plain, cipher) in samples {
            let ff1 = Ff1::new(&bytes(key), radix).unwrap();
            let encrypted = ff1.encrypt(&numerals(plain), &bytes(tweak)).unwrap();
            assert_eq!(text(&encrypted), cipher, "{key} {radix} {tweak}");
            assert_eq!(text(&ff1.decrypt(&encrypted, &bytes(tweak)).unwrap()), plain);
        }
    }

    #[test]
    fn domains_that_are_too_small_or_too_large_are_refused() {
        let ff1 = Ff1::new(&bytes(KEY_128), 10).unwrap();
        assert_eq!(ff1.encrypt(&numerals("12345"), &[]), Err(Ff1Error::Length));
        assert_eq!(ff1.encrypt(&[1; 73], &[]), Err(Ff1Error::Length));
        assert!(ff1.encrypt(&[1; 72], &[]).is_ok());
        assert_eq!(ff1.encrypt(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], &[]), Err(Ff1Error::Numeral));
        assert!(matches!(Ff1::new(&[0; 15], 10), Err(Ff1Error::KeyLength(_))));
        assert!(matches!(Ff1::new(&bytes(KEY_128), 1), Err(Ff1Error::Radix)));
    }
}
