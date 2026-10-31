//! PEM, the textual encoding of certificates and keys (RFC 7468): `-----BEGIN LABEL-----`, the
//! DER in Base64, `-----END LABEL-----`. Text between blocks is ignored, as the RFC allows.

use eggdb_crypto::{Base64Error, base64_decode, base64_encode};

const BEGIN_PREFIX: &str = "-----BEGIN ";
const END_PREFIX: &str = "-----END ";
const BOUNDARY_SUFFIX: &str = "-----";
const LINE_LENGTH: usize = 64;

/// The label of a certificate block.
pub const CERTIFICATE_LABEL: &str = "CERTIFICATE";
/// The label of a PKCS#8 private key block.
pub const PRIVATE_KEY_LABEL: &str = "PRIVATE KEY";
/// The label of a SEC 1 elliptic-curve private key block.
pub const EC_PRIVATE_KEY_LABEL: &str = "EC PRIVATE KEY";
/// The label of a PKCS#1 RSA private key block.
pub const RSA_PRIVATE_KEY_LABEL: &str = "RSA PRIVATE KEY";
/// The label of an encrypted PKCS#8 private key block, which EggDB does not read.
pub const ENCRYPTED_PRIVATE_KEY_LABEL: &str = "ENCRYPTED PRIVATE KEY";

/// One PEM block: its label and the DER it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PemBlock {
    /// The label between `BEGIN` and the dashes, such as `CERTIFICATE`.
    pub label: String,
    /// The decoded contents.
    pub contents: Vec<u8>,
}

/// Why PEM text was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PemError {
    /// A `BEGIN` line with no matching `END` line.
    MissingEnd {
        /// The label of the unfinished block.
        label: String,
    },
    /// Header lines inside the block (`Proc-Type:`, `DEK-Info:`), which mean a key encrypted
    /// the legacy OpenSSL way.
    EncryptedHeaders,
    /// The block's text is not valid Base64.
    Base64(Base64Error),
}

impl std::fmt::Display for PemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingEnd { label } => write!(f, "the PEM block {label} has no END line"),
            Self::EncryptedHeaders => f.write_str(
                "the PEM block has headers: encrypted keys are not supported, decrypt it first",
            ),
            Self::Base64(error) => write!(f, "the PEM block is not valid Base64: {error}"),
        }
    }
}

impl std::error::Error for PemError {}

fn boundary_label<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    line.strip_prefix(prefix)?.strip_suffix(BOUNDARY_SUFFIX)
}

/// Every PEM block in `text`, in order. Lines outside blocks are ignored; a block must end with
/// an `END` line of the same label.
pub fn pem_decode_all(text: &str) -> Result<Vec<PemBlock>, PemError> {
    let mut blocks = Vec::new();
    let mut lines = text.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(label) = boundary_label(line, BEGIN_PREFIX) else {
            continue;
        };
        let mut base64 = Vec::new();
        let mut finished = false;
        for line in lines.by_ref() {
            if boundary_label(line, END_PREFIX) == Some(label) {
                finished = true;
                break;
            }
            if line.contains(':') {
                return Err(PemError::EncryptedHeaders);
            }
            base64.extend(line.bytes().filter(|byte| !byte.is_ascii_whitespace()));
        }
        if !finished {
            return Err(PemError::MissingEnd {
                label: label.to_string(),
            });
        }
        let contents = base64_decode(&base64).map_err(PemError::Base64)?;
        blocks.push(PemBlock {
            label: label.to_string(),
            contents,
        });
    }
    Ok(blocks)
}

/// The PEM text of one block: Base64 in lines of 64 characters, each line ending with `\n`.
pub fn pem_encode(label: &str, contents: &[u8]) -> String {
    let base64 = base64_encode(contents);
    let mut text = format!("{BEGIN_PREFIX}{label}{BOUNDARY_SUFFIX}\n");
    for line in base64.as_bytes().chunks(LINE_LENGTH) {
        text.extend(line.iter().map(|&byte| char::from(byte)));
        text.push('\n');
    }
    text.push_str(&format!("{END_PREFIX}{label}{BOUNDARY_SUFFIX}\n"));
    text
}
