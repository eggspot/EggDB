use crate::hash::HashFunction;
use crate::hmac::Hmac;

/// Why HKDF could not produce its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HkdfError {
    /// The output asked for is longer than HKDF allows: 255 blocks of the hash's digest.
    OutputTooLong,
}

impl std::fmt::Display for HkdfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutputTooLong => f.write_str("HKDF output longer than 255 hash blocks"),
        }
    }
}

impl std::error::Error for HkdfError {}

/// HKDF-Extract (RFC 5869): the pseudorandom key made from `input_key` and `salt` with the
/// hash function `H`. An empty salt stands for a salt of zero bytes as long as the digest.
pub fn hkdf_extract<H: HashFunction>(salt: &[u8], input_key: &[u8]) -> H::Digest {
    let mut mac = Hmac::<H>::new(salt);
    mac.update(input_key);
    mac.finish()
}

/// HKDF-Expand (RFC 5869): fills `output` with key material from the pseudorandom key `prk`
/// and the context `info`, with the hash function `H`.
pub fn hkdf_expand<H: HashFunction>(
    prk: &[u8],
    info: &[u8],
    output: &mut [u8],
) -> Result<(), HkdfError> {
    if output.len() > 255 * H::DIGEST_BYTES {
        return Err(HkdfError::OutputTooLong);
    }
    let keyed = Hmac::<H>::new(prk);
    let mut previous: Option<H::Digest> = None;
    for (index, chunk) in output.chunks_mut(H::DIGEST_BYTES).enumerate() {
        let mut mac = keyed.clone();
        if let Some(previous) = &previous {
            mac.update(previous.as_ref());
        }
        mac.update(info);
        mac.update(&[(index + 1) as u8]);
        let block = mac.finish();
        chunk.copy_from_slice(&block.as_ref()[..chunk.len()]);
        previous = Some(block);
    }
    Ok(())
}
