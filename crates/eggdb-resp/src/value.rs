use std::collections::{BTreeMap, BTreeSet, VecDeque};

const FORMAT_VERSION: u8 = 1;

/// A sorted set: each member's score, and the members in score order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SortedSet {
    scores: BTreeMap<Vec<u8>, f64>,
}

fn order(left: &(f64, Vec<u8>), right: &(f64, Vec<u8>)) -> std::cmp::Ordering {
    left.0.total_cmp(&right.0).then_with(|| left.1.cmp(&right.1))
}

impl SortedSet {
    /// The member's score.
    pub fn score(&self, member: &[u8]) -> Option<f64> {
        self.scores.get(member).copied()
    }

    /// Sets the member's score; returns whether the member is new.
    pub fn insert(&mut self, member: Vec<u8>, score: f64) -> bool {
        self.scores.insert(member, score).is_none()
    }

    /// Removes the member.
    pub fn remove(&mut self, member: &[u8]) -> bool {
        self.scores.remove(member).is_some()
    }

    /// How many members.
    pub fn len(&self) -> usize {
        self.scores.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.scores.is_empty()
    }

    /// The members with their scores, lowest score first, ties by member bytes.
    pub fn ordered(&self) -> Vec<(f64, Vec<u8>)> {
        let mut all: Vec<(f64, Vec<u8>)> = self
            .scores
            .iter()
            .map(|(member, score)| (*score, member.clone()))
            .collect();
        all.sort_by(order);
        all
    }
}

/// A stream's entry identifier: milliseconds and a sequence number.
pub type StreamId = (u64, u64);

/// A stream: entries of field-value pairs in identifier order, and the last identifier given.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Stream {
    /// The highest identifier ever added.
    pub last: StreamId,
    /// The entries.
    pub entries: BTreeMap<StreamId, Vec<(Vec<u8>, Vec<u8>)>>,
}

/// A value of one of the six types.
#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    /// A string (bytes).
    String(Vec<u8>),
    /// A hash: field to value.
    Hash(BTreeMap<Vec<u8>, Vec<u8>>),
    /// A list.
    List(VecDeque<Vec<u8>>),
    /// A set.
    Set(BTreeSet<Vec<u8>>),
    /// A sorted set.
    SortedSet(SortedSet),
    /// A stream.
    Stream(Stream),
}

impl Data {
    /// The type's name, as `TYPE` answers.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::String(_) => "string",
            Self::Hash(_) => "hash",
            Self::List(_) => "list",
            Self::Set(_) => "set",
            Self::SortedSet(_) => "zset",
            Self::Stream(_) => "stream",
        }
    }

    /// Whether an aggregate has no elements left, so its key goes away.
    pub fn is_empty_aggregate(&self) -> bool {
        match self {
            Self::String(_) => false,
            Self::Hash(map) => map.is_empty(),
            Self::List(list) => list.is_empty(),
            Self::Set(set) => set.is_empty(),
            Self::SortedSet(set) => set.is_empty(),
            Self::Stream(_) => false,
        }
    }
}

/// A stored key's value and its expiry time (milliseconds since 1970), if any.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The value.
    pub data: Data,
    /// When the key expires.
    pub expires_at_ms: Option<u64>,
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, count: usize) -> Option<&[u8]> {
        let end = self.at.checked_add(count)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u32(&mut self) -> Option<usize> {
        let bytes = self.take(4)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?) as usize)
    }

    fn bytes(&mut self) -> Option<Vec<u8>> {
        let length = self.u32()?;
        self.take(length).map(<[u8]>::to_vec)
    }

    fn count(&mut self) -> Option<usize> {
        let count = self.u32()?;
        (count <= self.bytes.len()).then_some(count)
    }
}

impl Entry {
    /// The entry as stored bytes.
    pub fn encode(&self) -> Vec<u8> {
        let kind = match &self.data {
            Data::String(_) => 0u8,
            Data::Hash(_) => 1,
            Data::List(_) => 2,
            Data::Set(_) => 3,
            Data::SortedSet(_) => 4,
            Data::Stream(_) => 5,
        };
        let mut out = vec![FORMAT_VERSION, kind];
        out.extend_from_slice(&self.expires_at_ms.unwrap_or(0).to_le_bytes());
        match &self.data {
            Data::String(bytes) => out.extend_from_slice(bytes),
            Data::Hash(map) => {
                out.extend_from_slice(&(map.len() as u32).to_le_bytes());
                for (field, value) in map {
                    put_bytes(&mut out, field);
                    put_bytes(&mut out, value);
                }
            }
            Data::List(list) => {
                out.extend_from_slice(&(list.len() as u32).to_le_bytes());
                for item in list {
                    put_bytes(&mut out, item);
                }
            }
            Data::Set(set) => {
                out.extend_from_slice(&(set.len() as u32).to_le_bytes());
                for member in set {
                    put_bytes(&mut out, member);
                }
            }
            Data::SortedSet(set) => {
                out.extend_from_slice(&(set.len() as u32).to_le_bytes());
                for (member, score) in &set.scores {
                    out.extend_from_slice(&score.to_le_bytes());
                    put_bytes(&mut out, member);
                }
            }
            Data::Stream(stream) => {
                out.extend_from_slice(&stream.last.0.to_le_bytes());
                out.extend_from_slice(&stream.last.1.to_le_bytes());
                out.extend_from_slice(&(stream.entries.len() as u32).to_le_bytes());
                for ((millis, sequence), pairs) in &stream.entries {
                    out.extend_from_slice(&millis.to_le_bytes());
                    out.extend_from_slice(&sequence.to_le_bytes());
                    out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
                    for (field, value) in pairs {
                        put_bytes(&mut out, field);
                        put_bytes(&mut out, value);
                    }
                }
            }
        }
        out
    }

