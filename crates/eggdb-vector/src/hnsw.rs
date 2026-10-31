use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};

/// How an [`Hnsw`] graph is built and searched, with pgvector's defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswOptions {
    /// Neighbours kept per node on the upper layers (`m`); twice as many on layer 0.
    pub m: usize,
    /// Candidates considered while inserting (`ef_construction`).
    pub ef_construction: usize,
}

impl Default for HnswOptions {
    fn default() -> Self {
        Self { m: 16, ef_construction: 64 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    distance: f64,
    node: usize,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance.total_cmp(&other.distance).then(self.node.cmp(&other.node))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A hierarchical navigable small world graph over vectors of one dimension, searched by
/// Euclidean distance. Each node keeps its caller's id.
#[derive(Debug, Clone)]
pub struct Hnsw {
    options: HnswOptions,
    dimensions: usize,
    vectors: Vec<Vec<f32>>,
    ids: Vec<u64>,
    links: Vec<Vec<Vec<usize>>>,
    entry: Option<usize>,
    seed: u64,
}

fn distance(left: &[f32], right: &[f32]) -> f64 {
    left.iter().zip(right).map(|(a, b)| f64::from(a - b).powi(2)).sum()
}

impl Hnsw {
    /// An empty graph for vectors of `dimensions`.
    pub fn new(dimensions: usize, options: HnswOptions) -> Self {
        Self {
            options: HnswOptions { m: options.m.max(2), ef_construction: options.ef_construction.max(options.m.max(2)) },
            dimensions,
            vectors: Vec::new(),
            ids: Vec::new(),
            links: Vec::new(),
            entry: None,
            seed: 0x2545_f491_4f6c_dd1d,
        }
    }

    /// How many vectors the graph holds.
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    /// Whether the graph is empty.
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    fn random_level(&mut self) -> usize {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        let uniform = ((self.seed >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
        let scale = 1.0 / (self.options.m as f64).ln();
        ((-uniform.ln() * scale) as usize).min(16)
    }

    fn search_layer(&self, query: &[f32], entries: &[usize], ef: usize, layer: usize) -> Vec<Candidate> {
        let mut visited: HashSet<usize> = entries.iter().copied().collect();
        let mut candidates: BinaryHeap<std::cmp::Reverse<Candidate>> = BinaryHeap::new();
        let mut found: BinaryHeap<Candidate> = BinaryHeap::new();
        for &node in entries {
            let candidate = Candidate { distance: distance(query, &self.vectors[node]), node };
            candidates.push(std::cmp::Reverse(candidate));
            found.push(candidate);
        }
        while let Some(std::cmp::Reverse(nearest)) = candidates.pop() {
            if found.len() >= ef && found.peek().is_some_and(|furthest| nearest.distance > furthest.distance) {
                break;
            }
            for &neighbour in self.links[nearest.node].get(layer).map_or(&[][..], Vec::as_slice) {
                if !visited.insert(neighbour) {
                    continue;
                }
                let candidate = Candidate { distance: distance(query, &self.vectors[neighbour]), node: neighbour };
                if found.len() < ef || found.peek().is_some_and(|furthest| candidate.distance < furthest.distance) {
                    candidates.push(std::cmp::Reverse(candidate));
                    found.push(candidate);
                    if found.len() > ef {
                        found.pop();
                    }
                }
            }
        }
        let mut sorted = found.into_vec();
        sorted.sort();
        sorted
    }

    fn prune(&mut self, node: usize, layer: usize, keep: usize) {
        let base = self.vectors[node].clone();
        let mut neighbours: Vec<Candidate> = self.links[node][layer]
            .iter()
            .map(|&other| Candidate { distance: distance(&base, &self.vectors[other]), node: other })
            .collect();
        neighbours.sort();
        neighbours.truncate(keep);
        self.links[node][layer] = neighbours.into_iter().map(|candidate| candidate.node).collect();
    }

    /// Adds `vector` under `id`. Fails when its dimensions differ from the graph's.
    pub fn insert(&mut self, id: u64, vector: Vec<f32>) -> Result<(), crate::VectorError> {
        if vector.len() != self.dimensions {
            return Err(crate::VectorError(format!("expected {} dimensions, not {}", self.dimensions, vector.len())));
        }
        let level = self.random_level();
        let node = self.vectors.len();
        self.vectors.push(vector);
        self.ids.push(id);
        self.links.push(vec![Vec::new(); level + 1]);
        let Some(entry) = self.entry else {
            self.entry = Some(node);
            return Ok(());
        };
        let top = self.links[entry].len() - 1;
        let query = self.vectors[node].clone();
        let mut nearest = vec![entry];
        for layer in (level + 1..=top).rev() {
            nearest = vec![self.search_layer(&query, &nearest, 1, layer)[0].node];
        }
        for layer in (0..=level.min(top)).rev() {
            let found = self.search_layer(&query, &nearest, self.options.ef_construction, layer);
            let keep = if layer == 0 { self.options.m * 2 } else { self.options.m };
            let chosen: Vec<usize> = found.iter().take(self.options.m).map(|candidate| candidate.node).collect();
            for &neighbour in &chosen {
                self.links[neighbour][layer].push(node);
                if self.links[neighbour][layer].len() > keep {
                    self.prune(neighbour, layer, keep);
                }
            }
            self.links[node][layer] = chosen;
            nearest = found.iter().map(|candidate| candidate.node).collect();
        }
        if level > top {
            self.entry = Some(node);
        }
        Ok(())
    }

    /// The ids of about the `k` nearest vectors to `query`, nearest first, with their Euclidean
    /// distances; `ef` (at least `k`) trades speed for recall, as pgvector's `hnsw.ef_search`.
    pub fn search(&self, query: &[f32], k: usize, ef: usize) -> Vec<(u64, f64)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        if query.len() != self.dimensions || k == 0 {
            return Vec::new();
        }
        let top = self.links[entry].len() - 1;
        let mut nearest = vec![entry];
        for layer in (1..=top).rev() {
            nearest = vec![self.search_layer(query, &nearest, 1, layer)[0].node];
        }
        self.search_layer(query, &nearest, ef.max(k), 0)
            .into_iter()
            .take(k)
            .map(|candidate| (self.ids[candidate.node], candidate.distance.sqrt()))
            .collect()
    }
}
