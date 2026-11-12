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

/// How A* estimates the cost left from a node to the goal, from the difference of their
/// coordinates, as pgRouting's `heuristic` numbers them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Estimate {
    pub(crate) heuristic: u8,
    pub(crate) factor: f64,
    pub(crate) epsilon: f64,
}

impl Estimate {
    fn left(self, from: (f64, f64), goal: (f64, f64)) -> f64 {
        let (dx, dy) = ((goal.0 - from.0).abs() * self.factor, (goal.1 - from.1).abs() * self.factor);
        let raw = match self.heuristic {
            0 => 0.0,
            1 => dx.max(dy),
            2 => dx.min(dy),
            3 => dx * dx + dy * dy,
            4 => (dx * dx + dy * dy).sqrt(),
            _ => dx + dy,
        };
        raw * self.epsilon
    }
}

fn coordinates(edges_json: &str) -> SqlResult<HashMap<i64, (f64, f64)>> {
    let document = parse_document(edges_json).map_err(|_| invalid("the edges are not JSON"))?;
    let Json::Array(edges) = &document.value else {
        return Err(invalid("the edges are a JSON array of objects"));
    };
    let mut places = HashMap::new();
    for edge in edges {
        let point = |x: &str, y: &str| -> SqlResult<(f64, f64)> {
            match (number(edge, x), number(edge, y)) {
                (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Ok((x, y)),
                _ => Err(invalid("A* needs numeric x1, y1, x2 and y2 on every edge")),
            }
        };
        places.insert(whole(edge, "source")?, point("x1", "y1")?);
        places.insert(whole(edge, "target")?, point("x2", "y2")?);
    }
    Ok(places)
}

/// The path A* finds from `start` to `finish`, guided by each node's coordinates (`x1`, `y1`
/// of an edge's source and `x2`, `y2` of its target), in [`dijkstra`]'s form. With heuristic 0,
/// or an estimate that never overstates the cost left, it is a cheapest path; a larger
/// `epsilon` trades that for speed, as in pgRouting.
pub(crate) fn astar(edges_json: &str, start: i64, finish: i64, directed: bool, estimate: Estimate) -> SqlResult<String> {
    if estimate.heuristic > 5 || !estimate.factor.is_finite() || estimate.factor <= 0.0 || !estimate.epsilon.is_finite() || estimate.epsilon < 1.0 {
        return Err(invalid("the heuristic is 0 to 5, the factor above 0 and epsilon at least 1"));
    }
    let arcs = graph(edges_json, directed)?;
    let places = coordinates(edges_json)?;
    let goal = places.get(&finish).copied().unwrap_or((0.0, 0.0));
    let guess = |node: i64| places.get(&node).map_or(0.0, |place| estimate.left(*place, goal));
    let mut best: HashMap<i64, (f64, Option<(i64, i64, f64)>)> = HashMap::new();
    let mut closed: HashSet<i64> = HashSet::new();
    let mut heap = BinaryHeap::new();
    best.insert(start, (0.0, None));
    heap.push(Frontier { cost: guess(start), node: start });
    while let Some(Frontier { node, .. }) = heap.pop() {
        if !closed.insert(node) {
            continue;
        }
        if node == finish {
            break;
        }
        let spent = best.get(&node).map_or(0.0, |(known, _)| *known);
        for arc in arcs.get(&node).map(Vec::as_slice).unwrap_or_default() {
            let reached = spent + arc.cost;
            let better = best.get(&arc.to).is_none_or(|(known, _)| reached < *known);
            if better {
                best.insert(arc.to, (reached, Some((node, arc.edge, arc.cost))));
                heap.push(Frontier { cost: reached + guess(arc.to), node: arc.to });
            }
        }
    }
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

const MOST_TOUR_NODES: usize = 2_000;
const MOST_IMPROVING_PASSES: usize = 1_000;

fn tour_rows(nodes: &[i64], tour: &[usize], cost: &dyn Fn(usize, usize) -> f64) -> String {
    let mut rows = Vec::with_capacity(tour.len() + 1);
    let mut total = 0.0;
    let mut previous: Option<usize> = None;
    for (seq, &at) in tour.iter().chain(tour.first()).enumerate() {
        let step = previous.map_or(0.0, |from| cost(from, at));
        total += step;
        rows.push(format!(
            "{{\"seq\": {}, \"node\": {}, \"cost\": {step}, \"agg_cost\": {total}}}",
            seq + 1,
            nodes[at]
        ));
        previous = Some(at);
    }
    format!("[{}]", rows.join(", "))
}

/// A short closed tour over `count` places with the costs `cost`, starting at place `start`
/// and visiting `last` just before it returns, when given: the nearest-neighbour tour, then
/// improved by 2-opt moves until none shortens it. Like pgRouting's own, it is a heuristic, not
/// always the shortest tour.
fn short_tour(count: usize, start: usize, last: Option<usize>, cost: &dyn Fn(usize, usize) -> f64) -> Vec<usize> {
    let mut tour = vec![start];
    let mut left: Vec<usize> = (0..count).filter(|place| *place != start && Some(*place) != last).collect();
    while !left.is_empty() {
        let from = tour.last().copied().unwrap_or(start);
        let nearest = left
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| cost(from, **a).total_cmp(&cost(from, **b)).then(a.cmp(b)))
            .map_or(0, |(at, _)| at);
        tour.push(left.swap_remove(nearest));
    }
    if let Some(last) = last.filter(|last| *last != start) {
        tour.push(last);
    }
    let movable_end = if last.is_some() { tour.len().saturating_sub(1) } else { tour.len() };
    for _ in 0..MOST_IMPROVING_PASSES {
        let mut improved = false;
        for i in 1..movable_end.saturating_sub(1) {
            for j in (i + 1)..movable_end {
                let before = tour[i - 1];
                let after = tour.get(j + 1).copied().unwrap_or(tour[0]);
                let current = cost(before, tour[i]) + cost(tour[j], after);
                let swapped = cost(before, tour[j]) + cost(tour[i], after);
                let asymmetric_change: f64 = tour[i..=j].windows(2).map(|pair| cost(pair[1], pair[0]) - cost(pair[0], pair[1])).sum();
                if swapped + asymmetric_change + 1e-9 < current {
                    tour[i..=j].reverse();
                    improved = true;
                }
            }
        }
        if !improved {
            break;
        }
    }
    tour
}

fn place_of(nodes: &[i64], vid: i64, what: &str) -> SqlResult<Option<usize>> {
    if vid == 0 {
        return Ok(None);
    }
    nodes
        .iter()
        .position(|node| *node == vid)
        .map(Some)
        .ok_or_else(|| invalid(&format!("the {what} vertex {vid} is not in the matrix")))
}

/// A travelling salesperson tour over a cost matrix (pgRouting's `pgr_TSP`): `matrix` is a
/// JSON array of `{start_vid, end_vid, agg_cost}`; a missing cost is taken from the opposite
/// direction, as pgRouting does. `start` 0 means the smallest vertex; `last`, when not 0, is
/// visited just before returning. Rows are `{seq, node, cost, agg_cost}`, ending back at the
/// start.
pub(crate) fn tsp(matrix_json: &str, start: i64, last: i64) -> SqlResult<String> {
    let document = parse_document(matrix_json).map_err(|_| invalid("the matrix is not JSON"))?;
    let Json::Array(cells) = &document.value else {
        return Err(invalid("the matrix is a JSON array of objects"));
    };
    let mut costs: HashMap<(i64, i64), f64> = HashMap::new();
    let mut seen: HashSet<i64> = HashSet::new();
    for cell in cells {
        let (from, to) = (whole(cell, "start_vid")?, whole(cell, "end_vid")?);
        let cost = number(cell, "agg_cost").ok_or_else(|| invalid("every cell needs a numeric agg_cost"))?;
        if !cost.is_finite() || cost < 0.0 {
            return Err(invalid("costs must be finite and not negative"));
        }
        seen.insert(from);
        seen.insert(to);
        if from != to {
            costs.insert((from, to), cost);
        }
        if seen.len() > MOST_TOUR_NODES {
            return Err(SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "pgr_TSP visits at most 2,000 vertices"));
        }
    }
    let mut nodes: Vec<i64> = seen.into_iter().collect();
    nodes.sort_unstable();
    if nodes.is_empty() {
        return Ok("[]".to_owned());
    }
    for (a, &from) in nodes.iter().enumerate() {
        for &to in nodes.iter().skip(a + 1) {
            if !costs.contains_key(&(from, to)) && !costs.contains_key(&(to, from)) {
                return Err(invalid(&format!("the matrix has no cost between {from} and {to}")));
            }
        }
    }
    let cost = |from: usize, to: usize| -> f64 {
        if from == to {
            return 0.0;
        }
        let (a, b) = (nodes[from], nodes[to]);
        costs.get(&(a, b)).or_else(|| costs.get(&(b, a))).copied().unwrap_or(f64::INFINITY)
    };
    let start = place_of(&nodes, start, "start")?.unwrap_or(0);
    let last = place_of(&nodes, last, "end")?;
    let tour = short_tour(nodes.len(), start, last, &cost);
    Ok(tour_rows(&nodes, &tour, &cost))
}

