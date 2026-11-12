use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, parse_document};
use crate::types::Value;
use eggdb_crypto::{EcdsaP256Signature, P256PublicKey, base64_decode, ecdsa_p256_verify, sha256};

pub(crate) const FUNCTIONS: [&str; 2] = ["webauthn_registration", "webauthn_assertion"];
const MOST_CBOR_DEPTH: usize = 16;
const MOST_CBOR_ITEMS: u64 = 1024;
const MOST_INPUT_BYTES: usize = 64 * 1024;
const FLAG_USER_PRESENT: u8 = 0x01;
const FLAG_USER_VERIFIED: u8 = 0x04;
const FLAG_ATTESTED_CREDENTIAL: u8 = 0x40;
const AUTHENTICATOR_DATA_HEADER: usize = 37;
const COSE_ES256: i64 = -7;

#[derive(Debug, Clone, PartialEq)]
enum Cbor {
    Unsigned(u64),
    Negative(i64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    Map(Vec<(Cbor, Cbor)>),
    Simple(u8),
}

impl Cbor {
    fn integer(&self) -> Option<i64> {
        match self {
            Self::Unsigned(value) => i64::try_from(*value).ok(),
            Self::Negative(value) => Some(*value),
            _ => None,
        }
    }

    fn field(&self, key: &Cbor) -> Option<&Cbor> {
        match self {
            Self::Map(entries) => entries.iter().find(|(name, _)| name == key).map(|(_, value)| value),
            _ => None,
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, count: usize) -> Option<&[u8]> {
        let end = self.at.checked_add(count)?;
        let taken = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(taken)
    }

    fn argument(&mut self, additional: u8) -> Option<u64> {
        match additional {
            0..=23 => Some(u64::from(additional)),
            24 => self.take(1).map(|bytes| u64::from(bytes[0])),
            25 => self.take(2).map(|bytes| u64::from(u16::from_be_bytes([bytes[0], bytes[1]]))),
            26 => self.take(4).and_then(|bytes| bytes.try_into().ok()).map(|bytes: [u8; 4]| u64::from(u32::from_be_bytes(bytes))),
            27 => self.take(8).and_then(|bytes| bytes.try_into().ok()).map(u64::from_be_bytes),
            _ => None,
        }
    }

    fn item(&mut self, depth: usize) -> Option<Cbor> {
        if depth > MOST_CBOR_DEPTH {
            return None;
        }
        let head = *self.take(1)?.first()?;
        let major = head >> 5;
        let additional = head & 0x1f;
        if major == 7 {
            return (additional < 24).then_some(Cbor::Simple(additional));
        }
        let argument = self.argument(additional)?;
        match major {
            0 => Some(Cbor::Unsigned(argument)),
            1 => i64::try_from(argument).ok().map(|value| Cbor::Negative(-1 - value)),
            2 => Some(Cbor::Bytes(self.take(usize::try_from(argument).ok()?)?.to_vec())),
            3 => String::from_utf8(self.take(usize::try_from(argument).ok()?)?.to_vec()).ok().map(Cbor::Text),
            4 => {
                if argument > MOST_CBOR_ITEMS {
                    return None;
                }
                (0..argument).map(|_| self.item(depth + 1)).collect::<Option<Vec<_>>>().map(Cbor::Array)
            }
            5 => {
                if argument > MOST_CBOR_ITEMS {
                    return None;
                }
                (0..argument)
                    .map(|_| Some((self.item(depth + 1)?, self.item(depth + 1)?)))
                    .collect::<Option<Vec<_>>>()
                    .map(Cbor::Map)
            }
            _ => None,
        }
    }
}

fn cbor(bytes: &[u8]) -> Option<(Cbor, usize)> {
    let mut reader = Reader { bytes, at: 0 };
    let item = reader.item(0)?;
    Some((item, reader.at))
}

fn refused(message: &str) -> SqlError {
    SqlError::new(SqlState::INVALID_AUTHORIZATION_SPECIFICATION, format!("the passkey response is not accepted: {message}"))
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

fn check_client_data(client_data: &[u8], wanted_type: &str, challenge: &[u8], origin: &str) -> SqlResult<()> {
    let text = std::str::from_utf8(client_data).map_err(|_| refused("clientDataJSON is not UTF-8"))?;
    let document = parse_document(text).map_err(|_| refused("clientDataJSON is not JSON"))?;
    let Json::Object(members) = document.value else {
        return Err(refused("clientDataJSON is not an object"));
    };
    let field = |name: &str| {
        members.iter().find(|(key, _)| key == name).and_then(|(_, node)| match &node.value {
            Json::String(text) => Some(text.as_str()),
            _ => None,
        })
    };
    if field("type") != Some(wanted_type) {
        return Err(refused(&format!("clientDataJSON's type is not {wanted_type}")));
    }
    let given = field("challenge").and_then(from_base64url).ok_or_else(|| refused("clientDataJSON has no challenge"))?;
    if !eggdb_crypto::constant_time_eq(&given, challenge) {
        return Err(refused("the challenge differs"));
    }
    if field("origin") != Some(origin) {
        return Err(refused("the origin differs"));
    }
    Ok(())
}

struct AuthenticatorData<'a> {
    flags: u8,
    sign_count: u32,
    rest: &'a [u8],
}

fn authenticator_data<'a>(data: &'a [u8], rp_id: &str, user_verification: bool) -> SqlResult<AuthenticatorData<'a>> {
    if data.len() < AUTHENTICATOR_DATA_HEADER {
        return Err(refused("authenticatorData is too short"));
    }
    if !eggdb_crypto::constant_time_eq(&data[..32], &sha256(rp_id.as_bytes())) {
        return Err(refused("the relying party id differs"));
    }
    let flags = data[32];
    if flags & FLAG_USER_PRESENT == 0 {
        return Err(refused("the user was not present"));
    }
    if user_verification && flags & FLAG_USER_VERIFIED == 0 {
        return Err(refused("the user was not verified"));
    }
    let sign_count = u32::from_be_bytes([data[33], data[34], data[35], data[36]]);
    Ok(AuthenticatorData { flags, sign_count, rest: &data[AUTHENTICATOR_DATA_HEADER..] })
}

