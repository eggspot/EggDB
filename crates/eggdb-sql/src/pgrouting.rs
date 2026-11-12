/// The statements `CREATE EXTENSION pgrouting` runs: pgRouting's `pgr_dijkstra` (one to one,
/// one to many, many to one and many to many), `pgr_dijkstraCost` and `pgr_dijkstraCostMatrix`,
/// `pgr_drivingDistance` and `pgr_connectedComponents`, written from its documentation, over an
/// edges query with `id`, `source`, `target`, `cost` and optionally `reverse_cost`, computed by
/// EggDB's own graph code (D57); and `pgr_createTopology` and `pgr_createVerticesTable`, which
/// number line ends as vertices (D831).
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
CREATE FUNCTION public.pgr_astar(edges_sql text, start_vid bigint, end_vid bigint, directed boolean DEFAULT true, heuristic integer DEFAULT 5, factor double precision DEFAULT 1, epsilon double precision DEFAULT 1) RETURNS TABLE (seq integer, path_seq integer, node bigint, edge bigint, cost double precision, agg_cost double precision) LANGUAGE plpgsql AS $$
DECLARE
    step record;
    counted integer := 0;
BEGIN
    FOR step IN SELECT s.value AS body FROM jsonb_array_elements(eggdb_route_astar(public.pgr_edges_json(edges_sql), start_vid, end_vid, directed, heuristic, factor, epsilon)) s LOOP
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
CREATE FUNCTION public.pgr_dijkstracost(edges_sql text, from_vid bigint, to_vid bigint, directed boolean DEFAULT true) RETURNS TABLE (start_vid bigint, end_vid bigint, agg_cost double precision) LANGUAGE plpgsql AS $$
DECLARE
    total double precision;
BEGIN
    SELECT p.agg_cost INTO total FROM public.pgr_dijkstra(edges_sql, from_vid, to_vid, directed) p WHERE p.edge = -1;
    IF total IS NOT NULL THEN
        start_vid := from_vid;
        end_vid := to_vid;
        agg_cost := total;
        RETURN NEXT;
    END IF;
END
$$;
CREATE FUNCTION public.pgr_bddijkstra(edges_sql text, start_vid bigint, end_vid bigint, directed boolean DEFAULT true) RETURNS TABLE (seq integer, path_seq integer, node bigint, edge bigint, cost double precision, agg_cost double precision) LANGUAGE sql AS $$
    SELECT * FROM public.pgr_dijkstra(edges_sql, start_vid, end_vid, directed)
$$;
CREATE FUNCTION public.pgr_dijkstra(edges_sql text, start_vids bigint[], end_vids bigint[], directed boolean DEFAULT true) RETURNS TABLE (seq integer, path_seq integer, start_vid bigint, end_vid bigint, node bigint, edge bigint, cost double precision, agg_cost double precision) LANGUAGE plpgsql AS $$
DECLARE
    graph jsonb := public.pgr_edges_json(edges_sql);
    origin bigint;
    destination bigint;
    step record;
    counted integer := 0;
    along integer;
BEGIN
    FOR origin IN SELECT DISTINCT o FROM unnest(start_vids) o WHERE o IS NOT NULL ORDER BY o LOOP
        FOR destination IN SELECT DISTINCT d FROM unnest(end_vids) d WHERE d IS NOT NULL ORDER BY d LOOP
            IF origin <> destination THEN
                along := 0;
                FOR step IN SELECT s.value AS body FROM jsonb_array_elements(eggdb_route_dijkstra(graph, origin, destination, directed)) s LOOP
                    counted := counted + 1;
                    along := along + 1;
                    seq := counted;
                    path_seq := along;
                    start_vid := origin;
                    end_vid := destination;
                    node := (step.body ->> 'node')::bigint;
                    edge := (step.body ->> 'edge')::bigint;
                    cost := (step.body ->> 'cost')::double precision;
                    agg_cost := (step.body ->> 'agg_cost')::double precision;
                    RETURN NEXT;
                END LOOP;
            END IF;
        END LOOP;
    END LOOP;
END
$$;
CREATE FUNCTION public.pgr_dijkstra(edges_sql text, start_vid bigint, end_vids bigint[], directed boolean DEFAULT true) RETURNS TABLE (seq integer, path_seq integer, end_vid bigint, node bigint, edge bigint, cost double precision, agg_cost double precision) LANGUAGE sql AS $$
    SELECT p.seq, p.path_seq, p.end_vid, p.node, p.edge, p.cost, p.agg_cost FROM public.pgr_dijkstra(edges_sql, ARRAY[start_vid], end_vids, directed) p
$$;
CREATE FUNCTION public.pgr_dijkstra(edges_sql text, start_vids bigint[], end_vid bigint, directed boolean DEFAULT true) RETURNS TABLE (seq integer, path_seq integer, start_vid bigint, node bigint, edge bigint, cost double precision, agg_cost double precision) LANGUAGE sql AS $$
    SELECT p.seq, p.path_seq, p.start_vid, p.node, p.edge, p.cost, p.agg_cost FROM public.pgr_dijkstra(edges_sql, start_vids, ARRAY[end_vid], directed) p
