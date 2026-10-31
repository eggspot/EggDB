//! The published test vectors of RSA signatures (D35): every Wycheproof RSASSA-PKCS1-v1_5
//! 2048-bit signing case (deterministic, so the signature must match byte for byte) and
//! verification case with SHA-256, and every Wycheproof RSASSA-PSS 2048-bit case with SHA-256,
//! MGF1-SHA-256 and a 32-byte salt.

use eggdb_crypto::{
    HashFunction, RsaPrivateKey, RsaPublicKey, Sha1, Sha256, Sha384, Sha512, rsa_pkcs1v15_sign,
    rsa_pkcs1v15_verify, rsa_pss_sign, rsa_pss_verify,
};

fn bytes(text: &str) -> Vec<u8> {
    if text == "-" {
        return Vec::new();
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

struct Case<'a> {
    key: Vec<&'a str>,
    id: &'a str,
    result: &'a str,
    message: Vec<u8>,
    signature: Vec<u8>,
}

fn cases(data: &str) -> Vec<Case<'_>> {
    let mut keys: Vec<Vec<&str>> = Vec::new();
    let mut grouped = Vec::new();
    for line in data.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(' ').collect();
        if fields[0] == "key" {
            keys.push(fields[1..].to_vec());
        } else {
            grouped.push((keys.len() - 1, fields));
        }
    }
    grouped
        .into_iter()
        .map(|(key, fields)| {
            let [id, result, message, signature] = fields[..] else {
                panic!("malformed line {fields:?}");
            };
            Case {
                key: keys[key].clone(),
                id,
                result,
                message: bytes(message),
                signature: bytes(signature),
            }
        })
        .collect()
}

fn sign_and_verify_pkcs1<H: eggdb_crypto::RsaPkcs1Hash>(
    key: &RsaPrivateKey,
    message: &[u8],
) -> Vec<u8> {
    let digest = H::digest(message);
    let signature = rsa_pkcs1v15_sign::<H>(key, digest.as_ref()).unwrap();
    assert!(rsa_pkcs1v15_verify::<H>(
        key.public_key(),
        digest.as_ref(),
        &signature
    ));
    signature
}

#[test]
fn pkcs1_signatures_match_every_wycheproof_signing_case() {
    let data = include_str!("data/wycheproof_rsa_pkcs1_sign_2048.txt");
    let mut checked = 0;
    let mut loaded: Option<(Vec<&str>, RsaPrivateKey)> = None;
    for case in cases(data) {
        let [hash, modulus, public_exponent, private_exponent] = case.key[..] else {
            panic!("malformed key {:?}", case.key);
        };
        if loaded.as_ref().is_none_or(|(fields, _)| *fields != case.key) {
            let key = RsaPrivateKey::new(
                &bytes(modulus),
                &bytes(public_exponent),
                &bytes(private_exponent),
            )
            .unwrap();
            loaded = Some((case.key.clone(), key));
        }
        let key = &loaded.as_ref().unwrap().1;
        let signature = match hash {
            "SHA-1" => sign_and_verify_pkcs1::<Sha1>(key, &case.message),
            "SHA-256" => sign_and_verify_pkcs1::<Sha256>(key, &case.message),
            "SHA-384" => sign_and_verify_pkcs1::<Sha384>(key, &case.message),
            "SHA-512" => sign_and_verify_pkcs1::<Sha512>(key, &case.message),
            other => panic!("unexpected hash {other}"),
        };
        assert_eq!(signature, case.signature, "Wycheproof case {}", case.id);
        checked += 1;
    }
    assert_eq!(checked, 35);
}

#[test]
fn pkcs1_verification_passes_every_wycheproof_case() {
    let data = include_str!("data/wycheproof_rsa_pkcs1_2048_sha256.txt");
    let mut checked = 0;
    for case in cases(data) {
        let key = RsaPublicKey::new(&bytes(case.key[0]), &bytes(case.key[1])).unwrap();
        let verified = rsa_pkcs1v15_verify::<Sha256>(
            &key,
            &Sha256::digest(&case.message),
            &case.signature,
        );
        if case.result != "acceptable" {
            assert_eq!(verified, case.result == "valid", "Wycheproof case {}", case.id);
        }
        checked += 1;
    }
    assert_eq!(checked, 259);
}

#[test]
fn pss_verification_passes_every_wycheproof_case() {
    let data = include_str!("data/wycheproof_rsa_pss_2048_sha256_mgf1_32.txt");
    let mut checked = 0;
    for case in cases(data) {
        let key = RsaPublicKey::new(&bytes(case.key[0]), &bytes(case.key[1])).unwrap();
        let verified = rsa_pss_verify::<Sha256>(
            &key,
            &Sha256::digest(&case.message),
            &case.signature,
            32,
        );
        assert_eq!(verified, case.result == "valid", "Wycheproof case {}", case.id);
        checked += 1;
    }
    assert_eq!(checked, 108);
}

fn first_signing_key() -> RsaPrivateKey {
    let data = include_str!("data/wycheproof_rsa_pkcs1_sign_2048.txt");
    let line = data
        .lines()
        .find(|line| line.starts_with("key SHA-256 "))
        .unwrap();
    let fields: Vec<&str> = line.split(' ').collect();
    RsaPrivateKey::new(&bytes(fields[2]), &bytes(fields[3]), &bytes(fields[4])).unwrap()
}

#[test]
fn a_pss_signature_verifies_with_its_salt_length_only() {
    let key = first_signing_key();
    let digest = Sha256::digest(b"EggDB speaks TLS");
    let salt = [0x5au8; 32];
    let signature = rsa_pss_sign::<Sha256>(&key, &digest, &salt).unwrap();
    assert_eq!(signature.len(), key.public_key().signature_bytes());
    assert!(rsa_pss_verify::<Sha256>(key.public_key(), &digest, &signature, 32));
    assert!(!rsa_pss_verify::<Sha256>(key.public_key(), &digest, &signature, 20));
    let other = Sha256::digest(b"EggDB speaks TLS!");
    assert!(!rsa_pss_verify::<Sha256>(key.public_key(), &other, &signature, 32));
    let mut changed = signature.clone();
    changed[100] ^= 0x10;
    assert!(!rsa_pss_verify::<Sha256>(key.public_key(), &digest, &changed, 32));
}

#[test]
fn a_private_exponent_that_does_not_match_is_refused() {
    let data = include_str!("data/wycheproof_rsa_pkcs1_sign_2048.txt");
    let line = data
        .lines()
        .find(|line| line.starts_with("key SHA-256 "))
        .unwrap();
    let fields: Vec<&str> = line.split(' ').collect();
    let mut private_exponent = bytes(fields[4]);
    let last = private_exponent.len() - 1;
    private_exponent[last] ^= 2;
    assert!(RsaPrivateKey::new(&bytes(fields[2]), &bytes(fields[3]), &private_exponent).is_err());
    let public = RsaPublicKey::new(&bytes(fields[2]), &bytes(fields[3])).unwrap();
    assert_eq!(public.modulus(), bytes(fields[2]));
    assert_eq!(public.public_exponent(), bytes(fields[3]));
}
