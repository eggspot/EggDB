use crate::{
    QueryResult, Value, connected_components, label_propagation, node_similarity, pagerank, run,
    shortest_path, triangle_count,
};
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
    let found = query(
        &keyspace,
        "MATCH (p:Person)-[:WORKS_ON]->(:Project {name: 'EggDB'}) WHERE p.born < 1900 RETURN p.name AS name",
    );
    assert_eq!(found.columns, ["name"]);
    assert_eq!(found.rows, vec![vec![text("Ada")]]);
    let knows = query(
        &keyspace,
        "MATCH (a)-[k:KNOWS]->(b) RETURN a.name, b.name, k.since ORDER BY a.name",
    );
    assert_eq!(knows.columns, ["a.name", "b.name", "k.since"]);
    assert_eq!(
        knows.rows,
        vec![
            vec![text("Ada"), text("Grace"), Value::Integer(1840)],
            vec![text("Grace"), text("Linus"), Value::Null]
        ]
    );
    let either = query(
        &keyspace,
        "MATCH (:Person {name: 'Grace'})-[:KNOWS]-(other) RETURN other.name ORDER BY other.name",
    );
    assert_eq!(either.rows, vec![vec![text("Ada")], vec![text("Linus")]]);
}

#[test]
fn variable_length_paths_and_aggregation_work() {
    let keyspace = seeded();
    let reach = query(
        &keyspace,
        "MATCH (:Person {name: 'Ada'})-[:KNOWS*1..3]->(p) RETURN p.name ORDER BY p.name",
    );
    assert_eq!(reach.rows, vec![vec![text("Grace")], vec![text("Linus")]]);
    let counted = query(
        &keyspace,
        "MATCH (p:Person) OPTIONAL MATCH (p)-[:KNOWS]->(friend) RETURN p.name, count(friend) AS friends ORDER BY friends DESC, p.name",
    );
    assert_eq!(counted.rows[0], vec![text("Ada"), Value::Integer(1)]);
    assert_eq!(counted.rows[2], vec![text("Linus"), Value::Integer(0)]);
    let summary = query(
        &keyspace,
        "MATCH (p:Person) RETURN count(*) AS people, min(p.born) AS first, collect(p.name) AS names",
    );
    assert_eq!(summary.rows[0][0], Value::Integer(3));
    assert_eq!(summary.rows[0][1], Value::Integer(1815));
    let piped = query(
        &keyspace,
        "UNWIND [1, 2, 3] AS n WITH n * 10 AS tens WHERE tens > 10 RETURN sum(tens)",
    );
    assert_eq!(piped.rows, vec![vec![Value::Integer(50)]]);
}

