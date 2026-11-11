const USER_PREFIX: u8 = 20 << 3;
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
const ENCODED_KEY_BYTES: usize = 35;

fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = BASE32.iter().position(|known| *known == byte)? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for byte in bytes {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// The Ed25519 public key a user NKey (`U` followed by 55 base32 characters) encodes, after
/// checking its prefix and CRC-16; `None` for anything else.
pub(crate) fn user_public_key(nkey: &str) -> Option<[u8; 32]> {
    let raw = base32_decode(nkey)?;
    if raw.len() != ENCODED_KEY_BYTES || raw[0] != USER_PREFIX {
        return None;
    }
    let (body, checksum) = raw.split_at(ENCODED_KEY_BYTES - 2);
    if crc16(body).to_le_bytes() != [checksum[0], checksum[1]] {
        return None;
    }
    body[1..].try_into().ok()
}

/// Whether `signature` (base64url, padded or not, as nats clients send it) is the user NKey's
/// Ed25519 signature of the server's `nonce`.
pub(crate) fn signed_nonce(nkey: &str, nonce: &str, signature: &str) -> bool {
    let Some(public_key) = user_public_key(nkey) else {
        return false;
    };
    let mut standard: String = signature
        .trim_end_matches('=')
        .chars()
        .map(|character| match character {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    while standard.len() % 4 != 0 {
        standard.push('=');
    }
    let Ok(decoded) = eggdb_crypto::base64_decode(standard.as_bytes()) else {
        return false;
    };
    let Ok(signature) = <[u8; 64]>::try_from(decoded.as_slice()) else {
        return false;
    };
    eggdb_crypto::ed25519_verify(&public_key, nonce.as_bytes(), &signature)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base32_encode(bytes: &[u8]) -> String {
        let mut out = String::new();
        let mut buffer: u32 = 0;
        let mut bits = 0;
        for byte in bytes {
            buffer = (buffer << 8) | u32::from(*byte);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(char::from(BASE32[((buffer >> bits) & 31) as usize]));
            }
        }
        if bits > 0 {
            out.push(char::from(BASE32[((buffer << (5 - bits)) & 31) as usize]));
        }
        out
    }

    #[test]
    fn user_nkeys_round_trip_and_bad_ones_are_refused() {
        let key = [7u8; 32];
        let mut raw = vec![USER_PREFIX];
        raw.extend_from_slice(&key);
        let checksum = crc16(&raw).to_le_bytes();
        raw.extend_from_slice(&checksum);
        let nkey = base32_encode(&raw);
        assert!(nkey.starts_with('U'));
        assert_eq!(nkey.len(), 56);
        assert_eq!(user_public_key(&nkey), Some(key));
        let mut bent = nkey.clone().into_bytes();
        bent[10] = if bent[10] == b'A' { b'B' } else { b'A' };
        assert_eq!(user_public_key(std::str::from_utf8(&bent).unwrap()), None);
        assert_eq!(user_public_key("not a key"), None);
        assert!(!signed_nonce(&nkey, "nonce", "AAAA"));
        assert_eq!(crc16(b"123456789"), 0x31c3);
    }
}
