use crate::store::GraphRead;
use crate::{GraphError, GraphResult, Value};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, VecDeque};

const MAX_ALL_PAIRS_NODES: usize = 100_000;
const MAX_LOUVAIN_PASSES: usize = 100;

struct Indexed {
    ids: Vec<u64>,
    neighbours: Vec<Vec<usize>>,
}

fn too_large(what: &str) -> GraphError {
    GraphError::new(
        "Neo.ClientError.Procedure.ProcedureCallFailed",
        format!("{what} reads every pair of nodes and is limited to {MAX_ALL_PAIRS_NODES} nodes"),
    )
}

fn indexed(graph: &GraphRead<'_>, directed: bool) -> GraphResult<Indexed> {
    let ids = graph.node_ids()?;
    let position: HashMap<u64, usize> = ids.iter().enumerate().map(|(at, id)| (*id, at)).collect();
    let mut neighbours = vec![Vec::new(); ids.len()];
    for (at, node) in ids.iter().enumerate() {
        let mut seen = Vec::new();
        for relationship in graph.touching(*node, true)? {
            if let Some(found) = graph.relationship(relationship)?
                && let Some(&end) = position.get(&found.end)
                && end != at
            {
                seen.push(end);
            }
        }
        if !directed {
            for relationship in graph.touching(*node, false)? {
                if let Some(found) = graph.relationship(relationship)?
                    && let Some(&start) = position.get(&found.start)
                    && start != at
                {
                    seen.push(start);
                }
            }
        }
        seen.sort_unstable();
        seen.dedup();
        neighbours[at] = seen;
    }
    Ok(Indexed { ids, neighbours })
}

/// Each node's betweenness centrality (Brandes' algorithm over unweighted shortest paths); in an
/// undirected graph each path is counted once.
pub(crate) fn betweenness_in(graph: &GraphRead<'_>, directed: bool) -> GraphResult<Vec<(u64, f64)>> {
    let Indexed { ids, neighbours } = indexed(graph, directed)?;
    if ids.len() > MAX_ALL_PAIRS_NODES {
        return Err(too_large("betweenness"));
    }
    let count = ids.len();
    let mut centrality = vec![0.0f64; count];
    let mut stack = Vec::with_capacity(count);
    let mut predecessors: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut paths = vec![0.0f64; count];
    let mut distance = vec![-1i64; count];
    let mut dependency = vec![0.0f64; count];
    let mut queue = VecDeque::new();
    for source in 0..count {
        stack.clear();
        for at in 0..count {
            predecessors[at].clear();
            paths[at] = 0.0;
            distance[at] = -1;
            dependency[at] = 0.0;
        }
        paths[source] = 1.0;
        distance[source] = 0;
        queue.push_back(source);
        while let Some(node) = queue.pop_front() {
            stack.push(node);
            for &next in &neighbours[node] {
                if distance[next] < 0 {
                    distance[next] = distance[node] + 1;
                    queue.push_back(next);
                }
                if distance[next] == distance[node] + 1 {
                    paths[next] += paths[node];
                    predecessors[next].push(node);
                }
            }
        }
        while let Some(node) = stack.pop() {
            for &previous in &predecessors[node] {
                dependency[previous] += paths[previous] / paths[node] * (1.0 + dependency[node]);
            }
            if node != source {
                centrality[node] += dependency[node];
            }
        }
    }
    let scale = if directed { 1.0 } else { 0.5 };
    Ok(ids
        .into_iter()
        .zip(centrality)
        .map(|(id, score)| (id, score * scale))
        .collect())
}

