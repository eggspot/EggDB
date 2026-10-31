//! External authentication for EggDB's servers, written from the public specifications:
//! JSON Web Tokens (RFC 7519) signed as JSON Web Signatures (RFC 7515) with keys from a JSON
//! Web Key Set (RFC 7517), as OpenID Connect providers issue them; LDAP simple binds (RFC 4511,
//! with distinguished names escaped as RFC 4514 says); and a rule file that picks the method for
//! each user.

mod ber;
mod json;
pub mod jwt;
pub mod ldap;
pub mod rules;

pub use jwt::{JwtError, JwtKeys, JwtPolicy};
pub use ldap::{LdapConfig, LdapError};
pub use rules::{AuthMethod, AuthRules, RulesError};
