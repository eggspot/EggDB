//! The published test vectors of every algorithm in `eggdb-crypto` (D35): FIPS 180-4 and NIST for
//! SHA-256 and SHA-1, RFC 4231 for HMAC-SHA-256, RFC 7914 and the widely published password/salt vectors
//! for PBKDF2-HMAC-SHA-256, RFC 4648 for Base64.

use eggdb_crypto::{
    Base64Error, HmacSha256, Sha1, Sha256, base64_decode, base64_encode, constant_time_eq,
    hmac_sha256, pbkdf2_hmac_sha256, sha1, sha256,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn sha256_matches_the_fips_180_examples() {
    let cases: [(&[u8], &str); 4] = [
        (
            b"",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
        ),
        (
            b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
        ),
    ];
    for (message, digest) in cases {
        assert_eq!(hex(&sha256(message)), digest);
    }
}

#[test]
fn sha256_of_a_million_a_matches_the_nist_long_message() {
    let mut hash = Sha256::new();
    let chunk = [b'a'; 1000];
    for _ in 0..1000 {
        hash.update(&chunk);
    }
    assert_eq!(
        hex(&hash.finish()),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
}

#[test]
fn sha256_gives_the_same_digest_however_the_message_is_split() {
    let message: Vec<u8> = (0..300u32).map(|index| (index * 7 + 3) as u8).collect();
    for length in [0, 1, 55, 56, 63, 64, 65, 119, 120, 128, 300] {
        let whole = sha256(&message[..length]);
        for split in [1, 3, 17, 63, 64, 65] {
            let mut hash = Sha256::new();
            for piece in message[..length].chunks(split) {
                hash.update(piece);
            }
            assert_eq!(hash.finish(), whole, "length {length}, pieces of {split}");
        }
    }
}

#[test]
fn hmac_sha256_matches_rfc_4231() {
    let long_key = vec![0xaa; 131];
    let cases: [(Vec<u8>, Vec<u8>, &str); 6] = [
        (
            vec![0x0b; 20],
            b"Hi There".to_vec(),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
        ),
        (
            b"Jefe".to_vec(),
            b"what do ya want for nothing?".to_vec(),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
        ),
        (
            vec![0xaa; 20],
            vec![0xdd; 50],
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
        ),
        (
            (1..=25).collect(),
            vec![0xcd; 50],
            "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
        ),
        (
            long_key.clone(),
            b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
        ),
        (
            long_key,
            b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.".to_vec(),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2",
        ),
    ];
    for (key, data, mac) in cases {
        assert_eq!(hex(&hmac_sha256(&key, &data)), mac);
        let mut split = HmacSha256::new(&key);
        for piece in data.chunks(7) {
            split.update(piece);
        }
        assert_eq!(hex(&split.finish()), mac);
    }
    let truncated = hmac_sha256(&[0x0c; 20], b"Test With Truncation");
    assert_eq!(hex(&truncated[..16]), "a3b6167473100ee06e0c796c2955552b");
}

#[test]
fn pbkdf2_hmac_sha256_matches_rfc_7914() {
    let mut output = [0u8; 64];
    pbkdf2_hmac_sha256(b"passwd", b"salt", 1, &mut output);
    assert_eq!(
        output.to_vec(),
        unhex(
            "55 ac 04 6e 56 e3 08 9f ec 16 91 c2 25 44 b6 05
             f9 41 85 21 6d de 04 65 e6 8b 9d 57 c2 0d ac bc
             49 ca 9c cc f1 79 b6 45 99 16 64 b3 9d 77 ef 31
             7c 71 b8 45 b1 e3 0b d5 09 11 20 41 d3 a1 97 83"
        )
    );
    pbkdf2_hmac_sha256(b"Password", b"NaCl", 80000, &mut output);
    assert_eq!(
        output.to_vec(),
        unhex(
            "4d dc d8 f6 0b 98 be 21 83 0c ee 5e f2 27 01 f9
             64 1a 44 18 d0 4c 04 14 ae ff 08 87 6b 34 ab 56
             a1 d4 25 a1 22 58 33 54 9a db 84 1b 51 c9 b3 17
             6a 27 2b de bb a1 d0 78 47 8f 62 b3 97 f3 3c 8d"
        )
    );
}

#[test]
fn pbkdf2_hmac_sha256_matches_the_published_password_salt_vectors() {
    let cases = [
        (
            1,
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b",
        ),
        (
            2,
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43",
        ),
        (
            4096,
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a",
        ),
    ];
    for (iterations, expected) in cases {
        let mut output = [0u8; 32];
        pbkdf2_hmac_sha256(b"password", b"salt", iterations, &mut output);
        assert_eq!(hex(&output), expected, "{iterations} iterations");
    }
    let mut short = [0u8; 5];
    pbkdf2_hmac_sha256(b"password", b"salt", 4096, &mut short);
    assert_eq!(hex(&short), "c5e478d592");
}

#[test]
fn base64_matches_rfc_4648_and_round_trips() {
    let cases: [(&[u8], &str); 7] = [
        (b"", ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (b"foob", "Zm9vYg=="),
        (b"fooba", "Zm9vYmE="),
        (b"foobar", "Zm9vYmFy"),
    ];
    for (bytes, text) in cases {
        assert_eq!(base64_encode(bytes), text);
        assert_eq!(base64_decode(text.as_bytes()).as_deref(), Ok(bytes));
    }
    let all: Vec<u8> = (0..=255).collect();
    assert_eq!(base64_decode(base64_encode(&all).as_bytes()), Ok(all));
}

#[test]
fn base64_refuses_anything_but_the_canonical_padded_form() {
    assert_eq!(base64_decode(b"Zg="), Err(Base64Error::Length));
    assert_eq!(base64_decode(b"Zg"), Err(Base64Error::Length));
    assert_eq!(base64_decode(b"Z===").err(), Some(Base64Error::Symbol));
    assert_eq!(base64_decode(b"Zg==Zm9v"), Err(Base64Error::Symbol));
    assert_eq!(base64_decode(b"Zm9 "), Err(Base64Error::Symbol));
    assert_eq!(base64_decode(b"Zm-v"), Err(Base64Error::Symbol));
    assert_eq!(base64_decode(b"Zh=="), Err(Base64Error::TrailingBits));
    assert_eq!(base64_decode(b"Zm9="), Err(Base64Error::TrailingBits));
}

#[test]
fn base64_decoding_never_panics_on_any_short_input() {
    let symbols = b"Aa0+/=-\x00\xff";
    let mut input = Vec::new();
    for length in 0..=8usize {
        let combinations = symbols.len().pow(length.min(5) as u32);
        for mut index in 0..combinations {
            input.clear();
            for _ in 0..length {
                input.push(symbols[index % symbols.len()]);
                index /= symbols.len();
            }
            if let Ok(bytes) = base64_decode(&input) {
                assert_eq!(base64_encode(&bytes).as_bytes(), input.as_slice());
            }
        }
    }
}

#[test]
fn constant_time_eq_compares_bytes_and_lengths() {
    assert!(constant_time_eq(b"", b""));
    assert!(constant_time_eq(b"secret", b"secret"));
    assert!(!constant_time_eq(b"secret", b"secreT"));
    assert!(!constant_time_eq(b"Secret", b"secret"));
    assert!(!constant_time_eq(b"secret", b"secrets"));
}

#[test]
fn sha1_matches_the_fips_180_examples() {
    let cases: [(&[u8], &str); 3] = [
        (b"", "da39a3ee5e6b4b0d3255bfef95601890afd80709"),
        (b"abc", "a9993e364706816aba3e25717850c26c9cd0d89d"),
        (
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
        ),
    ];
    for (message, digest) in cases {
        assert_eq!(hex(&sha1(message)), digest);
    }
}

#[test]
fn sha1_of_a_million_a_matches_the_nist_long_message() {
    let mut hash = Sha1::new();
    let chunk = [b'a'; 1000];
    for _ in 0..1000 {
        hash.update(&chunk);
    }
    assert_eq!(
        hex(&hash.finish()),
        "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
    );
}

#[test]
fn sha1_pads_correctly_at_every_block_boundary() {
    let cases = [
        (55, "c1c8bbdc22796e28c0e15163d20899b65621d65a"),
        (56, "c2db330f6083854c99d4b5bfb6e8f29f201be699"),
        (63, "03f09f5b158a7a8cdad920bddc29b81c18a551f5"),
        (64, "0098ba824b5c16427bd7a1122a5a442a25ec644d"),
        (65, "11655326c708d70319be2610e8a57d9a5b959d3b"),
        (127, "89d95fa32ed44a7c610b7ee38517ddf57e0bb975"),
        (128, "ad5b3fdbcb526778c2839d2f151ea753995e26a0"),
    ];
    for (length, digest) in cases {
        assert_eq!(hex(&sha1(&vec![b'a'; length])), digest, "{length} bytes");
    }
}

#[test]
fn sha1_gives_the_same_digest_however_the_message_is_split() {
    let message: Vec<u8> = (0..300u32).map(|index| (index * 7 % 251) as u8).collect();
    let whole = sha1(&message);
    for split in [1, 3, 55, 56, 63, 64, 65, 128, 200] {
        let mut hash = Sha1::new();
        for chunk in message.chunks(split) {
            hash.update(chunk);
        }
        assert_eq!(hash.finish(), whole, "chunks of {split}");
    }
}
