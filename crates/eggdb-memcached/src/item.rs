const FORMAT_VERSION: u8 = 1;

/// A stored item: its data, the client's flags, its CAS value and when it expires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The value's bytes.
    pub data: Vec<u8>,
    /// The 32-bit flags the client stored with it.
    pub flags: u32,
    /// The unique value `gets` returns and `cas` compares.
    pub cas: u64,
    /// When it expires, in milliseconds since 1970, if ever.
    pub expires_at_ms: Option<u64>,
}

impl Item {
    /// The stored bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![FORMAT_VERSION];
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.cas.to_le_bytes());
        out.extend_from_slice(&self.expires_at_ms.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    /// The item stored as `bytes`, or `None` when they are not one.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.first() != Some(&FORMAT_VERSION) || bytes.len() < 21 {
            return None;
        }
        let flags = u32::from_le_bytes(bytes[1..5].try_into().ok()?);
        let cas = u64::from_le_bytes(bytes[5..13].try_into().ok()?);
        let expires = u64::from_le_bytes(bytes[13..21].try_into().ok()?);
        Some(Self {
            data: bytes[21..].to_vec(),
            flags,
            cas,
            expires_at_ms: (expires != 0).then_some(expires),
        })
    }

    /// Whether it has expired at `now_ms`.
    pub fn expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_some_and(|at| at <= now_ms)
    }
}

const RELATIVE_LIMIT_SECONDS: i64 = 60 * 60 * 24 * 30;

/// The expiry time an `exptime` argument means at `now_ms`: 0 never, up to 30 days seconds from
/// now, more a Unix time, a negative value already expired.
pub fn expiry_of(exptime: i64, now_ms: u64) -> Option<u64> {
    match exptime {
        0 => None,
        negative if negative < 0 => Some(0),
        relative if relative <= RELATIVE_LIMIT_SECONDS => Some(now_ms.saturating_add(relative as u64 * 1000)),
        absolute => Some((absolute as u64).saturating_mul(1000)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_round_trip_and_exptime_follows_the_protocol() {
        let item = Item { data: b"hello".to_vec(), flags: 7, cas: 42, expires_at_ms: Some(9) };
        assert_eq!(Item::decode(&item.encode()), Some(item));
        assert_eq!(Item::decode(&[1, 2, 3]), None);
        assert_eq!(expiry_of(0, 5000), None);
        assert_eq!(expiry_of(10, 5000), Some(15_000));
        assert_eq!(expiry_of(2_000_000_000, 5000), Some(2_000_000_000_000));
        assert_eq!(expiry_of(-1, 5000), Some(0));
    }
}
