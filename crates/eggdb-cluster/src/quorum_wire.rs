//! The messages of leaderless quorums over the network (D1097), as length-prefixed frames: a
//! big-endian `u32` length, then a kind byte and its fields. Every length is checked against the
//! bytes left, so a damaged or hostile frame is an error, never a panic.

use crate::NodeId;
use crate::quorum::{Stamp, Version, VersionVector};
use std::io::{self, Read, Write};

/// The largest frame either side accepts: 64 MiB.
pub const MAX_FRAME_BYTES: u32 = 64 << 20;

/// A request to a quorum node, from a client or from another node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// The first frame on a connection to a node with a password: the password.
    Hello {
        /// The password's bytes.
        password: Vec<u8>,
    },
    /// A client's write through this node as coordinator; `None` deletes.
    Put {
        /// The key.
        key: Vec<u8>,
        /// The value, or `None` to delete.
        value: Option<Vec<u8>>,
        /// What the client read, which the write supersedes.
        context: VersionVector,
    },
    /// A client's read through this node as coordinator.
    Get {
        /// The key.
        key: Vec<u8>,
    },
    /// Keep this version of a key as one of its home replicas.
    Store {
        /// The key.
        key: Vec<u8>,
        /// The version.
        version: Version,
    },
    /// Send the versions kept for a key.
    Fetch {
        /// The key.
        key: Vec<u8>,
    },
    /// Keep this version for `owner`, a home replica that could not be reached, until it can.
    Hint {
        /// The home replica the version belongs to.
        owner: NodeId,
        /// The key.
        key: Vec<u8>,
        /// The version.
        version: Version,
    },
    /// Send the Merkle tree's leaves over the keys both this node and `peer` are homes of.
    Tree {
        /// The node asking.
        peer: NodeId,
    },
    /// Send the keys under these leaves of the tree shared with `peer`.
    KeysAt {
        /// The node asking.
        peer: NodeId,
        /// The leaves that differ.
        leaves: Vec<u32>,
    },
}

/// A quorum node's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Done.
    Ack,
    /// A write succeeded with this version.
    Written(Version),
    /// A read's values (one, or siblings; `None` for a deletion) and its context.
    Values {
        /// The values in stamp order.
        values: Vec<Option<Vec<u8>>>,
        /// The merged vector to write back with.
        context: VersionVector,
    },
    /// The versions kept for a key.
    Versions(Vec<Version>),
    /// A Merkle tree's leaf digests.
    Leaves(Vec<u128>),
    /// Keys.
    Keys(Vec<Vec<u8>>),
    /// The request failed.
    Failed(String),
}

const PUT: u8 = 1;
const GET: u8 = 2;
const STORE: u8 = 3;
const FETCH: u8 = 4;
const HINT: u8 = 5;
const TREE: u8 = 6;
const KEYS_AT: u8 = 7;
const HELLO: u8 = 8;
const ACK: u8 = 0x80;
const WRITTEN: u8 = 0x81;
const VALUES: u8 = 0x82;
const VERSIONS: u8 = 0x83;
const LEAVES: u8 = 0x84;
const KEYS: u8 = 0x85;
const FAILED: u8 = 0x86;

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn put_optional(out: &mut Vec<u8>, bytes: Option<&[u8]>) {
    match bytes {
        Some(bytes) => {
            out.push(1);
            put_bytes(out, bytes);
        }
        None => out.push(0),
    }
}

fn put_vector(out: &mut Vec<u8>, vector: &VersionVector) {
    let entries: Vec<(NodeId, u64)> = vector.entries().collect();
    out.extend_from_slice(&u32::try_from(entries.len()).unwrap_or(u32::MAX).to_be_bytes());
    for (node, count) in entries {
        out.extend_from_slice(&node.to_be_bytes());
        out.extend_from_slice(&count.to_be_bytes());
    }
}

fn put_version(out: &mut Vec<u8>, version: &Version) {
    put_vector(out, &version.vector);
    out.extend_from_slice(&version.stamp.millis.to_be_bytes());
    out.extend_from_slice(&version.stamp.counter.to_be_bytes());
    out.extend_from_slice(&version.stamp.node.to_be_bytes());
    put_optional(out, version.value.as_deref());
}