#[test]
fn updates_merge_set_remove_and_delete() {
    let keyspace = seeded();
    query(
        &keyspace,
        "MATCH (p:Person {name: 'Linus'}) SET p.born = 1970, p:Engineer REMOVE p.unknown",
    );
    assert_eq!(
        query(&keyspace, "MATCH (p:Engineer) RETURN p.born").rows,
        vec![vec![Value::Integer(1970)]]
    );
    query(&keyspace, "MERGE (t:Tag {name: 'db'})");
    query(&keyspace, "MERGE (t:Tag {name: 'db'})");
    assert_eq!(
        query(&keyspace, "MATCH (t:Tag) RETURN count(t)").rows,
        vec![vec![Value::Integer(1)]]
    );
    let refused = run(
        &keyspace,
        "MATCH (p:Person {name: 'Ada'}) DELETE p",
        &[],
        false,
    )
    .unwrap_err();
    assert_eq!(
        refused.code,
        "Neo.ClientError.Schema.ConstraintValidationFailed"
    );
    query(&keyspace, "MATCH (p:Person {name: 'Ada'}) DETACH DELETE p");
    assert_eq!(
        query(&keyspace, "MATCH (p:Person) RETURN count(*)").rows,
        vec![vec![Value::Integer(2)]]
    );
    assert_eq!(
        query(&keyspace, "MATCH ()-[k:KNOWS]->() RETURN count(k)").rows,
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn parameters_errors_and_read_only_mode() {
    let keyspace = seeded();
    let found = run(
        &keyspace,
        "MATCH (p:Person) WHERE p.born > $after RETURN p.name ORDER BY p.born",
        &[("after".to_owned(), Value::Integer(1900))],
        true,
    )
    .unwrap();
    assert_eq!(found.rows, vec![vec![text("Grace")], vec![text("Linus")]]);
    assert_eq!(
        run(&keyspace, "CREATE (n)", &[], true).unwrap_err().code,
        "Neo.ClientError.Statement.AccessMode"
    );
    assert_eq!(
        run(&keyspace, "MATCH (n) RETURN n.x +", &[], false)
            .unwrap_err()
            .code,
        "Neo.ClientError.Statement.SyntaxError"
    );
    assert_eq!(
        run(&keyspace, "MATCH (n) RETURN $missing", &[], false)
            .unwrap_err()
            .code,
        "Neo.ClientError.Statement.ParameterMissing"
    );
    assert!(run(&keyspace, "CREATE (n {bad: {nested: 1}})", &[], false).is_err());
    assert!(run(&keyspace, "RETURN 1 / 0", &[], false).is_err());
    let deep = format!("RETURN {}1{}", "(".repeat(200), ")".repeat(200));
    assert!(run(&keyspace, &deep, &[], false).is_err());
    let long_sum = format!("RETURN 1{}", "+1".repeat(100_000));
    assert!(run(&keyspace, &long_sum, &[], false).is_err());
    let long_path = format!("WITH {{a: 1}} AS m RETURN m{}", ".a".repeat(100_000));
    assert!(run(&keyspace, &long_path, &[], false).is_err());
    let fine_sum = format!("RETURN 1{}", "+1".repeat(100));
    assert_eq!(
        query(&keyspace, &fine_sum).rows,
        vec![vec![Value::Integer(101)]]
    );
    for hostile in [
        "MATCH (n",
        "RETURN 'open",
        "MATCH (a)-[*0..99]->(b) RETURN a",
        "RETURN 99999999999999999999",
        "\u{1F600}",
    ] {
        assert!(run(&keyspace, hostile, &[], false).is_err(), "{hostile}");
    }
    assert_eq!(
        query(&keyspace, "MATCH (n) RETURN count(n)").rows,
        vec![vec![Value::Integer(4)]]
    );
}

#[test]
fn algorithms_find_paths_components_and_ranks() {
    let keyspace = seeded();
    query(&keyspace, "CREATE (:Island {name: 'alone'})");
    let ids = |label: &str, name: &str| match &query(
        &keyspace,
        &format!("MATCH (n:{label} {{name: '{name}'}}) RETURN id(n)"),
    )
    .rows[0][0]
    {
        Value::Integer(id) => *id as u64,
        _ => unreachable!(),
    };
    let (ada, linus, eggdb, alone) = (
        ids("Person", "Ada"),
        ids("Person", "Linus"),
        ids("Project", "EggDB"),
        ids("Island", "alone"),
    );
    assert_eq!(
        shortest_path(&keyspace, ada, eggdb, true, 5)
            .unwrap()
            .map(|path| path.len()),
        Some(2)
    );
    assert_eq!(shortest_path(&keyspace, linus, ada, true, 5).unwrap(), None);
    assert!(
        shortest_path(&keyspace, linus, ada, false, 5)
            .unwrap()
            .is_some()
    );
    let components = connected_components(&keyspace).unwrap();
    let of = |node: u64| {
        components
            .iter()
            .find(|(id, _)| *id == node)
            .map(|(_, component)| *component)
    };
    assert_eq!(of(ada), of(eggdb));
    assert_ne!(of(ada), of(alone));
    let ranks = pagerank(&keyspace, 30, 0.85).unwrap();
    let rank_of = |node: u64| {
        ranks
            .iter()
            .find(|(id, _)| *id == node)
            .map(|(_, rank)| *rank)
            .unwrap_or(0.0)
    };
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
    let names = query(
        &keyspace,
        "MATCH (n:N) RETURN id(n), n.name ORDER BY n.name",
    );
    let id_of = |name: &str| {
        names
            .rows
            .iter()
            .find(|row| row[1] == text(name))
            .and_then(|row| match row[0] {
                Value::Integer(id) => Some(id as u64),
                _ => None,
            })
            .unwrap()
    };
    let community = |name: &str| {
        communities
            .iter()
            .find(|(node, _)| *node == id_of(name))
            .map(|(_, label)| *label)
    };
    assert_eq!(community("a"), community("b"));
    assert_eq!(community("e"), community("f"));
    assert_ne!(community("a"), community("f"));
    let triangles = triangle_count(&keyspace).unwrap();
    assert!(triangles.iter().all(|(_, count)| *count == 1));
    let similar = node_similarity(&keyspace).unwrap();
    let (a, b) = (id_of("a"), id_of("b"));
    let pair = similar
        .iter()
        .find(|(x, y, _)| (*x, *y) == (a.min(b), a.max(b)))
        .map(|(_, _, score)| *score);
    assert_eq!(pair, Some(1.0 / 3.0));
}

#[test]
fn call_yields_the_algorithms_as_rows() {
    let keyspace = two_triangles();
    let ranked = query(
        &keyspace,
        "CALL eggdb.pageRank(20, 0.85) YIELD nodeId, score MATCH (n) WHERE id(n) = nodeId RETURN n.name AS name ORDER BY score DESC, name LIMIT 2",
    );
    assert_eq!(ranked.rows.len(), 2);
    let standalone = query(&keyspace, "CALL eggdb.triangleCount()");
    assert_eq!(standalone.columns, ["nodeId", "triangles"]);
    assert_eq!(standalone.rows.len(), 6);
    let communities = query(
        &keyspace,
        "CALL eggdb.labelPropagation() YIELD communityId AS community RETURN count(DISTINCT community) AS communities",
    );
    assert_eq!(communities.rows, vec![vec![Value::Integer(2)]]);
    let path = query(
        &keyspace,
        "MATCH (a:N {name: 'a'}), (f:N {name: 'f'}) CALL eggdb.shortestPath(a, f, 10, false) YIELD hops RETURN hops",
    );
    assert_eq!(path.rows, vec![vec![Value::Integer(3)]]);
    for bad in [
        "CALL eggdb.noSuch()",
        "CALL eggdb.pageRank(-1)",
        "CALL eggdb.triangleCount() YIELD missing RETURN missing",
        "CALL eggdb.pageRank(1, 2, 3)",
    ] {
        assert!(run(&keyspace, bad, &[], false).is_err(), "{bad}");
    }
}

#[test]
fn centrality_communities_cores_and_weighted_paths_come_out_of_call() {
    let keyspace = two_triangles();
    let louvain = query(
        &keyspace,
        "CALL eggdb.louvain() YIELD communityId AS community RETURN count(DISTINCT community) AS communities",
    );
    assert_eq!(louvain.rows, vec![vec![Value::Integer(2)]]);
    let cores = query(
        &keyspace,
        "CALL eggdb.kCore() YIELD coreNumber RETURN max(coreNumber) AS deepest",
    );
    assert_eq!(cores.rows, vec![vec![Value::Integer(2)]]);
    let between = query(
        &keyspace,
        "CALL eggdb.betweenness() YIELD nodeId, score MATCH (n) WHERE id(n) = nodeId RETURN n.name AS name ORDER BY score DESC, name LIMIT 1",
    );
    assert_eq!(between.rows.len(), 1);
    let close = query(&keyspace, "CALL eggdb.closeness()");
    assert_eq!(close.columns, ["nodeId", "score"]);
    assert_eq!(close.rows.len(), 6);
    let weighted = query(
        &keyspace,
        "MATCH (a:N {name: 'a'}), (f:N {name: 'f'}) CALL eggdb.weightedShortestPath(a, f, 'weight', false) YIELD totalCost RETURN totalCost",
    );
    assert_eq!(weighted.rows, vec![vec![Value::Float(3.0)]]);
    assert!(run(&keyspace, "CALL eggdb.closeness(1)", &[], false).is_err());
    let embedded = query(&keyspace, "CALL eggdb.fastRP(8, 2)");
    assert_eq!(embedded.columns, ["nodeId", "embedding"]);
    assert_eq!(embedded.rows.len(), 6);
    assert!(embedded.rows.iter().all(|row| matches!(&row[1], Value::List(items) if items.len() == 8)));
}

#[test]
fn the_longest_operator_chain_runs_on_a_default_thread_and_a_longer_one_is_refused() {
    let outcome = std::thread::spawn(|| {
        let keyspace = MemoryKeyspace::default();
        let chain = |operators: usize| format!("RETURN 1{}", " + 1".repeat(operators));
        let longest = run(&keyspace, &chain(128), &[], false).map(|result| result.rows);
        let longer = run(&keyspace, &chain(129), &[], false).map(|result| result.rows);
        (longest, longer.is_err())
    })
    .join()
    .unwrap();
    assert_eq!(outcome.0.unwrap(), vec![vec![Value::Integer(129)]]);
    assert!(outcome.1);
}

fn movies() -> MemoryKeyspace {
    let keyspace = MemoryKeyspace::default();
    query(
        &keyspace,
        "CREATE (charlie:Person {name: 'Charlie Sheen'}), (martin:Person {name: 'Martin Sheen'}),
                (michael:Person {name: 'Michael Douglas'}), (oliver:Person {name: 'Oliver Stone'}),
                (rob:Person {name: 'Rob Reiner'}), (wallStreet:Movie {title: 'Wall Street'}),
                (president:Movie {title: 'The American President'}),
                (charlie)-[:ACTED_IN]->(wallStreet), (martin)-[:ACTED_IN]->(wallStreet),
                (michael)-[:ACTED_IN]->(wallStreet), (martin)-[:ACTED_IN]->(president),
                (michael)-[:ACTED_IN]->(president), (oliver)-[:DIRECTED]->(wallStreet),
                (rob)-[:DIRECTED]->(president), (charlie)-[:FATHER]->(martin)",
    );
    keyspace
}

fn error_code(keyspace: &MemoryKeyspace, text: &str) -> String {
    match run(keyspace, text, &[], false) {
        Ok(result) => panic!("{text} returned {result:?}"),
        Err(error) => error.code,
    }
}

#[test]
fn named_paths_give_their_nodes_relationships_and_length() {
    let keyspace = movies();
    let found = query(
        &keyspace,
        "MATCH p = (:Person {name: 'Charlie Sheen'})-[:FATHER]->(:Person)-[:ACTED_IN]->(m:Movie)
         RETURN length(p) AS hops, [n IN nodes(p) | coalesce(n.name, n.title)] AS names,
                [r IN relationships(p) | type(r)] AS kinds
         ORDER BY m.title",
    );
    assert_eq!(found.columns, ["hops", "names", "kinds"]);
    assert_eq!(
        found.rows,
        vec![
            vec![
                Value::Integer(2),
                Value::List(vec![
                    text("Charlie Sheen"),
                    text("Martin Sheen"),
                    text("The American President")
                ]),
                Value::List(vec![text("FATHER"), text("ACTED_IN")])
            ],
            vec![
                Value::Integer(2),
                Value::List(vec![
                    text("Charlie Sheen"),
                    text("Martin Sheen"),
                    text("Wall Street")
                ]),
                Value::List(vec![text("FATHER"), text("ACTED_IN")])
            ]
        ]
    );
    let walked_backwards = query(
        &keyspace,
        "MATCH p = (:Movie {title: 'Wall Street'})<-[:DIRECTED]-(director) RETURN p, director.name",
    );
    let [row] = walked_backwards.rows.as_slice() else {
        panic!("{walked_backwards:?}");
    };
    let Value::Path(path) = &row[0] else {
        panic!("{row:?}");
    };
    assert_eq!(path.nodes.len(), 2);
    assert_eq!(path.relationships.len(), 1);
    assert_eq!(path.relationships[0].start, path.nodes[1].id);
    assert_eq!(row[1], text("Oliver Stone"));
    let created = query(
        &keyspace,
        "CREATE p = (:City {name: 'Lyon'})-[:ROAD]->(:City {name: 'Paris'}) RETURN length(p), [n IN nodes(p) | n.name]",
    );
    assert_eq!(
        created.rows,
        vec![vec![
            Value::Integer(1),
            Value::List(vec![text("Lyon"), text("Paris")])
        ]]
    );
}

#[test]
fn shortest_path_and_all_shortest_paths_follow_the_manual() {
    let keyspace = movies();
    let one = query(
        &keyspace,
        "MATCH (martin:Person {name: 'Martin Sheen'}), (oliver:Person {name: 'Oliver Stone'}),
               p = shortestPath((martin)-[*..15]-(oliver))
         RETURN length(p), [n IN nodes(p) | coalesce(n.name, n.title)]",
    );
    assert_eq!(
        one.rows,
        vec![vec![
            Value::Integer(2),
            Value::List(vec![
                text("Martin Sheen"),
                text("Wall Street"),
                text("Oliver Stone")
            ])
        ]]
    );
    let all = query(
        &keyspace,
        "MATCH (martin:Person {name: 'Martin Sheen'}), (michael:Person {name: 'Michael Douglas'}),
               p = allShortestPaths((martin)-[*]-(michael))
         RETURN [n IN nodes(p) | coalesce(n.name, n.title)] AS names ORDER BY names",
    );
    assert_eq!(
        all.rows,
        vec![
            vec![Value::List(vec![
                text("Martin Sheen"),
                text("The American President"),
                text("Michael Douglas")
            ])],
            vec![Value::List(vec![
                text("Martin Sheen"),
                text("Wall Street"),
                text("Michael Douglas")
            ])]
        ]
    );
    let typed = query(
        &keyspace,
        "MATCH (charlie:Person {name: 'Charlie Sheen'}), (rob:Person {name: 'Rob Reiner'}),
               p = shortestPath((charlie)-[:ACTED_IN|DIRECTED*]-(rob))
         RETURN length(p)",
    );
    assert_eq!(typed.rows, vec![vec![Value::Integer(4)]]);
    let any_type = query(
        &keyspace,
        "MATCH (charlie:Person {name: 'Charlie Sheen'}), (rob:Person {name: 'Rob Reiner'})
         MATCH p = shortestPath((charlie)-[rels*]-(rob))
         RETURN length(p), size(rels)",
    );
    assert_eq!(
        any_type.rows,
        vec![vec![Value::Integer(3), Value::Integer(3)]]
    );
    let too_short = query(
        &keyspace,
        "MATCH (charlie:Person {name: 'Charlie Sheen'}), (rob:Person {name: 'Rob Reiner'})
         OPTIONAL MATCH p = shortestPath((charlie)-[*..2]-(rob)) RETURN p",
    );
    assert_eq!(too_short.rows, vec![vec![Value::Null]]);
    assert_eq!(
        error_code(
            &keyspace,
            "MATCH p = shortestPath((a)-->(b)-->(c)) RETURN p"
        ),
        "Neo.ClientError.Statement.SyntaxError"
    );
    assert_eq!(
        error_code(
            &keyspace,
            "MATCH (a), (b), p = shortestPath((a)-[*2..5]-(b)) RETURN p"
        ),
        "Neo.ClientError.Statement.SyntaxError"
    );
}

#[test]
fn merge_runs_on_create_and_on_match_set() {
    let keyspace = movies();
    let created = query(
        &keyspace,
        "MERGE (keanu:Person {name: 'Keanu Reeves'}) ON CREATE SET keanu.created = 1 ON MATCH SET keanu.lastSeen = 1
         RETURN keanu.name, keanu.created, keanu.lastSeen",
    );
    assert_eq!(
        created.rows,
        vec![vec![text("Keanu Reeves"), Value::Integer(1), Value::Null]]
    );
    let matched = query(
        &keyspace,
        "MERGE (keanu:Person {name: 'Keanu Reeves'}) ON CREATE SET keanu.created = 2 ON MATCH SET keanu.lastSeen = 2
         RETURN keanu.name, keanu.created, keanu.lastSeen",
    );
    assert_eq!(
        matched.rows,
        vec![vec![
            text("Keanu Reeves"),
            Value::Integer(1),
            Value::Integer(2)
        ]]
    );
    let found = query(
        &keyspace,
        "MERGE (person:Movie) ON MATCH SET person.found = true RETURN person.title, person.found ORDER BY person.title",
    );
    assert_eq!(
        found.rows,
        vec![
            vec![text("The American President"), Value::Boolean(true)],
            vec![text("Wall Street"), Value::Boolean(true)]
        ]
    );
    for _ in 0..2 {
        let merged = query(
            &keyspace,
            "MATCH (charlie:Person {name: 'Charlie Sheen'}), (wallStreet:Movie {title: 'Wall Street'})
             MERGE (charlie)-[r:ACTED_IN]->(wallStreet) ON MATCH SET r.seen = true
             RETURN charlie.name, type(r), wallStreet.title, r.seen",
        );
        assert_eq!(
            merged.rows,
            vec![vec![
                text("Charlie Sheen"),
                text("ACTED_IN"),
                text("Wall Street"),
                Value::Boolean(true)
            ]]
        );
    }
    let undirected = query(
        &keyspace,
        "MATCH (charlie:Person {name: 'Charlie Sheen'}), (oliver:Person {name: 'Oliver Stone'})
         MERGE (charlie)-[r:KNOWS]-(oliver) ON CREATE SET r.since = 1987 RETURN r.since",
    );
    assert_eq!(undirected.rows, vec![vec![Value::Integer(1987)]]);
    assert_eq!(
        query(
            &keyspace,
            "MATCH (:Person {name: 'Oliver Stone'})-[r:KNOWS]-(:Person {name: 'Charlie Sheen'}) RETURN count(r)"
        )
        .rows,
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn foreach_updates_every_element() {
    let keyspace = MemoryKeyspace::default();
    query(
        &keyspace,
        "CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})-[:KNOWS]->(c:Person {name: 'C'})-[:KNOWS]->(d:Person {name: 'D'})",
    );
    query(
        &keyspace,
        "MATCH p = (start)-[*]->(finish) WHERE start.name = 'A' AND finish.name = 'D'
         FOREACH (n IN nodes(p) | SET n.marked = true)",
    );
    assert_eq!(
        query(&keyspace, "MATCH (n) WHERE n.marked RETURN count(n)").rows,
        vec![vec![Value::Integer(4)]]
    );
    query(
        &keyspace,
        "MATCH (a {name: 'A'})
         FOREACH (name IN ['Mike', 'Carl', 'Bruce'] | CREATE (a)-[:FRIEND]->(:Person {name: name}))",
    );
    assert_eq!(
        query(
            &keyspace,
            "MATCH ({name: 'A'})-[:FRIEND]->(f) RETURN f.name ORDER BY f.name"
        )
        .rows,
        vec![vec![text("Bruce")], vec![text("Carl")], vec![text("Mike")]]
    );
    query(
        &keyspace,
        "FOREACH (x IN [1, 2] | FOREACH (y IN ['a', 'b'] | MERGE (:Pair {x: x, y: y})))
         FOREACH (x IN [1] | MERGE (:Pair {x: x, y: 'a'}))",
    );
    assert_eq!(
        query(&keyspace, "MATCH (p:Pair) RETURN count(p)").rows,
        vec![vec![Value::Integer(4)]]
    );
    query(
        &keyspace,
        "MATCH (p:Pair) WITH collect(p) AS pairs FOREACH (p IN pairs | DELETE p)",
    );
    assert_eq!(
        query(&keyspace, "MATCH (p:Pair) RETURN count(p)").rows,
        vec![vec![Value::Integer(0)]]
    );
    assert_eq!(
        error_code(&keyspace, "FOREACH (x IN [1] | MATCH (n) RETURN n)"),
        "Neo.ClientError.Statement.SyntaxError"
    );
}

#[test]
fn union_and_union_all_combine_results() {
    let keyspace = MemoryKeyspace::default();
    query(
        &keyspace,
        "CREATE (:Actor {name: 'Anthony Hopkins'}), (:Actor {name: 'Helen Mirren'}), (:Actor {name: 'Hitchcock'}), (:Movie {title: 'Hitchcock'})",
    );
    let all = query(
        &keyspace,
        "MATCH (n:Actor) RETURN n.name AS name UNION ALL MATCH (n:Movie) RETURN n.title AS name",
    );
    assert_eq!(all.columns, ["name"]);
    assert_eq!(all.rows.len(), 4);
    let distinct = query(
        &keyspace,
        "MATCH (n:Actor) RETURN n.name AS name UNION MATCH (n:Movie) RETURN n.title AS name",
    );
    let mut names: Vec<Value> = distinct.rows.into_iter().flatten().collect();
    names.sort_by(crate::order);
    assert_eq!(
        names,
        vec![
            text("Anthony Hopkins"),
            text("Helen Mirren"),
            text("Hitchcock")
        ]
    );
    assert_eq!(
        error_code(&keyspace, "RETURN 1 AS a UNION RETURN 2 AS b"),
        "Neo.ClientError.Statement.SyntaxError"
    );
    assert_eq!(
        error_code(
            &keyspace,
            "RETURN 1 AS a UNION RETURN 2 AS a UNION ALL RETURN 3 AS a"
        ),
        "Neo.ClientError.Statement.SyntaxError"
    );
    assert_eq!(
        error_code(&keyspace, "CREATE (:X) UNION RETURN 1 AS a"),
        "Neo.ClientError.Statement.SyntaxError"
    );
    assert_eq!(
        query(&keyspace, "MATCH (n:X) RETURN count(n)").rows,
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn list_and_pattern_comprehensions_follow_the_manual() {
    let keyspace = movies();
    assert_eq!(
        query(
            &keyspace,
            "RETURN [x IN range(0, 10) WHERE x % 2 = 0 | x ^ 3] AS result"
        )
        .rows,
        vec![vec![Value::List(
            [0.0, 8.0, 64.0, 216.0, 512.0, 1000.0]
                .into_iter()
                .map(Value::Float)
                .collect()
        )]]
    );
    assert_eq!(
        query(
            &keyspace,
            "RETURN [x IN range(0, 10) WHERE x % 2 = 0] AS result"
        )
        .rows,
        vec![vec![Value::List(
            [0, 2, 4, 6, 8, 10]
                .into_iter()
                .map(Value::Integer)
                .collect()
        )]]
    );
    assert_eq!(
        query(
            &keyspace,
            "RETURN [x IN [1, 2] | x * 10] AS result, [x IN null | x] AS none"
        )
        .rows,
        vec![vec![
            Value::List(vec![Value::Integer(10), Value::Integer(20)]),
            Value::Null
        ]]
    );
    assert_eq!(
        query(&keyspace, "RETURN [(1 + 2), 4] AS plain").rows,
        vec![vec![Value::List(vec![
            Value::Integer(3),
            Value::Integer(4)
        ])]]
    );
    let titles = query(
        &keyspace,
        "MATCH (a:Person {name: 'Martin Sheen'})
         RETURN [(a)-[:ACTED_IN]->(m:Movie) WHERE m.title CONTAINS 'Street' | m.title] AS street,
                size([(a)-->(m:Movie) | m]) AS movies",
    );
    assert_eq!(
        titles.rows,
        vec![vec![
            Value::List(vec![text("Wall Street")]),
            Value::Integer(2)
        ]]
    );
}

#[test]
fn nested_pattern_comprehensions_parse_in_linear_time() {
    let keyspace = seeded();
    let levels = 40;
    let nested = format!(
        "RETURN {}1{} AS x",
        "[(a)<-[{k: ".repeat(levels),
        "}]-(b)]".repeat(levels)
    );
    let started = std::time::Instant::now();
    let outcome = run(&keyspace, &nested, &[], false);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(outcome.is_err());
    let comprehension = query(
        &keyspace,
        "MATCH (a:Person {name: 'Ada'}) RETURN [(a)-[:KNOWS]->(b) | [(b)-[:KNOWS]->(c) | c.name]] AS x",
    );
    assert_eq!(
        comprehension.rows,
        vec![vec![Value::List(vec![Value::List(vec![text("Linus")])])]]
    );
    assert_eq!(
        query(&keyspace, "WITH 1 AS x RETURN [x = 1, (x) + 1] AS list").rows,
        vec![vec![Value::List(vec![
            Value::Boolean(true),
            Value::Integer(2)
        ])]]
    );
}

#[test]
fn foreach_body_runs_are_bounded_per_query() {
    let keyspace = MemoryKeyspace::default();
    assert_eq!(
        error_code(
            &keyspace,
            "FOREACH (x IN range(1, 3000) | FOREACH (y IN range(1, 3000) | FOREACH (z IN [] | CREATE ())))"
        ),
        "Neo.TransientError.General.MemoryPoolOutOfMemoryError"
    );
    assert_eq!(
        query(&keyspace, "MATCH (n) RETURN count(n)").rows,
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn union_and_distinct_remove_duplicates_quickly() {
    let keyspace = MemoryKeyspace::default();
    let started = std::time::Instant::now();
    let union = query(
        &keyspace,
        "UNWIND range(1, 50000) AS x RETURN x % 10 AS y UNION UNWIND range(1, 50000) AS x RETURN x % 7 AS y",
    );
    assert_eq!(union.rows.len(), 10);
    assert_eq!(union.rows.first(), Some(&vec![Value::Integer(1)]));
    let distinct = query(
        &keyspace,
        "UNWIND range(1, 50000) AS x RETURN DISTINCT x % 13 AS y",
    );
    assert_eq!(distinct.rows.len(), 13);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    for (text, rows) in [
        ("UNWIND [0.0, -0.0] AS x RETURN DISTINCT x", 1),
        ("UNWIND [[0.0], [-0.0]] AS x RETURN DISTINCT x", 1),
        ("UNWIND [{a: 0.0}, {a: -0.0}] AS x RETURN DISTINCT x", 1),
        ("RETURN 0.0 AS x UNION RETURN -0.0 AS x", 1),
        ("UNWIND [0.0, 0.5, '0.0', [0.0]] AS x RETURN DISTINCT x", 4),
    ] {
        assert_eq!(query(&keyspace, text).rows.len(), rows, "{text}");
    }
}

#[test]
fn comprehension_output_is_bounded_per_comprehension() {
    let keyspace = MemoryKeyspace::default();
    assert_eq!(
        error_code(
            &keyspace,
            "RETURN [x IN range(1, 2100) | range(1, 2100)] AS x"
        ),
        "Neo.TransientError.General.MemoryPoolOutOfMemoryError"
    );
    assert_eq!(
        query(
            &keyspace,
            "RETURN size([x IN range(1, 1000) | [x, {a: x}]]) AS x"
        )
        .rows,
        vec![vec![Value::Integer(1000)]]
    );
    query(&keyspace, "CREATE (a:A), (a)-[:R]->(:B), (a)-[:R]->(:B)");
    assert_eq!(
        error_code(
            &keyspace,
            "MATCH (a:A) RETURN [(a)-->(b) | range(1, 2200000)] AS x"
        ),
        "Neo.TransientError.General.MemoryPoolOutOfMemoryError"
    );
    assert_eq!(
        query(
            &keyspace,
            "MATCH (a:A) RETURN [(a)-->(b) | range(1, 3)] AS x"
        )
        .rows
        .len(),
        1
    );
}

#[test]
fn all_shortest_paths_hold_a_bounded_number_of_relationships_per_query() {
    let keyspace = MemoryKeyspace::default();
    query(
        &keyspace,
        "CREATE (:S), (:T) WITH 1 AS one UNWIND range(1, 70) AS i CREATE (:L1), (:L2), (:L3)",
    );
    for text in [
        "MATCH (s:S), (a:L1) CREATE (s)-[:R]->(a)",
        "MATCH (a:L1), (b:L2) CREATE (a)-[:R]->(b)",
        "MATCH (b:L2), (c:L3) CREATE (b)-[:R]->(c)",
        "MATCH (c:L3), (t:T) CREATE (c)-[:R]->(t)",
    ] {
        query(&keyspace, text);
    }
    assert_eq!(
        error_code(
            &keyspace,
            "MATCH p = allShortestPaths((s:S)-[*]->(t:T)) RETURN count(p) AS n"
        ),
        "Neo.TransientError.General.MemoryPoolOutOfMemoryError"
    );
    assert_eq!(
        query(
            &keyspace,
            "MATCH p = shortestPath((s:S)-[*]->(t:T)) RETURN length(p) AS n"
        )
        .rows,
        vec![vec![Value::Integer(4)]]
    );
    assert_eq!(
        query(
            &keyspace,
            "MATCH (b:L2) WITH b LIMIT 1 MATCH p = allShortestPaths((s:S)-[*]->(b)) RETURN count(p) AS n"
        )
        .rows,
        vec![vec![Value::Integer(70)]]
    );
}

#[test]
fn a_path_variable_is_declared_once_and_deleting_paths_checks_nodes_after_the_clause() {
    let keyspace = seeded();
    assert_eq!(
        error_code(
            &keyspace,
            "MATCH p = (a:Person)-->(b) MATCH p = (c:Project)<--(d) RETURN p"
        ),
        "Neo.ClientError.Statement.SyntaxError"
    );
    assert_eq!(
        error_code(
            &keyspace,
            "MATCH p = (a:Person {name: 'Ada'})-->(b) CREATE p = (:X)-[:R]->(:Y)"
        ),
        "Neo.ClientError.Statement.SyntaxError"
    );
    assert_eq!(
        error_code(
            &keyspace,
            "MATCH (n:Nope) CREATE p = shortestPath((a)-[:R]->(b))"
        ),
        "Neo.ClientError.Statement.SyntaxError"
    );
    let shared = MemoryKeyspace::default();
    query(&shared, "CREATE (a:P)-[:R]->(:Q), (a)-[:R]->(:Q)");
    query(&shared, "MATCH p = (:P)-->() DELETE p");
    assert_eq!(
        query(&shared, "MATCH (n) RETURN count(n)").rows,
        vec![vec![Value::Integer(0)]]
    );
    query(&shared, "CREATE (a:P)-[:R]->(:Q)");
    assert_eq!(
        error_code(&shared, "MATCH (a:P) DELETE a"),
        "Neo.ClientError.Schema.ConstraintValidationFailed"
    );
}

fn labelled() -> MemoryKeyspace {
    let keyspace = MemoryKeyspace::default();
    query(
        &keyspace,
        "CREATE (hub:Hub), (ab:A:B {name: 'ab'}), (a:A {name: 'a'}), (b:B {name: 'b'}), (c:C {name: 'c'}),
                (hub)-[:T]->(ab), (hub)-[:U]->(a), (hub)-[:V]->(b), (hub)-[:T]->(c)",
    );
    keyspace
}

fn names(result: QueryResult) -> Vec<Value> {
    result.rows.into_iter().flatten().collect()
}

#[test]
fn label_predicates_test_nodes_and_relationships_in_expressions() {
    let keyspace = labelled();
    assert_eq!(
        names(query(&keyspace, "MATCH (n) WHERE n:A:B RETURN n.name")),
        vec![text("ab")]
    );
    assert_eq!(
        names(query(
            &keyspace,
            "MATCH (n) WHERE n:A|B RETURN n.name ORDER BY n.name"
        )),
        vec![text("a"), text("ab"), text("b")]
    );
    assert_eq!(
        query(
            &keyspace,
            "MATCH (n) WHERE n.name IS NOT NULL RETURN n.name, n:C AS c, NOT n:A AS notA ORDER BY n.name"
        )
        .rows,
        vec![
            vec![text("a"), Value::Boolean(false), Value::Boolean(false)],
            vec![text("ab"), Value::Boolean(false), Value::Boolean(false)],
            vec![text("b"), Value::Boolean(false), Value::Boolean(true)],
            vec![text("c"), Value::Boolean(true), Value::Boolean(true)],
        ]
    );
    assert_eq!(
        names(query(
            &keyspace,
            "MATCH (:Hub)-[r]->(m) WHERE r:T|U RETURN m.name ORDER BY m.name"
        )),
        vec![text("a"), text("ab"), text("c")]
    );
    assert_eq!(
        names(query(
            &keyspace,
            "MATCH (:Hub)-[r]->(m) RETURN r:V AS v ORDER BY m.name"
        )),
        vec![
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(false)
        ]
    );
    assert_eq!(
        names(query(
            &keyspace,
            "OPTIONAL MATCH (n:Nothing) RETURN n:A AS x"
        )),
        vec![Value::Null]
    );
    assert_eq!(
        error_code(&keyspace, "RETURN 1:A AS x"),
        "Neo.ClientError.Statement.TypeError"
    );
    for mixed in [
        "MATCH (n) WHERE n:A:B|C RETURN n",
        "MATCH (n) WHERE n:A|B:C RETURN n",
        "MATCH (n) WHERE n:A|:B RETURN n",
        "MATCH (n:A:B|C) RETURN n",
    ] {
        assert_eq!(
            error_code(&keyspace, mixed),
            "Neo.ClientError.Statement.SyntaxError",
            "{mixed}"
        );
    }
}

#[test]
fn label_alternatives_match_in_patterns_and_are_refused_when_creating() {
    let keyspace = labelled();
    assert_eq!(
        names(query(&keyspace, "MATCH (n:A|C) RETURN count(n)")),
        vec![Value::Integer(3)]
    );
    assert_eq!(
        names(query(&keyspace, "MATCH (:Hub)-[r:T|U]->() RETURN count(r)")),
        vec![Value::Integer(3)]
    );
    for refused in [
        "CREATE (n:A|B)",
        "MERGE (n:A|B {name: 'x'})",
        "FOREACH (x IN [1] | CREATE (:A|B))",
    ] {
        assert_eq!(
            error_code(&keyspace, refused),
            "Neo.ClientError.Statement.SyntaxError",
            "{refused}"
        );
    }
    assert_eq!(
        names(query(&keyspace, "MATCH (n) RETURN count(n)")),
        vec![Value::Integer(5)]
    );
}

#[test]
fn a_bar_after_a_label_ends_a_comprehension_predicate_or_foreach_list() {
    let keyspace = labelled();
    let found = "MATCH (n) WHERE n.name IS NOT NULL WITH collect(n) AS found ";
    assert_eq!(
        names(query(
            &keyspace,
            &format!("{found}RETURN [x IN found WHERE x:C | x.name] AS names")
        )),
        vec![Value::List(vec![text("c")])]
    );
    assert_eq!(
        names(query(
            &keyspace,
            &format!("{found}RETURN [x IN found WHERE x:A:B | x.name] AS names")
        )),
        vec![Value::List(vec![text("ab")])]
    );
    assert_eq!(
        names(query(
            &keyspace,
            &format!("{found}RETURN size([x IN found WHERE (x:A|B) | x.name]) AS n")
        )),
        vec![Value::Integer(3)]
    );
    assert_eq!(
        names(query(
            &keyspace,
            &format!("{found}RETURN size([b IN [x IN found | x:A|B] WHERE b]) AS n")
        )),
        vec![Value::Integer(3)]
    );
    assert_eq!(
        error_code(
            &keyspace,
            &format!("{found}RETURN [x IN found WHERE x:A|B | x.name] AS names")
        ),
        "Neo.ClientError.Statement.SyntaxError"
    );
    assert_eq!(
        names(query(
            &keyspace,
            "MATCH (h:Hub) RETURN size([(h)-->(m) WHERE m:B | m.name]) AS b,
                    size([(h)-->(m) WHERE (m:A|C) | m.name]) AS ac,
                    size([(h)-->(m:A|C) | m]) AS pattern"
        )),
        vec![Value::Integer(2), Value::Integer(3), Value::Integer(3)]
    );
    query(
        &keyspace,
        &format!("{found}FOREACH (x IN [y IN found WHERE (y:A|B)] | SET x.flag = true)"),
    );
    assert_eq!(
        names(query(&keyspace, "MATCH (n) WHERE n.flag RETURN count(n)")),
        vec![Value::Integer(3)]
    );
    query(
        &keyspace,
        &format!("{found}FOREACH (x IN found | SET x.either = x:A|C)"),
    );
    assert_eq!(
        names(query(
            &keyspace,
            "MATCH (n) WHERE n.either RETURN n.name ORDER BY n.name"
        )),
        vec![text("a"), text("ab"), text("c")]
    );
}

fn people() -> MemoryKeyspace {
    let keyspace = MemoryKeyspace::default();
    query(
        &keyspace,
        "CREATE (a:Person:Child {age: 20, name: 'Alice'}), (b:Person {age: 27, name: 'Bob'}),
                (c:Person:Parent {age: 65, name: 'Charlie'}), (d:Person {age: 30, name: 'Dora'})
         CREATE (a)-[:FRIEND_OF]->(b), (a)-[:CHILD_OF]->(c), (:Counter {count: 0})",
    );
    keyspace
}

#[test]
fn call_subqueries_import_variables_and_return_rows_for_each_row() {
    let keyspace = people();
    assert_eq!(
        query(
            &keyspace,
            "UNWIND [0, 1, 2] AS x CALL { WITH x RETURN x * 10 AS y } RETURN x, y"
        )
        .rows,
        vec![
            vec![Value::Integer(0), Value::Integer(0)],
            vec![Value::Integer(1), Value::Integer(10)],
            vec![Value::Integer(2), Value::Integer(20)],
        ]
    );
    assert_eq!(
        query(
            &keyspace,
            "CALL {
               MATCH (p:Person) RETURN p ORDER BY p.age ASC LIMIT 1
               UNION
               MATCH (p:Person) RETURN p ORDER BY p.age DESC LIMIT 1
             }
             RETURN p.name, p.age ORDER BY p.name"
        )
        .rows,
        vec![
            vec![text("Alice"), Value::Integer(20)],
            vec![text("Charlie"), Value::Integer(65)],
        ]
    );
    assert_eq!(
        query(
            &keyspace,
            "MATCH (p:Person)
             CALL { WITH p MATCH (p)-[:FRIEND_OF]-(c:Person) RETURN c.name AS friend }
             RETURN p.name AS name, friend ORDER BY name"
        )
        .rows,
        vec![
            vec![text("Alice"), text("Bob")],
            vec![text("Bob"), text("Alice")],
        ]
    );
    assert_eq!(
        query(
            &keyspace,
            "UNWIND [1, 2, 3] AS x
             CALL { MATCH (n:Counter) SET n.count = n.count + 1 RETURN n.count AS innerCount }
             WITH innerCount MATCH (n:Counter) RETURN innerCount, n.count AS totalCount"
        )
        .rows,
        vec![
            vec![Value::Integer(1), Value::Integer(3)],
            vec![Value::Integer(2), Value::Integer(3)],
            vec![Value::Integer(3), Value::Integer(3)],
        ]
    );
    assert_eq!(
        names(query(
            &keyspace,
            "WITH 1 AS a, 2 AS b CALL { WITH * RETURN a + b AS c } RETURN c"
        )),
        vec![Value::Integer(3)]
    );
    assert_eq!(
        names(query(
            &keyspace,
            "UNWIND [1, 2] AS x CALL { WITH x CALL { WITH x RETURN x * 2 AS y } RETURN y * 10 AS z } RETURN z"
        )),
        vec![Value::Integer(20), Value::Integer(40)]
    );
}

#[test]
fn unit_call_subqueries_write_and_pass_the_outer_rows_through() {
    let keyspace = people();
    assert_eq!(
        names(query(
            &keyspace,
            "MATCH (p:Person) CALL { WITH p UNWIND range(1, 5) AS i CREATE (:Person {name: p.name}) } RETURN count(*) AS n"
        )),
        vec![Value::Integer(4)]
    );
    assert_eq!(
        names(query(&keyspace, "MATCH (p:Person) RETURN count(p)")),
        vec![Value::Integer(24)]
    );
    query(
        &keyspace,
        "MATCH (p:Person {age: 30}) CALL { WITH p SET p.seen = true }",
    );
    assert_eq!(
        names(query(
            &keyspace,
            "MATCH (p:Person) WHERE p.seen RETURN p.age"
        )),
        vec![Value::Integer(30)]
    );
    let refused = run(
        &keyspace,
        "MATCH (p:Person) CALL { WITH p SET p.refused = true } RETURN count(*) AS n",
        &[],
        true,
    )
    .unwrap_err();
    assert_eq!(refused.code, "Neo.ClientError.Statement.AccessMode");
    let reading = run(
        &keyspace,
        "CALL { MATCH (p:Person {age: 65}) RETURN p.name AS name } RETURN name",
        &[],
        true,
    )
    .unwrap();
    assert_eq!(names(reading), vec![text("Charlie")]);
}

#[test]
fn call_subquery_errors_follow_neo4j() {
    let keyspace = people();
    for (text, code) in [
        (
            "UNWIND [1] AS x CALL { RETURN 1 AS x } RETURN x",
            "Neo.ClientError.Statement.SemanticError",
        ),
        (
            "WITH 1 AS x CALL { WITH x RETURN x } RETURN x",
            "Neo.ClientError.Statement.SemanticError",
        ),
        (
            "WITH 1 AS x CALL { WITH x AS y RETURN y AS z } RETURN z",
            "Neo.ClientError.Statement.SemanticError",
        ),
        (
            "WITH 1 AS x CALL { WITH x WHERE x > 0 RETURN x AS y } RETURN y",
            "Neo.ClientError.Statement.SemanticError",
        ),
        (
            "WITH 1 AS x CALL { WITH x ORDER BY x RETURN x AS y } RETURN y",
            "Neo.ClientError.Statement.SemanticError",
        ),
        (
            "WITH 1 AS x CALL { RETURN x AS y } RETURN y",
            "Neo.ClientError.Statement.SyntaxError",
        ),
        (
            "UNWIND [1] AS x CALL { WITH x CREATE (:Batch) } IN TRANSACTIONS ON ERROR CONTINUE",
            "Neo.ClientError.Statement.FeatureNotSupported",
        ),
        (
            "UNWIND [1] AS x CALL { WITH x CREATE (:Batch) } IN TRANSACTIONS OF 10 ROWS REPORT STATUS AS s",
            "Neo.ClientError.Statement.FeatureNotSupported",
        ),
        (
            "CALL { RETURN 1 AS x }",
            "Neo.ClientError.Statement.SyntaxError",
        ),
        (
            "CALL { RETURN 1 AS x UNION RETURN 2 AS y } RETURN x",
            "Neo.ClientError.Statement.SyntaxError",
        ),
        (
            "CALL { MATCH (p:Person) } RETURN 1 AS x",
            "Neo.ClientError.Statement.SyntaxError",
        ),
        (
            "FOREACH (x IN [1] | CALL { CREATE (:Batch) })",
            "Neo.ClientError.Statement.SyntaxError",
        ),
    ] {
        assert_eq!(error_code(&keyspace, text), code, "{text}");
    }
    assert_eq!(
        names(query(&keyspace, "MATCH (n:Batch) RETURN count(n)")),
        vec![Value::Integer(0)]
    );
}

#[test]
fn shortest_paths_work_as_expressions() {
    let keyspace = movies();
    assert_eq!(
        names(query(
            &keyspace,
            "MATCH (martin:Person {name: 'Martin Sheen'}), (oliver:Person {name: 'Oliver Stone'})
             RETURN length(shortestPath((martin)-[*..15]-(oliver))) AS hops"
        )),
        vec![Value::Integer(2)]
    );
    assert_eq!(
        names(query(
            &keyspace,
            "MATCH (martin:Person {name: 'Martin Sheen'}), (michael:Person {name: 'Michael Douglas'})
             RETURN [p IN allShortestPaths((martin)-[*]-(michael)) | length(p)] AS lengths"
        )),
        vec![Value::List(vec![Value::Integer(2), Value::Integer(2)])]
    );
    assert_eq!(
        query(
            &keyspace,
            "MATCH (charlie:Person {name: 'Charlie Sheen'}), (rob:Person {name: 'Rob Reiner'})
             RETURN shortestPath((charlie)-[*..2]-(rob)) AS one, allShortestPaths((charlie)-[*..2]-(rob)) AS every"
        )
        .rows,
        vec![vec![Value::Null, Value::List(Vec::new())]]
    );
    assert_eq!(
        names(query(
            &keyspace,
            "MATCH (a:Person), (b:Person)
             WHERE a.name = 'Charlie Sheen' AND shortestPath((a)-[:FATHER]->(b)) IS NOT NULL
             RETURN b.name"
        )),
        vec![text("Martin Sheen")]
    );
    assert_eq!(
        error_code(
            &keyspace,
            "MATCH (martin:Person {name: 'Martin Sheen'}) RETURN shortestPath((martin)-[*]-(other)) AS p"
        ),
        "Neo.ClientError.Statement.SemanticError"
    );
}

#[test]
fn merge_refuses_a_null_property_value() {
    let keyspace = MemoryKeyspace::default();
    let refused = |text: &str, parameters: &[(String, Value)]| {
        let error = run(&keyspace, text, parameters, false).unwrap_err();
        assert_eq!(
            error.code, "Neo.ClientError.Statement.SemanticError",
            "{text}"
        );
        error.message
    };
    assert_eq!(
        refused("MERGE (n:L {x: null})", &[]),
        "Cannot merge the following node because of null property value for 'x': (:L {x: null})"
    );
    assert_eq!(
        refused(
            "MERGE (n:L {x: $value})",
            &[("value".to_owned(), Value::Null)]
        ),
        "Cannot merge the following node because of null property value for 'x': (:L {x: null})"
    );
    assert_eq!(
        refused("MERGE (n:L {a: 'one', x: null}) RETURN n", &[]),
        "Cannot merge the following node because of null property value for 'x': (:L {a: 'one', x: null})"
    );
    query(&keyspace, "CREATE (:A {id: 1}), (:B {id: 2})");
    assert_eq!(
        refused(
            "MATCH (a:A), (b:B) MERGE (a)-[:R {since: $since}]->(b)",
            &[("since".to_owned(), Value::Null)]
        ),
        "Cannot merge the following relationship because of null property value for 'since': ()-[:R {since: null}]->()"
    );
    assert_eq!(
        query(
            &keyspace,
            "MATCH (n) OPTIONAL MATCH (n)-[r]->() RETURN count(DISTINCT n) AS nodes, count(r) AS relationships"
        )
        .rows,
        vec![vec![Value::Integer(2), Value::Integer(0)]]
    );
    assert_eq!(
        names(query(&keyspace, "MERGE (n:L {x: 1}) RETURN n.x")),
        vec![Value::Integer(1)]
    );
}

#[test]
fn list_predicates_answer_and_steer_shortest_paths() {
    let keyspace = MemoryKeyspace::default();
    query(
        &keyspace,
        "CREATE (a:N {name: 'a'}), (b:N {name: 'b'}), (c:N {name: 'c'}), (d:N {name: 'd'}), (e:N {name: 'e'})
         CREATE (a)-[:R {w: 1}]->(b), (b)-[:R {w: 9}]->(c), (a)-[:R {w: 1}]->(d), (d)-[:R {w: 1}]->(e), (e)-[:R {w: 2}]->(c)",
    );
    let predicates = query(
        &keyspace,
        "RETURN all(x IN [1, 2] WHERE x > 0), any(x IN [1, 2] WHERE x > 1), none(x IN [1, 2] WHERE x > 5), single(x IN [1, 2] WHERE x > 1), all(x IN [1, null] WHERE x > 0)",
    );
    assert_eq!(
        predicates.rows,
        vec![vec![
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Null
        ]]
    );
    let light = query(
        &keyspace,
        "MATCH (a:N {name: 'a'}), (c:N {name: 'c'}), p = shortestPath((a)-[*]->(c))
         WHERE all(r IN relationships(p) WHERE r.w < 5)
         RETURN [n IN nodes(p) | n.name]",
    );
    assert_eq!(
        light.rows,
        vec![vec![Value::List(vec![text("a"), text("d"), text("e"), text("c")])]]
    );
    let two = query(
        &keyspace,
        "MATCH p = SHORTEST 2 PATHS (a:N {name: 'a'})-[:R]->+(c:N {name: 'c'})
         RETURN [n IN nodes(p) | n.name] AS names ORDER BY length(p)",
    );
    assert_eq!(
        two.rows,
        vec![
            vec![Value::List(vec![text("a"), text("b"), text("c")])],
            vec![Value::List(vec![text("a"), text("d"), text("e"), text("c")])]
        ]
    );
    let any = query(
        &keyspace,
        "MATCH p = ANY SHORTEST (a:N {name: 'a'})-[:R]->{1,5}(c:N {name: 'c'}) RETURN length(p)",
    );
    assert_eq!(any.rows, vec![vec![Value::Integer(2)]]);
}
