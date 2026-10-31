//! TLS 1.2's key derivation (RFC 5246 section 5, RFC 7627): the PRF, the master secret and the
//! key block.

use crate::cipher_suite::HashKind;

/// TLS 1.2's PRF: P_hash(secret, label + seed) to `length` bytes.
pub fn prf(hash: HashKind, secret: &[u8], label: &[u8], seed: &[u8], length: usize) -> Vec<u8> {
    let mut label_and_seed = label.to_vec();
    label_and_seed.extend_from_slice(seed);
    let mut output = Vec::with_capacity(length + hash.digest_bytes());
    let mut chain = hash.hmac(secret, &label_and_seed);
    while output.len() < length {
        let mut input = chain.clone();
        input.extend_from_slice(&label_and_seed);
        output.extend_from_slice(&hash.hmac(secret, &input));
        chain = hash.hmac(secret, &chain);
    }
    output.truncate(length);
    output
}

/// The extended master secret (RFC 7627) over the session hash of ClientHello..ClientKeyExchange.
pub fn extended_master_secret(hash: HashKind, pre_master_secret: &[u8], session_hash: &[u8]) -> Vec<u8> {
    prf(hash, pre_master_secret, b"extended master secret", session_hash, 48)
}

/// The original master secret from the two randoms, for peers without extended master secret.
pub fn master_secret(
    hash: HashKind,
    pre_master_secret: &[u8],
    client_random: &[u8; 32],
    server_random: &[u8; 32],
) -> Vec<u8> {
    let mut seed = client_random.to_vec();
    seed.extend_from_slice(server_random);
    prf(hash, pre_master_secret, b"master secret", &seed, 48)
}

/// The write keys and IVs of both sides, from the key block (RFC 5246 section 6.3; AEAD suites
/// have no MAC keys).
#[derive(Clone, PartialEq, Eq)]
pub struct KeyBlock {
    /// The client's write key.
    pub client_key: Vec<u8>,
    /// The server's write key.
    pub server_key: Vec<u8>,
    /// The client's fixed IV.
    pub client_iv: Vec<u8>,
    /// The server's fixed IV.
    pub server_iv: Vec<u8>,
}

impl std::fmt::Debug for KeyBlock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("KeyBlock { .. }")
    }
}

/// The key block for keys of `key_bytes` and IVs of `iv_bytes`.
pub fn key_block(
    hash: HashKind,
    master_secret: &[u8],
    client_random: &[u8; 32],
    server_random: &[u8; 32],
    key_bytes: usize,
    iv_bytes: usize,
) -> KeyBlock {
    let mut seed = server_random.to_vec();
    seed.extend_from_slice(client_random);
    let block = prf(hash, master_secret, b"key expansion", &seed, 2 * (key_bytes + iv_bytes));
    let (client_key, rest) = block.split_at(key_bytes);
    let (server_key, rest) = rest.split_at(key_bytes);
    let (client_iv, server_iv) = rest.split_at(iv_bytes);
    KeyBlock {
        client_key: client_key.to_vec(),
        server_key: server_key.to_vec(),
        client_iv: client_iv.to_vec(),
        server_iv: server_iv.to_vec(),
    }
}

/// The 12-byte `verify_data` of a Finished message over the handshake hash.
pub fn finished_verify_data(hash: HashKind, master_secret: &[u8], from_client: bool, handshake_hash: &[u8]) -> Vec<u8> {
    let label: &[u8] = if from_client { b"client finished" } else { b"server finished" };
    prf(hash, master_secret, label, handshake_hash, 12)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap()).collect()
    }

    #[test]
    fn the_prf_matches_the_published_sha256_test_vector() {
        let secret = hex("9bbe436ba940f017b17652849a71db35");
        let seed = hex("a0ba9f936cda311827a6f796ffd5198c");
        let output = prf(HashKind::Sha256, &secret, b"test label", &seed, 100);
        assert_eq!(
            output,
            hex(concat!(
                "e3f229ba727be17b8d122620557cd453c2aab21d07c3d495329b52d4e61edb5a6b301791e90d35c9c9a46b4e14baf9af",
                "0fa022f7077def17abfd3797c0564bab4fbc91666e9def9b97fce34f796789baa48082d122ee42c5a72e5a5110fff701",
                "87347b66"
            ))
        );
    }

    #[test]
    fn the_key_block_splits_in_rfc_order() {
        let block = key_block(HashKind::Sha256, &[1; 48], &[2; 32], &[3; 32], 16, 4);
        let mut seed = [3u8; 32].to_vec();
        seed.extend_from_slice(&[2; 32]);
        let raw = prf(HashKind::Sha256, &[1; 48], b"key expansion", &seed, 40);
        assert_eq!(block.client_key, raw[..16]);
        assert_eq!(block.server_key, raw[16..32]);
        assert_eq!(block.client_iv, raw[32..36]);
        assert_eq!(block.server_iv, raw[36..40]);
    }
}