    /// Reads stored bytes, or `None` when they are not an entry.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let [FORMAT_VERSION, kind, ..] = bytes else {
            return None;
        };
        let mut cursor = Cursor { bytes, at: 2 };
        let expires = u64::from_le_bytes(cursor.take(8)?.try_into().ok()?);
        let data = match kind {
            0 => Data::String(bytes[cursor.at..].to_vec()),
            1 => {
                let count = cursor.count()?;
                let mut map = BTreeMap::new();
                for _ in 0..count {
                    let field = cursor.bytes()?;
                    map.insert(field, cursor.bytes()?);
                }
                Data::Hash(map)
            }
            2 => {
                let count = cursor.count()?;
                let mut list = VecDeque::with_capacity(count);
                for _ in 0..count {
                    list.push_back(cursor.bytes()?);
                }
                Data::List(list)
            }
            3 => {
                let count = cursor.count()?;
                let mut set = BTreeSet::new();
                for _ in 0..count {
                    set.insert(cursor.bytes()?);
                }
                Data::Set(set)
            }
            4 => {
                let count = cursor.count()?;
                let mut set = SortedSet::default();
                for _ in 0..count {
                    let score = f64::from_le_bytes(cursor.take(8)?.try_into().ok()?);
                    if score.is_nan() {
                        return None;
                    }
                    set.insert(cursor.bytes()?, score);
                }
                Data::SortedSet(set)
            }
            5 => {
                let mut word = || -> Option<u64> { Some(u64::from_le_bytes(cursor.take(8)?.try_into().ok()?)) };
                let last = (word()?, word()?);
                let count = cursor.count()?;
                let mut entries = BTreeMap::new();
                for _ in 0..count {
                    let millis = u64::from_le_bytes(cursor.take(8)?.try_into().ok()?);
                    let sequence = u64::from_le_bytes(cursor.take(8)?.try_into().ok()?);
                    let pairs_count = cursor.count()?;
                    let mut pairs = Vec::with_capacity(pairs_count.min(1024));
                    for _ in 0..pairs_count {
                        let field = cursor.bytes()?;
                        pairs.push((field, cursor.bytes()?));
                    }
                    entries.insert((millis, sequence), pairs);
                }
                Data::Stream(Stream { last, entries })
            }
            _ => return None,
        };
        if *kind != 0 && cursor.at != bytes.len() {
            return None;
        }
        Some(Self {
            data,
            expires_at_ms: (expires != 0).then_some(expires),
        })
    }

    /// Whether the key has expired at `now_ms`.
    pub fn expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_some_and(|at| at <= now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_type_round_trips_and_damaged_bytes_are_refused() {
        let mut zset = SortedSet::default();
        zset.insert(b"b".to_vec(), 2.0);
        zset.insert(b"a".to_vec(), 2.0);
        zset.insert(b"c".to_vec(), -1.5);
        let entries = [
            Data::String(b"hello".to_vec()),
            Data::Hash(BTreeMap::from([(b"f".to_vec(), b"v".to_vec())])),
            Data::List(VecDeque::from([b"x".to_vec(), Vec::new()])),
            Data::Set(BTreeSet::from([b"m".to_vec()])),
            Data::SortedSet(zset.clone()),
        ];
        for data in entries {
            let entry = Entry {
                data,
                expires_at_ms: Some(42),
            };
            assert_eq!(Entry::decode(&entry.encode()), Some(entry));
        }
        let ordered: Vec<Vec<u8>> = zset.ordered().into_iter().map(|(_, member)| member).collect();
        assert_eq!(ordered, vec![b"c".to_vec(), b"a".to_vec(), b"b".to_vec()]);
        for damaged in [&[][..], &[9, 0], &[1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255]] {
            assert_eq!(Entry::decode(damaged), None);
        }
    }
}
