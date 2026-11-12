use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;
use eggdb_crypto::{EcdsaP256Signature, P256PrivateKey, Sha256, base64_decode, base64_encode, constant_time_eq, ecdsa_p256_sign, pbkdf2_hmac_sha256, sha256};

pub(crate) const FUNCTIONS: [&str; 5] = ["eggdb_password_hash", "eggdb_password_verify", "eggdb_jwt_sign", "eggdb_jwt_public_jwk", "eggdb_jwt_verify"];
const PASSWORD_SCHEME: &str = "pbkdf2-sha256";
const PASSWORD_ITERATIONS: u32 = 600_000;
const LEAST_ITERATIONS: u32 = 100_000;
const MOST_ITERATIONS: u32 = 10_000_000;
const SALT_BYTES: usize = 16;
const HASH_BYTES: usize = 32;
const MOST_PASSWORD_BYTES: usize = 1024;
const MOST_CLAIMS_BYTES: usize = 64 * 1024;
const LEEWAY_SECONDS: i64 = 60;

fn invalid(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

fn base64url(bytes: &[u8]) -> String {
    base64_encode(bytes)
        .trim_end_matches('=')
        .chars()
        .map(|character| match character {
            '+' => '-',
            '/' => '_',
            other => other,
        })
        .collect()
}

fn from_base64url(text: &str) -> Option<Vec<u8>> {
    let mut standard: String = text
        .chars()
        .map(|character| match character {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    while !standard.len().is_multiple_of(4) {
        standard.push('=');
    }
    base64_decode(standard.as_bytes()).ok()
}

fn text_of(value: &Value) -> Option<String> {
    value.to_text()
}

fn bytes_of(value: &Value) -> Vec<u8> {
    match value {
        Value::Bytea(bytes) => bytes.clone(),
        other => other.to_text().unwrap_or_default().into_bytes(),
    }
}

fn password_hash(password: &str) -> SqlResult<String> {
    if password.len() > MOST_PASSWORD_BYTES {
        return Err(invalid(format!("eggdb_password_hash takes at most {MOST_PASSWORD_BYTES} bytes")));
    }
    let mut salt = [0u8; SALT_BYTES];
    crate::random::fill(&mut salt)?;
    let mut hash = [0u8; HASH_BYTES];
    pbkdf2_hmac_sha256(password.as_bytes(), &salt, PASSWORD_ITERATIONS, &mut hash);
    Ok(format!("{PASSWORD_SCHEME}${PASSWORD_ITERATIONS}${}${}", base64_encode(&salt), base64_encode(&hash)))
}

fn password_verify(password: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    let (Some(PASSWORD_SCHEME), Some(iterations), Some(salt), Some(hash), None) = (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(iterations), Ok(salt), Ok(hash)) = (iterations.parse::<u32>(), base64_decode(salt.as_bytes()), base64_decode(hash.as_bytes())) else {
        return false;
    };
    if !(LEAST_ITERATIONS..=MOST_ITERATIONS).contains(&iterations) || hash.len() != HASH_BYTES || password.len() > MOST_PASSWORD_BYTES {
        return false;
    }
    let mut computed = [0u8; HASH_BYTES];
    pbkdf2_hmac_sha256(password.as_bytes(), &salt, iterations, &mut computed);
    constant_time_eq(&computed, &hash)
}

fn private_key(bytes: &[u8]) -> SqlResult<P256PrivateKey> {
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| invalid("a signing key is 32 bytes, a P-256 private key"))?;
    P256PrivateKey::from_bytes(&bytes).map_err(|_| invalid("the signing key is not a valid P-256 private key"))
}

fn public_jwk_fields(key: &P256PrivateKey) -> (String, String, String) {
    let point = key.public_key().to_sec1_uncompressed();
    let x = base64url(&point[1..33]);
    let y = base64url(&point[33..65]);
    let thumbprint = base64url(&sha256(format!("{{\"crv\":\"P-256\",\"kty\":\"EC\",\"x\":\"{x}\",\"y\":\"{y}\"}}").as_bytes()));
    (x, y, thumbprint)
}

fn public_jwk(key_bytes: &[u8]) -> SqlResult<String> {
    let key = private_key(key_bytes)?;
    let (x, y, kid) = public_jwk_fields(&key);
    Ok(format!("{{\"kty\": \"EC\", \"crv\": \"P-256\", \"x\": \"{x}\", \"y\": \"{y}\", \"kid\": \"{kid}\", \"alg\": \"ES256\", \"use\": \"sig\"}}"))
}

fn jwt_sign(claims: &str, key_bytes: &[u8]) -> SqlResult<String> {
    if claims.len() > MOST_CLAIMS_BYTES {
        return Err(invalid(format!("eggdb_jwt_sign takes claims of at most {MOST_CLAIMS_BYTES} bytes")));
    }
    let parsed = crate::json::parse_document(claims).map_err(|error| invalid(format!("eggdb_jwt_sign: {}", error.message)))?;
    if !matches!(parsed.value, crate::json::Json::Object(_)) {
        return Err(invalid("eggdb_jwt_sign takes the claims as a JSON object"));
    }
    let key = private_key(key_bytes)?;
    let (_, _, kid) = public_jwk_fields(&key);
    let header = format!("{{\"alg\":\"ES256\",\"typ\":\"JWT\",\"kid\":\"{kid}\"}}");
    let signed = format!("{}.{}", base64url(header.as_bytes()), base64url(claims.as_bytes()));
    let EcdsaP256Signature { r, s } = ecdsa_p256_sign::<Sha256>(&key, &sha256(signed.as_bytes()));
    let mut signature = Vec::with_capacity(64);
    signature.extend_from_slice(&r);
    signature.extend_from_slice(&s);
    Ok(format!("{signed}.{}", base64url(&signature)))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

fn jwt_verify(token: &str, jwks: &str, issuer: &str, audience: &str) -> SqlResult<String> {
    let refused = |why: String| SqlError::new(SqlState::INVALID_AUTHORIZATION_SPECIFICATION, format!("the token is not accepted: {why}"));
    let keys = eggdb_auth::jwt::JwtKeys::from_jwks(jwks).map_err(|error| invalid(format!("eggdb_jwt_verify: {error}")))?;
    let policy = eggdb_auth::jwt::JwtPolicy {
        issuer: issuer.to_owned(),
        audience: audience.to_owned(),
        user_claim: "sub".to_owned(),
        leeway_seconds: LEEWAY_SECONDS,
    };
    eggdb_auth::jwt::verify(token, &keys, &policy, now_unix()).map_err(|error| refused(error.to_string()))?;
    let payload = token.trim().split('.').nth(1).and_then(from_base64url).ok_or_else(|| refused("malformed".to_owned()))?;
    String::from_utf8(payload).map_err(|_| refused("malformed".to_owned()))
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    let wrong = || Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("{name} does not take these arguments")));
    Some(match (name, values) {
        ("eggdb_password_hash", [password]) => text_of(password).map_or_else(wrong, |password| password_hash(&password).map(Value::Text)),
        ("eggdb_password_verify", [password, stored]) => match (text_of(password), text_of(stored)) {
            (Some(password), Some(stored)) => Ok(Value::Boolean(password_verify(&password, &stored))),
            _ => wrong(),
        },
        ("eggdb_jwt_sign", [claims, key]) => text_of(claims).map_or_else(wrong, |claims| jwt_sign(&claims, &bytes_of(key)).map(Value::Text)),
        ("eggdb_jwt_public_jwk", [key]) => public_jwk(&bytes_of(key)).and_then(|text| crate::json::normalize(&text).map(Value::Jsonb)),
        ("eggdb_jwt_verify", [token, jwks, issuer, audience]) => match (text_of(token), text_of(jwks), text_of(issuer), text_of(audience)) {
            (Some(token), Some(jwks), Some(issuer), Some(audience)) => {
                jwt_verify(&token, &jwks, &issuer, &audience).and_then(|claims| crate::json::normalize(&claims).map(Value::Jsonb))
            }
            _ => wrong(),
        },
        _ => wrong(),
    })
}

pub(crate) fn result_type(name: &str) -> crate::types::SqlType {
    match name {
        "eggdb_password_verify" => crate::types::SqlType::Boolean,
        "eggdb_jwt_public_jwk" | "eggdb_jwt_verify" => crate::types::SqlType::Jsonb,
        _ => crate::types::SqlType::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_hash_with_salt_and_verify() {
        crate::memory_store::use_seeded_random_bytes();
        let stored = password_hash("correct horse").unwrap();
        assert!(stored.starts_with("pbkdf2-sha256$600000$"));
        assert!(password_verify("correct horse", &stored));
        assert!(!password_verify("wrong horse", &stored));
        assert!(!password_verify("correct horse", "pbkdf2-sha256$1$AAAA$AAAA"));
    }

    #[test]
    fn signed_tokens_verify_against_their_own_public_key() {
        let key = [7u8; 32];
        let now = now_unix();
        let claims = format!("{{\"sub\":\"app_user\",\"iss\":\"https://auth.example\",\"aud\":\"shop\",\"exp\":{},\"user_id\":42}}", now + 300);
        let token = jwt_sign(&claims, &key).unwrap();
        let jwks = format!("{{\"keys\": [{}]}}", public_jwk(&key).unwrap());
        assert_eq!(jwt_verify(&token, &jwks, "https://auth.example", "shop").unwrap(), claims);
        assert!(jwt_verify(&token, &jwks, "https://auth.example", "other").is_err());
        let mut tampered = token.clone();
        tampered.replace_range(tampered.len() - 4.., "AAAA");
        assert!(jwt_verify(&tampered, &jwks, "https://auth.example", "shop").is_err());
    }
}
