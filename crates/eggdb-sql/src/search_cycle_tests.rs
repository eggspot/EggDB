use crate::arrays::ArrayElement;
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::types::{DefinedType, SqlType};
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn last(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn fails(session: &mut Session<'_>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error,
    }
}

fn column_types(session: &mut Session<'_>, sql: &str) -> Vec<(String, Option<SqlType>)> {
    let mut types = Vec::new();
    let outcome = session.execute(sql, &mut |result| {
        types = result
            .columns
            .iter()
            .flatten()
            .map(|column| (column.name.clone(), column.sql_type))
            .collect();
    });
    if let Err(error) = outcome {
        panic!("{sql}: {error}");
    }
    types
}

fn graph0(session: &mut Session<'_>) {
    last(session, "CREATE TABLE graph0 (f int, t int, label text)");
    last(
        session,
        "INSERT INTO graph0 VALUES (1, 2, 'arc 1 -> 2'), (1, 3, 'arc 1 -> 3'), \
         (2, 3, 'arc 2 -> 3'), (1, 4, 'arc 1 -> 4'), (4, 5, 'arc 4 -> 5')",
    );
}

fn graph(session: &mut Session<'_>) {
    last(session, "CREATE TABLE graph (f int, t int, label text)");
    last(
        session,
        "INSERT INTO graph VALUES (1, 2, 'arc 1 -> 2'), (1, 3, 'arc 1 -> 3'), \
         (2, 3, 'arc 2 -> 3'), (1, 4, 'arc 1 -> 4'), (4, 5, 'arc 4 -> 5'), (5, 1, 'arc 5 -> 1')",
    );
}

fn tree(session: &mut Session<'_>) {
    last(session, "CREATE TABLE tree (id int, link int, data text)");
    last(
        session,
        "INSERT INTO tree VALUES (1, NULL, 'root'), (2, 1, 'a'), (3, 1, 'b'), (4, 2, 'c')",
    );
}

const GRAPH0_WALK: &str = "WITH RECURSIVE search_graph(f, t, label) AS ( \
     SELECT * FROM graph0 g \
     UNION ALL \
     SELECT g.* FROM graph0 g, search_graph sg WHERE g.f = sg.t)";

const GRAPH_WALK: &str = "WITH RECURSIVE search_graph(f, t, label) AS ( \
     SELECT * FROM graph g \
     UNION ALL \
     SELECT g.* FROM graph g, search_graph sg WHERE g.f = sg.t)";

#[test]
fn search_depth_first_orders_by_an_array_of_rows() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    graph0(&mut session);
    assert_eq!(
        last(
            &mut session,
            &format!(
                "{GRAPH0_WALK} SEARCH DEPTH FIRST BY f, t SET seq SELECT * FROM search_graph ORDER BY seq"
            )
        ),
        "f|t|label|seq\n\
         1|2|arc 1 -> 2|{\"(1,2)\"}\n\
         2|3|arc 2 -> 3|{\"(1,2)\",\"(2,3)\"}\n\
         1|3|arc 1 -> 3|{\"(1,3)\"}\n\
         1|4|arc 1 -> 4|{\"(1,4)\"}\n\
         4|5|arc 4 -> 5|{\"(1,4)\",\"(4,5)\"}\n\
         2|3|arc 2 -> 3|{\"(2,3)\"}\n\
         4|5|arc 4 -> 5|{\"(4,5)\"}\n\
         SELECT 7"
    );
}

#[test]
fn search_breadth_first_orders_by_depth_then_the_columns() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    graph0(&mut session);
    assert_eq!(
        last(
            &mut session,
            &format!(
                "{GRAPH0_WALK} SEARCH BREADTH FIRST BY f, t SET seq SELECT * FROM search_graph ORDER BY seq"
            )
        ),
        "f|t|label|seq\n\
         1|2|arc 1 -> 2|(0,1,2)\n\
         1|3|arc 1 -> 3|(0,1,3)\n\
         1|4|arc 1 -> 4|(0,1,4)\n\
         2|3|arc 2 -> 3|(0,2,3)\n\
         4|5|arc 4 -> 5|(0,4,5)\n\
         2|3|arc 2 -> 3|(1,2,3)\n\
         4|5|arc 4 -> 5|(1,4,5)\n\
         SELECT 7"
    );
}

