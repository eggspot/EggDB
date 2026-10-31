use crate::{Coord, Geometry};
use std::fmt::Write;

fn coord(out: &mut String, coord: &Coord) {
    let _ = write!(out, "[{},{}]", coord.x, coord.y);
}

fn coords(out: &mut String, coords: &[Coord]) {
    out.push('[');
    for (at, item) in coords.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        coord(out, item);
    }
    out.push(']');
}

fn rings(out: &mut String, rings: &[Vec<Coord>]) {
    out.push('[');
    for (at, ring) in rings.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        coords(out, ring);
    }
    out.push(']');
}

fn body(out: &mut String, geometry: &Geometry) {
    match geometry {
        Geometry::Point(Some(point)) => coord(out, point),
        Geometry::Point(None) => out.push_str("[]"),
        Geometry::LineString(line) | Geometry::MultiPoint(line) => coords(out, line),
        Geometry::Polygon(polygon) | Geometry::MultiLineString(polygon) => rings(out, polygon),
        Geometry::MultiPolygon(polygons) => {
            out.push('[');
            for (at, polygon) in polygons.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                rings(out, polygon);
            }
            out.push(']');
        }
        Geometry::Collection(_) => {}
    }
}

/// Writes the geometry as an RFC 7946 GeoJSON geometry object.
pub fn to_geojson(geometry: &Geometry) -> String {
    let mut out = String::new();
    if let Geometry::Collection(items) = geometry {
        out.push_str("{\"type\":\"GeometryCollection\",\"geometries\":[");
        for (at, item) in items.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            out.push_str(&to_geojson(item));
        }
        out.push_str("]}");
        return out;
    }
    let kind = &geometry.type_name()[3..];
    let _ = write!(out, "{{\"type\":\"{kind}\",\"coordinates\":");
    body(&mut out, geometry);
    out.push('}');
    out
}
