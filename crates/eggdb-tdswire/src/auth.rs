use eggdb_crypto::{constant_time_eq, pbkdf2_hmac_sha256};

/// The PBKDF2-HMAC-SHA-256 iterations `eggdb serve` derives a login's digest with, as
/// PostgreSQL's SCRAM default and RFC 7677's minimum (D103).
pub const DEFAULT_ITERATIONS: u32 = 4096;

const SALT_BYTES: usize = 16;
const DIGEST_BYTES: usize = 32;

/// What a server keeps to check one SQL login's password, never the password itself: a random
/// salt and `PBKDF2-HMAC-SHA-256(password, salt, iterations)`. A TDS client sends its password
/// only obfuscated, so the server recomputes the digest from it and compares in constant
/// time (D122).
#[derive(Clone)]
pub struct TdsCredentials {
    salt: [u8; SALT_BYTES],
    digest: [u8; DIGEST_BYTES],
    iterations: u32,
    empty: bool,
}

impl std::fmt::Debug for TdsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TdsCredentials")
            .field("iterations", &self.iterations)
            .finish_non_exhaustive()
    }
}

fn digest_of(password: &[u8], salt: &[u8; SALT_BYTES], iterations: u32) -> [u8; DIGEST_BYTES] {
    let mut digest = [0u8; DIGEST_BYTES];
    pbkdf2_hmac_sha256(password, salt, iterations.max(1), &mut digest);
    digest
}

impl TdsCredentials {
    /// The credentials for `password` with a fresh random salt from the operating system.
    pub fn derive(password: &[u8], iterations: u32) -> std::io::Result<Self> {
        let mut salt = [0u8; SALT_BYTES];
        eggdb_platform::secure_random::fill(&mut salt)?;
        Ok(Self::derive_with_salt(password, salt, iterations))
    }

    /// The credentials for `password` with the given `salt`, for tests and known answers.
    pub fn derive_with_salt(password: &[u8], salt: [u8; SALT_BYTES], iterations: u32) -> Self {
        Self {
            digest: digest_of(password, &salt, iterations),
            salt,
            iterations: iterations.max(1),
            empty: password.is_empty(),
        }
    }

    /// Credentials no password matches, for a user that does not exist, checked with the same
    /// work as a real user's so both fail alike.
    pub fn matching_nothing(secret: &[u8; 32], iterations: u32) -> Self {
        let mut salt = [0u8; SALT_BYTES];
        salt.copy_from_slice(&secret[..SALT_BYTES]);
        let mut digest = [0u8; DIGEST_BYTES];
        digest.copy_from_slice(secret);
        Self {
            salt,
            digest,
            iterations: iterations.max(1),
            empty: false,
        }
    }

    /// Whether `password` is the one these credentials were derived from.
    pub fn verify(&self, password: &[u8]) -> bool {
        let digest = digest_of(password, &self.salt, self.iterations);
        constant_time_eq(&digest, &self.digest)
    }

    /// Whether these are the credentials of an empty password, which a server refuses to serve.
    pub fn is_for_empty_password(&self) -> bool {
        self.empty
    }

    /// The PBKDF2 iterations a check costs.
    pub fn iterations(&self) -> u32 {
        self.iterations
    }
}

#[cfg(test)]
mod tests {
    use super::TdsCredentials;

    #[test]
    fn only_the_password_the_credentials_came_from_matches() {
        let credentials = TdsCredentials::derive_with_salt(b"secret", [7; 16], 2);
        assert!(credentials.verify(b"secret"));
        assert!(!credentials.verify(b"Secret"));
        assert!(!credentials.verify(b""));
        assert!(!credentials.is_for_empty_password());
        let nothing = TdsCredentials::matching_nothing(&[9; 32], 2);
        assert!(!nothing.verify(b"secret"));
        assert!(!nothing.verify(&[]));
        assert!(TdsCredentials::derive_with_salt(b"", [0; 16], 1).is_for_empty_password());
        assert!(!format!("{credentials:?}").contains("secret"));
    }
}