$$;
CREATE FUNCTION public.pgr_dijkstracost(edges_sql text, start_vids bigint[], end_vids bigint[], directed boolean DEFAULT true) RETURNS TABLE (start_vid bigint, end_vid bigint, agg_cost double precision) LANGUAGE sql AS $$
    SELECT p.start_vid, p.end_vid, p.agg_cost FROM public.pgr_dijkstra(edges_sql, start_vids, end_vids, directed) p WHERE p.edge = -1 ORDER BY 1, 2
$$;
CREATE FUNCTION public.pgr_dijkstracostmatrix(edges_sql text, vids bigint[], directed boolean DEFAULT true) RETURNS TABLE (start_vid bigint, end_vid bigint, agg_cost double precision) LANGUAGE sql AS $$
    SELECT c.start_vid, c.end_vid, c.agg_cost FROM public.pgr_dijkstracost(edges_sql, vids, vids, directed) c
$$;
CREATE FUNCTION public.pgr_vertex_near(vertices text, wanted_x double precision, wanted_y double precision, tolerance double precision) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    found bigint;
BEGIN
    EXECUTE format('SELECT v.id FROM %s v WHERE (v.x - $1) * (v.x - $1) + (v.y - $2) * (v.y - $2) <= $3 * $3 ORDER BY (v.x - $1) * (v.x - $1) + (v.y - $2) * (v.y - $2), v.id LIMIT 1', vertices)
        INTO found USING wanted_x, wanted_y, tolerance;
    IF found IS NULL THEN
        EXECUTE format('INSERT INTO %s (x, y, the_geom) VALUES ($1, $2, ST_AsText(ST_MakePoint($1, $2))) RETURNING id', vertices)
            INTO found USING wanted_x, wanted_y;
    END IF;
    RETURN found;
END
$$;
CREATE FUNCTION public.pgr_createtopology(edge_table text, tolerance double precision, the_geom text DEFAULT 'the_geom', id text DEFAULT 'id', source text DEFAULT 'source', target text DEFAULT 'target', rows_where text DEFAULT 'true', clean boolean DEFAULT false) RETURNS varchar LANGUAGE plpgsql AS $$
DECLARE
    vertices text := edge_table || '_vertices_pgr';
    edge record;
    start_vertex bigint;
    end_vertex bigint;
BEGIN
    IF tolerance IS NULL OR tolerance < 0 THEN
        RAISE EXCEPTION 'the tolerance must be zero or more' USING ERRCODE = '22023';
    END IF;
    IF clean THEN
        EXECUTE format('DROP TABLE IF EXISTS %s', vertices);
    END IF;
    EXECUTE format('CREATE TABLE IF NOT EXISTS %s (id bigserial PRIMARY KEY, cnt integer, chk integer, ein integer, eout integer, x double precision NOT NULL, y double precision NOT NULL, the_geom text)', vertices);
    FOR edge IN EXECUTE format('SELECT e.%I AS edge_id, ST_X(ST_StartPoint(e.%I)) AS start_x, ST_Y(ST_StartPoint(e.%I)) AS start_y, ST_X(ST_EndPoint(e.%I)) AS end_x, ST_Y(ST_EndPoint(e.%I)) AS end_y FROM %s e WHERE %s ORDER BY e.%I', id, the_geom, the_geom, the_geom, the_geom, edge_table, rows_where, id) LOOP
        IF edge.start_x IS NULL OR edge.end_x IS NULL THEN
            CONTINUE;
        END IF;
        start_vertex := public.pgr_vertex_near(vertices, edge.start_x, edge.start_y, tolerance);
        end_vertex := public.pgr_vertex_near(vertices, edge.end_x, edge.end_y, tolerance);
        EXECUTE format('UPDATE %s SET %I = $1, %I = $2 WHERE %I = $3', edge_table, source, target, id) USING start_vertex, end_vertex, edge.edge_id;
    END LOOP;
    EXECUTE format('UPDATE %s v SET cnt = (SELECT count(*) FROM %s e WHERE e.%I = v.id OR e.%I = v.id)', vertices, edge_table, source, target);
    RETURN 'OK';
END
$$;
CREATE FUNCTION public.pgr_createverticestable(edge_table text, the_geom text DEFAULT 'the_geom', source text DEFAULT 'source', target text DEFAULT 'target', rows_where text DEFAULT 'true') RETURNS varchar LANGUAGE plpgsql AS $$
DECLARE
    vertices text := edge_table || '_vertices_pgr';
