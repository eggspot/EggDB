use crate::centrality::{Indexed, indexed, mixed, normalized};
use crate::topology::Topology;
use crate::{GraphError, GraphResult};

const NEGATIVE_SAMPLES: usize = 5;
const NEGATIVE_TABLE_SIZE: usize = 1 << 20;
const STARTING_RATE: f64 = 0.025;
const SMALLEST_RATE: f64 = 0.0001;
const MAX_TRAINING_UPDATES: u128 = 4_000_000_000;

/// How node2vec walks the graph and trains its embeddings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Node2VecSettings {
    pub(crate) dimension: usize,
    pub(crate) walk_length: usize,
    pub(crate) walks_per_node: usize,
    pub(crate) window: usize,
    pub(crate) epochs: usize,
    pub(crate) return_factor: f64,
    pub(crate) in_out_factor: f64,
}

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = mixed(self.0);
        self.0
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound.max(1) as u64) as usize
    }
}

fn step(neighbours: &[Vec<usize>], previous: Option<usize>, current: usize, settings: &Node2VecSettings, random: &mut Random) -> Option<usize> {
    let around = &neighbours[current];
    if around.is_empty() {
        return None;
    }
    let Some(previous) = previous else {
        return Some(around[random.below(around.len())]);
    };
    if settings.return_factor == 1.0 && settings.in_out_factor == 1.0 {
        return Some(around[random.below(around.len())]);
    }
    let weight = |next: usize| {
        if next == previous {
            1.0 / settings.return_factor
        } else if neighbours[previous].binary_search(&next).is_ok() {
            1.0
        } else {
            1.0 / settings.in_out_factor
        }
    };
    let total: f64 = around.iter().map(|next| weight(*next)).sum();
    let mut target = random.unit() * total;
    for &next in around {
        target -= weight(next);
        if target <= 0.0 {
            return Some(next);
        }
    }
    around.last().copied()
}

fn walks(neighbours: &[Vec<usize>], ids: &[u64], settings: &Node2VecSettings) -> Vec<Vec<usize>> {
    let mut out = Vec::with_capacity(ids.len() * settings.walks_per_node);
    for round in 0..settings.walks_per_node {
        for (start, id) in ids.iter().enumerate() {
            let mut random = Random(id.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ round as u64);
            let mut walk = vec![start];
            let mut previous = None;
            while walk.len() < settings.walk_length.max(1) {
                let current = walk[walk.len() - 1];
                let Some(next) = step(neighbours, previous, current, settings, &mut random) else {
                    break;
                };
                previous = Some(current);
                walk.push(next);
            }
            out.push(walk);
        }
    }
    out
}

fn negative_table(walks: &[Vec<usize>], nodes: usize) -> Vec<usize> {
    let mut counts = vec![0u64; nodes];
    for walk in walks {
        for &node in walk {
            counts[node] += 1;
        }
    }
    let weights: Vec<f64> = counts.iter().map(|count| (*count as f64).powf(0.75)).collect();
    let total: f64 = weights.iter().sum();
    let size = NEGATIVE_TABLE_SIZE.min(nodes.saturating_mul(100)).max(1);
    let mut table = Vec::with_capacity(size);
    if total <= 0.0 {
        return (0..nodes).collect();
    }
    for (node, weight) in weights.iter().enumerate() {
        let slots = ((weight / total) * size as f64).round() as usize;
        table.extend(std::iter::repeat_n(node, slots.max(usize::from(*weight > 0.0))));
    }
    table
}

fn sigmoid(value: f64) -> f64 {
    if value > 6.0 {
        1.0
    } else if value < -6.0 {
        0.0
    } else {
        1.0 / (1.0 + (-value).exp())
    }
}

fn train_pair(input: &mut [f64], output: &mut [f64], label: f64, rate: f64, gradient: &mut [f64]) {
    let score: f64 = input.iter().zip(output.iter()).map(|(a, b)| a * b).sum();
    let error = (label - sigmoid(score)) * rate;
    for ((slot, input_value), output_value) in gradient.iter_mut().zip(input.iter()).zip(output.iter_mut()) {
        *slot += error * *output_value;
        *output_value += error * input_value;
    }
}

