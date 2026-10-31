//! The published test vectors of AES and AES-GCM (D35): FIPS 197's appendix examples, the
//! examples of the GCM specification, and every Wycheproof case with a 96-bit nonce for AES-GCM
//! and ChaCha20-Poly1305 (valid ones must seal to the published bytes and open, invalid ones
//! must be refused).

use eggdb_crypto::{
    AEAD_NONCE_BYTES, AEAD_TAG_BYTES, AeadError, Aes, AesGcm, AesKeyLengthError,
    ChaCha20Poly1305,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    if text == "-" {
        return Vec::new();
    }
    let digits = text.as_bytes();
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn array<const N: usize>(text: &str) -> [u8; N] {
    unhex(text).try_into().unwrap()
}

fn sequence(length: u8) -> Vec<u8> {
    (0..length).collect()
}

#[test]
fn aes_matches_the_fips_197_appendix_c_examples() {
    let plaintext: [u8; 16] = array("00112233445566778899aabbccddeeff");
    let cases = [
        (16, "69c4e0d86a7b0430d8cdb78070b4c55a"),
        (24, "dda97ca4864cdfe06eaf70a0ec0d7191"),
        (32, "8ea2b7ca516745bfeafc49904b496089"),
    ];
    for (key_length, ciphertext) in cases {
        let cipher = Aes::new(&sequence(key_length)).unwrap();
        let mut block = plaintext;
        cipher.encrypt_block(&mut block);
        assert_eq!(hex(&block), ciphertext, "AES with a {key_length}-byte key");
    }
}

#[test]
fn aes_matches_the_fips_197_appendix_b_cipher_example() {
    let cipher = Aes::new(&unhex("2b7e151628aed2a6abf7158809cf4f3c")).unwrap();
    let mut block: [u8; 16] = array("3243f6a8885a308d313198a2e0370734");
    cipher.encrypt_block(&mut block);
    assert_eq!(hex(&block), "3925841d02dc09fbdc118597196a0b32");
}

#[test]
fn aes_encrypts_many_blocks_like_one_at_a_time() {
    let cipher = Aes::new(&sequence(32)).unwrap();
    let mut blocks: Vec<[u8; 16]> = (0..11u8).map(|index| [index; 16]).collect();
    let expected: Vec<[u8; 16]> = blocks
        .iter()
        .map(|block| {
            let mut one = *block;
            cipher.encrypt_block(&mut one);
            one
        })
        .collect();
    cipher.encrypt_blocks(&mut blocks);
    assert_eq!(blocks, expected);
}

#[test]
fn aes_refuses_keys_of_other_lengths() {
    for length in [0, 1, 15, 17, 23, 25, 31, 33, 64] {
        assert_eq!(
            Aes::new(&vec![0u8; length]).err(),
            Some(AesKeyLengthError { length })
        );
        assert!(AesGcm::new(&vec![0u8; length]).is_err());
    }
}

#[test]
fn aes_gcm_matches_the_gcm_specification_test_cases_1_and_2() {
    let aead = AesGcm::new(&[0u8; 16]).unwrap();
    let nonce = [0u8; AEAD_NONCE_BYTES];
    let mut empty = [0u8; 0];
    let tag = aead.seal_in_place(&nonce, &[], &mut empty);
    assert_eq!(hex(&tag), "58e2fccefa7e3061367f1d57a4e7455a");
    let mut block = [0u8; 16];
    let tag = aead.seal_in_place(&nonce, &[], &mut block);
    assert_eq!(hex(&block), "0388dace60b6a392f328c2b971b2fe78");
    assert_eq!(hex(&tag), "ab6e47d42cec13bdf53a67b21257bddf");
}

enum Aead {
    Gcm(AesGcm),
    ChaCha(ChaCha20Poly1305),
}

impl Aead {
    fn seal(&self, nonce: &[u8; 12], aad: &[u8], buffer: &mut [u8]) -> [u8; AEAD_TAG_BYTES] {
        match self {
            Self::Gcm(aead) => aead.seal_in_place(nonce, aad, buffer),
            Self::ChaCha(aead) => aead.seal_in_place(nonce, aad, buffer),
        }
    }

    fn open(
        &self,
        nonce: &[u8; 12],
        aad: &[u8],
        buffer: &mut [u8],
        tag: &[u8; AEAD_TAG_BYTES],
    ) -> Result<(), AeadError> {
        match self {
            Self::Gcm(aead) => aead.open_in_place(nonce, aad, buffer, tag),
            Self::ChaCha(aead) => aead.open_in_place(nonce, aad, buffer, tag),
        }
    }
}

fn run_wycheproof(cases: &str, make: impl Fn(&[u8]) -> Aead) -> usize {
    let mut checked = 0;
    for line in cases.lines().filter(|line| !line.starts_with('#')) {
        let fields: Vec<&str> = line.split(' ').collect();
        let [id, result, key, nonce, aad, message, ciphertext, tag] = fields[..] else {
            panic!("malformed vector line {line}");
        };
        let aead = make(&unhex(key));
        let nonce: [u8; AEAD_NONCE_BYTES] = array(nonce);
        let aad = unhex(aad);
        let message = unhex(message);
        let ciphertext = unhex(ciphertext);
        let tag: [u8; AEAD_TAG_BYTES] = array(tag);
        let mut opened = ciphertext.clone();
        let outcome = aead.open(&nonce, &aad, &mut opened, &tag);
        match result {
            "valid" => {
                assert_eq!(outcome, Ok(()), "case {id} must open");
                assert_eq!(opened, message, "case {id} plaintext");
                let mut sealed = message.clone();
                let sealed_tag = aead.seal(&nonce, &aad, &mut sealed);
                assert_eq!(sealed, ciphertext, "case {id} ciphertext");
                assert_eq!(sealed_tag, tag, "case {id} tag");
            }
            _ => {
                assert_eq!(outcome, Err(AeadError), "case {id} must be refused");
                assert_eq!(opened, ciphertext, "case {id} buffer untouched");
            }
        }
        checked += 1;
    }
    checked
}

#[test]
fn aes_gcm_passes_every_wycheproof_case_with_a_96_bit_nonce() {
    let checked = run_wycheproof(include_str!("data/wycheproof_aes_gcm.txt"), |key| {
        Aead::Gcm(AesGcm::new(key).unwrap())
    });
    assert_eq!(checked, 197);
}

#[test]
fn chacha20_poly1305_passes_every_wycheproof_case() {
    let checked = run_wycheproof(include_str!("data/wycheproof_chacha20_poly1305.txt"), |key| {
        Aead::ChaCha(ChaCha20Poly1305::new(&key.try_into().unwrap()))
    });
    assert_eq!(checked, 316);
}