/// Each node's closeness centrality, Wasserman and Faust's form for graphs that are not
/// connected: the reachable share of the graph over the mean distance to the reached nodes.
pub(crate) fn closeness_in(graph: &GraphRead<'_>, directed: bool) -> GraphResult<Vec<(u64, f64)>> {
    let Indexed { ids, neighbours } = indexed(graph, directed)?;
    if ids.len() > MAX_ALL_PAIRS_NODES {
        return Err(too_large("closeness"));
    }
    let count = ids.len();
    let mut distance = vec![usize::MAX; count];
    let mut queue = VecDeque::new();
    let mut scores = Vec::with_capacity(count);
    for source in 0..count {
        distance.iter_mut().for_each(|slot| *slot = usize::MAX);
        distance[source] = 0;
        queue.push_back(source);
        let mut reached = 0usize;
        let mut total = 0usize;
        while let Some(node) = queue.pop_front() {
            for &next in &neighbours[node] {
                if distance[next] == usize::MAX {
                    distance[next] = distance[node] + 1;
                    reached += 1;
                    total += distance[next];
                    queue.push_back(next);
                }
            }
        }
        let score = if total == 0 || count < 2 {
            0.0
        } else {
            let reached = reached as f64;
            (reached / total as f64) * (reached / (count - 1) as f64)
        };
        scores.push((ids[source], score));
    }
    Ok(scores)
}

/// Each node's core number: the largest k for which it belongs to the graph's k-core (the part
/// where every node has at least k neighbours), relationships read without direction.
pub(crate) fn k_core_in(graph: &GraphRead<'_>) -> GraphResult<Vec<(u64, u64)>> {
    let Indexed { ids, neighbours } = indexed(graph, false)?;
    let count = ids.len();
    let mut degree: Vec<usize> = neighbours.iter().map(Vec::len).collect();
    let mut core = vec![0usize; count];
    let mut removed = vec![false; count];
    let mut by_degree: BTreeSet<(usize, usize)> =
        (0..count).map(|node| (degree[node], node)).collect();
    let mut current = 0usize;
    while let Some((node_degree, node)) = by_degree.pop_first() {
        current = current.max(node_degree);
        core[node] = current;
        removed[node] = true;
        for &next in &neighbours[node] {
            if removed[next] {
                continue;
            }
            by_degree.remove(&(degree[next], next));
            degree[next] = degree[next].saturating_sub(1);
            by_degree.insert((degree[next], next));
        }
    }
    Ok(ids
        .into_iter()
        .zip(core)
        .map(|(id, core)| (id, core as u64))
        .collect())
}

#[derive(PartialEq)]
struct Frontier {
    cost: f64,
    node: u64,
}

impl Eq for Frontier {}

impl Ord for Frontier {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .cost
            .total_cmp(&self.cost)
            .then_with(|| other.node.cmp(&self.node))
    }
}

impl PartialOrd for Frontier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn weight_of(properties: &[(String, Value)], property: &str) -> GraphResult<f64> {
    match properties.iter().find(|(name, _)| name == property) {
        None => Ok(1.0),
        Some((_, value)) => match value.as_f64() {
            Some(weight) if weight >= 0.0 && weight.is_finite() => Ok(weight),
            _ => Err(GraphError::new(
                "Neo.ClientError.Procedure.ProcedureCallFailed",
                format!("the weight property {property} must hold numbers of 0 or more"),
            )),
        },
    }
}

/// The nodes and total weight of a cheapest path from `from` to `to` (Dijkstra's algorithm), each
/// relationship costing its `property` (1 when it has none).
pub(crate) fn weighted_shortest_path_in(
    graph: &GraphRead<'_>,
    from: u64,
    to: u64,
    property: &str,
    directed: bool,
) -> GraphResult<Option<(Vec<u64>, f64)>> {
    let mut best: HashMap<u64, f64> = HashMap::from([(from, 0.0)]);
    let mut previous: HashMap<u64, u64> = HashMap::new();
    let mut frontier = BinaryHeap::from([Frontier {
        cost: 0.0,
        node: from,
    }]);
    while let Some(Frontier { cost, node }) = frontier.pop() {
        if best.get(&node).is_some_and(|known| cost > *known) {
            continue;
        }
        if node == to {
            let mut path = vec![to];
            let mut at = to;
            while at != from {
                at = previous.get(&at).copied().unwrap_or(from);
                path.push(at);
            }
            path.reverse();
            return Ok(Some((path, cost)));
        }
        let mut steps = Vec::new();
        for relationship in graph.touching(node, true)? {
            if let Some(found) = graph.relationship(relationship)? {
                steps.push((found.end, weight_of(&found.properties, property)?));
            }
        }
        if !directed {
            for relationship in graph.touching(node, false)? {
                if let Some(found) = graph.relationship(relationship)? {
                    steps.push((found.start, weight_of(&found.properties, property)?));
                }
            }
        }
        for (next, weight) in steps {
            let reached = cost + weight;
            if best.get(&next).is_none_or(|known| reached < *known) {
                best.insert(next, reached);
                previous.insert(next, node);
                frontier.push(Frontier {
                    cost: reached,
                    node: next,
                });
            }
        }
    }
    Ok(None)
}

