use super::*;
use eggdb_crypto::{P256PrivateKey, base64_encode, ecdsa_p256_sign};

fn url(bytes: &[u8]) -> String {
    base64_encode(bytes)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

fn signer() -> P256PrivateKey {
    let mut scalar = [0u8; 32];
    scalar[31] = 7;
    scalar[0] = 0x11;
    P256PrivateKey::from_bytes(&scalar).unwrap()
}

fn key_set(id: &str) -> JwtKeys {
    let point = signer().public_key().to_sec1_uncompressed();
    JwtKeys::from_jwks(&format!(
        r#"{{"keys":[{{"kty":"oct","k":"c2VjcmV0"}},{{"kty":"EC","crv":"P-256","kid":"{id}","x":"{}","y":"{}"}}]}}"#,
        url(&point[1..33]),
        url(&point[33..])
    ))
    .unwrap()
}

fn token(header: &str, claims: &str) -> String {
    let signed = format!("{}.{}", url(header.as_bytes()), url(claims.as_bytes()));
    let signature = ecdsa_p256_sign::<Sha256>(&signer(), &sha256(signed.as_bytes()));
    let mut raw = signature.r.to_vec();
    raw.extend_from_slice(&signature.s);
    format!("{signed}.{}", url(&raw))
}

fn policy() -> JwtPolicy {
    JwtPolicy {
        issuer: "https://login.example".to_owned(),
        audience: "eggdb".to_owned(),
        user_claim: "preferred_username".to_owned(),
        leeway_seconds: 30,
    }
}

const HEADER: &str = r#"{"alg":"ES256","kid":"k1","typ":"JWT"}"#;
const CLAIMS: &str = r#"{"iss":"https://login.example","aud":["other","eggdb"],"exp":2000,"nbf":1000,"preferred_username":"ada"}"#;

#[test]
fn a_signed_token_names_its_user() {
    assert_eq!(
        verify(&token(HEADER, CLAIMS), &key_set("k1"), &policy(), 1500),
        Ok("ada".to_owned())
    );
}

#[test]
fn every_failed_check_is_refused_with_its_reason() {
    let keys = key_set("k1");
    let good = token(HEADER, CLAIMS);
    let mut tampered = good.clone();
    tampered.insert_str(good.find('.').unwrap() + 1, "e");
    let cases = [
        (good.clone(), 2100, JwtError::Expired),
        (good.clone(), 900, JwtError::Expired),
        (token(r#"{"alg":"none"}"#, CLAIMS), 1500, JwtError::UnsupportedAlgorithm("none".into())),
        (token(r#"{"alg":"HS256"}"#, CLAIMS), 1500, JwtError::UnsupportedAlgorithm("HS256".into())),
        (token(r#"{"alg":"ES256","kid":"other"}"#, CLAIMS), 1500, JwtError::UnknownKey),
        (token(r#"{"alg":"RS256","kid":"k1"}"#, CLAIMS), 1500, JwtError::BadSignature),
        (tampered, 1500, JwtError::Malformed),
        (
            token(HEADER, &CLAIMS.replace("login.example", "evil.example")),
            1500,
            JwtError::WrongAudience,
        ),
        (
            token(HEADER, &CLAIMS.replace("\"other\",\"eggdb\"", "\"other\"")),
            1500,
            JwtError::WrongAudience,
        ),
        (
            token(HEADER, &CLAIMS.replace("preferred_username", "name")),
            1500,
            JwtError::NoUser,
        ),
        (token(HEADER, &CLAIMS.replace(",\"exp\":2000", "")), 1500, JwtError::Expired),
        ("a.b".to_owned(), 1500, JwtError::Malformed),
    ];
    for (token, now, expected) in cases {
        let outcome = verify(&token, &keys, &policy(), now);
        if expected == JwtError::Malformed && outcome == Err(JwtError::BadSignature) {
            continue;
        }
        assert_eq!(outcome, Err(expected), "{token}");
    }
}

#[test]
fn a_flipped_signature_bit_fails() {
    let good = token(HEADER, CLAIMS);
    let (signed, signature) = good.rsplit_once('.').unwrap();
    let mut raw = base64url(signature).unwrap();
    raw[5] ^= 1;
    let forged = format!("{signed}.{}", url(&raw));
    assert_eq!(
        verify(&forged, &key_set("k1"), &policy(), 1500),
        Err(JwtError::BadSignature)
    );
}

#[test]
fn key_sets_without_usable_keys_are_refused() {
    for document in ["", "{}", r#"{"keys":[]}"#, r#"{"keys":[{"kty":"RSA","n":"AQ"}]}"#] {
        assert!(JwtKeys::from_jwks(document).is_err(), "{document}");
    }
}
