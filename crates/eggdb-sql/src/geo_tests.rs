use crate::memory_store::MemoryStore;
use crate::session::Session;
use crate::tests::render;

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    let mut out = Vec::new();
    session
        .execute(sql, &mut |result| out.push(render(&result)))
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    out.join("\n")
}

#[test]
fn postgis_functions_read_measure_and_write_geometries() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE EXTENSION IF NOT EXISTS postgis");
    assert_eq!(
        run(&mut session, "SELECT ST_AsText(ST_MakePoint(1, 2))"),
        run(&mut session, "SELECT 'POINT(1 2)'")
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT ST_AsEWKT(ST_SetSRID(ST_MakePoint(-71.06, 42.36), 4326))"
        ),
        run(&mut session, "SELECT 'SRID=4326;POINT(-71.06 42.36)'")
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT ST_Area('POLYGON((0 0,0 10,10 10,10 0,0 0))'), ST_Distance('POINT(0 0)', 'POINT(3 4)')"
        ),
        run(
            &mut session,
            "SELECT 100::double precision, 5::double precision"
        )
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT ST_Contains('POLYGON((0 0,0 10,10 10,10 0,0 0))', 'POINT(5 5)'), ST_Intersects('LINESTRING(0 0,2 2)', 'LINESTRING(0 2,2 0)')"
        ),
        run(&mut session, "SELECT true, true")
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT ST_AsGeoJSON('POINT(1 2)'), ST_X('POINT(1 2)'), ST_SRID(ST_GeomFromText('POINT(1 2)', 3857))"
        ),
        run(
            &mut session,
            r#"SELECT '{"type":"Point","coordinates":[1,2]}', 1::double precision, 3857"#
        )
    );
    assert_eq!(
        run(
            &mut session,
            r#"SELECT ST_AsText(ST_GeomFromGeoJSON('{"type":"LineString","coordinates":[[0,0],[1,1]]}'))"#
        ),
        run(&mut session, "SELECT 'LINESTRING(0 0,1 1)'")
    );
    assert!(
        session
            .execute(
                "SELECT ST_Distance('SRID=4326;POINT(0 0)', 'SRID=3857;POINT(1 1)')",
                &mut |_| {}
            )
            .is_err()
    );
    assert!(
        session
            .execute("SELECT ST_AsText('NOT A GEOMETRY')", &mut |_| {})
            .is_err()
    );
}

#[test]
fn geometry_columns_answer_spatial_queries() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        "CREATE TABLE places (name text, location geometry(Point, 4326))",
    );
    run(
        &mut session,
        "INSERT INTO places VALUES ('a', 'SRID=4326;POINT(1 1)'), ('b', 'SRID=4326;POINT(5 5)'), ('c', 'SRID=4326;POINT(20 20)')",
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT name FROM places WHERE ST_DWithin(location, 'SRID=4326;POINT(0 0)', 8) ORDER BY ST_Distance(location, 'SRID=4326;POINT(0 0)')"
        ),
        run(&mut session, "SELECT 'a' AS name UNION ALL SELECT 'b'")
    );
}

#[test]
fn tiles_web_mercator_and_geohashes_serve_web_maps() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        run(&mut session, "SELECT ST_AsEWKT(ST_TileEnvelope(1, 1, 0))"),
        run(
            &mut session,
            "SELECT 'SRID=3857;POLYGON((0 0,0 20037508.342789244,20037508.342789244 20037508.342789244,20037508.342789244 0,0 0))'"
        )
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT ST_SRID(ST_Transform(ST_SetSRID(ST_MakePoint(105.8542, 21.0285), 4326), 3857))"
        ),
        run(&mut session, "SELECT 3857")
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT round(ST_X(ST_Transform(ST_Transform('SRID=4326;POINT(105.8542 21.0285)', 3857), 4326))::numeric, 6)"
        ),
        run(&mut session, "SELECT 105.854200")
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT ST_GeoHash(ST_MakePoint(-5.6, 42.6), 5)"
        ),
        run(&mut session, "SELECT 'ezs42'")
    );
    for bad in [
        "SELECT ST_TileEnvelope(1, 2, 0)",
        "SELECT ST_Transform('POINT(1 2)', 3857)",
        "SELECT ST_Transform('SRID=4326;POINT(1 2)', 2154)",
        "SELECT ST_GeoHash('LINESTRING(0 0,1 1)')",
    ] {
        assert!(session.execute(bad, &mut |_| {}).is_err(), "{bad}");
    }
}