#[test]
fn manual_search_order_examples_walk_the_tree() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    tree(&mut session);
    let walk = "WITH RECURSIVE search_tree(id, link, data) AS ( \
         SELECT t.id, t.link, t.data FROM tree t \
         UNION ALL \
         SELECT t.id, t.link, t.data FROM tree t, search_tree st WHERE t.id = st.link)";
    assert_eq!(
        last(
            &mut session,
            &format!(
                "{walk} SEARCH DEPTH FIRST BY id SET ordercol SELECT * FROM search_tree ORDER BY ordercol"
            )
        ),
        "id|link|data|ordercol\n\
         1|NULL|root|{(1)}\n\
         2|1|a|{(2)}\n\
         1|NULL|root|{(2),(1)}\n\
         3|1|b|{(3)}\n\
         1|NULL|root|{(3),(1)}\n\
         4|2|c|{(4)}\n\
         2|1|a|{(4),(2)}\n\
         1|NULL|root|{(4),(2),(1)}\n\
         SELECT 8"
    );
    assert_eq!(
        last(
            &mut session,
            &format!(
                "{walk} SEARCH BREADTH FIRST BY id SET ordercol SELECT * FROM search_tree ORDER BY ordercol"
            )
        ),
        "id|link|data|ordercol\n\
         1|NULL|root|(0,1)\n\
         2|1|a|(0,2)\n\
         3|1|b|(0,3)\n\
         4|2|c|(0,4)\n\
         1|NULL|root|(1,1)\n\
         1|NULL|root|(1,1)\n\
         2|1|a|(1,2)\n\
         1|NULL|root|(2,1)\n\
         SELECT 8"
    );
}

#[test]
fn cycle_marks_the_row_that_repeats_and_stops_there() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    graph(&mut session);
    assert_eq!(
        last(
            &mut session,
            &format!(
                "{GRAPH_WALK} CYCLE f, t SET is_cycle USING path SELECT * FROM search_graph ORDER BY path"
            )
        ),
        "f|t|label|is_cycle|path\n\
         1|2|arc 1 -> 2|f|{\"(1,2)\"}\n\
         2|3|arc 2 -> 3|f|{\"(1,2)\",\"(2,3)\"}\n\
         1|3|arc 1 -> 3|f|{\"(1,3)\"}\n\
         1|4|arc 1 -> 4|f|{\"(1,4)\"}\n\
         4|5|arc 4 -> 5|f|{\"(1,4)\",\"(4,5)\"}\n\
         5|1|arc 5 -> 1|f|{\"(1,4)\",\"(4,5)\",\"(5,1)\"}\n\
         1|2|arc 1 -> 2|f|{\"(1,4)\",\"(4,5)\",\"(5,1)\",\"(1,2)\"}\n\
         2|3|arc 2 -> 3|f|{\"(1,4)\",\"(4,5)\",\"(5,1)\",\"(1,2)\",\"(2,3)\"}\n\
         1|3|arc 1 -> 3|f|{\"(1,4)\",\"(4,5)\",\"(5,1)\",\"(1,3)\"}\n\
         1|4|arc 1 -> 4|t|{\"(1,4)\",\"(4,5)\",\"(5,1)\",\"(1,4)\"}\n\
         2|3|arc 2 -> 3|f|{\"(2,3)\"}\n\
         4|5|arc 4 -> 5|f|{\"(4,5)\"}\n\
         5|1|arc 5 -> 1|f|{\"(4,5)\",\"(5,1)\"}\n\
         1|2|arc 1 -> 2|f|{\"(4,5)\",\"(5,1)\",\"(1,2)\"}\n\
         2|3|arc 2 -> 3|f|{\"(4,5)\",\"(5,1)\",\"(1,2)\",\"(2,3)\"}\n\
         1|3|arc 1 -> 3|f|{\"(4,5)\",\"(5,1)\",\"(1,3)\"}\n\
         1|4|arc 1 -> 4|f|{\"(4,5)\",\"(5,1)\",\"(1,4)\"}\n\
         4|5|arc 4 -> 5|t|{\"(4,5)\",\"(5,1)\",\"(1,4)\",\"(4,5)\"}\n\
         5|1|arc 5 -> 1|f|{\"(5,1)\"}\n\
         1|2|arc 1 -> 2|f|{\"(5,1)\",\"(1,2)\"}\n\
         2|3|arc 2 -> 3|f|{\"(5,1)\",\"(1,2)\",\"(2,3)\"}\n\
         1|3|arc 1 -> 3|f|{\"(5,1)\",\"(1,3)\"}\n\
         1|4|arc 1 -> 4|f|{\"(5,1)\",\"(1,4)\"}\n\
         4|5|arc 4 -> 5|f|{\"(5,1)\",\"(1,4)\",\"(4,5)\"}\n\
         5|1|arc 5 -> 1|t|{\"(5,1)\",\"(1,4)\",\"(4,5)\",\"(5,1)\"}\n\
         SELECT 25"
    );
}

