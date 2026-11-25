use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::Session;

fn last(session: &mut Session<'_>, sql: &str) -> String {
    let mut outputs = Vec::new();
    session
        .execute(sql, &mut |result| outputs.push(render(&result)))
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    outputs.pop().unwrap_or_default()
}

#[test]
fn gin_gist_spgist_and_brin_indexes_are_created_kept_and_answer_queries_like_none() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE docs (id integer PRIMARY KEY, tags text[], body tsvector, data jsonb, spot point);
         CREATE INDEX docs_tags ON docs USING gin (tags);
         CREATE INDEX docs_body ON docs USING GIN (body);
         CREATE INDEX docs_data ON docs USING gin (data jsonb_path_ops) WITH (fastupdate = off);
         CREATE INDEX docs_spot ON docs USING gist (spot);
         CREATE INDEX docs_id ON docs USING brin (id) WITH (pages_per_range = 32);
         INSERT INTO docs VALUES (1, ARRAY['a', 'b'], 'fat cat', '{\"k\": 1}', '(1,1)'),
                                 (2, ARRAY['c'], 'lazy dog', '{\"k\": 2}', '(5,5)')",
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM docs WHERE tags @> ARRAY['b'] AND id >= 1 ORDER BY id"),
        "id\n1\nSELECT 1"
    );
    last(&mut session, "UPDATE docs SET tags = ARRAY['b'] WHERE id = 2; DELETE FROM docs WHERE id = 1");
    assert_eq!(
        last(&mut session, "SELECT id FROM docs WHERE tags @> ARRAY['b'] ORDER BY id"),
        "id\n2\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT am.amname FROM pg_class c JOIN pg_am am ON am.oid = c.relam WHERE c.relname IN ('docs_tags', 'docs_spot', 'docs_id') ORDER BY c.relname"
        ),
        "amname\nbrin\ngist\ngin\nSELECT 3"
    );
    let refused = session.execute("CREATE UNIQUE INDEX docs_unique ON docs USING gin (tags)", &mut |_| {}).unwrap_err();
    assert_eq!(refused.state().code(), "0A000");
}
