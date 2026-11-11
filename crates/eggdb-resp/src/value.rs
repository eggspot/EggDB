use std::collections::{BTreeMap, BTreeSet, VecDeque};

const FORMAT_VERSION: u8 = 1;
const HEADER_BYTES: usize = 10;
const UNKNOWN_ENTRIES_READ: u64 = u64::MAX;

/// A sorted set: each member's score, and the members in score order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SortedSet {
    scores: BTreeMap<Vec<u8>, f64>,
}

fn order(left: &(f64, Vec<u8>), right: &(f64, Vec<u8>)) -> std::cmp::Ordering {
    left.0
        .total_cmp(&right.0)
        .then_with(|| left.1.cmp(&right.1))
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

    /// The `count` members with the lowest scores, lowest first, or with `highest` the `count`
    /// with the highest scores, highest first; ties order by member bytes as in
    /// [`SortedSet::ordered`]. Only the chosen members are copied.
    pub fn first(&self, count: usize, highest: bool) -> Vec<(f64, Vec<u8>)> {
        let compare = |left: &(f64, &[u8]), right: &(f64, &[u8])| {
            let ascending = left.0.total_cmp(&right.0).then_with(|| left.1.cmp(right.1));
            if highest {
                ascending.reverse()
            } else {
                ascending
            }
        };
        let mut all: Vec<(f64, &[u8])> = self
            .scores
            .iter()
            .map(|(member, score)| (*score, member.as_slice()))
            .collect();
        let count = count.min(all.len());
        if count == 0 {
            return Vec::new();
        }
        if count < all.len() {
            all.select_nth_unstable_by(count - 1, compare);
            all.truncate(count);
        }
        all.sort_by(compare);
        all.into_iter()
            .map(|(score, member)| (score, member.to_vec()))
            .collect()
    }
}

/// A stream's entry identifier: milliseconds and a sequence number.
pub type StreamId = (u64, u64);

/// A stream entry's field names and values, in order.
pub type Fields = Vec<(Vec<u8>, Vec<u8>)>;

/// An entry delivered to a consumer of a group and not yet acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEntry {
    /// The consumer that holds it.
    pub consumer: Vec<u8>,
    /// When it was last delivered, in milliseconds since 1970.
    pub delivered_at_ms: u64,
    /// How many times it was delivered.
    pub deliveries: u64,
}

/// A consumer of a group, and when it was last seen and last active.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Consumer {
    /// When it last made any request, in milliseconds since 1970.
    pub seen_at_ms: u64,
    /// When it last read or claimed an entry, in milliseconds since 1970.
    pub active_at_ms: u64,
}

/// A consumer group: the last entry it delivered, the entries delivered and not acknowledged,
/// and its consumers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConsumerGroup {
    /// The highest identifier delivered to the group.
    pub last_delivered: StreamId,
    /// How many entries the group has read, when that is known.
    pub entries_read: Option<u64>,
    /// The pending entries list.
    pub pending: BTreeMap<StreamId, PendingEntry>,
    /// The consumers, by name.
    pub consumers: BTreeMap<Vec<u8>, Consumer>,
}

/// A stream: entries of field-value pairs in identifier order, the last identifier given, and
/// its consumer groups.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Stream {
    /// The highest identifier ever added.
    pub last: StreamId,
    /// The entries.
    pub entries: BTreeMap<StreamId, Fields>,
    /// The consumer groups, by name.
    pub groups: BTreeMap<Vec<u8>, ConsumerGroup>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum StoredKind {
    String = 0,
    Hash = 1,
    List = 2,
    Set = 3,
    SortedSet = 4,
    Stream = 5,
    StreamWithGroups = 6,
}

