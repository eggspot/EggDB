//! EggDB's cryptography, written from the public standards and checked against their published
//! test vectors (D23, D35):
//! - [`Sha256`] and [`sha256`]: SHA-256 (FIPS 180-4);
//! - [`Sha512`], [`sha512`], [`Sha384`] and [`sha384`]: SHA-512 and SHA-384 (FIPS 180-4);
//! - [`Sha1`] and [`sha1`]: SHA-1 (FIPS 180-4), only for protocols that require it, such as
//!   MySQL's `mysql_native_password`;
//! - [`HashFunction`]: what every hash above offers, so the keyed functions are written once;
//! - [`Hmac`], [`hmac`] and [`hmac_sha256`]: HMAC over any of the hashes (RFC 2104, vectors from
//!   RFC 4231);
//! - [`hkdf_extract`] and [`hkdf_expand`]: HKDF (RFC 5869), the key derivation of TLS 1.3;
//! - [`ChaCha20`], [`Poly1305`] and the AEAD [`ChaCha20Poly1305`] (RFC 8439);
//! - [`Aes`] (FIPS 197) and the AEAD [`AesGcm`] (NIST SP 800-38D), with a computed S-box and a
//!   masked GHASH, so neither looks up memory by a secret;
//! - [`x25519`], [`x25519_public_key`] and [`x25519_shared_secret`]: the X25519 key exchange
//!   (RFC 7748), by a constant-time Montgomery ladder;
//! - [`pbkdf2_hmac_sha256`]: PBKDF2 with HMAC-SHA-256 (RFC 8018, vectors from RFC 7914);
//! - [`base64_encode`] and [`base64_decode`]: Base64 with padding, the standard alphabet
//!   (RFC 4648), decoded strictly;
//! - [`constant_time_eq`]: comparing secrets in time that depends only on their lengths.
//!
//! It has no dependencies and never panics on its input.

mod aead;
mod aes;
mod base64;
mod chacha20;
mod constant_time;
mod gcm;
mod hash;
mod hkdf;
mod hmac;
mod pbkdf2;
mod poly1305;
mod sha1;
mod sha256;
mod sha512;
mod x25519;

pub use aead::{AEAD_NONCE_BYTES, AEAD_TAG_BYTES, AeadError, ChaCha20Poly1305};
pub use aes::{AES_BLOCK_BYTES, Aes, AesKeyLengthError};
pub use base64::{Base64Error, base64_decode, base64_encode};
pub use chacha20::{CHACHA20_BLOCK_BYTES, CHACHA20_KEY_BYTES, CHACHA20_NONCE_BYTES, ChaCha20};
pub use constant_time::constant_time_eq;
pub use gcm::AesGcm;
pub use hash::{HashFunction, MAX_HASH_BLOCK_BYTES, MAX_HASH_DIGEST_BYTES};
pub use hkdf::{HkdfError, hkdf_expand, hkdf_extract};
pub use hmac::{Hmac, HmacSha256, hmac, hmac_sha256};
pub use pbkdf2::pbkdf2_hmac_sha256;
pub use poly1305::{POLY1305_KEY_BYTES, POLY1305_TAG_BYTES, Poly1305, poly1305};
pub use sha1::{SHA1_BLOCK_BYTES, SHA1_DIGEST_BYTES, Sha1, sha1};
pub use sha256::{SHA256_BLOCK_BYTES, SHA256_DIGEST_BYTES, Sha256, sha256};
pub use sha512::{
    SHA384_DIGEST_BYTES, SHA512_BLOCK_BYTES, SHA512_DIGEST_BYTES, Sha384, Sha512, sha384, sha512,
};
pub use x25519::{
    X25519_KEY_BYTES, X25519LowOrderPoint, x25519, x25519_public_key, x25519_shared_secret,
};
