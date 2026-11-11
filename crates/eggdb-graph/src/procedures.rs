use crate::algorithms::{
    connected_components_in, label_propagation_in, node_similarity_in, pagerank_in,
    shortest_path_in, triangle_count_in,
};
use crate::centrality::{
    betweenness_in, closeness_in, fast_rp_in, k_core_in, leiden_in, louvain_in,
    weighted_shortest_path_in,
};
use crate::store::GraphRead;
use crate::{GraphError, GraphResult, Value};

type Records = (Vec<&'static str>, Vec<Vec<Value>>);

const PROCEDURES: [&str; 13] = [
    "eggdb.fastRP",
    "eggdb.pageRank",
    "eggdb.connectedComponents",
    "eggdb.labelPropagation",
    "eggdb.triangleCount",
    "eggdb.nodeSimilarity",
    "eggdb.shortestPath",
    "eggdb.betweenness",
    "eggdb.closeness",
    "eggdb.kCore",
    "eggdb.louvain",
    "eggdb.leiden",
    "eggdb.weightedShortestPath",
];

fn direction_argument(value: Option<&Value>, default: bool) -> GraphResult<bool> {
    match value {
        None => Ok(default),
        Some(Value::Boolean(flag)) => Ok(*flag),
        Some(_) => Err(argument_error("directed must be true or false")),
    }
}

fn scores(found: Vec<(u64, f64)>) -> Records {
    (
        vec!["nodeId", "score"],
        found
            .into_iter()
            .map(|(node, score)| vec![id(node), Value::Float(score)])
            .collect(),
    )
}

fn argument_error(message: impl Into<String>) -> GraphError {
    GraphError::new("Neo.ClientError.Procedure.ProcedureCallFailed", message)
}

fn id(node: u64) -> Value {
    i64::try_from(node).map_or(Value::Null, Value::Integer)
}

fn node_argument(value: Option<&Value>, what: &str) -> GraphResult<u64> {
    match value {
        Some(Value::Integer(number)) => {
            u64::try_from(*number).map_err(|_| argument_error(format!("{what} must be a node id")))
        }
        Some(Value::Node(node)) => Ok(node.id),
        _ => Err(argument_error(format!(
            "{what} must be a node or a node id"
        ))),
    }
}

fn count_argument(value: Option<&Value>, default: usize, what: &str) -> GraphResult<usize> {
    match value {
        None => Ok(default),
        Some(Value::Integer(number)) if (0..=1_000).contains(number) => Ok(*number as usize),
        _ => Err(argument_error(format!(
            "{what} must be an integer from 0 to 1000"
        ))),
    }
}

fn pairs(name_a: &'static str, name_b: &'static str, found: Vec<(u64, u64)>) -> Records {
    (
        vec![name_a, name_b],
        found.into_iter().map(|(a, b)| vec![id(a), id(b)]).collect(),
    )
}

/// Runs a graph algorithm procedure: its column names and its records.
pub(crate) fn call(
    graph: &GraphRead<'_>,
    procedure: &str,
    arguments: &[Value],
) -> GraphResult<Records> {
    let most = match procedure {
        "eggdb.pageRank" | "eggdb.shortestPath" | "eggdb.weightedShortestPath" => 4,
        "eggdb.labelPropagation" | "eggdb.betweenness" | "eggdb.closeness" | "eggdb.louvain"
        | "eggdb.leiden" => 1,
        "eggdb.fastRP" => 2,
        _ if PROCEDURES.contains(&procedure) => 0,
        _ => {
            return Err(GraphError::new(
                "Neo.ClientError.Procedure.ProcedureNotFound",
                format!(
                    "There is no procedure with the name `{procedure}`; EggDB has {}",
                    PROCEDURES.join(", ")
                ),
            ));
        }
    };
    if arguments.len() > most {
        return Err(argument_error(format!(
            "{procedure} takes at most {most} arguments"
        )));
    }
    Ok(match procedure {
        "eggdb.pageRank" => {
            let iterations = count_argument(arguments.first(), 20, "iterations")?;
            let damping = match arguments.get(1) {
                None => 0.85,
                Some(value) => value
                    .as_f64()
                    .filter(|damping| (0.0..1.0).contains(damping))
                    .ok_or_else(|| argument_error("damping must be a number from 0 up to 1"))?,
            };
            if arguments.len() > 2 {
                return Err(argument_error("eggdb.pageRank takes (iterations, damping)"));
            }
            let ranks = pagerank_in(graph, iterations, damping)?;
            (
                vec!["nodeId", "score"],
                ranks
                    .into_iter()
                    .map(|(node, score)| vec![id(node), Value::Float(score)])
                    .collect(),
            )
        }
        "eggdb.connectedComponents" => {
            pairs("nodeId", "componentId", connected_components_in(graph)?)
        }
        "eggdb.labelPropagation" => pairs(
            "nodeId",
            "communityId",
            label_propagation_in(graph, count_argument(arguments.first(), 10, "iterations")?)?,
        ),
        "eggdb.triangleCount" => pairs("nodeId", "triangles", triangle_count_in(graph)?),
        "eggdb.betweenness" => scores(betweenness_in(
            graph,
            direction_argument(arguments.first(), false)?,
        )?),
        "eggdb.closeness" => scores(closeness_in(
            graph,
            direction_argument(arguments.first(), false)?,
        )?),
        "eggdb.kCore" => pairs("nodeId", "coreNumber", k_core_in(graph)?),
        "eggdb.fastRP" => {
            let dimension = count_argument(arguments.first(), 64, "dimension")?;
            if dimension == 0 {
                return Err(argument_error("dimension must be from 1 to 1000"));
            }
            let iterations = count_argument(arguments.get(1), 3, "iterations")?.min(20);
            (
                vec!["nodeId", "embedding"],
                fast_rp_in(graph, dimension, iterations)?
                    .into_iter()
                    .map(|(node, vector)| {
                        vec![
                            id(node),
                            Value::List(vector.into_iter().map(Value::Float).collect()),
                        ]
                    })
                    .collect(),
            )
        }
        "eggdb.louvain" => pairs(
            "nodeId",
            "communityId",
            louvain_in(graph, count_argument(arguments.first(), 10, "maxLevels")?)?,
        ),
        "eggdb.leiden" => pairs(
            "nodeId",
            "communityId",
            leiden_in(graph, count_argument(arguments.first(), 10, "maxLevels")?)?,
        ),
        "eggdb.weightedShortestPath" => {
            let from = node_argument(arguments.first(), "the start")?;
            let to = node_argument(arguments.get(1), "the end")?;
            let property = match arguments.get(2) {
                None => "weight".to_owned(),
                Some(Value::String(name)) => name.clone(),
                Some(_) => return Err(argument_error("the weight property must be a string")),
            };
            let directed = direction_argument(arguments.get(3), true)?;
            let records = weighted_shortest_path_in(graph, from, to, &property, directed)?
                .map(|(path, cost)| {
                    vec![
                        Value::List(path.iter().map(|node| id(*node)).collect()),
                        Value::Float(cost),
                    ]
                })
                .into_iter()
                .collect();
            (vec!["nodeIds", "totalCost"], records)
        }
        "eggdb.nodeSimilarity" => (
            vec!["node1", "node2", "similarity"],
            node_similarity_in(graph)?
                .into_iter()
                .map(|(a, b, score)| vec![id(a), id(b), Value::Float(score)])
                .collect(),
        ),
        _ => {
            let from = node_argument(arguments.first(), "the start")?;
            let to = node_argument(arguments.get(1), "the end")?;
            let max_hops = count_argument(arguments.get(2), 15, "maxHops")?;
            let directed = match arguments.get(3) {
                None => true,
                Some(Value::Boolean(flag)) => *flag,
                Some(_) => return Err(argument_error("directed must be true or false")),
            };
            let records = shortest_path_in(graph, from, to, directed, max_hops)?
                .map(|path| {
                    vec![
                        Value::List(path.iter().map(|node| id(*node)).collect()),
                        Value::Integer(path.len().saturating_sub(1) as i64),
                    ]
                })
                .into_iter()
                .collect();
            (vec!["nodeIds", "hops"], records)
        }
    })
}