struct Level {
    links: Vec<BTreeMap<usize, f64>>,
    inside: Vec<f64>,
}

impl Level {
    fn degree(&self, node: usize) -> f64 {
        self.links[node].values().sum::<f64>() + self.inside[node]
    }
}

fn local_moves(level: &Level) -> (Vec<usize>, bool) {
    let count = level.links.len();
    let degrees: Vec<f64> = (0..count).map(|node| level.degree(node)).collect();
    let twice_total: f64 = degrees.iter().sum();
    let mut community: Vec<usize> = (0..count).collect();
    let mut totals = degrees.clone();
    let mut moved_any = false;
    if twice_total == 0.0 {
        return (community, false);
    }
    for _ in 0..MAX_LOUVAIN_PASSES {
        let mut moved = false;
        for node in 0..count {
            let own = community[node];
            totals[own] -= degrees[node];
            let mut toward: BTreeMap<usize, f64> = BTreeMap::new();
            for (&next, &weight) in &level.links[node] {
                *toward.entry(community[next]).or_insert(0.0) += weight;
            }
            let gain = |target: usize, links: f64| links - totals[target] * degrees[node] / twice_total;
            let mut best = own;
            let mut best_gain = gain(own, toward.get(&own).copied().unwrap_or(0.0));
            for (&target, &links) in &toward {
                let candidate = gain(target, links);
                if candidate > best_gain + 1e-12 {
                    best = target;
                    best_gain = candidate;
                }
            }
            totals[best] += degrees[node];
            if best != own {
                community[node] = best;
                moved = true;
                moved_any = true;
            }
        }
        if !moved {
            break;
        }
    }
    (community, moved_any)
}

fn aggregated(level: &Level, community: &[usize]) -> (Level, Vec<usize>) {
    let mut renumbered: BTreeMap<usize, usize> = BTreeMap::new();
    for &found in community {
        let next = renumbered.len();
        renumbered.entry(found).or_insert(next);
    }
    let mapping: Vec<usize> = community.iter().map(|found| renumbered[found]).collect();
    let count = renumbered.len();
    let mut links = vec![BTreeMap::new(); count];
    let mut inside = vec![0.0; count];
    for (node, edges) in level.links.iter().enumerate() {
        let from = mapping[node];
        inside[from] += level.inside[node];
        for (&next, &weight) in edges {
            let to = mapping[next];
            if from == to {
                inside[from] += weight;
            } else {
                *links[from].entry(to).or_insert(0.0) += weight;
            }
        }
    }
    (Level { links, inside }, mapping)
}

/// Each node's community by the Louvain method (modularity, relationships read without
/// direction and weight 1), at most `max_levels` aggregation levels; a community is named by its
/// smallest node id. Nodes are visited in id order, so the result is deterministic.
pub(crate) fn louvain_in(graph: &GraphRead<'_>, max_levels: usize) -> GraphResult<Vec<(u64, u64)>> {
    let Indexed { ids, neighbours } = indexed(graph, false)?;
    let mut level = Level {
        links: neighbours
            .iter()
            .map(|next| next.iter().map(|&node| (node, 1.0)).collect())
            .collect(),
        inside: vec![0.0; ids.len()],
    };
    let mut assignment: Vec<usize> = (0..ids.len()).collect();
    for _ in 0..max_levels.max(1) {
        let (community, moved) = local_moves(&level);
        if !moved {
            break;
        }
        let (next, mapping) = aggregated(&level, &community);
        for slot in &mut assignment {
            *slot = mapping[*slot];
        }
        level = next;
    }
    let mut smallest: HashMap<usize, u64> = HashMap::new();
    for (at, &found) in assignment.iter().enumerate() {
        let entry = smallest.entry(found).or_insert(ids[at]);
        *entry = (*entry).min(ids[at]);
    }
    Ok(ids
        .iter()
        .zip(&assignment)
        .map(|(id, found)| (*id, smallest[found]))
        .collect())
}
