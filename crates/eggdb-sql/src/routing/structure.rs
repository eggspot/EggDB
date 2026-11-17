use super::{Graph, graph, invalid, shortest};
use crate::error::{SqlError, SqlResult, SqlState};
use std::collections::{BTreeSet, HashMap};

const MOST_ALL_PAIRS_NODES: usize = 4_096;

struct Dense {
    nodes: Vec<i64>,
    links: Vec<Vec<(usize, i64)>>,
}

impl Dense {
    fn of(arcs: &Graph) -> Self {
        let mut named: BTreeSet<i64> = arcs.keys().copied().collect();
        named.extend(arcs.values().flatten().map(|arc| arc.to));
        let nodes: Vec<i64> = named.into_iter().collect();
        let place: HashMap<i64, usize> = nodes.iter().enumerate().map(|(at, node)| (*node, at)).collect();
        let mut links = vec![Vec::new(); nodes.len()];
        for (from, outgoing) in arcs {
            let Some(&from) = place.get(from) else {
                continue;
            };
            for arc in outgoing {
                if let (Some(&to), Some(list)) = (place.get(&arc.to), links.get_mut(from)) {
                    list.push((to, arc.edge));
                }
            }
        }
        Self { nodes, links }
    }

    fn node(&self, at: usize) -> i64 {
        self.nodes.get(at).copied().unwrap_or_default()
    }

    fn outgoing(&self, at: usize) -> &[(usize, i64)] {
        self.links.get(at).map(Vec::as_slice).unwrap_or_default()
    }
}

fn rows(rows: impl Iterator<Item = String>) -> String {
    format!("[{}]", rows.collect::<Vec<_>>().join(", "))
}

/// The strongly connected components of the directed graph of `edges`, each node with its
/// component, named by the component's smallest node, ordered by node.
pub(crate) fn strong_components(edges_json: &str) -> SqlResult<String> {
    let dense = Dense::of(&graph(edges_json, true)?);
    let count = dense.nodes.len();
    let mut order: Vec<Option<usize>> = vec![None; count];
    let mut low = vec![0; count];
    let mut on_stack = vec![false; count];
    let mut stack = Vec::new();
    let mut component = vec![0; count];
    let mut next = 0;
    for root in 0..count {
        if order.get(root).copied().flatten().is_some() {
            continue;
        }
        let mut work: Vec<(usize, usize)> = vec![(root, 0)];
        if let (Some(slot), Some(lowest), Some(held)) = (order.get_mut(root), low.get_mut(root), on_stack.get_mut(root)) {
            *slot = Some(next);
            *lowest = next;
            *held = true;
        }
        next += 1;
        stack.push(root);
        while let Some(&(node, at)) = work.last() {
            if let Some(&(to, _)) = dense.outgoing(node).get(at) {
                if let Some(top) = work.last_mut() {
                    top.1 += 1;
                }
                match order.get(to).copied().flatten() {
                    None => {
                        if let (Some(slot), Some(lowest), Some(held)) = (order.get_mut(to), low.get_mut(to), on_stack.get_mut(to)) {
                            *slot = Some(next);
                            *lowest = next;
                            *held = true;
                        }
                        next += 1;
                        stack.push(to);
                        work.push((to, 0));
                    }
                    Some(seen) if on_stack.get(to).copied().unwrap_or(false) => {
                        if let Some(lowest) = low.get_mut(node) {
                            *lowest = (*lowest).min(seen);
                        }
                    }
                    Some(_) => {}
                }
                continue;
            }
            work.pop();
            let node_low = low.get(node).copied().unwrap_or_default();
            if let Some(&(parent, _)) = work.last()
                && let Some(lowest) = low.get_mut(parent)
            {
                *lowest = (*lowest).min(node_low);
            }
            if Some(node_low) == order.get(node).copied().flatten() {
                let mut members = Vec::new();
                while let Some(member) = stack.pop() {
                    if let Some(held) = on_stack.get_mut(member) {
                        *held = false;
                    }
                    members.push(member);
                    if member == node {
                        break;
                    }
                }
                let name = members.iter().map(|member| dense.node(*member)).min().unwrap_or_default();
                for member in members {
                    if let Some(slot) = component.get_mut(member) {
                        *slot = name;
                    }
                }
            }
        }
    }
    Ok(rows((0..count).map(|at| {
        format!(
            "{{\"component\": {}, \"node\": {}}}",
            component.get(at).copied().unwrap_or_default(),
            dense.node(at)
        )
    })))
}

