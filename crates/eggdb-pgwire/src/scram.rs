use eggdb_crypto::{
    SHA256_DIGEST_BYTES, base64_decode, base64_encode, constant_time_eq, hmac_sha256,
    pbkdf2_hmac_sha256, sha256,
};
use std::fmt;

/// The SASL mechanism this server always offers (RFC 7677).
pub const SCRAM_SHA_256: &str = "SCRAM-SHA-256";

/// SCRAM-SHA-256 bound to the TLS connection with `tls-server-end-point` (RFC 5929), offered on
/// TLS connections.
pub const SCRAM_SHA_256_PLUS: &str = "SCRAM-SHA-256-PLUS";

const TLS_SERVER_END_POINT_FLAG: &str = "p=tls-server-end-point";

/// The fewest PBKDF2 iterations a verifier may use: RFC 7677's minimum and PostgreSQL's default.
pub const MIN_SCRAM_ITERATIONS: u32 = 4096;

/// The salt length of the verifiers this server derives, in bytes.
pub const SCRAM_SALT_BYTES: usize = 16;

/// A user's SCRAM-SHA-256 verifier (RFC 5802, section 3): the salt, the iteration count, and the
/// stored and server keys derived from the password. The password itself is not kept.
#[derive(Clone, PartialEq, Eq)]
pub struct ScramVerifier {
    salt: Vec<u8>,
    iterations: u32,
    stored_key: [u8; SHA256_DIGEST_BYTES],
    server_key: [u8; SHA256_DIGEST_BYTES],
}

impl fmt::Debug for ScramVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScramVerifier")
            .field("iterations", &self.iterations)
            .finish_non_exhaustive()
    }
}

impl ScramVerifier {
    /// Derives the verifier of `password` with `salt` over `iterations` rounds of PBKDF2, at
    /// least [`MIN_SCRAM_ITERATIONS`].
    pub fn derive(password: &[u8], salt: &[u8], iterations: u32) -> Self {
        let iterations = iterations.max(MIN_SCRAM_ITERATIONS);
        let mut salted = [0u8; SHA256_DIGEST_BYTES];
        pbkdf2_hmac_sha256(password, salt, iterations, &mut salted);
        let client_key = hmac_sha256(&salted, b"Client Key");
        Self {
            salt: salt.to_vec(),
            iterations,
            stored_key: sha256(&client_key),
            server_key: hmac_sha256(&salted, b"Server Key"),
        }
    }

    /// The PBKDF2 iteration count.
    pub fn iterations(&self) -> u32 {
        self.iterations
    }

    fn unmatchable(salt: &[u8], iterations: u32, stored_key: [u8; SHA256_DIGEST_BYTES]) -> Self {
        Self {
            salt: salt.to_vec(),
            iterations: iterations.max(MIN_SCRAM_ITERATIONS),
            stored_key,
            server_key: stored_key,
        }
    }

    /// The verifier a stored role keeps, written as PostgreSQL writes `pg_authid.rolpassword`
    /// (`SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`), or `None` when the text
    /// is not one or asks for fewer than [`MIN_SCRAM_ITERATIONS`].
    pub fn from_stored(text: &str) -> Option<Self> {
        let (iterations, salt, stored_key, server_key) = eggdb_sql::parse_scram_verifier(text)?;
        (iterations >= MIN_SCRAM_ITERATIONS).then_some(Self {
            salt,
            iterations,
            stored_key,
            server_key,
        })
    }

    /// A verifier no password matches, for a user that does not exist, so the exchange looks
    /// the same as for a real user (a salt derived from `secret` and the name) and fails at
    /// its end.
    pub fn for_unknown_user(secret: &[u8], user: &str, iterations: u32) -> Self {
        let mut message = b"salt ".to_vec();
        message.extend_from_slice(user.as_bytes());
        let salt = hmac_sha256(secret, &message);
        message[..5].copy_from_slice(b"key  ");
        let stored_key = hmac_sha256(secret, &message);
        Self::unmatchable(&salt[..SCRAM_SALT_BYTES], iterations, stored_key)
    }
}

