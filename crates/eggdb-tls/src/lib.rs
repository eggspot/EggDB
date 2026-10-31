//! EggDB's own TLS, written from the RFCs on [`eggdb_crypto`] with no third-party code (D1,
//! D23, D35), so every protocol EggDB serves can be reached safely beyond loopback. Built so
//! far:
//! - [`der`]: a strict DER reader and a DER writer (ITU-T X.690);
//! - [`oid`]: the object identifiers of certificates and keys;
//! - [`pem`]: PEM blocks (RFC 7468);
//! - [`signature`]: signature algorithms, public keys and checking a signature (RFC 4055,
//!   RFC 5758, RFC 8017);
//! - [`x509`]: X.509 certificates (RFC 5280);
//! - [`private_key`]: P-256 and RSA private keys from PKCS#8, SEC 1 and PKCS#1, and signing;
//! - [`self_signed`]: self-signed P-256 server certificates;
//! - [`verify`]: chains to trust anchors and server-name matching (RFC 5280, RFC 6125);
//! - [`codec`]: TLS's presentation language;
//! - [`alert`]: alerts;
//! - [`cipher_suite`]: the cipher suites, their hashes and their record AEADs;
//! - [`key_schedule`]: the TLS 1.3 key schedule (RFC 8446 section 7);
//! - [`record`]: the record layer and record protection for TLS 1.3 and TLS 1.2;
//! - [`handshake`]: the handshake messages and their extensions.
//!
//! Nothing here panics on its input: every parser returns an error as a value.

pub mod alert;
pub mod cipher_suite;
pub mod codec;
pub mod der;
pub mod handshake;
pub mod key_schedule;
pub mod oid;
pub mod pem;
pub mod private_key;
pub mod record;
pub mod self_signed;
pub mod signature;
pub mod verify;
pub mod x509;
mod x509_time;
