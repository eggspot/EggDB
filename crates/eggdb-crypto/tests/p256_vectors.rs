//! The published test vectors of P-256 (D35): the deterministic ECDSA signatures of RFC 6979
//! appendix A.2.5, every Wycheproof ECDSA P-256 case with SHA-256 and SHA-512 (signatures as
//! `r || s`), and every Wycheproof ECDH P-256 case, whose invalid public keys must be refused.

use eggdb_crypto::{
    EcdsaP256Signature, HashFunction, P256PrivateKey, P256PublicKey, Sha1, Sha256, Sha384,
    Sha512, ecdsa_p256_sign, ecdsa_p256_verify, p256_shared_secret,
};

const RFC_6979_PRIVATE_KEY: &str =
    "c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721";
const RFC_6979_PUBLIC_X: &str = "60fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6";
const RFC_6979_PUBLIC_Y: &str = "7903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn bytes(text: &str) -> Vec<u8> {
    if text == "-" {
        return Vec::new();
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn array(text: &str) -> [u8; 32] {
    bytes(text).try_into().unwrap()
}

fn cases(data: &str) -> impl Iterator<Item = Vec<&str>> {
    data.lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(|line| line.split(' ').collect())
}

fn rfc_6979_key() -> P256PrivateKey {
    P256PrivateKey::from_bytes(&array(RFC_6979_PRIVATE_KEY)).unwrap()
}

fn check_rfc_6979<H: HashFunction>(message: &str, r: &str, s: &str) {
    let key = rfc_6979_key();
    let digest = H::digest(message.as_bytes());
    let signature = ecdsa_p256_sign::<H>(&key, digest.as_ref());
    assert_eq!(hex(&signature.r), r, "r of {message:?}");
    assert_eq!(hex(&signature.s), s, "s of {message:?}");
    assert!(ecdsa_p256_verify(
        &key.public_key(),
        digest.as_ref(),
        &signature
    ));
}

#[test]
fn the_rfc_6979_key_has_its_published_public_key() {
    let encoded = rfc_6979_key().public_key().to_sec1_uncompressed();
    assert_eq!(
        hex(&encoded),
        format!("04{RFC_6979_PUBLIC_X}{RFC_6979_PUBLIC_Y}")
    );
    assert_eq!(hex(&rfc_6979_key().to_bytes()), RFC_6979_PRIVATE_KEY);
}

#[test]
fn ecdsa_signatures_match_rfc_6979_appendix_a_2_5_for_sample() {
    check_rfc_6979::<Sha1>(
        "sample",
        "61340c88c3aaebeb4f6d667f672ca9759a6ccaa9fa8811313039ee4a35471d32",
        "6d7f147dac089441bb2e2fe8f7a3fa264b9c475098fdcf6e00d7c996e1b8b7eb",
    );
    check_rfc_6979::<Sha256>(
        "sample",
        "efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716",
        "f7cb1c942d657c41d436c7a1b6e29f65f3e900dbb9aff4064dc4ab2f843acda8",
    );
    check_rfc_6979::<Sha384>(
        "sample",
        "0eafea039b20e9b42309fb1d89e213057cbf973dc0cfc8f129edddc800ef7719",
        "4861f0491e6998b9455193e34e7b0d284ddd7149a74b95b9261f13abde940954",
    );
    check_rfc_6979::<Sha512>(
        "sample",
        "8496a60b5e9b47c825488827e0495b0e3fa109ec4568fd3f8d1097678eb97f00",
        "2362ab1adbe2b8adf9cb9edab740ea6049c028114f2460f96554f61fae3302fe",
    );
}

#[test]
fn ecdsa_signatures_match_rfc_6979_appendix_a_2_5_for_test() {
    check_rfc_6979::<Sha1>(
        "test",
        "0cbcc86fd6abd1d99e703e1ec50069ee5c0b4ba4b9ac60e409e8ec5910d81a89",
        "01b9d7b73dfaa60d5651ec4591a0136f87653e0fd780c3b1bc872ffdeae479b1",
    );
    check_rfc_6979::<Sha256>(
        "test",
        "f1abb023518351cd71d881567b1ea663ed3efcf6c5132b354f28d3b0b7d38367",
        "019f4113742a2b14bd25926b49c649155f267e60d3814b4c0cc84250e46f0083",
    );
    check_rfc_6979::<Sha384>(
        "test",
        "83910e8b48bb0c74244ebdf7f07a1c5413d61472bd941ef3920e623fbccebeb6",
        "8ddbec54cf8cd5874883841d712142a56a8d0f218f5003cb0296b6b509619f2c",
    );
    check_rfc_6979::<Sha512>(
        "test",
        "461d93f31b6540894788fd206c07cfa0cc35f46fa3c91816fff1040ad1581a04",
        "39af9f15de0db8d97e72719c74820d304ce5226e32dedae67519e840d1194e55",
    );
}

fn check_wycheproof_ecdsa<H: HashFunction>(data: &str) -> usize {
    let mut checked = 0;
    for fields in cases(data) {
        let [id, result, public, message, signature] = fields[..] else {
            panic!("malformed line {fields:?}");
        };
        let public = P256PublicKey::from_sec1(&bytes(public)).unwrap();
        let digest = H::digest(&bytes(message));
        let signature = bytes(signature);
        let verified = match <[u8; 64]>::try_from(signature.as_slice()) {
            Ok(both) => {
                let mut signature = EcdsaP256Signature {
                    r: [0; 32],
                    s: [0; 32],
                };
                signature.r.copy_from_slice(&both[..32]);
                signature.s.copy_from_slice(&both[32..]);
                ecdsa_p256_verify(&public, digest.as_ref(), &signature)
            }
            Err(_) => false,
        };
        assert_eq!(verified, result == "valid", "Wycheproof case {id}");
        checked += 1;
    }
    checked
}

#[test]
fn ecdsa_with_sha_256_passes_every_wycheproof_case() {
    let checked = check_wycheproof_ecdsa::<Sha256>(include_str!(
        "data/wycheproof_ecdsa_p256_sha256.txt"
    ));
    assert_eq!(checked, 262);
}

#[test]
fn ecdsa_with_sha_512_passes_every_wycheproof_case() {
    let checked = check_wycheproof_ecdsa::<Sha512>(include_str!(
        "data/wycheproof_ecdsa_p256_sha512.txt"
    ));
    assert_eq!(checked, 332);
}

#[test]
fn ecdh_passes_every_wycheproof_case_and_refuses_invalid_public_keys() {
    let mut checked = 0;
    for fields in cases(include_str!("data/wycheproof_ecdh_p256.txt")) {
        let [id, result, private, public, shared] = fields[..] else {
            panic!("malformed line {fields:?}");
        };
        let private = P256PrivateKey::from_bytes(&array(private)).unwrap();
        match P256PublicKey::from_sec1(&bytes(public)) {
            Ok(public) => {
                assert_eq!(result, "valid", "Wycheproof case {id} must be refused");
                let secret = p256_shared_secret(&private, &public).unwrap();
                assert_eq!(hex(&secret), shared, "Wycheproof case {id}");
            }
            Err(_) => assert_eq!(result, "invalid", "Wycheproof case {id} must be accepted"),
        }
        checked += 1;
    }
    assert_eq!(checked, 355);
}

#[test]
fn signing_is_deterministic_and_verifies_across_hashes() {
    let key = rfc_6979_key();
    let digest = Sha256::digest(b"EggDB signs its own certificates");
    let first = ecdsa_p256_sign::<Sha256>(&key, &digest);
    let second = ecdsa_p256_sign::<Sha256>(&key, &digest);
    assert_eq!(first, second);
    assert!(ecdsa_p256_verify(&key.public_key(), &digest, &first));
    let other_digest = Sha256::digest(b"EggDB signs its own certificates.");
    assert!(!ecdsa_p256_verify(&key.public_key(), &other_digest, &first));
}
