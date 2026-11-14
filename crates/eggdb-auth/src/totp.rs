//! Time-based one-time passwords (RFC 6238, over RFC 4226's HOTP with HMAC-SHA1), the second
//! factor authenticator apps show, and base32 (RFC 4648) for their `otpauth://` enrolment URIs
//! (D1187).

use eggdb_crypto::{Hmac, Sha1};

/// Seconds each code lasts.
pub const STEP_SECONDS: i64 = 30;
/// Digits in a code.
pub const DIGITS: u32 = 6;
const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// The HOTP value of `secret` at `counter`, cut to `digits` digits.
pub fn hotp(secret: &[u8], counter: u64, digits: u32) -> u32 {
    let mut mac = Hmac::<Sha1>::new(secret);
    mac.update(&counter.to_be_bytes());
    let digest = mac.finish();
    let offset = usize::from(digest[digest.len() - 1] & 0x0f);
    let value = u32::from_be_bytes([digest[offset] & 0x7f, digest[offset + 1], digest[offset + 2], digest[offset + 3]]);
    value % 10u32.pow(digits.min(9))
}

/// The time step of Unix time `now`.
pub fn step_at(now: i64) -> u64 {
    u64::try_from(now.div_euclid(STEP_SECONDS)).unwrap_or(0)
}

/// The step whose code `code` is, among the step of `now` and the one before and after it (for
/// clocks a little off), and later than `last_used`, so a code is accepted once; `None` when it
/// matches none.
pub fn matching_step(secret: &[u8], code: &str, now: i64, last_used: Option<u64>) -> Option<u64> {
    if code.len() != DIGITS as usize || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let given: u32 = code.parse().ok()?;
    let current = step_at(now);
    let mut found = None;
    for step in [current.saturating_sub(1), current, current.saturating_add(1)] {
        let same = eggdb_crypto::constant_time_eq(
            format!("{:0width$}", hotp(secret, step, DIGITS), width = DIGITS as usize).as_bytes(),
            format!("{given:0width$}", width = DIGITS as usize).as_bytes(),
        );
        if same && last_used.is_none_or(|last| step > last) {
            found = Some(step);
        }
    }
    found
}

/// `bytes` in RFC 4648 base32, without padding, as authenticator apps take secrets.
pub fn base32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((buffer >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(ALPHABET[((buffer << (5 - bits)) & 31) as usize]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_rfc_6238_and_rfc_4226() {
        let secret = b"12345678901234567890";
        assert_eq!(hotp(secret, 0, 6), 755_224);
        assert_eq!(hotp(secret, 9, 6), 520_489);
        assert_eq!(hotp(secret, step_at(59), 8), 94_287_082);
        assert_eq!(hotp(secret, step_at(1_111_111_109), 8), 7_081_804);
        assert_eq!(hotp(secret, step_at(2_000_000_000), 8), 69_279_037);
    }

    #[test]
    fn a_code_is_accepted_in_its_window_and_only_once() {
        let secret = b"12345678901234567890";
        let now = 1_111_111_109;
        let code = format!("{:06}", hotp(secret, step_at(now), 6));
        let step = matching_step(secret, &code, now, None).unwrap();
        assert_eq!(matching_step(secret, &code, now + 30, None), Some(step));
        assert_eq!(matching_step(secret, &code, now, Some(step)), None);
        assert_eq!(matching_step(secret, &code, now + 120, None), None);
        assert_eq!(matching_step(secret, "12345", now, None), None);
        assert_eq!(matching_step(secret, "12a456", now, None), None);
    }

    #[test]
    fn base32_follows_rfc_4648() {
        assert_eq!(base32(b""), "");
        assert_eq!(base32(b"f"), "MY");
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32(b"12345678901234567890"), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
    }
}