/// Each node's node2vec embedding (Grover and Leskovec, 2016): `walks_per_node` second-order
/// random walks from every node, biased by the return and in-out factors (both 1 gives
/// DeepWalk's uniform walks), then a skip-gram model with negative sampling trained on the
/// walks by stochastic gradient descent. Relationships are read without direction; a run is
/// repeatable, because every random choice is seeded by node ids.
pub(crate) fn node2vec_in(graph: &dyn Topology, settings: Node2VecSettings) -> GraphResult<Vec<(u64, Vec<f64>)>> {
    let Indexed { ids, neighbours } = indexed(graph, false)?;
    let count = ids.len();
    let dimension = settings.dimension.max(1);
    let walks = walks(&neighbours, &ids, &settings);
    let pairs: u128 = walks.iter().map(|walk| walk.len() as u128).sum::<u128>() * (2 * settings.window.max(1)) as u128;
    let updates = pairs * (NEGATIVE_SAMPLES as u128 + 1) * dimension as u128 * settings.epochs.max(1) as u128;
    if updates > MAX_TRAINING_UPDATES {
        return Err(GraphError::new(
            "Neo.ClientError.Procedure.ProcedureCallFailed",
            "node2vec would train too long on this graph; lower walksPerNode, walkLength, windowSize, epochs or dimension",
        ));
    }
    let mut random = Random(0x6E6F_6465_3276_6563);
    let mut input: Vec<f64> = (0..count * dimension).map(|_| (random.unit() - 0.5) / dimension as f64).collect();
    let mut output = vec![0.0f64; count * dimension];
    let table = negative_table(&walks, count);
    let total_steps = (walks.iter().map(Vec::len).sum::<usize>() * settings.epochs.max(1)).max(1) as f64;
    let mut done_steps = 0usize;
    let mut gradient = vec![0.0f64; dimension];
    for _ in 0..settings.epochs.max(1) {
        for walk in &walks {
            for (position, &center) in walk.iter().enumerate() {
                let rate = (STARTING_RATE * (1.0 - done_steps as f64 / total_steps)).max(SMALLEST_RATE);
                done_steps += 1;
                let reach = 1 + random.below(settings.window.max(1));
                let low = position.saturating_sub(reach);
                let high = (position + reach).min(walk.len() - 1);
                for (other_position, &context) in walk.iter().enumerate().take(high + 1).skip(low) {
                    if other_position == position {
                        continue;
                    }
                    gradient.iter_mut().for_each(|slot| *slot = 0.0);
                    let center_row = center * dimension..(center + 1) * dimension;
                    for sample in 0..=NEGATIVE_SAMPLES {
                        let (target, label) = if sample == 0 {
                            (context, 1.0)
                        } else {
                            let target = table[random.below(table.len())];
                            if target == context {
                                continue;
                            }
                            (target, 0.0)
                        };
                        let target_row = target * dimension..(target + 1) * dimension;
                        train_pair(&mut input[center_row.clone()], &mut output[target_row], label, rate, &mut gradient);
                    }
                    for (slot, change) in input[center_row].iter_mut().zip(&gradient) {
                        *slot += change;
                    }
                }
            }
        }
    }
    Ok(ids
        .into_iter()
        .enumerate()
        .map(|(node, id)| (id, normalized(input[node * dimension..(node + 1) * dimension].to_vec())))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::EdgeList;

    #[test]
    fn nodes_of_one_community_embed_closer_than_nodes_of_two() {
        let mut edges = Vec::new();
        for (a, b) in [(0, 1), (1, 2), (2, 3), (3, 0), (0, 2), (1, 3)] {
            edges.push((a, b));
            edges.push((a + 10, b + 10));
        }
        edges.push((3, 10));
        let graph = EdgeList::new((0..4).chain(10..14), edges);
        let settings = Node2VecSettings {
            dimension: 16,
            walk_length: 20,
            walks_per_node: 20,
            window: 3,
            epochs: 2,
            return_factor: 1.0,
            in_out_factor: 2.0,
        };
        let embedded = node2vec_in(&graph, settings).unwrap();
        let vector = |id: u64| embedded.iter().find(|(node, _)| *node == id).map(|(_, vector)| vector.clone()).unwrap();
        let cosine = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
        assert!(cosine(&vector(0), &vector(1)) > cosine(&vector(0), &vector(12)));
        assert_eq!(node2vec_in(&graph, settings).unwrap(), embedded);
    }
}