impl StoredKind {
    fn of(data: &Data) -> Self {
        match data {
            Data::String(_) => Self::String,
            Data::Hash(_) => Self::Hash,
            Data::List(_) => Self::List,
            Data::Set(_) => Self::Set,
            Data::SortedSet(_) => Self::SortedSet,
            Data::Stream(stream) if stream.groups.is_empty() => Self::Stream,
            Data::Stream(_) => Self::StreamWithGroups,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        [
            Self::String,
            Self::Hash,
            Self::List,
            Self::Set,
            Self::SortedSet,
            Self::Stream,
            Self::StreamWithGroups,
        ]
        .into_iter()
        .find(|kind| *kind as u8 == code)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoredHeader {
    pub(crate) kind: StoredKind,
    pub(crate) expires_at_ms: Option<u64>,
    pub(crate) elements: usize,
}

impl StoredHeader {
    pub(crate) fn read(bytes: &[u8]) -> Option<Self> {
        let [FORMAT_VERSION, code, ..] = bytes else {
            return None;
        };
        let kind = StoredKind::from_code(*code)?;
        let mut cursor = Cursor { bytes, at: 2 };
        let expires = u64::from_le_bytes(cursor.take(8)?.try_into().ok()?);
        let elements = match kind {
            StoredKind::String => bytes.len() - HEADER_BYTES,
            StoredKind::Stream | StoredKind::StreamWithGroups => {
                cursor.take(16)?;
                cursor.count()?
            }
            StoredKind::Hash | StoredKind::List | StoredKind::Set | StoredKind::SortedSet => {
                cursor.count()?
            }
        };
        Some(Self {
            kind,
            expires_at_ms: (expires != 0).then_some(expires),
            elements,
        })
    }

    pub(crate) fn expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_some_and(|at| at <= now_ms)
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

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn count(&mut self) -> Option<usize> {
        let count = self.u32()?;
        (count <= self.bytes.len()).then_some(count)
    }
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_groups(out: &mut Vec<u8>, groups: &BTreeMap<Vec<u8>, ConsumerGroup>) {
    out.extend_from_slice(&(groups.len() as u32).to_le_bytes());
    for (name, group) in groups {
        put_bytes(out, name);
        put_u64(out, group.last_delivered.0);
        put_u64(out, group.last_delivered.1);
        put_u64(out, group.entries_read.unwrap_or(UNKNOWN_ENTRIES_READ));
        out.extend_from_slice(&(group.pending.len() as u32).to_le_bytes());
        for ((millis, sequence), pending) in &group.pending {
            put_u64(out, *millis);
            put_u64(out, *sequence);
            put_bytes(out, &pending.consumer);
            put_u64(out, pending.delivered_at_ms);
            put_u64(out, pending.deliveries);
        }
        out.extend_from_slice(&(group.consumers.len() as u32).to_le_bytes());
        for (consumer, times) in &group.consumers {
            put_bytes(out, consumer);
            put_u64(out, times.seen_at_ms);
            put_u64(out, times.active_at_ms);
        }
    }
}

fn read_groups(cursor: &mut Cursor<'_>) -> Option<BTreeMap<Vec<u8>, ConsumerGroup>> {
    let count = cursor.count()?;
    if count == 0 {
        return None;
    }
    let mut groups = BTreeMap::new();
    for _ in 0..count {
        let name = cursor.bytes()?;
        if groups.keys().next_back().is_some_and(|previous| *previous >= name) {
            return None;
        }
        let last_delivered = (cursor.u64()?, cursor.u64()?);
        let entries_read = cursor.u64()?;
        let mut group = ConsumerGroup {
            last_delivered,
            entries_read: (entries_read != UNKNOWN_ENTRIES_READ).then_some(entries_read),
            ..ConsumerGroup::default()
        };
        let pending_count = cursor.count()?;
        for _ in 0..pending_count {
            let id = (cursor.u64()?, cursor.u64()?);
            if group.pending.keys().next_back().is_some_and(|previous| *previous >= id) {
                return None;
            }
            let consumer = cursor.bytes()?;
            let delivered_at_ms = cursor.u64()?;
            let deliveries = cursor.u64()?;
            group.pending.insert(
                id,
                PendingEntry {
                    consumer,
                    delivered_at_ms,
                    deliveries,
                },
            );
        }
        let consumer_count = cursor.count()?;
        for _ in 0..consumer_count {
            let consumer = cursor.bytes()?;
            if group.consumers.keys().next_back().is_some_and(|previous| *previous >= consumer) {
                return None;
            }
            let seen_at_ms = cursor.u64()?;
            let active_at_ms = cursor.u64()?;
            group.consumers.insert(
                consumer,
                Consumer {
                    seen_at_ms,
                    active_at_ms,
                },
            );
        }
        if group
            .pending
            .values()
            .any(|pending| !group.consumers.contains_key(&pending.consumer))
        {
            return None;
        }
        groups.insert(name, group);
    }
    Some(groups)
}

impl Entry {
    /// The entry as stored bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![FORMAT_VERSION, StoredKind::of(&self.data) as u8];
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
                if !stream.groups.is_empty() {
                    put_groups(&mut out, &stream.groups);
                }
            }
        }
        out
    }

    /// Reads stored bytes, or `None` when they are not an entry.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let [FORMAT_VERSION, code, ..] = bytes else {
            return None;
        };
        let kind = StoredKind::from_code(*code)?;
        let mut cursor = Cursor { bytes, at: 2 };
        let expires = u64::from_le_bytes(cursor.take(8)?.try_into().ok()?);
        let data = match kind {
            StoredKind::String => Data::String(bytes[cursor.at..].to_vec()),
            StoredKind::Hash => {
                let count = cursor.count()?;
                let mut map = BTreeMap::new();
                for _ in 0..count {
                    let field = cursor.bytes()?;
                    map.insert(field, cursor.bytes()?);
                }
                Data::Hash(map)
            }
            StoredKind::List => {
                let count = cursor.count()?;
                let mut list = VecDeque::with_capacity(count.min(1024));
                for _ in 0..count {
                    list.push_back(cursor.bytes()?);
                }
                Data::List(list)
            }
            StoredKind::Set => {
                let count = cursor.count()?;
                let mut set = BTreeSet::new();
                for _ in 0..count {
                    set.insert(cursor.bytes()?);
                }
                Data::Set(set)
            }
            StoredKind::SortedSet => {
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
            StoredKind::Stream | StoredKind::StreamWithGroups => {
                let mut word = || -> Option<u64> {
                    Some(u64::from_le_bytes(cursor.take(8)?.try_into().ok()?))
                };
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
                let groups = if kind == StoredKind::StreamWithGroups {
                    read_groups(&mut cursor)?
                } else {
                    BTreeMap::new()
                };
                Data::Stream(Stream {
                    last,
                    entries,
                    groups,
                })
            }
        };
        if kind != StoredKind::String && cursor.at != bytes.len() {
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

    fn elements(data: &Data) -> usize {
        match data {
            Data::String(bytes) => bytes.len(),
            Data::Hash(map) => map.len(),
            Data::List(list) => list.len(),
            Data::Set(set) => set.len(),
            Data::SortedSet(set) => set.len(),
            Data::Stream(stream) => stream.entries.len(),
        }
    }

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
            let bytes = entry.encode();
            let header = StoredHeader::read(&bytes).unwrap();
            assert_eq!(header.kind, StoredKind::of(&entry.data));
            assert_eq!(header.expires_at_ms, Some(42));
            assert_eq!(header.elements, elements(&entry.data));
            assert_eq!(Entry::decode(&bytes), Some(entry));
        }
        let ordered: Vec<Vec<u8>> = zset
            .ordered()
            .into_iter()
            .map(|(_, member)| member)
            .collect();
        assert_eq!(ordered, vec![b"c".to_vec(), b"a".to_vec(), b"b".to_vec()]);
        for damaged in [
            &[][..],
            &[9, 0],
            &[1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255],
        ] {
            assert_eq!(Entry::decode(damaged), None);
        }
        for damaged in [
            &[][..],
            &[9, 0],
            &[1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            &[1, 6],
        ] {
            assert_eq!(StoredHeader::read(damaged), None);
        }
    }

    fn grouped_stream() -> Entry {
        let mut group = ConsumerGroup {
            last_delivered: (5, 1),
            entries_read: Some(2),
            ..ConsumerGroup::default()
        };
        group.consumers.insert(
            b"c1".to_vec(),
            Consumer {
                seen_at_ms: 7,
                active_at_ms: 6,
            },
        );
        group.pending.insert(
            (5, 1),
            PendingEntry {
                consumer: b"c1".to_vec(),
                delivered_at_ms: 6,
                deliveries: 2,
            },
        );
        Entry {
            data: Data::Stream(Stream {
                last: (5, 1),
                entries: BTreeMap::from([((5, 1), vec![(b"f".to_vec(), b"v".to_vec())])]),
                groups: BTreeMap::from([(b"g".to_vec(), group)]),
            }),
            expires_at_ms: None,
        }
    }

    const GROUPED_STREAM_VECTOR: &str = concat!(
        "0106", "0000000000000000",
        "0500000000000000", "0100000000000000", "01000000",
        "0500000000000000", "0100000000000000", "01000000", "0100000066", "0100000076",
        "01000000", "0100000067",
        "0500000000000000", "0100000000000000", "0200000000000000",
        "01000000", "0500000000000000", "0100000000000000", "020000006331",
        "0600000000000000", "0200000000000000",
        "01000000", "020000006331", "0700000000000000", "0600000000000000",
    );

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn consumer_groups_keep_their_pinned_layout_and_refuse_damage() {
        let entry = grouped_stream();
        let bytes = entry.encode();
        assert_eq!(hex(&bytes), GROUPED_STREAM_VECTOR);
        assert_eq!(Entry::decode(&bytes), Some(entry.clone()));
        let header = StoredHeader::read(&bytes).unwrap();
        assert_eq!(header.kind, StoredKind::StreamWithGroups);
        assert_eq!(header.elements, 1);
        for length in 0..bytes.len() {
            assert_eq!(Entry::decode(&bytes[..length]), None, "truncated to {length}");
        }
        let groups_start = HEADER_BYTES + 16 + 4 + 16 + 4 + 5 + 5;
        for at in groups_start..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0xFF;
            if let Some(decoded) = Entry::decode(&flipped) {
                assert_eq!(decoded.encode(), flipped, "byte {at}");
            }
        }
        let Data::Stream(mut stream) = entry.data else {
            unreachable!("the vector holds a stream");
        };
        stream.groups.clear();
        let plain = Entry {
            data: Data::Stream(stream),
            expires_at_ms: None,
        }
        .encode();
        assert_eq!(plain[1], StoredKind::Stream as u8);
    }

    #[test]
    fn the_first_members_from_either_end_match_the_full_order() {
        let mut zset = SortedSet::default();
        for (member, score) in [("d", 4.0), ("b", 2.0), ("a", 2.0), ("c", -1.5), ("e", 9.0)] {
            zset.insert(member.as_bytes().to_vec(), score);
        }
        let ordered = zset.ordered();
        for count in 0..=6 {
            let lowest: Vec<_> = ordered.iter().take(count).cloned().collect();
            let highest: Vec<_> = ordered.iter().rev().take(count).cloned().collect();
            assert_eq!(zset.first(count, false), lowest, "{count}");
            assert_eq!(zset.first(count, true), highest, "{count}");
        }
    }
}
