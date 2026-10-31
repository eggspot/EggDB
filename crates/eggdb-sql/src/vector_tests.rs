use crate::memory_store::MemoryStore;
use crate::session::Session;
use crate::tests::render;

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    let mut out = Vec::new();
    session.execute(sql, &mut |result| out.push(render(&result))).unwrap_or_else(|error| panic!("{sql}: {error}"));
    out.join("\n")
}

#[test]
fn pgvector_operators_and_functions_compute_distances() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE EXTENSION IF NOT EXISTS vector");
    assert_eq!(run(&mut session, "SELECT '[1,2,3]' <-> '[4,6,3]'"), run(&mut session, "SELECT 5::double precision"));
    assert_eq!(run(&mut session, "SELECT '[1,2,3]' <+> '[4,6,3]', '[1,2,3]' <#> '[4,6,3]'"), run(&mut session, "SELECT 7::double precision, -25::double precision"));
    assert_eq!(run(&mut session, "SELECT '[1,0]' <=> '[0,1]'"), run(&mut session, "SELECT 1::double precision"));
    assert_eq!(run(&mut session, "SELECT vector_dims('[1,2,3]'), l2_normalize('[3,4]')"), run(&mut session, "SELECT 3, '[0.6,0.8]'"));
    assert_eq!(run(&mut session, "SELECT l2_distance(ARRAY[0.0, 3.0], '[4,0]')"), run(&mut session, "SELECT 5::double precision"));
    assert!(session.execute("SELECT '[1,2]' <-> '[1,2,3]'", &mut |_| {}).is_err());
    assert!(session.execute("SELECT '[1,x]' <-> '[1,2]'", &mut |_| {}).is_err());
}

#[test]
fn nearest_neighbours_order_by_distance() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE items (id int, embedding vector(3))");
    run(&mut session, "INSERT INTO items VALUES (1, '[1,1,1]'), (2, '[2,2,2]'), (3, '[9,9,9]'), (4, NULL)");
    assert_eq!(
        run(&mut session, "SELECT id FROM items WHERE embedding IS NOT NULL ORDER BY embedding <-> '[8,8,8]' LIMIT 2"),
        run(&mut session, "SELECT 3 AS id UNION ALL SELECT 2")
    );
}

#[test]
fn local_embeddings_and_rank_fusion_support_hybrid_search() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE docs (id int, body text)");
    run(&mut session, "INSERT INTO docs VALUES (1, 'cats sleep on the sofa'), (2, 'quarterly revenue report'), (3, 'a sleeping cat')");
    assert_eq!(
        run(&mut session, "SELECT id FROM docs ORDER BY eggdb_embed(body) <=> eggdb_embed('sleepy cats') LIMIT 2"),
        run(&mut session, "SELECT 3 AS id UNION ALL SELECT 1")
    );
    assert_eq!(run(&mut session, "SELECT vector_dims(eggdb_embed('hello', 8))"), run(&mut session, "SELECT 8"));
    assert_eq!(run(&mut session, "SELECT rrf_score(1, 0)"), run(&mut session, "SELECT 1::double precision"));
    assert!(session.execute("SELECT rrf_score(0)", &mut |_| {}).is_err());
}
