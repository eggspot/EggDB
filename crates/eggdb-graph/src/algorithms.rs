use crate::store::GraphRead;
use crate::{GraphError, GraphResult};
use eggdb_keyspace::Keyspace;
use std::collections::{HashMap, VecDeque};

fn with_graph<T>(keyspace: &dyn Keyspace, body: impl FnOnce(&GraphRead<'_>) -> GraphResult<T>) -> GraphResult<T> {
    let mut body = Some(body);
    let mut outcome = None;
    keyspace
        .read(0, &mut |keys| {
            if let Some(body) = body.take() {
                outcome = Some(body(&GraphRead { keys }));
            }
            Ok(())
        })
        .map_err(|message| GraphError::new("Neo.DatabaseError.General.UnknownError", message))?;
    outcome.unwrap_or_else(|| Err(GraphError::new("Neo.DatabaseError.General.UnknownError", "the read did not run")))
}

fn neighbours(graph: &GraphRead<'_>, node: u64, directed: bool) -> GraphResult<Vec<u64>> {
    let mut out = Vec::new();
    for relationship in graph.touching(node, true)? {
        if let Some(found) = graph.relationship(relationship)? {
            out.push(found.end);
        }
    }
    if !directed {
        for relationship in graph.touching(node, false)? {
            if let Some(found) = graph.relationship(relationship)? {
                out.push(found.start);
            }
        }
    }
    Ok(out)
}

/// The nodes of a shortest path from `from` to `to` (following relationship directions when
/// `directed`), at most `max_hops` long, by breadth-first search.
pub fn shortest_path(keyspace: &dyn Keyspace, from: u64, to: u64, directed: bool, max_hops: usize) -> GraphResult<Option<Vec<u64>>> {
    with_graph(keyspace, |graph| {
        let mut previous: HashMap<u64, u64> = HashMap::new();
        let mut queue = VecDeque::from([(from, 0usize)]);
        previous.insert(from, from);
        while let Some((node, hops)) = queue.pop_front() {
            if node == to {
                let mut path = vec![to];
                let mut at = to;
                while at != from {
                    at = previous[&at];
                    path.push(at);
                }
                path.reverse();
                return Ok(Some(path));
            }
            if hops == max_hops {
                continue;
            }
            for next in neighbours(graph, node, directed)? {
                if let std::collections::hash_map::Entry::Vacant(slot) = previous.entry(next) {
                    slot.insert(node);
                    queue.push_back((next, hops + 1));
                }
            }
        }
        Ok(None)
    })
}

/// Each node with the smallest node id of its weakly connected component.
pub fn connected_components(keyspace: &dyn Keyspace) -> GraphResult<Vec<(u64, u64)>> {
    with_graph(keyspace, |graph| {
        let nodes = graph.node_ids()?;
        let mut component: HashMap<u64, u64> = HashMap::new();
        for &start in &nodes {
            if component.contains_key(&start) {
                continue;
            }
            let mut members = vec![start];
            let mut queue = VecDeque::from([start]);
            component.insert(start, start);
            while let Some(node) = queue.pop_front() {
                for next in neighbours(graph, node, false)? {
                    if !component.contains_key(&next) {
                        component.insert(next, start);
                        members.push(next);
                        queue.push_back(next);
                    }
                }
            }
            let least = members.iter().copied().min().unwrap_or(start);
            for member in members {
                component.insert(member, least);
            }
        }
        let mut out: Vec<(u64, u64)> = component.into_iter().collect();
        out.sort_unstable();
        Ok(out)
    })
}

/// PageRank over the directed relationships, `iterations` rounds with `damping` (0.85 is the
/// usual value); nodes without outgoing relationships spread their rank evenly.
pub fn pagerank(keyspace: &dyn Keyspace, iterations: usize, damping: f64) -> GraphResult<Vec<(u64, f64)>> {
    with_graph(keyspace, |graph| {
        let nodes = graph.node_ids()?;
        if nodes.is_empty() {
            return Ok(Vec::new());
        }
        let index: HashMap<u64, usize> = nodes.iter().enumerate().map(|(at, id)| (*id, at)).collect();
        let mut outgoing: Vec<Vec<usize>> = Vec::with_capacity(nodes.len());
        for &node in &nodes {
            outgoing.push(neighbours(graph, node, true)?.into_iter().filter_map(|next| index.get(&next).copied()).collect());
        }
        let count = nodes.len() as f64;
        let mut rank = vec![1.0 / count; nodes.len()];
        for _ in 0..iterations.min(1_000) {
            let dangling: f64 = rank.iter().zip(&outgoing).filter(|(_, targets)| targets.is_empty()).map(|(value, _)| value).sum();
            let mut next = vec![(1.0 - damping) / count + damping * dangling / count; nodes.len()];
            for (source, targets) in outgoing.iter().enumerate() {
                for &target in targets {
                    next[target] += damping * rank[source] / targets.len() as f64;
                }
            }
            rank = next;
        }
        Ok(nodes.into_iter().zip(rank).collect())
    })
}