fn put_count(out: &mut Vec<u8>, count: usize) {
    out.extend_from_slice(&u32::try_from(count).unwrap_or(u32::MAX).to_be_bytes());
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

    fn u128(&mut self) -> Option<u128> {
        Some(u128::from_be_bytes(self.take(16)?.try_into().ok()?))
    }

    fn count(&mut self, smallest_item: usize) -> Option<usize> {
        let count = usize::try_from(self.u32()?).ok()?;
        (count.checked_mul(smallest_item)? <= self.bytes.len()).then_some(count)
    }

    fn bytes(&mut self) -> Option<Vec<u8>> {
        let length = usize::try_from(self.u32()?).ok()?;
        Some(self.take(length)?.to_vec())
    }

    fn optional(&mut self) -> Option<Option<Vec<u8>>> {
        match self.byte()? {
            0 => Some(None),
            1 => Some(Some(self.bytes()?)),
            _ => None,
        }
    }

    fn vector(&mut self) -> Option<VersionVector> {
        let count = self.count(16)?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            entries.push((self.u64()?, self.u64()?));
        }
        Some(VersionVector::from_entries(entries))
    }

    fn version(&mut self) -> Option<Version> {
        let vector = self.vector()?;
        let stamp = Stamp {
            millis: self.u64()?,
            counter: self.u32()?,
            node: self.u64()?,
        };
        Some(Version {
            vector,
            stamp,
            value: self.optional()?,
        })
    }

    fn finished<T>(self, value: T) -> Option<T> {
        self.bytes.is_empty().then_some(value)
    }
}

/// The versions of one key as stored bytes: a count, then each version.
pub fn encode_versions(versions: &[Version]) -> Vec<u8> {
    let mut out = Vec::new();
    put_count(&mut out, versions.len());
    for version in versions {
        put_version(&mut out, version);
    }
    out
}

/// Reads what [`encode_versions`] wrote, or `None` for damaged bytes.
pub fn decode_versions(bytes: &[u8]) -> Option<Vec<Version>> {
    let mut reader = Reader { bytes };
    let count = reader.count(25)?;
    let mut versions = Vec::with_capacity(count);
    for _ in 0..count {
        versions.push(reader.version()?);
    }
    reader.finished(versions)
}

/// One version as stored bytes, for a hint.
pub fn encode_version(version: &Version) -> Vec<u8> {
    let mut out = Vec::new();
    put_version(&mut out, version);
    out
}

/// Reads what [`encode_version`] wrote, or `None` for damaged bytes.
pub fn decode_version(bytes: &[u8]) -> Option<Version> {
    let mut reader = Reader { bytes };
    let version = reader.version()?;
    reader.finished(version)
}

impl Request {
    /// The request's frame body.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Request::Hello { password } => {
                out.push(HELLO);
                put_bytes(&mut out, password);
            }
            Request::Put { key, value, context } => {
                out.push(PUT);
                put_bytes(&mut out, key);
                put_optional(&mut out, value.as_deref());
                put_vector(&mut out, context);
            }
            Request::Get { key } => {
                out.push(GET);
                put_bytes(&mut out, key);
            }
            Request::Store { key, version } => {
                out.push(STORE);
                put_bytes(&mut out, key);
                put_version(&mut out, version);
            }
            Request::Fetch { key } => {
                out.push(FETCH);
                put_bytes(&mut out, key);
            }
            Request::Hint { owner, key, version } => {
                out.push(HINT);
                out.extend_from_slice(&owner.to_be_bytes());
                put_bytes(&mut out, key);
                put_version(&mut out, version);
            }
            Request::Tree { peer } => {
                out.push(TREE);
                out.extend_from_slice(&peer.to_be_bytes());
            }
            Request::KeysAt { peer, leaves } => {
                out.push(KEYS_AT);
                out.extend_from_slice(&peer.to_be_bytes());
                put_count(&mut out, leaves.len());
                for leaf in leaves {
                    out.extend_from_slice(&leaf.to_be_bytes());
                }
            }
        }
        out
    }

    /// Reads a request's frame body, or `None` when it is not one.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut reader = Reader { bytes };
        let request = match reader.byte()? {
            HELLO => Request::Hello { password: reader.bytes()? },
            PUT => Request::Put {
                key: reader.bytes()?,
                value: reader.optional()?,
                context: reader.vector()?,
            },
            GET => Request::Get { key: reader.bytes()? },
            STORE => Request::Store {
                key: reader.bytes()?,
                version: reader.version()?,
            },
            FETCH => Request::Fetch { key: reader.bytes()? },
            HINT => Request::Hint {
                owner: reader.u64()?,
                key: reader.bytes()?,
                version: reader.version()?,
            },
            TREE => Request::Tree { peer: reader.u64()? },
            KEYS_AT => {
                let peer = reader.u64()?;
                let count = reader.count(4)?;
                let mut leaves = Vec::with_capacity(count);
                for _ in 0..count {
                    leaves.push(reader.u32()?);
                }
                Request::KeysAt { peer, leaves }
            }
            _ => return None,
        };
        reader.finished(request)
    }
}