/// Why a SCRAM exchange failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScramFailure {
    /// The client's message breaks RFC 5802's syntax or this exchange's rules, with what was
    /// wrong.
    Malformed(String),
    /// The client asked for something this server does not do, such as channel binding or an
    /// authorization identity.
    Unsupported(String),
    /// The proof does not match the verifier: a wrong password or an unknown user.
    WrongProof,
}

/// The server's side of one SCRAM-SHA-256 exchange (RFC 5802, RFC 7677), as PostgreSQL runs it
/// in its `AuthenticationSASL` messages.
#[derive(Debug)]
pub struct ScramExchange {
    verifier: ScramVerifier,
    server_nonce: String,
    state: ExchangeState,
    channel_binding: Option<Vec<u8>>,
    plus_selected: bool,
}

#[derive(Debug)]
enum ExchangeState {
    AwaitingClientFirst,
    AwaitingClientFinal {
        gs2_header: String,
        client_first_bare: String,
        server_first: String,
        nonce: String,
    },
    Done,
}

fn is_printable_nonce(nonce: &str) -> bool {
    !nonce.is_empty()
        && nonce
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b',')
}

impl ScramExchange {
    /// Starts an exchange against `verifier`; `server_nonce_bytes` are fresh random bytes from
    /// the operating system.
    pub fn new(verifier: ScramVerifier, server_nonce_bytes: &[u8]) -> Self {
        Self {
            verifier,
            server_nonce: base64_encode(server_nonce_bytes),
            state: ExchangeState::AwaitingClientFirst,
            channel_binding: None,
            plus_selected: false,
        }
    }

    /// An exchange on a TLS connection whose `tls-server-end-point` binding data is `binding`:
    /// [`SCRAM_SHA_256_PLUS`] is offered too.
    pub fn with_channel_binding(mut self, binding: Vec<u8>) -> Self {
        self.channel_binding = Some(binding);
        self
    }

