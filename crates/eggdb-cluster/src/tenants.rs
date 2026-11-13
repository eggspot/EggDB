//! The tenant prefix in the key layout (D20, D234, D1047): every key of a tenant's key space
//! starts with the engine's reserved prefix, the tag `0x08`, the tenant's name length (2 bytes,
//! big-endian) and its name, so a tenant's keys form one contiguous range that can be copied,
//! counted and moved to another shard as a unit, while order within a tenant is the order of
//! its own keys.

/// The prefix of every tenant key.
pub const TENANT_PREFIX: &[u8] = b"\xffCOL\x00\x08";

/// The longest tenant name, in bytes.
pub const MAX_TENANT_BYTES: usize = 255;

/// The prefix all of `tenant`'s keys start with; `None` for an empty or too long name.
pub fn tenant_prefix(tenant: &str) -> Option<Vec<u8>> {
    if tenant.is_empty() || tenant.len() > MAX_TENANT_BYTES {
        return None;
    }
    let mut prefix = Vec::with_capacity(TENANT_PREFIX.len() + 2 + tenant.len());
    prefix.extend_from_slice(TENANT_PREFIX);
    prefix.extend_from_slice(&(tenant.len() as u16).to_be_bytes());
    prefix.extend_from_slice(tenant.as_bytes());
    Some(prefix)
}

/// The stored key of `key` in `tenant`'s key space.
pub fn tenant_key(tenant: &str, key: &[u8]) -> Option<Vec<u8>> {
    let mut stored = tenant_prefix(tenant)?;
    stored.extend_from_slice(key);
    Some(stored)
}

/// The tenant a stored key belongs to, and the key within its space; `None` for a key outside
/// every tenant's space.
pub fn split_tenant_key(stored: &[u8]) -> Option<(&str, &[u8])> {
    let rest = stored.strip_prefix(TENANT_PREFIX)?;
    let (length, rest) = rest.split_first_chunk::<2>()?;
    let (name, key) = rest.split_at_checked(usize::from(u16::from_be_bytes(*length)))?;
    Some((std::str::from_utf8(name).ok()?, key))
}
