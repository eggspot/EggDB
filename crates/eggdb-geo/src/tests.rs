use crate::{Coord, Geometry, area, contains, distance, envelope, from_wkb, from_wkt, intersects, length, sphere_distance, to_geojson, to_wkb, to_wkt};

fn geometry(text: &str) -> Geometry {
    from_wkt(text).unwrap().geometry
}

#[test]
fn wkt_and_wkb_round_trip_every_type() {
    for text in [
        "POINT(1 2)",
        "POINT EMPTY",
        "LINESTRING(0 0,1 1,2 0.5)",
        "POLYGON((0 0,0 10,10 10,10 0,0 0),(2 2,2 3,3 3,2 2))",
        "MULTIPOINT(1 2,3 4)",
        "MULTILINESTRING((0 0,1 1),(2 2,3 3))",
        "MULTIPOLYGON(((0 0,0 1,1 1,0 0)),((5 5,5 6,6 6,5 5)))",
        "GEOMETRYCOLLECTION(POINT(1 1),LINESTRING(0 0,1 1))",
        "GEOMETRYCOLLECTION EMPTY",
    ] {
        let spatial = from_wkt(text).unwrap();
        assert_eq!(to_wkt(&spatial, false), text);
        assert_eq!(from_wkb(&to_wkb(&spatial, true)).unwrap(), spatial, "{text}");
    }
    let ewkt = from_wkt("SRID=4326;POINT(-71.06 42.36)").unwrap();
    assert_eq!(ewkt.srid, Some(4326));
    assert_eq!(to_wkt(&ewkt, true), "SRID=4326;POINT(-71.06 42.36)");
    assert_eq!(from_wkb(&to_wkb(&ewkt, true)).unwrap().srid, Some(4326));
    assert_eq!(from_wkt("multipoint((1 2),(3 4))").unwrap().geometry, Geometry::MultiPoint(vec![Coord { x: 1.0, y: 2.0 }, Coord { x: 3.0, y: 4.0 }]));
}

#[test]
fn malformed_input_is_refused() {
    for text in ["", "POINT(1)", "POINT Z (1 2 3)", "CIRCLE(1 2)", "POINT(1 2) extra", "LINESTRING(0 0,", "SRID=x;POINT(1 2)", "POINT(1e999 2)", "\u{3000}POINT\u{3000}(1 2)x"] {
        assert!(from_wkt(text).is_err(), "{text}");
    }
    let wkb = to_wkb(&from_wkt("LINESTRING(0 0,1 1)").unwrap(), false);
    for cut in 0..wkb.len() {
        assert!(from_wkb(&wkb[..cut]).is_err());
    }
    assert!(from_wkb(&[1, 2, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f]).is_err());
    let mut deep = "POINT(0 0)".to_owned();
    for _ in 0..40 {
        deep = format!("GEOMETRYCOLLECTION({deep})");
    }
    assert!(from_wkt(&deep).is_err());
}

#[test]
fn measures_and_predicates_follow_the_planar_definitions() {
    let square = geometry("POLYGON((0 0,0 10,10 10,10 0,0 0),(2 2,2 4,4 4,4 2,2 2))");
    assert_eq!(area(&square), 96.0);
    assert_eq!(length(&geometry("LINESTRING(0 0,3 4,3 10)")), 11.0);
    assert!(contains(&square, &geometry("POINT(5 5)")));
    assert!(!contains(&square, &geometry("POINT(3 3)")));
    assert!(!contains(&square, &geometry("LINESTRING(5 5,15 5)")));
    assert!(contains(&square, &geometry("LINESTRING(5 5,8 8)")));
    assert!(intersects(&square, &geometry("LINESTRING(5 5,15 5)")));
    assert!(intersects(&geometry("LINESTRING(0 0,2 2)"), &geometry("LINESTRING(0 2,2 0)")));
    assert!(!intersects(&geometry("LINESTRING(0 0,1 0)"), &geometry("LINESTRING(0 1,1 1)")));
    assert_eq!(distance(&geometry("POINT(0 0)"), &geometry("POINT(3 4)")), Some(5.0));
    assert_eq!(distance(&geometry("POINT(0 5)"), &geometry("LINESTRING(-1 0,1 0)")), Some(5.0));
    assert_eq!(distance(&square, &geometry("POINT(5 5)")), Some(0.0));
    assert_eq!(distance(&geometry("POINT EMPTY"), &square), None);
    assert_eq!(to_wkt(&crate::Spatial { srid: None, geometry: envelope(&geometry("LINESTRING(1 5,3 2)")).unwrap() }, false), "POLYGON((1 2,1 5,3 5,3 2,1 2))");
    let boston = geometry("POINT(-71.0589 42.3601)");
    let new_york = geometry("POINT(-74.0060 40.7128)");
    let metres = sphere_distance(&boston, &new_york).unwrap();
    assert!((metres - 306_000.0).abs() < 2_000.0, "{metres}");
}

#[test]
fn geojson_writes_rfc_7946_objects() {
    assert_eq!(to_geojson(&geometry("POINT(1 2)")), r#"{"type":"Point","coordinates":[1,2]}"#);
    assert_eq!(to_geojson(&geometry("POLYGON((0 0,0 1,1 1,0 0))")), r#"{"type":"Polygon","coordinates":[[[0,0],[0,1],[1,1],[0,0]]]}"#);
    assert_eq!(
        to_geojson(&geometry("GEOMETRYCOLLECTION(POINT(1 1))")),
        r#"{"type":"GeometryCollection","geometries":[{"type":"Point","coordinates":[1,1]}]}"#
    );
}

#[test]
fn web_mercator_tiles_and_geohashes_match_their_definitions() {
    use crate::{from_web_mercator, geohash, tile_envelope, to_web_mercator};
    let Geometry::Polygon(world) = tile_envelope(0, 0, 0).unwrap() else { unreachable!() };
    assert!((world[0][2].x - 20_037_508.342_789_244).abs() < 1e-6 && (world[0][0].y + 20_037_508.342_789_244).abs() < 1e-6);
    let Geometry::Polygon(tile) = tile_envelope(1, 1, 0).unwrap() else { unreachable!() };
    assert_eq!((tile[0][0].x, tile[0][1].y), (0.0, 20_037_508.342_789_244));
    assert!(tile_envelope(2, 4, 0).is_err() && tile_envelope(31, 0, 0).is_err());
    let hanoi = Geometry::Point(Some(Coord { x: 105.8542, y: 21.0285 }));
    let Geometry::Point(Some(projected)) = to_web_mercator(&hanoi).unwrap() else { unreachable!() };
    assert!((projected.x - 11_783_635.642).abs() < 0.01 && (projected.y - 2_395_277.233).abs() < 0.01, "{projected:?}");
    let Geometry::Point(Some(back)) = from_web_mercator(&Geometry::Point(Some(projected))).unwrap() else { unreachable!() };
    assert!((back.x - 105.8542).abs() < 1e-9 && (back.y - 21.0285).abs() < 1e-9);
    assert!(to_web_mercator(&Geometry::Point(Some(Coord { x: 0.0, y: 89.0 }))).is_err());
    assert_eq!(geohash(Coord { x: -5.6, y: 42.6 }, 5).unwrap(), "ezs42");
    assert_eq!(geohash(Coord { x: 10.40744, y: 57.64911 }, 11).unwrap(), "u4pruydqqvj");
    assert!(geohash(Coord { x: 0.0, y: 0.0 }, 0).is_err());
}