    /// The mechanisms to offer, strongest first.
    pub fn mechanisms(&self) -> &'static [&'static str] {
        if self.channel_binding.is_some() {
            &[SCRAM_SHA_256_PLUS, SCRAM_SHA_256]
        } else {
            &[SCRAM_SHA_256]
        }
    }

    /// Accepts the client's choice of `mechanism`, or refuses one this exchange does not offer.
    pub fn select_mechanism(&mut self, mechanism: &str) -> Result<(), ScramFailure> {
        if !self.mechanisms().contains(&mechanism) {
            return Err(ScramFailure::Malformed(
                "client selected an invalid SASL authentication mechanism".into(),
            ));
        }
        self.plus_selected = mechanism == SCRAM_SHA_256_PLUS;
        Ok(())
    }

    fn check_gs2_flag(&self, flag: &str) -> Result<(), ScramFailure> {
        let negotiation_error =
            || ScramFailure::Malformed("SCRAM channel binding negotiation error".into());
        match flag {
            "n" if !self.plus_selected => Ok(()),
            "y" if !self.plus_selected && self.channel_binding.is_none() => Ok(()),
            TLS_SERVER_END_POINT_FLAG if self.plus_selected => Ok(()),
            _ if flag.starts_with("p=") && self.plus_selected => Err(ScramFailure::Unsupported(
                "only the tls-server-end-point channel binding is supported".into(),
            )),
            "n" | "y" => Err(negotiation_error()),
            _ if flag.starts_with("p=") => Err(negotiation_error()),
            _ => Err(ScramFailure::Malformed("the GS2 header is malformed".into())),
        }
    }

    fn expected_binding(&self, gs2_header: &str) -> Vec<u8> {
        let mut expected = gs2_header.as_bytes().to_vec();
        if gs2_header.starts_with("p=")
            && let Some(binding) = &self.channel_binding
        {
            expected.extend_from_slice(binding);
        }
        expected
    }

    /// Reads the client-first-message and returns the server-first-message.
    pub fn client_first(&mut self, message: &[u8]) -> Result<String, ScramFailure> {
        if !matches!(self.state, ExchangeState::AwaitingClientFirst) {
            return Err(ScramFailure::Malformed("unexpected SCRAM message".into()));
        }
        self.state = ExchangeState::Done;
        let text = std::str::from_utf8(message)
            .map_err(|_| ScramFailure::Malformed("the SCRAM message is not UTF-8".into()))?;
        let (gs2_header, client_first_bare) = split_gs2_header(text)?;
        self.check_gs2_flag(gs2_header.split(',').next().unwrap_or_default())?;
        let mut attributes = client_first_bare.split(',');
        match attributes.next() {
            Some(user) if user.starts_with("n=") => {}
            Some(reserved) if reserved.starts_with("m=") => {
                return Err(ScramFailure::Unsupported(
                    "SCRAM extensions are not supported".into(),
                ));
            }
            _ => {
                return Err(ScramFailure::Malformed(
                    "the client-first-message has no user name attribute".into(),
                ));
            }
        }
        let client_nonce = attributes
            .next()
            .and_then(|nonce| nonce.strip_prefix("r="))
            .filter(|nonce| is_printable_nonce(nonce))
            .ok_or_else(|| {
                ScramFailure::Malformed("the client-first-message has no valid nonce".into())
            })?;
        let nonce = format!("{client_nonce}{}", self.server_nonce);
        let server_first = format!(
            "r={nonce},s={},i={}",
            base64_encode(&self.verifier.salt),
            self.verifier.iterations
        );
        self.state = ExchangeState::AwaitingClientFinal {
            gs2_header: gs2_header.to_owned(),
            client_first_bare: client_first_bare.to_owned(),
            server_first: server_first.clone(),
            nonce,
        };
        Ok(server_first)
    }

    /// Reads the client-final-message and, when its proof matches, returns the
    /// server-final-message.
    pub fn client_final(&mut self, message: &[u8]) -> Result<String, ScramFailure> {
        let ExchangeState::AwaitingClientFinal {
            gs2_header,
            client_first_bare,
            server_first,
            nonce,
        } = std::mem::replace(&mut self.state, ExchangeState::Done)
        else {
            return Err(ScramFailure::Malformed("unexpected SCRAM message".into()));
        };
        let text = std::str::from_utf8(message)
            .map_err(|_| ScramFailure::Malformed("the SCRAM message is not UTF-8".into()))?;
        let (without_proof, proof) = text.rsplit_once(",p=").ok_or_else(|| {
            ScramFailure::Malformed("the client-final-message has no proof".into())
        })?;
        let mut attributes = without_proof.split(',');
        let binding = attributes
            .next()
            .and_then(|binding| binding.strip_prefix("c="))
            .ok_or_else(|| {
                ScramFailure::Malformed("the client-final-message has no channel binding".into())
            })?;
        let expected_binding = base64_encode(&self.expected_binding(&gs2_header));
        if !constant_time_eq(binding.as_bytes(), expected_binding.as_bytes()) {
            return Err(ScramFailure::Malformed(
                "the channel binding does not match the client-first-message".into(),
            ));
        }
        let final_nonce = attributes
            .next()
            .and_then(|nonce| nonce.strip_prefix("r="))
            .ok_or_else(|| {
                ScramFailure::Malformed("the client-final-message has no nonce".into())
            })?;
        if !constant_time_eq(final_nonce.as_bytes(), nonce.as_bytes()) {
            return Err(ScramFailure::Malformed("the nonce does not match".into()));
        }
        let proof = base64_decode(proof.as_bytes())
            .map_err(|_| ScramFailure::Malformed("the proof is not valid base64".into()))?;
        if proof.len() != SHA256_DIGEST_BYTES {
            return Err(ScramFailure::Malformed(
                "the proof has the wrong length".into(),
            ));
        }
        let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
        let client_signature = hmac_sha256(&self.verifier.stored_key, auth_message.as_bytes());
        let mut client_key = [0u8; SHA256_DIGEST_BYTES];
        for ((key, proof), signature) in client_key.iter_mut().zip(&proof).zip(client_signature) {
            *key = proof ^ signature;
        }
        if !constant_time_eq(&sha256(&client_key), &self.verifier.stored_key) {
            return Err(ScramFailure::WrongProof);
        }
        let server_signature = hmac_sha256(&self.verifier.server_key, auth_message.as_bytes());
        Ok(format!("v={}", base64_encode(&server_signature)))
    }
}