BEGIN
    EXECUTE format('DROP TABLE IF EXISTS %s', vertices);
    EXECUTE format('CREATE TABLE %s (id bigint PRIMARY KEY, cnt integer, chk integer, ein integer, eout integer, x double precision, y double precision, the_geom text)', vertices);
    EXECUTE format('INSERT INTO %s (id, cnt, ein, eout) SELECT ends.vertex, count(*), count(*) FILTER (WHERE ends.incoming), count(*) FILTER (WHERE NOT ends.incoming) FROM (SELECT e.%I AS vertex, false AS incoming FROM %s e WHERE %s UNION ALL SELECT e.%I, true FROM %s e WHERE %s) ends WHERE ends.vertex IS NOT NULL GROUP BY ends.vertex', vertices, source, edge_table, rows_where, target, edge_table, rows_where);
    EXECUTE format('UPDATE %s v SET x = ST_X(ST_StartPoint(e.%I)), y = ST_Y(ST_StartPoint(e.%I)), the_geom = ST_AsText(ST_StartPoint(e.%I)) FROM %s e WHERE e.%I = v.id', vertices, the_geom, the_geom, the_geom, edge_table, source);
    EXECUTE format('UPDATE %s v SET x = ST_X(ST_EndPoint(e.%I)), y = ST_Y(ST_EndPoint(e.%I)), the_geom = ST_AsText(ST_EndPoint(e.%I)) FROM %s e WHERE e.%I = v.id AND v.x IS NULL', vertices, the_geom, the_geom, the_geom, edge_table, target);
    RETURN 'OK';
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

    #[test]
    fn many_to_many_paths_and_the_cost_matrix() {
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
            run(&mut session, "SELECT count(*) AS n FROM pgr_dijkstra('SELECT * FROM ways', ARRAY[1]::bigint[], ARRAY[3, 4]::bigint[])").unwrap(),
            "n\n7\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, "SELECT end_vid, agg_cost FROM pgr_dijkstra('SELECT * FROM ways', 1, ARRAY[3, 4]::bigint[]) WHERE edge = -1 ORDER BY end_vid").unwrap(),
            "end_vid|agg_cost\n3|3\n4|2\nSELECT 2"
        );
        assert_eq!(
            run(&mut session, "SELECT start_vid, end_vid, agg_cost FROM pgr_dijkstraCostMatrix('SELECT * FROM ways', ARRAY[1, 3]::bigint[])").unwrap(),
            "start_vid|end_vid|agg_cost\n1|3|3\n3|1|3\nSELECT 2"
        );
    }

    #[test]
    fn create_topology_numbers_line_ends_within_the_tolerance() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION pgrouting",
            "CREATE TABLE roads (id bigint PRIMARY KEY, source bigint, target bigint, the_geom text)",
            "INSERT INTO roads VALUES (1, NULL, NULL, 'LINESTRING(0 0, 1 0)'), (2, NULL, NULL, 'LINESTRING(1.0001 0, 2 0)'), (3, NULL, NULL, 'LINESTRING(1 0, 1 1)')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT pgr_createTopology('roads', 0.001) AS done").unwrap(), "done\nOK\nSELECT 1");
        assert_eq!(
            run(&mut session, "SELECT id, source, target FROM roads ORDER BY id").unwrap(),
            "id|source|target\n1|1|2\n2|2|3\n3|2|4\nSELECT 3"
        );
        assert_eq!(run(&mut session, "SELECT cnt FROM roads_vertices_pgr WHERE id = 2").unwrap(), "cnt\n3\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT pgr_createVerticesTable('roads') AS done").unwrap(), "done\nOK\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT count(*) AS n FROM roads_vertices_pgr").unwrap(), "n\n4\nSELECT 1");
        assert!(run(&mut session, "SELECT pgr_createTopology('roads', -1)").is_err());
    }

    #[test]
    fn astar_and_dijkstra_cost_answer_like_dijkstra() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION pgrouting",
            "CREATE TABLE ways (id bigint, source bigint, target bigint, cost double precision, x1 double precision, y1 double precision, x2 double precision, y2 double precision)",
            "INSERT INTO ways VALUES (1, 1, 2, 1, 0, 0, 1, 0), (2, 2, 3, 1, 1, 0, 2, 0), (3, 1, 3, 5, 0, 0, 2, 0)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT node, edge, agg_cost FROM pgr_aStar('SELECT * FROM ways', 1, 3)").unwrap(),
            "node|edge|agg_cost\n1|1|0\n2|2|1\n3|-1|2\nSELECT 3"
        );
        assert_eq!(
            run(&mut session, "SELECT agg_cost FROM pgr_dijkstraCost('SELECT id, source, target, cost FROM ways', 1, 3)").unwrap(),
            "agg_cost\n2\nSELECT 1"
        );
        assert_eq!(run(&mut session, "SELECT count(*) AS n FROM pgr_bdDijkstra('SELECT id, source, target, cost FROM ways', 1, 3)").unwrap(), "n\n3\nSELECT 1");
    }
}