fn public_key_of(cose: &Cbor) -> SqlResult<Vec<u8>> {
    let number = |key: i64| cose.field(&if key < 0 { Cbor::Negative(key) } else { Cbor::Unsigned(key as u64) });
    if number(1).and_then(Cbor::integer) != Some(2) || number(3).and_then(Cbor::integer) != Some(COSE_ES256) || number(-1).and_then(Cbor::integer) != Some(1) {
        return Err(refused("only ES256 keys on P-256 are accepted"));
    }
    let (Some(Cbor::Bytes(x)), Some(Cbor::Bytes(y))) = (number(-2), number(-3)) else {
        return Err(refused("the credential key has no coordinates"));
    };
    let mut point = vec![4u8];
    point.extend_from_slice(x);
    point.extend_from_slice(y);
    P256PublicKey::from_sec1(&point).map_err(|_| refused("the credential key is not on P-256"))?;
    Ok(point)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn registration(attestation_object: &[u8], client_data: &[u8], challenge: &[u8], origin: &str, rp_id: &str, user_verification: bool) -> SqlResult<String> {
    check_client_data(client_data, "webauthn.create", challenge, origin)?;
    let (object, _) = cbor(attestation_object).ok_or_else(|| refused("attestationObject is not CBOR"))?;
    let Some(Cbor::Bytes(auth_data)) = object.field(&Cbor::Text("authData".to_owned())) else {
        return Err(refused("attestationObject has no authData"));
    };
    let format = match object.field(&Cbor::Text("fmt".to_owned())) {
        Some(Cbor::Text(format)) => format.clone(),
        _ => return Err(refused("attestationObject has no fmt")),
    };
    let data = authenticator_data(auth_data, rp_id, user_verification)?;
    if data.flags & FLAG_ATTESTED_CREDENTIAL == 0 || data.rest.len() < 18 {
        return Err(refused("authenticatorData carries no credential"));
    }
    let length = usize::from(u16::from_be_bytes([data.rest[16], data.rest[17]]));
    let credential_id = data.rest.get(18..18 + length).ok_or_else(|| refused("the credential id is cut"))?;
    let (cose, _) = cbor(&data.rest[18 + length..]).ok_or_else(|| refused("the credential key is not CBOR"))?;
    let public_key = public_key_of(&cose)?;
    Ok(format!(
        "{{\"credential_id\": \"{}\", \"public_key\": \"{}\", \"sign_count\": {}, \"aaguid\": \"{}\", \"attestation_format\": \"{}\", \"user_verified\": {}}}",
        hex(credential_id),
        hex(&public_key),
        data.sign_count,
        hex(&data.rest[..16]),
        format.chars().filter(char::is_ascii_alphanumeric).collect::<String>(),
        data.flags & FLAG_USER_VERIFIED != 0
    ))
}

fn der_signature(der: &[u8]) -> Option<EcdsaP256Signature> {
    let mut reader = der;
    let mut next = |wanted: u8| -> Option<&[u8]> {
        let (&tag, rest) = reader.split_first()?;
        let (&length, rest) = rest.split_first()?;
        if tag != wanted || length >= 0x80 {
            return None;
        }
        let (value, rest) = rest.split_at_checked(usize::from(length))?;
        reader = rest;
        Some(value)
    };
    let sequence = next(0x30)?;
    let mut inner = sequence;
    let mut integer = || -> Option<[u8; 32]> {
        let (&tag, rest) = inner.split_first()?;
        let (&length, rest) = rest.split_first()?;
        if tag != 0x02 || length >= 0x80 {
            return None;
        }
        let (value, rest) = rest.split_at_checked(usize::from(length))?;
        inner = rest;
        let trimmed: &[u8] = value.strip_prefix(&[0u8]).unwrap_or(value);
        if trimmed.len() > 32 {
            return None;
        }
        let mut out = [0u8; 32];
        out[32 - trimmed.len()..].copy_from_slice(trimmed);
        Some(out)
    };
    let r = integer()?;
    let s = integer()?;
    Some(EcdsaP256Signature { r, s })
}

#[allow(clippy::too_many_arguments)]
fn assertion(public_key: &[u8], authenticator: &[u8], client_data: &[u8], signature: &[u8], challenge: &[u8], origin: &str, rp_id: &str, stored_count: i64, user_verification: bool) -> SqlResult<i64> {
    check_client_data(client_data, "webauthn.get", challenge, origin)?;
    let data = authenticator_data(authenticator, rp_id, user_verification)?;
    let key = P256PublicKey::from_sec1(public_key).map_err(|_| refused("the stored key is not a P-256 point"))?;
    let signature = der_signature(signature).ok_or_else(|| refused("the signature is not DER"))?;
    let mut signed = authenticator.to_vec();
    signed.extend_from_slice(&sha256(client_data));
    if !ecdsa_p256_verify(&key, &sha256(&signed), &signature) {
        return Err(refused("the signature does not verify"));
    }
    let count = i64::from(data.sign_count);
    if (count != 0 || stored_count != 0) && count <= stored_count {
        return Err(refused("the signature counter went back, so the passkey may be cloned"));
    }
    Ok(count)
}

fn flag_of(value: &Value) -> Option<bool> {
    match value {
        Value::Boolean(flag) => Some(*flag),
        _ => None,
    }
}

fn bytes_of(value: &Value) -> SqlResult<Vec<u8>> {
    let bytes = match value {
        Value::Bytea(bytes) => bytes.clone(),
        other => other.to_text().unwrap_or_default().into_bytes(),
    };
    if bytes.len() > MOST_INPUT_BYTES {
        return Err(SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "a passkey response is at most 64 KiB"));
    }
    Ok(bytes)
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    let wrong = || Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("{name} does not take these arguments")));
    Some((|| match (name, values) {
        ("webauthn_registration", [attestation, client_data, challenge, origin, rp_id, verification]) => {
            let (Some(origin), Some(rp_id), Some(verification)) = (origin.to_text(), rp_id.to_text(), flag_of(verification)) else {
                return wrong();
            };
            registration(&bytes_of(attestation)?, &bytes_of(client_data)?, &bytes_of(challenge)?, &origin, &rp_id, verification)
                .and_then(|text| crate::json::normalize(&text).map(Value::Jsonb))
        }
        ("webauthn_assertion", [public_key, authenticator, client_data, signature, challenge, origin, rp_id, stored_count, verification]) => {
            let (Some(origin), Some(rp_id), Some(stored_count), Some(verification)) = (origin.to_text(), rp_id.to_text(), stored_count.as_integer(), flag_of(verification)) else {
                return wrong();
            };
            assertion(&bytes_of(public_key)?, &bytes_of(authenticator)?, &bytes_of(client_data)?, &bytes_of(signature)?, &bytes_of(challenge)?, &origin, &rp_id, stored_count, verification)
                .map(Value::BigInt)
        }
        _ => wrong(),
    })())
}

