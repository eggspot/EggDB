//! EggDB's own TLS, written from the RFCs on [`eggdb_crypto`] with no third-party code (D1,
//! D23, D35), so every protocol EggDB serves can be reached safely beyond loopback. Built so
//! far:
//! - [`der`]: a strict DER reader and a DER writer (ITU-T X.690);
//! - [`oid`]: the object identifiers of certificates and keys;
//! - [`pem`]: PEM blocks (RFC 7468).
//!
//! Nothing here panics on its input: every parser returns an error as a value.

pub mod der;
pub mod oid;
pub mod pem;
