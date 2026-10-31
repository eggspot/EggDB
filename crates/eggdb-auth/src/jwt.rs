//! JSON Web Tokens (RFC 7519) in the JWS compact form (RFC 7515), checked against a JSON Web
//! Key Set (RFC 7517): RS256, RS384 and RS512 (RSASSA-PKCS1-v1_5), PS256 (RSASSA-PSS) and ES256
//! (ECDSA over P-256), as RFC 7518 defines them. `none` and HMAC algorithms are refused.

use crate::json::{self, Json};
use eggdb_crypto::{
    EcdsaP256Signature, P256PublicKey, RsaPublicKey, Sha256, Sha384, Sha512, base64_decode,
    ecdsa_p256_verify, rsa_pkcs1v15_verify, rsa_pss_verify, sha256, sha384, sha512,
};
use std::fmt;

const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_KEYS: usize = 64;

/// Why a token was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtError {
    /// The token is not three base64url parts of JSON.
    Malformed,
    /// The header names an algorithm this server does not accept.
    UnsupportedAlgorithm(String),
    /// No key in the set matches the header's `kid` and algorithm.
    UnknownKey,
    /// The signature does not verify.
    BadSignature,
    /// The token has expired, or is not valid yet.
    Expired,
    /// `iss` or `aud` differs from the policy's.
    WrongAudience,
    /// The claim that names the user is missing.
    NoUser,
    /// The key set itself could not be read.
    BadKeySet(String),
}

impl fmt::Display for JwtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => f.write_str("the token is not a signed JSON Web Token"),
            Self::UnsupportedAlgorithm(algorithm) => {
                write!(f, "the token's algorithm {algorithm} is not accepted")
            }
            Self::UnknownKey => f.write_str("no key in the key set signed the token"),
            Self::BadSignature => f.write_str("the token's signature does not verify"),
            Self::Expired => f.write_str("the token has expired or is not valid yet"),
            Self::WrongAudience => f.write_str("the token was issued by or for someone else"),
            Self::NoUser => f.write_str("the token does not name a user"),
            Self::BadKeySet(why) => write!(f, "the key set cannot be read: {why}"),
        }
    }
}

impl std::error::Error for JwtError {}

#[derive(Clone)]
enum Key {
    Rsa(RsaPublicKey),
    P256(P256PublicKey),
}

#[derive(Clone)]
struct NamedKey {
    id: Option<String>,
    key: Key,
}

/// The public keys tokens may be signed with, from a JSON Web Key Set document.
#[derive(Clone)]
pub struct JwtKeys {
    keys: Vec<NamedKey>,
}

impl fmt::Debug for JwtKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JwtKeys")
            .field("keys", &self.keys.len())
            .finish()
    }
}

fn base64url(text: &str) -> Option<Vec<u8>> {
    if text.contains(['+', '/', '=']) {
        return None;
    }
    let mut standard: String = text
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
    base64_decode(standard.as_bytes()).ok()
}

impl JwtKeys {
    /// Reads a JSON Web Key Set (`{"keys": [...]}`), keeping its RSA keys and its P-256 EC keys;
    /// keys of other types, and private parts, are ignored.
    pub fn from_jwks(document: &str) -> Result<Self, JwtError> {
        let parsed = json::parse(document.as_bytes())
            .ok_or_else(|| JwtError::BadKeySet("not JSON".to_owned()))?;
        let Some(Json::Array(entries)) = parsed.field("keys") else {
            return Err(JwtError::BadKeySet("no \"keys\" array".to_owned()));
        };
        let mut keys = Vec::new();
        for entry in entries.iter().take(MAX_KEYS) {
            let part = |name: &str| entry.field(name).and_then(Json::text).and_then(base64url);
            let key = match entry.field("kty").and_then(Json::text) {
                Some("RSA") => {
                    let (Some(modulus), Some(exponent)) = (part("n"), part("e")) else {
                        return Err(JwtError::BadKeySet("an RSA key without n or e".to_owned()));
                    };
                    Key::Rsa(
                        RsaPublicKey::new(&modulus, &exponent)
                            .map_err(|error| JwtError::BadKeySet(format!("{error:?}")))?,
                    )
                }
                Some("EC") if entry.field("crv").and_then(Json::text) == Some("P-256") => {
                    let (Some(x), Some(y)) = (part("x"), part("y")) else {
                        return Err(JwtError::BadKeySet("an EC key without x or y".to_owned()));
                    };
                    let mut point = vec![4u8];
                    point.extend_from_slice(&x);
                    point.extend_from_slice(&y);
                    Key::P256(
                        P256PublicKey::from_sec1(&point)
                            .map_err(|error| JwtError::BadKeySet(format!("{error:?}")))?,
                    )
                }
                _ => continue,
            };
            keys.push(NamedKey {
                id: entry.field("kid").and_then(Json::text).map(str::to_owned),
                key,
            });
        }
        if keys.is_empty() {
            return Err(JwtError::BadKeySet("no RSA or P-256 keys".to_owned()));
        }
        Ok(Self { keys })
    }
}