struct Cuts {
    points: BTreeSet<i64>,
    bridges: BTreeSet<i64>,
}

fn cuts(dense: &Dense) -> Cuts {
    let count = dense.nodes.len();
    let mut order: Vec<Option<usize>> = vec![None; count];
    let mut low = vec![0; count];
    let mut points = BTreeSet::new();
    let mut bridges = BTreeSet::new();
    let mut next = 0;
    for root in 0..count {
        if order.get(root).copied().flatten().is_some() {
            continue;
        }
        if let (Some(slot), Some(lowest)) = (order.get_mut(root), low.get_mut(root)) {
            *slot = Some(next);
            *lowest = next;
        }
        next += 1;
        let mut root_children = 0;
        let mut work: Vec<(usize, Option<i64>, usize)> = vec![(root, None, 0)];
        while let Some(&(node, entered_by, at)) = work.last() {
            if let Some(&(to, edge)) = dense.outgoing(node).get(at) {
                if let Some(top) = work.last_mut() {
                    top.2 += 1;
                }
                if Some(edge) == entered_by {
                    continue;
                }
                match order.get(to).copied().flatten() {
                    None => {
                        if let (Some(slot), Some(lowest)) = (order.get_mut(to), low.get_mut(to)) {
                            *slot = Some(next);
                            *lowest = next;
                        }
                        next += 1;
                        if node == root {
                            root_children += 1;
                        }
                        work.push((to, Some(edge), 0));
                    }
                    Some(seen) => {
                        if let Some(lowest) = low.get_mut(node) {
                            *lowest = (*lowest).min(seen);
                        }
                    }
                }
                continue;
            }
            work.pop();
            let Some(&(parent, _, _)) = work.last() else {
                continue;
            };
            let node_low = low.get(node).copied().unwrap_or_default();
            if let Some(lowest) = low.get_mut(parent) {
                *lowest = (*lowest).min(node_low);
            }
            let parent_order = order.get(parent).copied().flatten().unwrap_or_default();
            if parent != root && node_low >= parent_order {
                points.insert(dense.node(parent));
            }
            if node_low > parent_order
                && let Some(edge) = entered_by
            {
                bridges.insert(edge);
            }
        }
        if root_children > 1 {
            points.insert(dense.node(root));
        }
    }
    Cuts { points, bridges }
}

/// The nodes whose removal disconnects the undirected graph of `edges`, in order.
pub(crate) fn articulation_points(edges_json: &str) -> SqlResult<String> {
    let found = cuts(&Dense::of(&graph(edges_json, false)?));
    Ok(rows(found.points.iter().map(|node| format!("{{\"node\": {node}}}"))))
}

/// The edges whose removal disconnects the undirected graph of `edges`, in order of id; of
/// two parallel edges neither is a bridge.
pub(crate) fn bridges(edges_json: &str) -> SqlResult<String> {
    let found = cuts(&Dense::of(&graph(edges_json, false)?));
    Ok(rows(found.bridges.iter().map(|edge| format!("{{\"edge\": {edge}}}"))))
}

/// The nodes of the directed acyclic graph of `edges` in an order where every edge goes
/// forward, the smallest ready node first.
///
/// # Errors
///
/// A graph with a cycle.
pub(crate) fn topological_sort(edges_json: &str) -> SqlResult<String> {
    let dense = Dense::of(&graph(edges_json, true)?);
    let count = dense.nodes.len();
    let mut incoming = vec![0usize; count];
    for at in 0..count {
        for &(to, _) in dense.outgoing(at) {
            if let Some(slot) = incoming.get_mut(to) {
                *slot += 1;
            }
        }
    }
    let mut ready: BTreeSet<usize> = (0..count).filter(|at| incoming.get(*at) == Some(&0)).collect();
    let mut sorted = Vec::with_capacity(count);
    while let Some(at) = ready.pop_first() {
        sorted.push(dense.node(at));
        for &(to, _) in dense.outgoing(at) {
            if let Some(slot) = incoming.get_mut(to) {
                *slot -= 1;
                if *slot == 0 {
                    ready.insert(to);
                }
            }
        }
    }
    if sorted.len() < count {
        return Err(invalid("the graph has a cycle, so it has no topological order"));
    }
    Ok(rows(sorted.iter().map(|node| format!("{{\"node\": {node}}}"))))
}