impl Response {
    /// The response's frame body.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Response::Ack => out.push(ACK),
            Response::Written(version) => {
                out.push(WRITTEN);
                put_version(&mut out, version);
            }
            Response::Values { values, context } => {
                out.push(VALUES);
                put_count(&mut out, values.len());
                for value in values {
                    put_optional(&mut out, value.as_deref());
                }
                put_vector(&mut out, context);
            }
            Response::Versions(versions) => {
                out.push(VERSIONS);
                out.extend_from_slice(&encode_versions(versions));
            }
            Response::Leaves(leaves) => {
                out.push(LEAVES);
                put_count(&mut out, leaves.len());
                for leaf in leaves {
                    out.extend_from_slice(&leaf.to_be_bytes());
                }
            }
            Response::Keys(keys) => {
                out.push(KEYS);
                put_count(&mut out, keys.len());
                for key in keys {
                    put_bytes(&mut out, key);
                }
            }
            Response::Failed(message) => {
                out.push(FAILED);
                put_bytes(&mut out, message.as_bytes());
            }
        }
        out
    }

    /// Reads a response's frame body, or `None` when it is not one.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut reader = Reader { bytes };
        let response = match reader.byte()? {
            ACK => Response::Ack,
            WRITTEN => Response::Written(reader.version()?),
            VALUES => {
                let count = reader.count(1)?;
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    values.push(reader.optional()?);
                }
                Response::Values {
                    values,
                    context: reader.vector()?,
                }
            }
            VERSIONS => {
                let count = reader.count(25)?;
                let mut versions = Vec::with_capacity(count);
                for _ in 0..count {
                    versions.push(reader.version()?);
                }
                Response::Versions(versions)
            }
            LEAVES => {
                let count = reader.count(16)?;
                let mut leaves = Vec::with_capacity(count);
                for _ in 0..count {
                    leaves.push(reader.u128()?);
                }
                Response::Leaves(leaves)
            }
            KEYS => {
                let count = reader.count(4)?;
                let mut keys = Vec::with_capacity(count);
                for _ in 0..count {
                    keys.push(reader.bytes()?);
                }
                Response::Keys(keys)
            }
            FAILED => Response::Failed(String::from_utf8_lossy(&reader.bytes()?).into_owned()),
            _ => return None,
        };
        reader.finished(response)
    }
}

/// Writes one frame: its length, then `body`.
pub fn write_frame(stream: &mut (impl Write + ?Sized), body: &[u8]) -> io::Result<()> {
    let length = u32::try_from(body.len())
        .ok()
        .filter(|length| *length <= MAX_FRAME_BYTES)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the frame is over 64 MiB"))?;
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(body);
    stream.write_all(&frame)?;
    stream.flush()
}

/// Reads one frame's body; `Ok(None)` when the other side closed cleanly between frames.
pub fn read_frame(stream: &mut (impl Read + ?Sized)) -> io::Result<Option<Vec<u8>>> {
    let mut length = [0u8; 4];
    match stream.read_exact(&mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_be_bytes(length);
    if length > MAX_FRAME_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "the frame is over 64 MiB"));
    }
    let mut body = vec![0u8; length as usize];
    stream.read_exact(&mut body)?;
    Ok(Some(body))
}
