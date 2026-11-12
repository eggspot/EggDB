use crate::GraphResult;
use crate::store::GraphRead;
use std::collections::BTreeMap;

/// What the graph algorithms read: node ids and each node's neighbours.
pub trait Topology {
    /// Every node id, in ascending order.
    fn node_ids(&self) -> GraphResult<Vec<u64>>;

    /// The nodes the relationships leaving `node` reach, and with `directed` false also the nodes
    /// whose relationships enter it; one entry per relationship.
    fn neighbours(&self, node: u64, directed: bool) -> GraphResult<Vec<u64>>;
}

impl Topology for GraphRead<'_> {
    fn node_ids(&self) -> GraphResult<Vec<u64>> {
        GraphRead::node_ids(self)
    }

    fn neighbours(&self, node: u64, directed: bool) -> GraphResult<Vec<u64>> {
        let mut out = Vec::new();
        for relationship in self.touching(node, true)? {
            if let Some(found) = self.relationship(relationship)? {
                out.push(found.end);
            }
        }
        if !directed {
            for relationship in self.touching(node, false)? {
                if let Some(found) = self.relationship(relationship)? {
                    out.push(found.start);
                }
            }
        }
        Ok(out)
    }
}

/// A graph held in memory as its nodes and directed edges, for running the algorithms over
/// graphs that are not stored as EggDB's property graph, such as SQL/PGQ graphs over tables.
#[derive(Debug, Clone, Default)]
pub struct EdgeList {
    nodes: Vec<u64>,
    outgoing: BTreeMap<u64, Vec<u64>>,
    incoming: BTreeMap<u64, Vec<u64>>,
}

impl EdgeList {
    /// A graph of `nodes` and the directed `edges` (start, end) between them; an edge whose end
    /// is not one of the nodes is left out.
    pub fn new(nodes: impl IntoIterator<Item = u64>, edges: impl IntoIterator<Item = (u64, u64)>) -> Self {
        let mut nodes: Vec<u64> = nodes.into_iter().collect();
        nodes.sort_unstable();
        nodes.dedup();
        let mut outgoing: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        let mut incoming: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for (start, end) in edges {
            if nodes.binary_search(&start).is_ok() && nodes.binary_search(&end).is_ok() {
                outgoing.entry(start).or_default().push(end);
                incoming.entry(end).or_default().push(start);
            }
        }
        Self { nodes, outgoing, incoming }
    }
}

impl Topology for EdgeList {
    fn node_ids(&self) -> GraphResult<Vec<u64>> {
        Ok(self.nodes.clone())
    }

    fn neighbours(&self, node: u64, directed: bool) -> GraphResult<Vec<u64>> {
        let mut out = self.outgoing.get(&node).cloned().unwrap_or_default();
        if !directed {
            out.extend(self.incoming.get(&node).into_iter().flatten().copied());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;
    use crate::procedures::call_on;

    #[test]
    fn the_algorithms_run_over_an_edge_list() {
        let triangle = EdgeList::new([1, 2, 3, 4], [(1, 2), (2, 3), (3, 1), (3, 9)]);
        assert_eq!(triangle.neighbours(3, true).unwrap(), vec![1]);
        assert_eq!(triangle.neighbours(3, false).unwrap(), vec![1, 2]);
        let (columns, records) = call_on(&triangle, "eggdb.triangleCount", &[]).unwrap();
        assert_eq!(columns, vec!["nodeId", "triangles"]);
        assert_eq!(records[0], vec![Value::Integer(1), Value::Integer(1)]);
        assert_eq!(records[3], vec![Value::Integer(4), Value::Integer(0)]);
        assert!(call_on(&triangle, "eggdb.weightedShortestPath", &[]).is_err());
    }
}
