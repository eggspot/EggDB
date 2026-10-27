//! EggDB's cryptography, written from the public standards and checked against their published
//! test vectors (D23, D35):
//! - [`Sha256`] and [`sha256`]: SHA-256 (FIPS 180-4);
//! - [`hmac_sha256`]: HMAC-SHA-256 (RFC 2104, vectors from RFC 4231);
//! - [`pbkdf2_hmac_sha256`]: PBKDF2 with HMAC-SHA-256 (RFC 8018, vectors from RFC 7914);
//! - [`base64_encode`] and [`base64_decode`]: Base64 with padding, the standard alphabet
//!   (RFC 4648), decoded strictly;
//! - [`constant_time_eq`]: comparing secrets in time that depends only on their lengths.
//!
//! It has no dependencies and never panics on its input.

mod base64;
mod constant_time;
mod hmac;
mod pbkdf2;
mod sha256;

pub use base64::{Base64Error, base64_decode, base64_encode};
pub use constant_time::constant_time_eq;
pub use hmac::{HmacSha256, hmac_sha256};
pub use pbkdf2::pbkdf2_hmac_sha256;
pub use sha256::{SHA256_BLOCK_BYTES, SHA256_DIGEST_BYTES, Sha256, sha256};
