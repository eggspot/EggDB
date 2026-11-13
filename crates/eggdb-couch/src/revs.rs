//! Revision ids (`<generation>-<hash>`) and the choice of a document's winning revision, as
//! CouchDB documents them: the highest generation among the leaves that are not deleted, then
//! the greater hash; when every leaf is deleted, the same among the deleted ones.

/// A revision id split into its generation and its hash.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rev {
    /// How many edits deep the revision is, from 1.
    pub generation: u64,
    /// Its hash, as text.
    pub hash: String,
}

impl Rev {
    /// Reads `<generation>-<hash>`, or `None` when the text is not a revision id.
    pub fn parse(text: &str) -> Option<Self> {
        let (generation, hash) = text.split_once('-')?;
        let generation = generation.parse::<u64>().ok().filter(|generation| *generation > 0)?;
        if hash.is_empty() || hash.len() > 128 || !hash.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return None;
        }
        Some(Self { generation, hash: hash.to_owned() })
    }

    /// The revision id as text.
    pub fn text(&self) -> String {
        format!("{}-{}", self.generation, self.hash)
    }
}

/// One known revision of a document: its id, its parent (none for the first, or when the
/// history before it was cut), and whether it deletes the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevNode {
    /// The revision.
    pub rev: Rev,
    /// Its parent.
    pub parent: Option<Rev>,
    /// Whether it is a deletion.
    pub deleted: bool,
}

/// The revisions no other revision names as its parent.
pub fn leaves(nodes: &[RevNode]) -> Vec<&RevNode> {
    nodes
        .iter()
        .filter(|node| !nodes.iter().any(|other| other.parent.as_ref() == Some(&node.rev)))
        .collect()
}

/// The winning revision among `nodes`, as every CouchDB replica picks it.
pub fn winner(nodes: &[RevNode]) -> Option<&RevNode> {
    let leaves = leaves(nodes);
    let live = leaves.iter().filter(|node| !node.deleted).max_by(|a, b| a.rev.cmp(&b.rev));
    live.or_else(|| leaves.iter().max_by(|a, b| a.rev.cmp(&b.rev))).copied()
}

/// The ancestry of `rev` among `nodes`, newest first, `rev` included.
pub fn path_to(nodes: &[RevNode], rev: &Rev) -> Vec<Rev> {
    let mut path = Vec::new();
    let mut current = Some(rev.clone());
    while let Some(at) = current {
        if path.contains(&at) || path.len() > 1_000 {
            break;
        }
        current = nodes.iter().find(|node| node.rev == at).and_then(|node| node.parent.clone());
        path.push(at);
    }
    path
}

/// A new revision's hash: the MD5 of its parent, whether it deletes, and its body.
pub fn new_hash(parent: Option<&Rev>, deleted: bool, body: &str) -> String {
    let mut input = parent.map(Rev::text).unwrap_or_default().into_bytes();
    input.push(u8::from(deleted));
    input.extend_from_slice(body.as_bytes());
    eggdb_crypto::md5_checksum(&input).iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(rev: &str, parent: Option<&str>, deleted: bool) -> RevNode {
        RevNode { rev: Rev::parse(rev).unwrap(), parent: parent.map(|text| Rev::parse(text).unwrap()), deleted }
    }

    #[test]
    fn the_winner_is_the_deepest_live_leaf_then_the_greater_hash() {
        let nodes = vec![node("1-a", None, false), node("2-b", Some("1-a"), false), node("2-c", Some("1-a"), false), node("3-d", Some("2-b"), true)];
        assert_eq!(winner(&nodes).unwrap().rev.text(), "2-c");
        let deleted = vec![node("1-a", None, true), node("1-b", None, true)];
        assert_eq!(winner(&deleted).unwrap().rev.text(), "1-b");
        assert_eq!(path_to(&nodes, &Rev::parse("3-d").unwrap()).iter().map(Rev::text).collect::<Vec<_>>(), vec!["3-d", "2-b", "1-a"]);
        assert_eq!(Rev::parse("0-a"), None);
        assert_eq!(Rev::parse("x-a"), None);
        assert_eq!(Rev::parse("1-"), None);
    }
}
