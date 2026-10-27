use eggdb_crypto::{constant_time_eq, sha1, sha256};

/// The length of the nonce a server sends in its handshake, which both authentication
/// methods hash with the password.
pub const NONCE_BYTES: usize = 20;
/// The name of MySQL's default authentication method since 8.0, the one EggDB offers first.
pub const CACHING_SHA2_PASSWORD: &str = "caching_sha2_password";
/// The name of MySQL's older authentication method, which MariaDB clients still use first.
pub const MYSQL_NATIVE_PASSWORD: &str = "mysql_native_password";

fn xor<const N: usize>(left: [u8; N], right: [u8; N]) -> [u8; N] {
    let mut out = [0u8; N];
    for (out, (left, right)) in out.iter_mut().zip(left.iter().zip(right)) {
        *out = left ^ right;
    }
    out
}

fn sha256_of_two(first: &[u8], second: &[u8]) -> [u8; 32] {
    let mut hash = eggdb_crypto::Sha256::new();
    hash.update(first);
    hash.update(second);
    hash.finish()
}

fn sha1_of_two(first: &[u8], second: &[u8]) -> [u8; 20] {
    let mut hash = eggdb_crypto::Sha1::new();
    hash.update(first);
    hash.update(second);
    hash.finish()
}

/// What a server keeps to check one user's password with either MySQL method, never the
/// password itself: `SHA256(SHA256(password))` for `caching_sha2_password` and
/// `SHA1(SHA1(password))` for `mysql_native_password`. Either lets the server check a client's
/// proof, but not compute one, so a copy of it does not let anyone log in (D106).
#[derive(Clone)]
pub struct MySqlCredentials {
    sha256_twice: [u8; 32],
    sha1_twice: [u8; 20],
    empty: bool,
}

impl std::fmt::Debug for MySqlCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MySqlCredentials").finish_non_exhaustive()
    }
}

impl MySqlCredentials {
    /// The credentials for `password`.
    pub fn derive(password: &[u8]) -> Self {
        Self {
            sha256_twice: sha256(&sha256(password)),
            sha1_twice: sha1(&sha1(password)),
            empty: password.is_empty(),
        }
    }

    /// Credentials no response matches, for a user that does not exist, so that the
    /// exchange looks the same as for a real user with a wrong password.
    pub fn matching_nothing(secret: &[u8; 32]) -> Self {
        Self {
            sha256_twice: sha256(secret),
            sha1_twice: sha1(secret),
            empty: false,
        }
    }

    /// Whether these are the credentials of an empty password, which a server refuses to serve.
    pub fn is_for_empty_password(&self) -> bool {
        self.empty
    }

    /// `SHA256(SHA256(password))`, as the `caching_sha2_password` known-answer vectors list it.
    pub fn caching_sha2_digest(&self) -> [u8; 32] {
        self.sha256_twice
    }

    /// `SHA1(SHA1(password))`, as the `mysql_native_password` known-answer vectors list it.
    pub fn native_digest(&self) -> [u8; 20] {
        self.sha1_twice
    }

    /// Whether `response` proves the password for `nonce` by `caching_sha2_password`'s fast
    /// path: `SHA256(password) XOR SHA256(SHA256(SHA256(password)) || nonce)`. An empty
    /// response matches only an empty password.
    pub fn check_caching_sha2(&self, nonce: &[u8; NONCE_BYTES], response: &[u8]) -> bool {
        if response.is_empty() {
            return self.empty;
        }
        let Ok(response) = <[u8; 32]>::try_from(response) else {
            return false;
        };
        let mask = sha256_of_two(&self.sha256_twice, nonce);
        let claimed_sha256 = xor(response, mask);
        constant_time_eq(&sha256(&claimed_sha256), &self.sha256_twice)
    }

    /// Whether `response` proves the password for `nonce` by `mysql_native_password`:
    /// `SHA1(password) XOR SHA1(nonce || SHA1(SHA1(password)))`. An empty response matches
    /// only an empty password.
    pub fn check_native(&self, nonce: &[u8; NONCE_BYTES], response: &[u8]) -> bool {
        if response.is_empty() {
            return self.empty;
        }
        let Ok(response) = <[u8; 20]>::try_from(response) else {
            return false;
        };
        let mask = sha1_of_two(nonce, &self.sha1_twice);
        let claimed_sha1 = xor(response, mask);
        constant_time_eq(&sha1(&claimed_sha1), &self.sha1_twice)
    }
}

/// The `caching_sha2_password` response a client sends for `password` and `nonce`; empty for
/// an empty password.
pub fn caching_sha2_response(password: &[u8], nonce: &[u8; NONCE_BYTES]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let once = sha256(password);
    let mask = sha256_of_two(&sha256(&once), nonce);
    xor(once, mask).to_vec()
}

/// The `mysql_native_password` response a client sends for `password` and `nonce`; empty for
/// an empty password.
pub fn native_response(password: &[u8], nonce: &[u8; NONCE_BYTES]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let once = sha1(password);
    let mask = sha1_of_two(nonce, &sha1(&once));
    xor(once, mask).to_vec()
}

#[cfg(test)]
mod tests {
    use super::{MySqlCredentials, NONCE_BYTES, caching_sha2_response, native_response};

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn nonce() -> [u8; NONCE_BYTES] {
        std::array::from_fn(|index| index as u8 + 1)
    }

    #[test]
    fn the_known_answer_vectors_match() {
        let credentials = MySqlCredentials::derive(b"secret");
        assert_eq!(
            hex(&credentials.native_digest()),
            "14e65567abdb5135d0cfd9a70b3032c179a49ee7"
        );
        assert_eq!(
            hex(&native_response(b"secret", &nonce())),
            "b32bb3a583e1340c0a1108d58b1be49781ad8c2f"
        );
        assert_eq!(
            hex(&credentials.caching_sha2_digest()),
            "3881219d087dd9c634373fd33dfa33a2cb6bfc6c520b64b8bb60ef2ceb534ae7"
        );
        assert_eq!(
            hex(&caching_sha2_response(b"secret", &nonce())),
            "746ebe205d56a0707acb3e796e834e0dd7b1d61743b26bd5202c7a623230c7c9"
        );
    }

    #[test]
    fn only_the_right_password_is_accepted() {
        let credentials = MySqlCredentials::derive(b"secret");
        let nonce = nonce();
        assert!(credentials.check_caching_sha2(&nonce, &caching_sha2_response(b"secret", &nonce)));
        assert!(credentials.check_native(&nonce, &native_response(b"secret", &nonce)));
        assert!(!credentials.check_caching_sha2(&nonce, &caching_sha2_response(b"Secret", &nonce)));
        assert!(!credentials.check_native(&nonce, &native_response(b"secret ", &nonce)));
        let other_nonce = [9u8; NONCE_BYTES];
        assert!(!credentials.check_native(&other_nonce, &native_response(b"secret", &nonce)));
        assert!(!credentials.check_caching_sha2(&nonce, &[]));
        assert!(!credentials.check_native(&nonce, &[0; 19]));
        assert!(!credentials.check_caching_sha2(&nonce, &[0; 33]));
        let empty = MySqlCredentials::derive(b"");
        assert!(empty.check_caching_sha2(&nonce, &[]));
        assert!(empty.check_native(&nonce, &[]));
        let nobody = MySqlCredentials::matching_nothing(&[7; 32]);
        assert!(!nobody.check_caching_sha2(&nonce, &[]));
    }
}