#[test]
fn cycle_takes_its_own_mark_and_default_values() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    graph(&mut session);
    assert_eq!(
        last(
            &mut session,
            &format!(
                "{GRAPH_WALK} CYCLE f, t SET is_cycle TO 'Y' DEFAULT 'N' USING path \
                 SELECT f, t, is_cycle FROM search_graph WHERE is_cycle = 'Y' ORDER BY path"
            )
        ),
        "f|t|is_cycle\n1|4|Y\n4|5|Y\n5|1|Y\nSELECT 3"
    );
    assert_eq!(
        last(
            &mut session,
            &format!(
                "{GRAPH_WALK} CYCLE f, t SET is_cycle TO 1 DEFAULT 0 USING path \
                 SELECT sum(is_cycle), count(*) FROM search_graph"
            )
        ),
        "sum|count\n3|25\nSELECT 1"
    );
}

#[test]
fn manual_cycle_detection_example_stops_on_a_repeated_node() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE nodes (id int, link int, data text)",
    );
    last(
        &mut session,
        "INSERT INTO nodes VALUES (1, 2, 'one'), (2, 3, 'two'), (3, 1, 'three')",
    );
    assert_eq!(
        last(
            &mut session,
            "WITH RECURSIVE search_graph(id, link, data, depth) AS ( \
                 SELECT g.id, g.link, g.data, 1 FROM nodes g WHERE g.id = 1 \
                 UNION ALL \
                 SELECT g.id, g.link, g.data, sg.depth + 1 FROM nodes g, search_graph sg \
                 WHERE g.id = sg.link \
             ) CYCLE id SET is_cycle USING path \
             SELECT * FROM search_graph ORDER BY depth"
        ),
        "id|link|data|depth|is_cycle|path\n\
         1|2|one|1|f|{(1)}\n\
         2|3|two|2|f|{(1),(2)}\n\
         3|1|three|3|f|{(1),(2),(3)}\n\
         1|2|one|4|t|{(1),(2),(3),(1)}\n\
         SELECT 4"
    );
}

#[test]
fn recursive_term_wildcards_keep_the_query_columns() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        last(
            &mut session,
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT * FROM t) \
             CYCLE n SET is_cycle USING path SELECT * FROM t ORDER BY path"
        ),
        "n|is_cycle|path\n1|f|{(1)}\n1|t|{(1),(1)}\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT t.* FROM t) \
             CYCLE n SET is_cycle USING path SELECT n, is_cycle FROM t ORDER BY path"
        ),
        "n|is_cycle\n1|f\n1|t\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT n % 3 + 1 FROM t) \
             CYCLE n SET is_cycle USING path SELECT n, is_cycle, path FROM t ORDER BY path"
        ),
        "n|is_cycle|path\n1|f|{(1)}\n2|f|{(1),(2)}\n3|f|{(1),(2),(3)}\n\
         1|t|{(1),(2),(3),(1)}\nSELECT 4"
    );
}

#[test]
fn search_and_cycle_together_add_three_columns_in_order() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    graph(&mut session);
    let query = format!(
        "{GRAPH_WALK} SEARCH DEPTH FIRST BY f, t SET seq CYCLE f, t SET is_cycle USING path \
         SELECT * FROM search_graph ORDER BY seq LIMIT 2"
    );
    assert_eq!(
        last(&mut session, &query),
        "f|t|label|seq|is_cycle|path\n\
         1|2|arc 1 -> 2|{\"(1,2)\"}|f|{\"(1,2)\"}\n\
         2|3|arc 2 -> 3|{\"(1,2)\",\"(2,3)\"}|f|{\"(1,2)\",\"(2,3)\"}\n\
         SELECT 2"
    );
    let types = column_types(&mut session, &query);
    let record_array = Some(SqlType::Array(ArrayElement::Record));
    assert_eq!(types.get(3), Some(&("seq".to_owned(), record_array)));
    assert_eq!(
        types.get(4),
        Some(&("is_cycle".to_owned(), Some(SqlType::Boolean)))
    );
    assert_eq!(types.get(5), Some(&("path".to_owned(), record_array)));
    assert_eq!(
        crate::pg_catalog::type_oid(SqlType::Array(ArrayElement::Record)),
        2287
    );
}

