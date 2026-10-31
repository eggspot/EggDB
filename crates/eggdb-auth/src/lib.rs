//! External authentication for EggDB's servers, written from the public specifications:
//! JSON Web Tokens (RFC 7519) signed as JSON Web Signatures (RFC 7515) with keys from a JSON
//! Web Key Set (RFC 7517), as OpenID Connect providers issue them; LDAP simple binds (RFC 4511,
//! with distinguished names escaped as RFC 4514 says); and a rule file that picks the method for
//! each user. SCRAM-SHA-256 (RFC 5802 and RFC 7677) lives here too, so every wire protocol
//! that speaks SASL shares one implementation, and AWS Signature Version 4 lives here for the
//! AWS-compatible endpoints.

mod ber;
mod json;
pub mod jwt;
pub mod ldap;
pub mod rules;
pub mod scram;
pub mod sigv4;

pub use jwt::{JwtError, JwtKeys, JwtPolicy};
pub use ldap::{LdapConfig, LdapError};
pub use rules::{AuthMethod, AuthRules, RulesError};
