//! The shard map as bytes, so it can be stored in the cluster's replicated database (D1039):
//! a version byte, the epoch, the strategy and every shard, with every length checked against
//! the bytes left when read back.

use crate::{NodeId, Shard, ShardId, ShardMap, Strategy};
use std::collections::BTreeMap;

const VERSION: u8 = 1;
const HASH: u8 = 0;
const RANGE: u8 = 1;
const TIME: u8 = 2;
const REGION: u8 = 3;

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let (taken, rest) = self.bytes.split_at_checked(count)?;
        self.bytes = rest;
        Some(taken)
    }

    fn byte(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn count(&mut self, smallest_item: usize) -> Option<usize> {
        let count = usize::try_from(self.u32()?).ok()?;
        (count <= self.bytes.len() / smallest_item.max(1)).then_some(count)
    }

    fn bytes(&mut self) -> Option<Vec<u8>> {
        let length = self.count(1)?;
        Some(self.take(length)?.to_vec())
    }

    fn text(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?).ok()
    }
}

impl ShardMap {
    /// The map as stored.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        out.extend_from_slice(&self.epoch.to_be_bytes());
        match &self.strategy {
            Strategy::Hash { shards } => {
                out.push(HASH);
                out.extend_from_slice(&shards.to_be_bytes());
            }
            Strategy::Range { bounds } => {
                out.push(RANGE);
                out.extend_from_slice(&(bounds.len() as u32).to_be_bytes());
                for bound in bounds {
                    put_bytes(&mut out, bound);
                }
            }
            Strategy::Time { bucket_ms } => {
                out.push(TIME);
                out.extend_from_slice(&bucket_ms.to_be_bytes());
            }
            Strategy::Region { regions } => {
                out.push(REGION);
                out.extend_from_slice(&(regions.len() as u32).to_be_bytes());
                for region in regions {
                    put_bytes(&mut out, region.as_bytes());
                }
            }
        }
        out.extend_from_slice(&(self.shards.len() as u32).to_be_bytes());
        for shard in self.shards.values() {
            out.extend_from_slice(&shard.id.to_be_bytes());
            out.extend_from_slice(&shard.owner.to_be_bytes());
            match &shard.region {
                Some(region) => {
                    out.push(1);
                    put_bytes(&mut out, region.as_bytes());
                }
                None => out.push(0),
            }
        }
        out
    }

    /// Reads [`ShardMap::encode`]'s bytes; `None` when they do not have its layout or were
    /// written by a newer version.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut reader = Reader { bytes };
        if reader.byte()? != VERSION {
            return None;
        }
        let epoch = reader.u64()?;
        let strategy = match reader.byte()? {
            HASH => Strategy::Hash {
                shards: reader.u32()?,
            },
            RANGE => {
                let count = reader.count(4)?;
                let mut bounds = Vec::with_capacity(count);
                for _ in 0..count {
                    bounds.push(reader.bytes()?);
                }
                Strategy::Range { bounds }
            }
            TIME => Strategy::Time {
                bucket_ms: reader.u64()?,
            },
            REGION => {
                let count = reader.count(4)?;
                let mut regions = Vec::with_capacity(count);
                for _ in 0..count {
                    regions.push(reader.text()?);
                }
                Strategy::Region { regions }
            }
            _ => return None,
        };
        let count = reader.count(4 + 8 + 1)?;
        let mut shards = BTreeMap::new();
        for _ in 0..count {
            let id: ShardId = reader.u32()?;
            let owner: NodeId = reader.u64()?;
            let region = match reader.byte()? {
                0 => None,
                1 => Some(reader.text()?),
                _ => return None,
            };
            if shards.insert(id, Shard { id, owner, region }).is_some() {
                return None;
            }
        }
        reader.bytes.is_empty().then_some(Self {
            strategy,
            shards,
            epoch,
        })
    }
}