pub(crate) fn result_type(name: &str) -> crate::types::SqlType {
    if name == "webauthn_assertion" { crate::types::SqlType::BigInt } else { crate::types::SqlType::Jsonb }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_crypto::{P256PrivateKey, Sha256, ecdsa_p256_sign};

    fn base64url(bytes: &[u8]) -> String {
        eggdb_crypto::base64_encode(bytes).trim_end_matches('=').replace('+', "-").replace('/', "_")
    }

    fn der(signature: &EcdsaP256Signature) -> Vec<u8> {
        let integer = |value: &[u8; 32]| {
            let trimmed: Vec<u8> = value.iter().copied().skip_while(|byte| *byte == 0).collect();
            let mut out = vec![0x02];
            if trimmed.first().is_some_and(|byte| byte & 0x80 != 0) {
                out.push(trimmed.len() as u8 + 1);
                out.push(0);
            } else {
                out.push(trimmed.len() as u8);
            }
            out.extend_from_slice(&trimmed);
            out
        };
        let mut body = integer(&signature.r);
        body.extend(integer(&signature.s));
        let mut out = vec![0x30, body.len() as u8];
        out.extend(body);
        out
    }

    #[test]
    fn a_passkey_registers_and_then_signs_in() {
        let key = P256PrivateKey::from_bytes(&[5u8; 32]).unwrap();
        let point = key.public_key().to_sec1_uncompressed();
        let challenge = b"register-challenge-0123456789abc";
        let client_data = format!("{{\"type\":\"webauthn.create\",\"challenge\":\"{}\",\"origin\":\"https://shop.example\"}}", base64url(challenge));
        let mut cose = vec![0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21, 0x58, 0x20];
        cose.extend_from_slice(&point[1..33]);
        cose.extend_from_slice(&[0x22, 0x58, 0x20]);
        cose.extend_from_slice(&point[33..65]);
        let mut auth_data = sha256(b"shop.example").to_vec();
        auth_data.push(FLAG_USER_PRESENT | FLAG_USER_VERIFIED | FLAG_ATTESTED_CREDENTIAL);
        auth_data.extend_from_slice(&0u32.to_be_bytes());
        auth_data.extend_from_slice(&[0u8; 16]);
        auth_data.extend_from_slice(&[0, 4, 9, 9, 9, 9]);
        auth_data.extend_from_slice(&cose);
        let mut attestation = vec![0xa3, 0x63];
        attestation.extend_from_slice(b"fmt");
        attestation.extend_from_slice(&[0x64]);
        attestation.extend_from_slice(b"none");
        attestation.extend_from_slice(&[0x67]);
        attestation.extend_from_slice(b"attStmt");
        attestation.extend_from_slice(&[0xa0, 0x68]);
        attestation.extend_from_slice(b"authData");
        attestation.extend_from_slice(&[0x59, (auth_data.len() >> 8) as u8, auth_data.len() as u8]);
        attestation.extend_from_slice(&auth_data);
        let registered = registration(&attestation, client_data.as_bytes(), challenge, "https://shop.example", "shop.example", true).unwrap();
        assert!(registered.contains("\"credential_id\": \"09090909\""), "{registered}");
        assert!(registration(&attestation, client_data.as_bytes(), b"other", "https://shop.example", "shop.example", true).is_err());

        let challenge = b"sign-in-challenge-0123456789abcd";
        let client_data = format!("{{\"type\":\"webauthn.get\",\"challenge\":\"{}\",\"origin\":\"https://shop.example\"}}", base64url(challenge));
        let mut authenticator = sha256(b"shop.example").to_vec();
        authenticator.push(FLAG_USER_PRESENT | FLAG_USER_VERIFIED);
        authenticator.extend_from_slice(&3u32.to_be_bytes());
        let mut signed = authenticator.clone();
        signed.extend_from_slice(&sha256(client_data.as_bytes()));
        let signature = der(&ecdsa_p256_sign::<Sha256>(&key, &sha256(&signed)));
        assert_eq!(assertion(&point, &authenticator, client_data.as_bytes(), &signature, challenge, "https://shop.example", "shop.example", 2, true).unwrap(), 3);
        assert!(assertion(&point, &authenticator, client_data.as_bytes(), &signature, challenge, "https://shop.example", "shop.example", 3, true).is_err());
        assert!(assertion(&point, &authenticator, client_data.as_bytes(), &signature, challenge, "https://evil.example", "shop.example", 2, true).is_err());
    }
}