#[test]
fn breadth_first_sequence_is_a_record() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    graph0(&mut session);
    let types = column_types(
        &mut session,
        &format!("{GRAPH0_WALK} SEARCH BREADTH FIRST BY f SET seq SELECT * FROM search_graph"),
    );
    assert_eq!(
        types.get(3),
        Some(&(
            "seq".to_owned(),
            Some(SqlType::Defined(DefinedType::RECORD))
        ))
    );
}

#[test]
fn misuse_reports_postgresql_errors() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    graph(&mut session);
    let cases: [(String, &str, &str); 12] = [
        (
            "WITH search_graph(f, t, label) AS (SELECT * FROM graph g) \
             SEARCH DEPTH FIRST BY f, t SET seq SELECT * FROM search_graph"
                .to_owned(),
            "42601",
            "WITH query is not recursive",
        ),
        (
            format!(
                "{GRAPH_WALK} SEARCH DEPTH FIRST BY foo, tar SET seq SELECT * FROM search_graph"
            ),
            "42601",
            "search column \"foo\" not in WITH query column list",
        ),
        (
            format!(
                "{GRAPH_WALK} SEARCH DEPTH FIRST BY f, t, f SET seq SELECT * FROM search_graph"
            ),
            "42701",
            "search column \"f\" specified more than once",
        ),
        (
            format!("{GRAPH_WALK} SEARCH DEPTH FIRST BY f, t SET label SELECT * FROM search_graph"),
            "42601",
            "search sequence column name \"label\" already used in WITH query column list",
        ),
        (
            format!(
                "{GRAPH_WALK} CYCLE foo, tar SET is_cycle USING path SELECT * FROM search_graph"
            ),
            "42601",
            "cycle column \"foo\" not in WITH query column list",
        ),
        (
            format!(
                "{GRAPH_WALK} CYCLE f, t, f SET is_cycle USING path SELECT * FROM search_graph"
            ),
            "42701",
            "cycle column \"f\" specified more than once",
        ),
        (
            format!("{GRAPH_WALK} CYCLE f, t SET label USING path SELECT * FROM search_graph"),
            "42601",
            "cycle mark column name \"label\" already used in WITH query column list",
        ),
        (
            format!("{GRAPH_WALK} CYCLE f, t SET foo USING foo SELECT * FROM search_graph"),
            "42601",
            "cycle mark column name and cycle path column name are the same",
        ),
        (
            format!(
                "{GRAPH_WALK} CYCLE f, t SET is_cycle TO true DEFAULT 55 USING path SELECT * FROM search_graph"
            ),
            "42804",
            "CYCLE types boolean and integer cannot be matched",
        ),
        (
            format!(
                "{GRAPH_WALK} SEARCH DEPTH FIRST BY f, t SET foo CYCLE f, t SET foo USING path \
                 SELECT * FROM search_graph"
            ),
            "42601",
            "search sequence column name and cycle mark column name are the same",
        ),
        (
            "WITH RECURSIVE search_graph(f, t, label) AS ( \
                 SELECT * FROM graph g \
                 UNION ALL (SELECT * FROM graph g) \
                 UNION ALL (SELECT g.* FROM graph g, search_graph sg WHERE g.f = sg.t) \
             ) SEARCH DEPTH FIRST BY f, t SET seq SELECT * FROM search_graph"
                .to_owned(),
            "42601",
            "with a SEARCH or CYCLE clause, the left side of the UNION must be a SELECT",
        ),
        (
            "WITH RECURSIVE search_graph(f, t, label) AS ( \
                 SELECT * FROM graph g \
                 UNION ALL \
                 SELECT * FROM (SELECT g.* FROM graph g, search_graph sg WHERE g.f = sg.t) ss \
             ) SEARCH DEPTH FIRST BY f, t SET seq SELECT * FROM search_graph"
                .to_owned(),
            "42601",
            "with a SEARCH or CYCLE clause, the recursive reference to WITH query \"search_graph\" must be at the top level of its right-hand SELECT",
        ),
    ];
    for (sql, state, message) in cases {
        let error = fails(&mut session, &sql);
        assert_eq!(
            (error.state().code(), error.message()),
            (state, message),
            "{sql}"
        );
    }
}

#[test]
fn other_dialects_still_refuse_search_and_cycle() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    let error = fails(
        &mut session,
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
         CYCLE n SET is_cycle USING path SELECT * FROM t",
    );
    assert_eq!(error.state().code(), "42000");
}
