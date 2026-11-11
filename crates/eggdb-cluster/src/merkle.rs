//! Merkle trees for anti-entropy (D225): two replicas compare one root, then only the subtrees
//! that differ, and exchange only the keys under differing leaves, instead of every key.

const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013B;

/// A 128-bit FNV-1a digest, built up from byte pieces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Digest(u128);

impl Default for Digest {
    fn default() -> Self {
        Self(OFFSET)
    }
}

impl Digest {
    /// Takes in `bytes`, prefixed by their length so pieces can't run together.
    pub fn feed(&mut self, bytes: &[u8]) {
        for byte in (bytes.len() as u64).to_le_bytes().iter().chain(bytes) {
            self.0 ^= u128::from(*byte);
            self.0 = self.0.wrapping_mul(PRIME);
        }
    }

    /// The digest so far.
    pub fn value(self) -> u128 {
        self.0
    }
}

fn digest_of(pieces: &[&[u8]]) -> u128 {
    let mut digest = Digest::default();
    for piece in pieces {
        digest.feed(piece);
    }
    digest.value()
}

/// A Merkle tree over `(key, digest)` entries: keys fall into `2^depth` leaves by a hash of the key,
/// each leaf hashes its entries in key order, and each inner node hashes its two children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerkleTree {
    depth: u32,
    nodes: Vec<u128>,
}

impl MerkleTree {
    /// The leaf `key` falls into in a tree of `depth` levels.
    pub fn leaf_of(key: &[u8], depth: u32) -> usize {
        if depth == 0 {
            return 0;
        }
        (digest_of(&[key]) >> (128 - depth)) as usize
    }

    /// Builds the tree of `depth` levels (at most 20) over `entries`, which must come in key order.
    pub fn build<'k>(depth: u32, entries: impl IntoIterator<Item = (&'k [u8], u128)>) -> Self {
        let depth = depth.min(20);
        let leaves = 1usize << depth;
        let mut digests = vec![Digest::default(); leaves];
        for (key, value) in entries {
            let leaf = &mut digests[Self::leaf_of(key, depth)];
            leaf.feed(key);
            leaf.feed(&value.to_le_bytes());
        }
        let mut nodes = vec![0u128; 2 * leaves];
        for (at, digest) in digests.into_iter().enumerate() {
            nodes[leaves + at] = digest.value();
        }
        for at in (1..leaves).rev() {
            nodes[at] = digest_of(&[&nodes[2 * at].to_le_bytes(), &nodes[2 * at + 1].to_le_bytes()]);
        }
        Self { depth, nodes }
    }

    /// The root digest.
    pub fn root(&self) -> u128 {
        self.nodes.get(1).copied().unwrap_or_else(|| self.nodes.first().copied().unwrap_or(0))
    }

    /// The leaves where this tree and `other` (of the same depth) differ, found by descending only
    /// into differing subtrees.
    pub fn differing_leaves(&self, other: &Self) -> Vec<usize> {
        if self.depth != other.depth {
            return (0..1usize << self.depth.max(other.depth)).collect();
        }
        let leaves = 1usize << self.depth;
        if leaves == 1 {
            return if self.nodes[1] == other.nodes[1] { Vec::new() } else { vec![0] };
        }
        let mut differing = Vec::new();
        let mut stack = vec![1usize];
        while let Some(at) = stack.pop() {
            if self.nodes[at] == other.nodes[at] {
                continue;
            }
            if at >= leaves {
                differing.push(at - leaves);
            } else {
                stack.push(2 * at + 1);
                stack.push(2 * at);
            }
        }
        differing.sort_unstable();
        differing
    }
}
