use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, Node, parse_document};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};

const MOST_EDGES: usize = 5_000_000;

#[derive(Debug, Clone, Copy)]
struct Arc {
    edge: i64,
    to: i64,
    cost: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Frontier {
    cost: f64,
    node: i64,
}

impl Eq for Frontier {}

impl Ord for Frontier {
    fn cmp(&self, other: &Self) -> Ordering {
        other.cost.total_cmp(&self.cost).then_with(|| other.node.cmp(&self.node))
    }
}

impl PartialOrd for Frontier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn invalid(message: &str) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("routing: {message}"))
}

fn number(node: &Node, name: &str) -> Option<f64> {
    let Json::Object(members) = &node.value else {
        return None;
    };
    members.iter().find(|(key, _)| key == name).and_then(|(_, value)| match &value.value {
        Json::Number(text) => text.parse().ok(),
        _ => None,
    })
}

fn whole(node: &Node, name: &str) -> SqlResult<i64> {
    let value = number(node, name).ok_or_else(|| invalid(&format!("every edge needs a numeric {name}")))?;
    if value.fract() != 0.0 || value.abs() > 9.0e15 {
        return Err(invalid(&format!("{name} must be a whole number")));
    }
    Ok(value as i64)
}

type Graph = BTreeMap<i64, Vec<Arc>>;

fn graph(edges_json: &str, directed: bool) -> SqlResult<Graph> {
    let document = parse_document(edges_json).map_err(|_| invalid("the edges are not JSON"))?;
    let Json::Array(edges) = &document.value else {
        return Err(invalid("the edges are a JSON array of objects"));
    };
    if edges.len() > MOST_EDGES {
        return Err(SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "routing reads at most 5,000,000 edges"));
    }
    let mut arcs: Graph = BTreeMap::new();
    let mut add = |from: i64, to: i64, edge: i64, cost: f64| arcs.entry(from).or_default().push(Arc { edge, to, cost });
    for edge in edges {
        let (id, source, target) = (whole(edge, "id")?, whole(edge, "source")?, whole(edge, "target")?);
        let cost = number(edge, "cost").ok_or_else(|| invalid("every edge needs a numeric cost"))?;
        let reverse = number(edge, "reverse_cost");
        if !cost.is_finite() || reverse.is_some_and(|reverse| !reverse.is_finite()) {
            return Err(invalid("costs must be finite"));
        }
        if cost >= 0.0 {
            add(source, target, id, cost);
            if !directed {
                add(target, source, id, cost);
            }
        }
        if let Some(reverse) = reverse.filter(|reverse| *reverse >= 0.0) {
            add(target, source, id, reverse);
            if !directed {
                add(source, target, id, reverse);
            }
        }
    }
    for list in arcs.values_mut() {
        list.sort_by(|left, right| left.cost.total_cmp(&right.cost).then(left.edge.cmp(&right.edge)));
    }
    Ok(arcs)
}

fn shortest(arcs: &Graph, start: i64, limit: Option<f64>, stop_at: Option<i64>) -> HashMap<i64, (f64, Option<(i64, i64, f64)>)> {
    let mut best: HashMap<i64, (f64, Option<(i64, i64, f64)>)> = HashMap::new();
    let mut heap = BinaryHeap::new();
    best.insert(start, (0.0, None));
    heap.push(Frontier { cost: 0.0, node: start });
    while let Some(Frontier { cost, node }) = heap.pop() {
        if best.get(&node).is_some_and(|(known, _)| *known < cost) {
            continue;
        }
        if stop_at == Some(node) {
            break;
        }
        for arc in arcs.get(&node).map(Vec::as_slice).unwrap_or_default() {
            let reached = cost + arc.cost;
            if limit.is_some_and(|limit| reached > limit) {
                continue;
            }
            let better = best.get(&arc.to).is_none_or(|(known, via)| reached < *known || (reached == *known && via.is_some_and(|(_, edge, _)| arc.edge < edge)));
            if better {
                best.insert(arc.to, (reached, Some((node, arc.edge, arc.cost))));
                heap.push(Frontier { cost: reached, node: arc.to });
            }
        }
    }
    best
}