fn split_gs2_header(text: &str) -> Result<(&str, &str), ScramFailure> {
    let malformed = || ScramFailure::Malformed("the GS2 header is malformed".into());
    let first_comma = text.find(',').ok_or_else(malformed)?;
    let flag = &text[..first_comma];
    if !matches!(flag, "n" | "y") && !flag.starts_with("p=") {
        return Err(malformed());
    }
    let rest = &text[first_comma + 1..];
    let second_comma = rest.find(',').ok_or_else(malformed)?;
    if second_comma != 0 {
        return Err(ScramFailure::Unsupported(
            "an authorization identity is not supported".into(),
        ));
    }
    let header_length = first_comma + 2;
    Ok((&text[..header_length], &text[header_length..]))
}

#[cfg(test)]
mod tests {
    use super::{ScramExchange, ScramFailure, ScramVerifier};
    use eggdb_crypto::base64_decode;

    const CLIENT_FIRST: &str = "n,,n=user,r=rOprNGfwEbeRWgbNEkqO";
    const SERVER_FIRST: &str =
        "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
    const CLIENT_FINAL: &str = "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
    const SERVER_FINAL: &str = "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=";

    fn rfc_7677_exchange(password: &[u8]) -> ScramExchange {
        let salt = base64_decode(b"W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let verifier = ScramVerifier::derive(password, &salt, 4096);
        let mut exchange = ScramExchange::new(verifier, &[]);
        exchange.server_nonce = "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0".to_owned();
        exchange
    }

    #[test]
    fn the_rfc_7677_example_exchange_succeeds() {
        let mut exchange = rfc_7677_exchange(b"pencil");
        assert_eq!(
            exchange.client_first(CLIENT_FIRST.as_bytes()).unwrap(),
            SERVER_FIRST
        );
        assert_eq!(
            exchange.client_final(CLIENT_FINAL.as_bytes()).unwrap(),
            SERVER_FINAL
        );
    }

    #[test]
    fn a_wrong_password_fails_the_proof() {
        let mut exchange = rfc_7677_exchange(b"pencils");
        exchange.client_first(CLIENT_FIRST.as_bytes()).unwrap();
        assert_eq!(
            exchange.client_final(CLIENT_FINAL.as_bytes()),
            Err(ScramFailure::WrongProof)
        );
    }

    #[test]
    fn an_unknown_user_fails_the_proof_with_a_stable_salt() {
        let first = ScramVerifier::for_unknown_user(b"secret", "mallory", 4096);
        let second = ScramVerifier::for_unknown_user(b"secret", "mallory", 4096);
        assert_eq!(first, second);
        assert_eq!(first.salt.len(), super::SCRAM_SALT_BYTES);
        let mut exchange = ScramExchange::new(first, b"nonce bytes");
        exchange.client_first(b"n,,n=,r=abc").unwrap();
        let error = exchange.client_final(
            b"c=biws,r=abcbm9uY2UgYnl0ZXM=,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=",
        );
        assert_eq!(error, Err(ScramFailure::WrongProof));
    }

    #[test]
    fn malformed_and_unsupported_messages_are_refused() {
        for message in [
            "",
            "x,,n=,r=abc",
            "n,,",
            "n,,n=",
            "n,,r=abc",
            "n,,n=,r=",
            "n",
            "\u{ff}",
        ] {
            let mut exchange = rfc_7677_exchange(b"pencil");
            assert!(
                matches!(
                    exchange.client_first(message.as_bytes()),
                    Err(ScramFailure::Malformed(_))
                ),
                "{message:?}"
            );
        }
        let mut exchange = rfc_7677_exchange(b"pencil");
        assert!(matches!(
            exchange.client_first(b"p=tls-server-end-point,,n=,r=abc"),
            Err(ScramFailure::Malformed(_))
        ));
        for message in [
            "n,a=admin,n=,r=abc",
            "n,,m=x,n=,r=abc",
        ] {
            let mut exchange = rfc_7677_exchange(b"pencil");
            assert!(
                matches!(
                    exchange.client_first(message.as_bytes()),
                    Err(ScramFailure::Unsupported(_))
                ),
                "{message:?}"
            );
        }
        for message in [
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0",
            "c=eSws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=",
            "c=biws,r=rOprNGfwEbeRWgbNEkqO,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=",
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=AAAA",
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=!!!!",
        ] {
            let mut exchange = rfc_7677_exchange(b"pencil");
            exchange.client_first(CLIENT_FIRST.as_bytes()).unwrap();
            assert!(
                matches!(
                    exchange.client_final(message.as_bytes()),
                    Err(ScramFailure::Malformed(_))
                ),
                "{message:?}"
            );
        }
        let mut exchange = rfc_7677_exchange(b"pencil");
        assert!(exchange.client_final(CLIENT_FINAL.as_bytes()).is_err());
    }

    #[test]
    fn iteration_counts_below_the_minimum_are_raised() {
        let verifier = ScramVerifier::derive(b"pw", b"salt", 1);
        assert_eq!(verifier.iterations, super::MIN_SCRAM_ITERATIONS);
    }

    #[test]
    fn channel_binding_flags_follow_the_selected_mechanism() {
        let plus = || rfc_7677_exchange(b"pencil").with_channel_binding(vec![7; 32]);
        let mut exchange = plus();
        assert_eq!(exchange.mechanisms(), &[super::SCRAM_SHA_256_PLUS, super::SCRAM_SHA_256]);
        exchange.select_mechanism(super::SCRAM_SHA_256_PLUS).unwrap();
        assert!(exchange.client_first(b"p=tls-server-end-point,,n=,r=abc").is_ok());

        let mut exchange = plus();
        exchange.select_mechanism(super::SCRAM_SHA_256_PLUS).unwrap();
        assert!(matches!(exchange.client_first(b"n,,n=,r=abc"), Err(ScramFailure::Malformed(_))));

        let mut exchange = plus();
        exchange.select_mechanism(super::SCRAM_SHA_256_PLUS).unwrap();
        assert!(matches!(
            exchange.client_first(b"p=tls-unique,,n=,r=abc"),
            Err(ScramFailure::Unsupported(_))
        ));

        let mut exchange = plus();
        exchange.select_mechanism(super::SCRAM_SHA_256).unwrap();
        assert!(matches!(exchange.client_first(b"y,,n=,r=abc"), Err(ScramFailure::Malformed(_))));

        let mut exchange = plus();
        exchange.select_mechanism(super::SCRAM_SHA_256).unwrap();
        assert!(exchange.client_first(b"n,,n=,r=abc").is_ok());

        let mut exchange = rfc_7677_exchange(b"pencil");
        assert!(exchange.select_mechanism(super::SCRAM_SHA_256_PLUS).is_err());
        assert!(exchange.client_first(b"y,,n=,r=abc").is_ok());
    }
}
