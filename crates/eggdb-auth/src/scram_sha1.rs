//! SCRAM-SHA-1 (RFC 5802), only for MongoDB drivers that still ask for it: MongoDB derives its
//! password from `hex(md5("<user>:mongo:<password>"))`. No channel binding.

use crate::scram::{MIN_SCRAM_ITERATIONS, SCRAM_SALT_BYTES, ScramFailure};
use eggdb_crypto::{
    SHA1_DIGEST_BYTES, Sha1, base64_decode, base64_encode, constant_time_eq, hmac, md5_checksum,
    sha1,
};
use std::fmt;

/// The mechanism's name.
pub const SCRAM_SHA_1: &str = "SCRAM-SHA-1";

/// A user's SCRAM-SHA-1 verifier: salt, iterations and the stored and server keys.
#[derive(Clone, PartialEq, Eq)]
pub struct ScramSha1Verifier {
    salt: Vec<u8>,
    iterations: u32,
    stored_key: [u8; SHA1_DIGEST_BYTES],
    server_key: [u8; SHA1_DIGEST_BYTES],
}

impl fmt::Debug for ScramSha1Verifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScramSha1Verifier")
            .field("iterations", &self.iterations)
            .finish_non_exhaustive()
    }
}

fn pbkdf2_hmac_sha1(password: &[u8], salt: &[u8], iterations: u32) -> [u8; SHA1_DIGEST_BYTES] {
    let mut first = salt.to_vec();
    first.extend_from_slice(&1u32.to_be_bytes());
    let mut block = hmac::<Sha1>(password, &first);
    let mut out = block;
    for _ in 1..iterations {
        block = hmac::<Sha1>(password, &block);
        for (byte, next) in out.iter_mut().zip(block) {
            *byte ^= next;
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl ScramSha1Verifier {
    /// Derives the verifier of `password` with `salt` over `iterations` (at least 4096) rounds.
    pub fn derive(password: &[u8], salt: &[u8], iterations: u32) -> Self {
        let iterations = iterations.max(MIN_SCRAM_ITERATIONS);
        let salted = pbkdf2_hmac_sha1(password, salt, iterations);
        let client_key = hmac::<Sha1>(&salted, b"Client Key");
        Self {
            salt: salt.to_vec(),
            iterations,
            stored_key: sha1(&client_key),
            server_key: hmac::<Sha1>(&salted, b"Server Key"),
        }
    }

    /// The verifier MongoDB keeps for `user` with `password`: SCRAM over
    /// `hex(md5("<user>:mongo:<password>"))`.
    pub fn for_mongodb(user: &str, password: &str, salt: &[u8], iterations: u32) -> Self {
        let digest = hex(&md5_checksum(format!("{user}:mongo:{password}").as_bytes()));
        Self::derive(digest.as_bytes(), salt, iterations)
    }

    /// A verifier no password matches, for a user that does not exist (a salt derived from
    /// `secret` and the name), so the exchange looks the same and fails at its end.
    pub fn for_unknown_user(secret: &[u8], user: &str, iterations: u32) -> Self {
        let mut message = b"sha1 salt ".to_vec();
        message.extend_from_slice(user.as_bytes());
        let salt = hmac::<Sha1>(secret, &message);
        message[..10].copy_from_slice(b"sha1 key  ");
        let stored_key = hmac::<Sha1>(secret, &message);
        Self {
            salt: salt[..SCRAM_SALT_BYTES].to_vec(),
            iterations: iterations.max(MIN_SCRAM_ITERATIONS),
            stored_key,
            server_key: stored_key,
        }
    }

    /// The PBKDF2 iteration count.
    pub fn iterations(&self) -> u32 {
        self.iterations
    }

    /// The verifier as text: `SCRAM-SHA-1$<iterations>:<salt>$<StoredKey>:<ServerKey>`, base64
    /// parts, as [`ScramSha1Verifier::from_stored`] reads it.
    pub fn to_stored(&self) -> String {
        format!(
            "SCRAM-SHA-1${}:{}${}:{}",
            self.iterations,
            base64_encode(&self.salt),
            base64_encode(&self.stored_key),
            base64_encode(&self.server_key)
        )
    }

    /// The verifier [`ScramSha1Verifier::to_stored`] wrote, or `None` when the text is not one
    /// or asks for fewer than the least iterations.
    pub fn from_stored(text: &str) -> Option<Self> {
        let rest = text.strip_prefix("SCRAM-SHA-1$")?;
        let (head, keys) = rest.split_once('$')?;
        let (iterations, salt) = head.split_once(':')?;
        let (stored, server) = keys.split_once(':')?;
        let iterations: u32 = iterations.parse().ok().filter(|count| *count >= MIN_SCRAM_ITERATIONS)?;
        let salt = base64_decode(salt.as_bytes()).ok().filter(|salt| !salt.is_empty())?;
        let stored_key = base64_decode(stored.as_bytes()).ok()?.try_into().ok()?;
        let server_key = base64_decode(server.as_bytes()).ok()?.try_into().ok()?;
        Some(Self { salt, iterations, stored_key, server_key })
    }
}

#[derive(Debug)]
enum State {
    AwaitingClientFirst,
    AwaitingClientFinal {
        client_first_bare: String,
        server_first: String,
        nonce: String,
    },
    Done,
}

/// The server's side of one SCRAM-SHA-1 exchange.
#[derive(Debug)]
pub struct ScramSha1Exchange {
    verifier: ScramSha1Verifier,
    server_nonce: String,
    state: State,
}

fn malformed(why: &str) -> ScramFailure {
    ScramFailure::Malformed(why.into())
}

impl ScramSha1Exchange {
    /// Starts an exchange against `verifier` with fresh random `server_nonce_bytes`.
    pub fn new(verifier: ScramSha1Verifier, server_nonce_bytes: &[u8]) -> Self {
        Self {
            verifier,
            server_nonce: base64_encode(server_nonce_bytes),
            state: State::AwaitingClientFirst,
        }
    }

    /// Reads the client-first-message and returns the server-first-message.
    pub fn client_first(&mut self, message: &[u8]) -> Result<String, ScramFailure> {
        if !matches!(self.state, State::AwaitingClientFirst) {
            return Err(malformed("unexpected SCRAM message"));
        }
        self.state = State::Done;
        let text = std::str::from_utf8(message).map_err(|_| malformed("the SCRAM message is not UTF-8"))?;
        let client_first_bare = text
            .strip_prefix("n,,")
            .or_else(|| text.strip_prefix("y,,"))
            .ok_or_else(|| {
                if text.starts_with("p=") {
                    ScramFailure::Unsupported("channel binding is not supported with SCRAM-SHA-1".into())
                } else {
                    malformed("the GS2 header is malformed")
                }
            })?;
        let mut attributes = client_first_bare.split(',');
        if !attributes.next().is_some_and(|user| user.starts_with("n=")) {
            return Err(malformed("the client-first-message has no user name attribute"));
        }
        let client_nonce = attributes
            .next()
            .and_then(|nonce| nonce.strip_prefix("r="))
            .filter(|nonce| {
                !nonce.is_empty()
                    && nonce.bytes().all(|byte| (0x21..=0x7e).contains(&byte) && byte != b',')
            })
            .ok_or_else(|| malformed("the client-first-message has no valid nonce"))?;
        let nonce = format!("{client_nonce}{}", self.server_nonce);
        let server_first = format!(
            "r={nonce},s={},i={}",
            base64_encode(&self.verifier.salt),
            self.verifier.iterations
        );
        self.state = State::AwaitingClientFinal {
            client_first_bare: client_first_bare.to_owned(),
            server_first: server_first.clone(),
            nonce,
        };
        Ok(server_first)
    }

    /// Reads the client-final-message and, when its proof matches, returns the
    /// server-final-message.
    pub fn client_final(&mut self, message: &[u8]) -> Result<String, ScramFailure> {
        let State::AwaitingClientFinal {
            client_first_bare,
            server_first,
            nonce,
        } = std::mem::replace(&mut self.state, State::Done)
        else {
            return Err(malformed("unexpected SCRAM message"));
        };
        let text = std::str::from_utf8(message).map_err(|_| malformed("the SCRAM message is not UTF-8"))?;
        let (without_proof, proof) = text
            .rsplit_once(",p=")
            .ok_or_else(|| malformed("the client-final-message has no proof"))?;
        let mut attributes = without_proof.split(',');
        let binding = attributes
            .next()
            .and_then(|binding| binding.strip_prefix("c="))
            .ok_or_else(|| malformed("the client-final-message has no channel binding"))?;
        if binding != "biws" && binding != "eSws" {
            return Err(malformed("the channel binding does not match the client-first-message"));
        }
        let final_nonce = attributes
            .next()
            .and_then(|nonce| nonce.strip_prefix("r="))
            .ok_or_else(|| malformed("the client-final-message has no nonce"))?;
        if !constant_time_eq(final_nonce.as_bytes(), nonce.as_bytes()) {
            return Err(malformed("the nonce does not match"));
        }
        let proof = base64_decode(proof.as_bytes()).map_err(|_| malformed("the proof is not valid base64"))?;
        if proof.len() != SHA1_DIGEST_BYTES {
            return Err(malformed("the proof has the wrong length"));
        }
        let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
        let client_signature = hmac::<Sha1>(&self.verifier.stored_key, auth_message.as_bytes());
        let mut client_key = [0u8; SHA1_DIGEST_BYTES];
        for ((key, proof), signature) in client_key.iter_mut().zip(&proof).zip(client_signature) {
            *key = proof ^ signature;
        }
        if !constant_time_eq(&sha1(&client_key), &self.verifier.stored_key) {
            return Err(ScramFailure::WrongProof);
        }
        let server_signature = hmac::<Sha1>(&self.verifier.server_key, auth_message.as_bytes());
        Ok(format!("v={}", base64_encode(&server_signature)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_proof(salted: &[u8; 20], auth_message: &str) -> String {
        let client_key = hmac::<Sha1>(salted, b"Client Key");
        let signature = hmac::<Sha1>(&sha1(&client_key), auth_message.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(signature).map(|(a, b)| a ^ b).collect();
        base64_encode(&proof)
    }

    #[test]
    fn a_mongodb_password_completes_the_exchange_and_a_wrong_one_fails() {
        let salt = b"0123456789abcdef";
        let verifier = ScramSha1Verifier::for_mongodb("eggdb", "secret", salt, 4096);
        for (password, accepted) in [("secret", true), ("wrong", false)] {
            let mut exchange = ScramSha1Exchange::new(verifier.clone(), b"server-nonce");
            let server_first = exchange.client_first(b"n,,n=eggdb,r=client").unwrap();
            let nonce = server_first.split(',').next().unwrap().trim_start_matches("r=");
            let without_proof = format!("c=biws,r={nonce}");
            let auth_message = format!("n=eggdb,r=client,{server_first},{without_proof}");
            let digest = hex(&md5_checksum(format!("eggdb:mongo:{password}").as_bytes()));
            let salted = pbkdf2_hmac_sha1(digest.as_bytes(), salt, 4096);
            let message = format!("{without_proof},p={}", client_proof(&salted, &auth_message));
            assert_eq!(exchange.client_final(message.as_bytes()).is_ok(), accepted);
        }
    }
}
