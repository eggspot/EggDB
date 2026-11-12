/// The statements `CREATE EXTENSION pgrouting` runs: pgRouting's `pgr_dijkstra`,
/// `pgr_drivingDistance` and `pgr_connectedComponents`, written from its documentation, over an
/// edges query with `id`, `source`, `target`, `cost` and optionally `reverse_cost`, computed by
/// EggDB's own graph code (D57).
pub(crate) const PGROUTING_SCRIPT: &str = r#"
CREATE SCHEMA pgrouting;
CREATE FUNCTION public.pgr_edges_json(edges_sql text) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE
    collected jsonb;
BEGIN
    EXECUTE format('SELECT coalesce(jsonb_agg(to_jsonb(e)), ''[]''::jsonb) FROM (%s) e', edges_sql) INTO collected;
    RETURN collected;
END
$$;
CREATE FUNCTION public.pgr_dijkstra(edges_sql text, start_vid bigint, end_vid bigint, directed boolean DEFAULT true) RETURNS TABLE (seq integer, path_seq integer, node bigint, edge bigint, cost double precision, agg_cost double precision) LANGUAGE plpgsql AS $$
DECLARE
    step record;
    counted integer := 0;
BEGIN
    FOR step IN SELECT s.value AS body FROM jsonb_array_elements(eggdb_route_dijkstra(public.pgr_edges_json(edges_sql), start_vid, end_vid, directed)) s LOOP
        counted := counted + 1;
        seq := counted;
        path_seq := counted;
        node := (step.body ->> 'node')::bigint;
        edge := (step.body ->> 'edge')::bigint;
        cost := (step.body ->> 'cost')::double precision;
        agg_cost := (step.body ->> 'agg_cost')::double precision;
        RETURN NEXT;
    END LOOP;
END
$$;
CREATE FUNCTION public.pgr_drivingdistance(edges_sql text, start_vid bigint, distance double precision, directed boolean DEFAULT true) RETURNS TABLE (seq integer, node bigint, edge bigint, cost double precision, agg_cost double precision) LANGUAGE plpgsql AS $$
DECLARE
    step record;
    counted integer := 0;
BEGIN
    FOR step IN SELECT s.value AS body FROM jsonb_array_elements(eggdb_route_reach(public.pgr_edges_json(edges_sql), start_vid, distance, directed)) s LOOP
        counted := counted + 1;
        seq := counted;
        node := (step.body ->> 'node')::bigint;
        edge := (step.body ->> 'edge')::bigint;
        cost := (step.body ->> 'cost')::double precision;
        agg_cost := (step.body ->> 'agg_cost')::double precision;
        RETURN NEXT;
    END LOOP;
END
$$;
CREATE FUNCTION public.pgr_connectedcomponents(edges_sql text) RETURNS TABLE (seq integer, component bigint, node bigint) LANGUAGE plpgsql AS $$
DECLARE
    step record;
    counted integer := 0;
BEGIN
    FOR step IN SELECT s.value AS body FROM jsonb_array_elements(eggdb_route_components(public.pgr_edges_json(edges_sql))) s LOOP
        counted := counted + 1;
        seq := counted;
        component := (step.body ->> 'component')::bigint;
        node := (step.body ->> 'node')::bigint;
        RETURN NEXT;
    END LOOP;
END
$$;
"#;

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::tests::render;
    use crate::{Session, SqlError};

    fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(render(&result)))?;
        Ok(outputs.pop().unwrap_or_default())
    }

    #[test]
    fn pgr_dijkstra_follows_the_cheapest_edges() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION pgrouting",
            "CREATE TABLE ways (id bigint, source bigint, target bigint, cost double precision, reverse_cost double precision)",
            "INSERT INTO ways VALUES (1, 1, 2, 1, 1), (2, 2, 3, 5, -1), (3, 2, 4, 1, 1), (4, 4, 3, 1, 1)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT node, edge, agg_cost FROM pgr_dijkstra('SELECT id, source, target, cost, reverse_cost FROM ways', 1, 3)").unwrap(),
            "node|edge|agg_cost\n1|1|0\n2|3|1\n4|4|2\n3|-1|3\nSELECT 4"
        );
        assert_eq!(run(&mut session, "SELECT count(*) AS reached FROM pgr_drivingDistance('SELECT * FROM ways', 1, 2)").unwrap(), "reached\n3\nSELECT 1");
    }
}