/// The cost of the cheapest path between every two different nodes of `edges` that are
/// connected, ordered by start and end, as pgRouting's `pgr_floydWarshall` and `pgr_johnson`
/// give it.
///
/// # Errors
///
/// A graph of more than 4,096 nodes.
pub(crate) fn all_pairs(edges_json: &str, directed: bool) -> SqlResult<String> {
    let arcs = graph(edges_json, directed)?;
    let dense = Dense::of(&arcs);
    if dense.nodes.len() > MOST_ALL_PAIRS_NODES {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            "routing: all-pairs costs take at most 4,096 nodes",
        ));
    }
    let mut found = Vec::new();
    for &start in &dense.nodes {
        let reached = shortest(&arcs, start, None, None);
        let mut ends: Vec<(i64, f64)> = reached
            .iter()
            .filter(|(end, _)| **end != start)
            .map(|(end, (cost, _))| (*end, *cost))
            .collect();
        ends.sort_by_key(|(end, _)| *end);
        found.extend(ends.into_iter().map(|(end, cost)| {
            format!("{{\"start_vid\": {start}, \"end_vid\": {end}, \"agg_cost\": {cost}}}")
        }));
    }
    Ok(rows(found.into_iter()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(id: i64, source: i64, target: i64, cost: f64) -> String {
        format!("{{\"id\": {id}, \"source\": {source}, \"target\": {target}, \"cost\": {cost}}}")
    }

    fn edges(list: &[(i64, i64, i64, f64)]) -> String {
        rows(list.iter().map(|(id, source, target, cost)| edge(*id, *source, *target, *cost)))
    }

    #[test]
    fn strong_components_name_each_cycle_by_its_smallest_node() {
        let graph = edges(&[(1, 1, 2, 1.0), (2, 2, 3, 1.0), (3, 3, 1, 1.0), (4, 3, 4, 1.0), (5, 4, 5, 1.0), (6, 5, 4, 1.0)]);
        assert_eq!(
            strong_components(&graph).unwrap(),
            "[{\"component\": 1, \"node\": 1}, {\"component\": 1, \"node\": 2}, {\"component\": 1, \"node\": 3}, {\"component\": 4, \"node\": 4}, {\"component\": 4, \"node\": 5}]"
        );
    }

    #[test]
    fn articulation_points_and_bridges_of_two_triangles_joined_by_an_edge() {
        let graph = edges(&[
            (1, 1, 2, 1.0),
            (2, 2, 3, 1.0),
            (3, 3, 1, 1.0),
            (4, 3, 4, 1.0),
            (5, 4, 5, 1.0),
            (6, 5, 6, 1.0),
            (7, 6, 4, 1.0),
        ]);
        assert_eq!(articulation_points(&graph).unwrap(), "[{\"node\": 3}, {\"node\": 4}]");
        assert_eq!(bridges(&graph).unwrap(), "[{\"edge\": 4}]");
        let parallel = edges(&[(1, 1, 2, 1.0), (2, 1, 2, 1.0), (3, 2, 3, 1.0)]);
        assert_eq!(bridges(&parallel).unwrap(), "[{\"edge\": 3}]");
        assert_eq!(articulation_points(&parallel).unwrap(), "[{\"node\": 2}]");
    }

    #[test]
    fn topological_order_takes_the_smallest_ready_node_and_refuses_cycles() {
        let graph = edges(&[(1, 5, 2, 1.0), (2, 1, 2, 1.0), (3, 2, 3, 1.0), (4, 1, 3, 1.0)]);
        assert_eq!(
            topological_sort(&graph).unwrap(),
            "[{\"node\": 1}, {\"node\": 5}, {\"node\": 2}, {\"node\": 3}]"
        );
        assert!(topological_sort(&edges(&[(1, 1, 2, 1.0), (2, 2, 1, 1.0)])).is_err());
    }

    #[test]
    fn all_pairs_lists_every_reachable_pair_once() {
        let graph = edges(&[(1, 1, 2, 1.0), (2, 2, 3, 2.0), (3, 1, 3, 5.0)]);
        assert_eq!(
            all_pairs(&graph, true).unwrap(),
            "[{\"start_vid\": 1, \"end_vid\": 2, \"agg_cost\": 1}, {\"start_vid\": 1, \"end_vid\": 3, \"agg_cost\": 3}, {\"start_vid\": 2, \"end_vid\": 3, \"agg_cost\": 2}]"
        );
        assert_eq!(all_pairs(&graph, false).unwrap().matches("start_vid").count(), 6);
    }
}
