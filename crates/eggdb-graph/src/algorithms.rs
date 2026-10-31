use crate::store::GraphRead;
use crate::{GraphError, GraphResult};
use eggdb_keyspace::Keyspace;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ops::Bound::{Excluded, Unbounded};

const MAX_PAIRS: usize = 1 << 22;

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

fn undirected_sets(graph: &GraphRead<'_>) -> GraphResult<BTreeMap<u64, BTreeSet<u64>>> {
    let mut sets = BTreeMap::new();
    for node in graph.node_ids()? {
        let set: BTreeSet<u64> = neighbours(graph, node, false)?.into_iter().filter(|next| *next != node).collect();
        sets.insert(node, set);
    }
    Ok(sets)
}

/// The nodes of a shortest path from `from` to `to` (following relationship directions when
/// `directed`), at most `max_hops` long, by breadth-first search.
pub fn shortest_path(keyspace: &dyn Keyspace, from: u64, to: u64, directed: bool, max_hops: usize) -> GraphResult<Option<Vec<u64>>> {
    with_graph(keyspace, |graph| shortest_path_in(graph, from, to, directed, max_hops))
}

pub(crate) fn shortest_path_in(graph: &GraphRead<'_>, from: u64, to: u64, directed: bool, max_hops: usize) -> GraphResult<Option<Vec<u64>>> {
    let mut previous: HashMap<u64, u64> = HashMap::new();
    let mut queue = VecDeque::from([(from, 0usize)]);
    previous.insert(from, from);
    while let Some((node, hops)) = queue.pop_front() {
        if node == to {
            let mut path = vec![to];
            let mut at = to;
            while at != from {
                at = previous.get(&at).copied().unwrap_or(from);
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
}

/// Each node with the smallest node id of its weakly connected component.
pub fn connected_components(keyspace: &dyn Keyspace) -> GraphResult<Vec<(u64, u64)>> {
    with_graph(keyspace, connected_components_in)
}

pub(crate) fn connected_components_in(graph: &GraphRead<'_>) -> GraphResult<Vec<(u64, u64)>> {
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
}

/// PageRank over the directed relationships, `iterations` rounds with `damping` (0.85 is the
/// usual value); nodes without outgoing relationships spread their rank evenly.
pub fn pagerank(keyspace: &dyn Keyspace, iterations: usize, damping: f64) -> GraphResult<Vec<(u64, f64)>> {
    with_graph(keyspace, |graph| pagerank_in(graph, iterations, damping))
}

pub(crate) fn pagerank_in(graph: &GraphRead<'_>, iterations: usize, damping: f64) -> GraphResult<Vec<(u64, f64)>> {
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
}

/// Communities by label propagation over the undirected graph (Raghavan, Albert and Kumara,
/// 2007): every node starts with its own id as its label and repeatedly takes the label most of
/// its neighbours carry, visiting nodes in id order and breaking ties by keeping its own label
/// or else taking the largest, so the result is deterministic. Each node comes with its
/// community's label.
pub fn label_propagation(keyspace: &dyn Keyspace, iterations: usize) -> GraphResult<Vec<(u64, u64)>> {
    with_graph(keyspace, |graph| label_propagation_in(graph, iterations))
}

pub(crate) fn label_propagation_in(graph: &GraphRead<'_>, iterations: usize) -> GraphResult<Vec<(u64, u64)>> {
    let sets = undirected_sets(graph)?;
    let mut labels: BTreeMap<u64, u64> = sets.keys().map(|node| (*node, *node)).collect();
    for _ in 0..iterations.min(1_000) {
        let mut changed = false;
        for (node, set) in &sets {
            let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
            for next in set {
                if let Some(label) = labels.get(next) {
                    *counts.entry(*label).or_default() += 1;
                }
            }
            let Some(most) = counts.values().max().copied() else {
                continue;
            };
            let current = labels.get(node).copied();
            let best = match current {
                Some(label) if counts.get(&label) == Some(&most) => label,
                _ => counts.iter().filter(|(_, count)| **count == most).map(|(label, _)| *label).max().unwrap_or(*node),
            };
            if labels.get(node) != Some(&best) {
                labels.insert(*node, best);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    Ok(labels.into_iter().collect())
}

/// How many triangles each node is part of, over the undirected graph.
pub fn triangle_count(keyspace: &dyn Keyspace) -> GraphResult<Vec<(u64, u64)>> {
    with_graph(keyspace, triangle_count_in)
}

pub(crate) fn triangle_count_in(graph: &GraphRead<'_>) -> GraphResult<Vec<(u64, u64)>> {
    let sets = undirected_sets(graph)?;
    Ok(sets
        .iter()
        .map(|(node, set)| {
            let count = set
                .iter()
                .map(|a| set.range((Excluded(*a), Unbounded)).filter(|b| sets.get(a).is_some_and(|of_a| of_a.contains(b))).count() as u64)
                .sum();
            (*node, count)
        })
        .collect())
}

/// Jaccard similarity of the neighbour sets of every pair of nodes that share at least one
/// neighbour, highest first, then by node ids.
pub fn node_similarity(keyspace: &dyn Keyspace) -> GraphResult<Vec<(u64, u64, f64)>> {
    with_graph(keyspace, node_similarity_in)
}

pub(crate) fn node_similarity_in(graph: &GraphRead<'_>) -> GraphResult<Vec<(u64, u64, f64)>> {
    let sets = undirected_sets(graph)?;
    let mut pairs: BTreeSet<(u64, u64)> = BTreeSet::new();
    for set in sets.values() {
        for a in set {
            for b in set.range((Excluded(*a), Unbounded)) {
                pairs.insert((*a, *b));
            }
            if pairs.len() > MAX_PAIRS {
                return Err(GraphError::new("Neo.TransientError.General.MemoryPoolOutOfMemoryError", "node similarity would compare too many pairs"));
            }
        }
    }
    let empty = BTreeSet::new();
    let mut out: Vec<(u64, u64, f64)> = pairs
        .into_iter()
        .map(|(a, b)| {
            let (left, right) = (sets.get(&a).unwrap_or(&empty), sets.get(&b).unwrap_or(&empty));
            let shared = left.intersection(right).count() as f64;
            let all = left.union(right).count() as f64;
            (a, b, if all == 0.0 { 0.0 } else { shared / all })
        })
        .collect();
    out.sort_by(|x, y| y.2.total_cmp(&x.2).then(x.0.cmp(&y.0)).then(x.1.cmp(&y.1)));
    Ok(out)
}
