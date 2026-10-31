use crate::{QueryResult, Value, connected_components, label_propagation, node_similarity, pagerank, run, shortest_path, triangle_count};
use eggdb_keyspace::MemoryKeyspace;

fn query(keyspace: &MemoryKeyspace, text: &str) -> QueryResult {
    run(keyspace, text, &[], false).unwrap_or_else(|error| panic!("{text}: {error}"))
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn seeded() -> MemoryKeyspace {
    let keyspace = MemoryKeyspace::default();
    query(
        &keyspace,
        "CREATE (ada:Person {name: 'Ada', born: 1815}), (grace:Person {name: 'Grace', born: 1906}), (linus:Person {name: 'Linus', born: 1969}),
                (eggdb:Project {name: 'EggDB'})
         CREATE (ada)-[:KNOWS {since: 1840}]->(grace), (grace)-[:KNOWS]->(linus), (linus)-[:WORKS_ON]->(eggdb), (ada)-[:WORKS_ON]->(eggdb)",
    );
    keyspace
}

#[test]
fn match_where_and_return_read_the_graph() {
    let keyspace = seeded();
    let found = query(&keyspace, "MATCH (p:Person)-[:WORKS_ON]->(:Project {name: 'EggDB'}) WHERE p.born < 1900 RETURN p.name AS name");
    assert_eq!(found.columns, ["name"]);
    assert_eq!(found.rows, vec![vec![text("Ada")]]);
    let knows = query(&keyspace, "MATCH (a)-[k:KNOWS]->(b) RETURN a.name, b.name, k.since ORDER BY a.name");
    assert_eq!(knows.columns, ["a.name", "b.name", "k.since"]);
    assert_eq!(knows.rows, vec![vec![text("Ada"), text("Grace"), Value::Integer(1840)], vec![text("Grace"), text("Linus"), Value::Null]]);
    let either = query(&keyspace, "MATCH (:Person {name: 'Grace'})-[:KNOWS]-(other) RETURN other.name ORDER BY other.name");
    assert_eq!(either.rows, vec![vec![text("Ada")], vec![text("Linus")]]);
}

#[test]
fn variable_length_paths_and_aggregation_work() {
    let keyspace = seeded();
    let reach = query(&keyspace, "MATCH (:Person {name: 'Ada'})-[:KNOWS*1..3]->(p) RETURN p.name ORDER BY p.name");
    assert_eq!(reach.rows, vec![vec![text("Grace")], vec![text("Linus")]]);
    let counted = query(&keyspace, "MATCH (p:Person) OPTIONAL MATCH (p)-[:KNOWS]->(friend) RETURN p.name, count(friend) AS friends ORDER BY friends DESC, p.name");
    assert_eq!(counted.rows[0], vec![text("Ada"), Value::Integer(1)]);
    assert_eq!(counted.rows[2], vec![text("Linus"), Value::Integer(0)]);
    let summary = query(&keyspace, "MATCH (p:Person) RETURN count(*) AS people, min(p.born) AS first, collect(p.name) AS names");
    assert_eq!(summary.rows[0][0], Value::Integer(3));
    assert_eq!(summary.rows[0][1], Value::Integer(1815));
    let piped = query(&keyspace, "UNWIND [1, 2, 3] AS n WITH n * 10 AS tens WHERE tens > 10 RETURN sum(tens)");
    assert_eq!(piped.rows, vec![vec![Value::Integer(50)]]);
}

#[test]
fn updates_merge_set_remove_and_delete() {
    let keyspace = seeded();
    query(&keyspace, "MATCH (p:Person {name: 'Linus'}) SET p.born = 1970, p:Engineer REMOVE p.unknown");
    assert_eq!(query(&keyspace, "MATCH (p:Engineer) RETURN p.born").rows, vec![vec![Value::Integer(1970)]]);
    query(&keyspace, "MERGE (t:Tag {name: 'db'})");
    query(&keyspace, "MERGE (t:Tag {name: 'db'})");
    assert_eq!(query(&keyspace, "MATCH (t:Tag) RETURN count(t)").rows, vec![vec![Value::Integer(1)]]);
    let refused = run(&keyspace, "MATCH (p:Person {name: 'Ada'}) DELETE p", &[], false).unwrap_err();
    assert_eq!(refused.code, "Neo.ClientError.Schema.ConstraintValidationFailed");
    query(&keyspace, "MATCH (p:Person {name: 'Ada'}) DETACH DELETE p");
    assert_eq!(query(&keyspace, "MATCH (p:Person) RETURN count(*)").rows, vec![vec![Value::Integer(2)]]);
    assert_eq!(query(&keyspace, "MATCH ()-[k:KNOWS]->() RETURN count(k)").rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn parameters_errors_and_read_only_mode() {
    let keyspace = seeded();
    let found = run(&keyspace, "MATCH (p:Person) WHERE p.born > $after RETURN p.name ORDER BY p.born", &[("after".to_owned(), Value::Integer(1900))], true).unwrap();
    assert_eq!(found.rows, vec![vec![text("Grace")], vec![text("Linus")]]);
    assert_eq!(run(&keyspace, "CREATE (n)", &[], true).unwrap_err().code, "Neo.ClientError.Statement.AccessMode");
    assert_eq!(run(&keyspace, "MATCH (n) RETURN n.x +", &[], false).unwrap_err().code, "Neo.ClientError.Statement.SyntaxError");
    assert_eq!(run(&keyspace, "MATCH (n) RETURN $missing", &[], false).unwrap_err().code, "Neo.ClientError.Statement.ParameterMissing");
    assert!(run(&keyspace, "CREATE (n {bad: {nested: 1}})", &[], false).is_err());
    assert!(run(&keyspace, "RETURN 1 / 0", &[], false).is_err());
    let deep = format!("RETURN {}1{}", "(".repeat(200), ")".repeat(200));
    assert!(run(&keyspace, &deep, &[], false).is_err());
    for hostile in ["MATCH (n", "RETURN 'open", "MATCH (a)-[*0..99]->(b) RETURN a", "RETURN 99999999999999999999", "\u{1F600}"] {
        assert!(run(&keyspace, hostile, &[], false).is_err(), "{hostile}");
    }
    assert_eq!(query(&keyspace, "MATCH (n) RETURN count(n)").rows, vec![vec![Value::Integer(4)]]);
}

#[test]
fn algorithms_find_paths_components_and_ranks() {
    let keyspace = seeded();
    query(&keyspace, "CREATE (:Island {name: 'alone'})");
    let ids = |label: &str, name: &str| match &query(&keyspace, &format!("MATCH (n:{label} {{name: '{name}'}}) RETURN id(n)")).rows[0][0] {
        Value::Integer(id) => *id as u64,
        _ => unreachable!(),
    };
    let (ada, linus, eggdb, alone) = (ids("Person", "Ada"), ids("Person", "Linus"), ids("Project", "EggDB"), ids("Island", "alone"));
    assert_eq!(shortest_path(&keyspace, ada, eggdb, true, 5).unwrap().map(|path| path.len()), Some(2));
    assert_eq!(shortest_path(&keyspace, linus, ada, true, 5).unwrap(), None);
    assert!(shortest_path(&keyspace, linus, ada, false, 5).unwrap().is_some());
    let components = connected_components(&keyspace).unwrap();
    let of = |node: u64| components.iter().find(|(id, _)| *id == node).map(|(_, component)| *component);
    assert_eq!(of(ada), of(eggdb));
    assert_ne!(of(ada), of(alone));
    let ranks = pagerank(&keyspace, 30, 0.85).unwrap();
    let rank_of = |node: u64| ranks.iter().find(|(id, _)| *id == node).map(|(_, rank)| *rank).unwrap_or(0.0);
    assert!(rank_of(eggdb) > rank_of(ada));
    assert!((ranks.iter().map(|(_, rank)| rank).sum::<f64>() - 1.0).abs() < 1e-9);
}

fn two_triangles() -> MemoryKeyspace {
    let keyspace = MemoryKeyspace::default();
    query(
        &keyspace,
        "CREATE (a:N {name: 'a'}), (b:N {name: 'b'}), (c:N {name: 'c'}), (d:N {name: 'd'}), (e:N {name: 'e'}), (f:N {name: 'f'})
         CREATE (a)-[:L]->(b), (b)-[:L]->(c), (c)-[:L]->(a), (d)-[:L]->(e), (e)-[:L]->(f), (f)-[:L]->(d), (c)-[:L]->(d)",
    );
    keyspace
}

#[test]
fn communities_triangles_and_similarity_follow_the_structure() {
    let keyspace = two_triangles();
    let communities = label_propagation(&keyspace, 10).unwrap();
    let names = query(&keyspace, "MATCH (n:N) RETURN id(n), n.name ORDER BY n.name");
    let id_of = |name: &str| names.rows.iter().find(|row| row[1] == text(name)).and_then(|row| match row[0] { Value::Integer(id) => Some(id as u64), _ => None }).unwrap();
    let community = |name: &str| communities.iter().find(|(node, _)| *node == id_of(name)).map(|(_, label)| *label);
    assert_eq!(community("a"), community("b"));
    assert_eq!(community("e"), community("f"));
    assert_ne!(community("a"), community("f"));
    let triangles = triangle_count(&keyspace).unwrap();
    assert!(triangles.iter().all(|(_, count)| *count == 1));
    let similar = node_similarity(&keyspace).unwrap();
    let (a, b) = (id_of("a"), id_of("b"));
    let pair = similar.iter().find(|(x, y, _)| (*x, *y) == (a.min(b), a.max(b))).map(|(_, _, score)| *score);
    assert_eq!(pair, Some(1.0 / 3.0));
}

#[test]
fn call_yields_the_algorithms_as_rows() {
    let keyspace = two_triangles();
    let ranked = query(&keyspace, "CALL eggdb.pageRank(20, 0.85) YIELD nodeId, score MATCH (n) WHERE id(n) = nodeId RETURN n.name AS name ORDER BY score DESC, name LIMIT 2");
    assert_eq!(ranked.rows.len(), 2);
    let standalone = query(&keyspace, "CALL eggdb.triangleCount()");
    assert_eq!(standalone.columns, ["nodeId", "triangles"]);
    assert_eq!(standalone.rows.len(), 6);
    let communities = query(&keyspace, "CALL eggdb.labelPropagation() YIELD communityId AS community RETURN count(DISTINCT community) AS communities");
    assert_eq!(communities.rows, vec![vec![Value::Integer(2)]]);
    let path = query(&keyspace, "MATCH (a:N {name: 'a'}), (f:N {name: 'f'}) CALL eggdb.shortestPath(a, f, 10, false) YIELD hops RETURN hops");
    assert_eq!(path.rows, vec![vec![Value::Integer(3)]]);
    for bad in ["CALL eggdb.noSuch()", "CALL eggdb.pageRank(-1)", "CALL eggdb.triangleCount() YIELD missing RETURN missing", "CALL eggdb.pageRank(1, 2, 3)"] {
        assert!(run(&keyspace, bad, &[], false).is_err(), "{bad}");
    }
}