/// The cheapest path from `start` to `finish` over `edges` (pgRouting's edge columns `id`,
/// `source`, `target`, `cost` and optional `reverse_cost`; a negative cost means no way), as a
/// JSON array of steps `{node, edge, cost, agg_cost}` ending with `edge` -1 at `finish`; empty
/// when `finish` cannot be reached.
pub(crate) fn dijkstra(edges_json: &str, start: i64, finish: i64, directed: bool) -> SqlResult<String> {
    let arcs = graph(edges_json, directed)?;
    let best = shortest(&arcs, start, None, Some(finish));
    let Some((total, _)) = best.get(&finish) else {
        return Ok("[]".to_owned());
    };
    let mut steps = vec![format!("{{\"node\": {finish}, \"edge\": -1, \"cost\": 0, \"agg_cost\": {total}}}")];
    let mut at = finish;
    while let Some((_, Some((previous, edge, cost)))) = best.get(&at) {
        let reached = best.get(previous).map_or(0.0, |(known, _)| *known);
        steps.push(format!("{{\"node\": {previous}, \"edge\": {edge}, \"cost\": {cost}, \"agg_cost\": {reached}}}"));
        at = *previous;
        if steps.len() > best.len() + 1 {
            return Err(invalid("the path does not end"));
        }
    }
    steps.reverse();
    Ok(format!("[{}]", steps.join(", ")))
}

/// Every node reachable from `start` within `distance`, with the edge it was reached by (-1 for
/// `start`), that edge's cost and the cost from `start`, cheapest first.
pub(crate) fn driving_distance(edges_json: &str, start: i64, distance: f64, directed: bool) -> SqlResult<String> {
    if !distance.is_finite() || distance < 0.0 {
        return Err(invalid("the distance must be a finite number, at least 0"));
    }
    let arcs = graph(edges_json, directed)?;
    let mut reached: Vec<(i64, f64, Option<(i64, i64, f64)>)> = shortest(&arcs, start, Some(distance), None)
        .into_iter()
        .map(|(node, (cost, via))| (node, cost, via))
        .collect();
    reached.sort_by(|left, right| left.1.total_cmp(&right.1).then(left.0.cmp(&right.0)));
    let rows: Vec<String> = reached
        .iter()
        .map(|(node, total, via)| match via {
            Some((_, edge, cost)) => format!("{{\"node\": {node}, \"edge\": {edge}, \"cost\": {cost}, \"agg_cost\": {total}}}"),
            None => format!("{{\"node\": {node}, \"edge\": -1, \"cost\": 0, \"agg_cost\": 0}}"),
        })
        .collect();
    Ok(format!("[{}]", rows.join(", ")))
}

/// The connected components of the undirected graph of `edges`, each node with its component,
/// named by the component's smallest node.
pub(crate) fn connected_components(edges_json: &str) -> SqlResult<String> {
    let arcs = graph(edges_json, false)?;
    let mut component: BTreeMap<i64, i64> = BTreeMap::new();
    let nodes: Vec<i64> = arcs.keys().copied().collect();
    for start in nodes {
        if component.contains_key(&start) {
            continue;
        }
        let mut stack = vec![start];
        let mut members: HashSet<i64> = HashSet::new();
        while let Some(node) = stack.pop() {
            if component.contains_key(&node) || !members.insert(node) {
                continue;
            }
            for arc in arcs.get(&node).map(Vec::as_slice).unwrap_or_default() {
                if !component.contains_key(&arc.to) && !members.contains(&arc.to) {
                    stack.push(arc.to);
                }
            }
        }
        let name = members.iter().copied().min().unwrap_or(start);
        for member in members {
            component.insert(member, name);
        }
    }
    let rows: Vec<String> = component.iter().map(|(node, name)| format!("{{\"component\": {name}, \"node\": {node}}}")).collect();
    Ok(format!("[{}]", rows.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROADS: &str = r#"[{"id": 1, "source": 1, "target": 2, "cost": 1, "reverse_cost": 1}, {"id": 2, "source": 2, "target": 3, "cost": 5, "reverse_cost": -1}, {"id": 3, "source": 2, "target": 4, "cost": 1, "reverse_cost": 1}, {"id": 4, "source": 4, "target": 3, "cost": 1, "reverse_cost": 1}, {"id": 5, "source": 7, "target": 8, "cost": 2}]"#;

    #[test]
    fn cheapest_paths_reach_and_components() {
        assert_eq!(
            dijkstra(ROADS, 1, 3, true).unwrap(),
            r#"[{"node": 1, "edge": 1, "cost": 1, "agg_cost": 0}, {"node": 2, "edge": 3, "cost": 1, "agg_cost": 1}, {"node": 4, "edge": 4, "cost": 1, "agg_cost": 2}, {"node": 3, "edge": -1, "cost": 0, "agg_cost": 3}]"#
        );
        assert_eq!(dijkstra(ROADS, 3, 7, true).unwrap(), "[]");
        assert_eq!(driving_distance(ROADS, 1, 2.0, true).unwrap().matches("\"node\"").count(), 3);
        assert_eq!(
            connected_components(ROADS).unwrap(),
            r#"[{"component": 1, "node": 1}, {"component": 1, "node": 2}, {"component": 1, "node": 3}, {"component": 1, "node": 4}, {"component": 7, "node": 7}, {"component": 7, "node": 8}]"#
        );
        assert!(dijkstra("[{\"id\": 1}]", 1, 2, true).is_err());
    }
}