/// What a token must say to log a user in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwtPolicy {
    /// The `iss` the token must carry.
    pub issuer: String,
    /// A value the token's `aud` (a string or a list) must hold.
    pub audience: String,
    /// The claim that names the database user, `sub` unless set.
    pub user_claim: String,
    /// Seconds of clock difference allowed around `exp` and `nbf`.
    pub leeway_seconds: i64,
}

fn verifies(key: &Key, algorithm: &str, signed: &[u8], signature: &[u8]) -> bool {
    match (key, algorithm) {
        (Key::Rsa(key), "RS256") => rsa_pkcs1v15_verify::<Sha256>(key, &sha256(signed), signature),
        (Key::Rsa(key), "RS384") => rsa_pkcs1v15_verify::<Sha384>(key, &sha384(signed), signature),
        (Key::Rsa(key), "RS512") => rsa_pkcs1v15_verify::<Sha512>(key, &sha512(signed), signature),
        (Key::Rsa(key), "PS256") => rsa_pss_verify::<Sha256>(key, &sha256(signed), signature, 32),
        (Key::P256(key), "ES256") => {
            let (Ok(r), Ok(s)) = (
                <[u8; 32]>::try_from(signature.get(..32).unwrap_or_default()),
                <[u8; 32]>::try_from(signature.get(32..).unwrap_or_default()),
            ) else {
                return false;
            };
            ecdsa_p256_verify(key, &sha256(signed), &EcdsaP256Signature { r, s })
        }
        _ => false,
    }
}

/// Checks `token`'s signature against `keys` and its claims against `policy` at `now_unix`
/// (seconds since 1970), and returns the user its `user_claim` names.
pub fn verify(
    token: &str,
    keys: &JwtKeys,
    policy: &JwtPolicy,
    now_unix: i64,
) -> Result<String, JwtError> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(JwtError::Malformed);
    }
    let mut parts = token.trim().split('.');
    let (Some(header_text), Some(claims_text), Some(signature_text), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(JwtError::Malformed);
    };
    let header = base64url(header_text)
        .and_then(|bytes| json::parse(&bytes))
        .ok_or(JwtError::Malformed)?;
    let claims = base64url(claims_text)
        .and_then(|bytes| json::parse(&bytes))
        .ok_or(JwtError::Malformed)?;
    let signature = base64url(signature_text).ok_or(JwtError::Malformed)?;
    let algorithm = header
        .field("alg")
        .and_then(Json::text)
        .ok_or(JwtError::Malformed)?;
    if !matches!(algorithm, "RS256" | "RS384" | "RS512" | "PS256" | "ES256") {
        return Err(JwtError::UnsupportedAlgorithm(algorithm.to_owned()));
    }
    if header.field("crit").is_some() {
        return Err(JwtError::Malformed);
    }
    let key_id = header.field("kid").and_then(Json::text);
    let signed = &token.trim().as_bytes()[..header_text.len() + 1 + claims_text.len()];
    let candidates: Vec<&NamedKey> = keys
        .keys
        .iter()
        .filter(|named| key_id.is_none() || named.id.as_deref() == key_id)
        .collect();
    if candidates.is_empty() {
        return Err(JwtError::UnknownKey);
    }
    if !candidates
        .iter()
        .any(|named| verifies(&named.key, algorithm, signed, &signature))
    {
        return Err(JwtError::BadSignature);
    }
    let time = |name: &str| claims.field(name).and_then(Json::number).map(|value| value as i64);
    match time("exp") {
        Some(expires) if now_unix <= expires.saturating_add(policy.leeway_seconds) => {}
        _ => return Err(JwtError::Expired),
    }
    if let Some(not_before) = time("nbf")
        && now_unix < not_before.saturating_sub(policy.leeway_seconds)
    {
        return Err(JwtError::Expired);
    }
    if claims.field("iss").and_then(Json::text) != Some(policy.issuer.as_str()) {
        return Err(JwtError::WrongAudience);
    }
    let audience_matches = match claims.field("aud") {
        Some(Json::Text(audience)) => *audience == policy.audience,
        Some(Json::Array(audiences)) => audiences
            .iter()
            .any(|audience| audience.text() == Some(policy.audience.as_str())),
        _ => false,
    };
    if !audience_matches {
        return Err(JwtError::WrongAudience);
    }
    claims
        .field(&policy.user_claim)
        .and_then(Json::text)
        .filter(|user| !user.is_empty())
        .map(str::to_owned)
        .ok_or(JwtError::NoUser)
}

#[cfg(test)]
mod tests;
