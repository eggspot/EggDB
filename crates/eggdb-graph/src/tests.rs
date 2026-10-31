use crate::{QueryResult, Value, connected_components, pagerank, run, shortest_path};
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