/// A tour over points by their Euclidean distances (pgRouting's `pgr_TSPeuclidean`): `points`
/// is a JSON array of `{id, x, y}`.
pub(crate) fn tsp_euclidean(points_json: &str, start: i64, last: i64) -> SqlResult<String> {
    let document = parse_document(points_json).map_err(|_| invalid("the points are not JSON"))?;
    let Json::Array(items) = &document.value else {
        return Err(invalid("the points are a JSON array of objects"));
    };
    if items.len() > MOST_TOUR_NODES {
        return Err(SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "pgr_TSPeuclidean visits at most 2,000 points"));
    }
    let mut points: BTreeMap<i64, (f64, f64)> = BTreeMap::new();
    for item in items {
        let id = whole(item, "id")?;
        let (Some(x), Some(y)) = (number(item, "x"), number(item, "y")) else {
            return Err(invalid("every point needs numeric x and y"));
        };
        if !x.is_finite() || !y.is_finite() {
            return Err(invalid("coordinates must be finite"));
        }
        points.insert(id, (x, y));
    }
    let nodes: Vec<i64> = points.keys().copied().collect();
    let places: Vec<(f64, f64)> = points.values().copied().collect();
    if nodes.is_empty() {
        return Ok("[]".to_owned());
    }
    let cost = |from: usize, to: usize| -> f64 {
        let ((x1, y1), (x2, y2)) = (places[from], places[to]);
        (x1 - x2).hypot(y1 - y2)
    };
    let start = place_of(&nodes, start, "start")?.unwrap_or(0);
    let last = place_of(&nodes, last, "end")?;
    let tour = short_tour(nodes.len(), start, last, &cost);
    Ok(tour_rows(&nodes, &tour, &cost))
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

    #[test]
    fn tours_visit_every_vertex_once_and_return() {
        let square = r#"[{"id": 1, "x": 0, "y": 0}, {"id": 2, "x": 1, "y": 1}, {"id": 3, "x": 1, "y": 0}, {"id": 4, "x": 0, "y": 1}]"#;
        let tour = tsp_euclidean(square, 1, 0).unwrap();
        assert!(tour.contains("\"seq\": 5, \"node\": 1, \"cost\": 1, \"agg_cost\": 4}"), "{tour}");
        let ended = tsp_euclidean(square, 1, 3).unwrap();
        assert!(ended.contains("\"seq\": 4, \"node\": 3"), "{ended}");
        let matrix = r#"[{"start_vid": 1, "end_vid": 2, "agg_cost": 1}, {"start_vid": 2, "end_vid": 3, "agg_cost": 1}, {"start_vid": 1, "end_vid": 3, "agg_cost": 5}]"#;
        let tour = tsp(matrix, 0, 0).unwrap();
        assert!(tour.starts_with("[{\"seq\": 1, \"node\": 1, \"cost\": 0, \"agg_cost\": 0}"), "{tour}");
        assert!(tour.contains("\"agg_cost\": 7}"), "{tour}");
        assert!(tsp(r#"[{"start_vid": 1, "end_vid": 2, "agg_cost": 1}, {"start_vid": 3, "end_vid": 4, "agg_cost": 1}]"#, 0, 0).is_err());
        assert!(tsp(matrix, 9, 0).is_err());
    }

    #[test]
    fn astar_finds_the_cheapest_path_with_an_admissible_estimate() {
        let grid = r#"[{"id": 1, "source": 1, "target": 2, "cost": 1, "x1": 0, "y1": 0, "x2": 1, "y2": 0}, {"id": 2, "source": 2, "target": 3, "cost": 1, "x1": 1, "y1": 0, "x2": 2, "y2": 0}, {"id": 3, "source": 1, "target": 4, "cost": 1, "x1": 0, "y1": 0, "x2": 0, "y2": 1}, {"id": 4, "source": 4, "target": 3, "cost": 5, "x1": 0, "y1": 1, "x2": 2, "y2": 0}]"#;
        let estimate = Estimate { heuristic: 4, factor: 1.0, epsilon: 1.0 };
        assert_eq!(astar(grid, 1, 3, true, estimate).unwrap(), dijkstra(grid, 1, 3, true).unwrap());
        assert_eq!(astar(grid, 3, 1, true, estimate).unwrap(), "[]");
        assert!(astar(grid, 1, 3, true, Estimate { heuristic: 9, ..estimate }).is_err());
        assert!(astar(ROADS, 1, 3, true, estimate).is_err());
    }
}
