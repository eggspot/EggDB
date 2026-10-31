use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, Node, parse_document};
use crate::types::{SqlType, Value};
use eggdb_geo::{Coord, Geometry, Spatial};

/// The spatial functions EggDB answers, PostGIS's names (also MySQL's `ST_` functions).
pub(crate) const FUNCTIONS: [&str; 33] = [
    "st_geomfromtext",
    "st_geometryfromtext",
    "st_geomfromewkt",
    "st_geogfromtext",
    "st_geographyfromtext",
    "st_astext",
    "st_asewkt",
    "st_asbinary",
    "st_asewkb",
    "st_geomfromwkb",
    "st_geomfromewkb",
    "st_asgeojson",
    "st_geomfromgeojson",
    "st_makepoint",
    "st_point",
    "st_setsrid",
    "st_srid",
    "st_x",
    "st_y",
    "st_distance",
    "st_dwithin",
    "st_contains",
    "st_within",
    "st_intersects",
    "st_disjoint",
    "st_area",
    "st_length",
    "st_envelope",
    "st_distancesphere",
    "st_npoints",
    "st_geometrytype",
    "st_isempty",
    "st_equals",
];

/// The result type of a spatial function.
pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    Some(match name {
        "st_asbinary" | "st_asewkb" => SqlType::Bytea,
        "st_x" | "st_y" | "st_distance" | "st_area" | "st_length" | "st_distancesphere" => SqlType::Double,
        "st_srid" | "st_npoints" => SqlType::Integer,
        "st_dwithin" | "st_contains" | "st_within" | "st_intersects" | "st_disjoint" | "st_isempty" | "st_equals" => SqlType::Boolean,
        name if FUNCTIONS.contains(&name) => SqlType::Text,
        _ => return None,
    })
}

fn geo_error(error: eggdb_geo::GeoError) -> SqlError {
    SqlError::new(SqlState::INTERNAL_ERROR, error.0).with_hint("\"POINT(1 2)\" and \"SRID=4326;POINT(1 2)\" are geometries")
}

fn spatial_of(value: &Value) -> SqlResult<Option<Spatial>> {
    Ok(match value {
        Value::Null => None,
        Value::Text(text) | Value::Bpchar(text) => Some(eggdb_geo::from_wkt(text).map_err(geo_error)?),
        Value::Bytea(bytes) => Some(eggdb_geo::from_wkb(bytes).map_err(geo_error)?),
        other => {
            return Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("a geometry is expected, not {}", other.type_name())));
        }
    })
}

fn geometry_value(spatial: &Spatial) -> Value {
    Value::Text(eggdb_geo::to_wkt(spatial, true))
}

fn number(value: &Value) -> SqlResult<f64> {
    value.as_float().filter(|number| number.is_finite()).ok_or_else(|| SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "a finite number is expected"))
}

fn srid_of(value: &Value) -> SqlResult<Option<i32>> {
    let srid = value.as_integer().and_then(|srid| i32::try_from(srid).ok()).ok_or_else(|| SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "an SRID is an integer"))?;
    Ok((srid != 0).then_some(srid))
}

fn same_srid(left: &Spatial, right: &Spatial) -> SqlResult<()> {
    if left.srid == right.srid {
        return Ok(());
    }
    Err(SqlError::new(
        SqlState::INTERNAL_ERROR,
        format!("Operation on mixed SRID geometries ({} != {})", left.srid.unwrap_or(0), right.srid.unwrap_or(0)),
    ))
}

fn coord_of(node: &Node) -> SqlResult<Coord> {
    let invalid = || SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "invalid GeoJSON coordinates");
    let Json::Array(items) = &node.value else {
        return Err(invalid());
    };
    let value = |at: usize| match items.get(at).map(|item| &item.value) {
        Some(Json::Number(text)) => text.parse::<f64>().ok().filter(|number| number.is_finite()).ok_or_else(invalid),
        _ => Err(invalid()),
    };
    Ok(Coord { x: value(0)?, y: value(1)? })
}

fn list<T>(node: &Node, each: impl Fn(&Node) -> SqlResult<T>) -> SqlResult<Vec<T>> {
    match &node.value {
        Json::Array(items) => items.iter().map(each).collect(),
        _ => Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "invalid GeoJSON coordinates")),
    }
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, value)| value),
        _ => None,
    }
}

fn from_geojson(node: &Node, depth: usize) -> SqlResult<Geometry> {
    let invalid = |message: &str| SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message.to_owned());
    if depth > eggdb_geo::MAX_DEPTH {
        return Err(invalid("GeoJSON collections are nested too deeply"));
    }
    let kind = match member(node, "type").map(|node| &node.value) {
        Some(Json::String(kind)) => kind.as_str(),
        _ => return Err(invalid("GeoJSON has no type")),
    };
    if kind == "GeometryCollection" {
        let items = member(node, "geometries").ok_or_else(|| invalid("a GeometryCollection has no geometries"))?;
        return Ok(Geometry::Collection(list(items, |item| from_geojson(item, depth + 1))?));
    }
    let coordinates = member(node, "coordinates").ok_or_else(|| invalid("GeoJSON has no coordinates"))?;
    Ok(match kind {
        "Point" => Geometry::Point(match &coordinates.value {
            Json::Array(items) if items.is_empty() => None,
            _ => Some(coord_of(coordinates)?),
        }),
        "LineString" => Geometry::LineString(list(coordinates, coord_of)?),
        "MultiPoint" => Geometry::MultiPoint(list(coordinates, coord_of)?),
        "Polygon" => Geometry::Polygon(list(coordinates, |ring| list(ring, coord_of))?),
        "MultiLineString" => Geometry::MultiLineString(list(coordinates, |line| list(line, coord_of))?),
        "MultiPolygon" => Geometry::MultiPolygon(list(coordinates, |polygon| list(polygon, |ring| list(ring, coord_of)))?),
        other => return Err(invalid(&format!("unknown GeoJSON type {other}"))),
    })
}

/// Runs a spatial function. Geometries are EWKT text (or WKB bytes) in and EWKT text out.
pub(crate) fn function(name: &str, arguments: &[Value]) -> SqlResult<Value> {
    if arguments.iter().any(|argument| matches!(argument, Value::Null)) && name != "st_makepoint" {
        return Ok(Value::Null);
    }
    let spatial = |at: usize| -> SqlResult<Spatial> {
        arguments.get(at).map(spatial_of).transpose()?.flatten().ok_or_else(|| SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("function {name} needs a geometry argument")))
    };
    let pair = || -> SqlResult<(Spatial, Spatial)> {
        let (left, right) = (spatial(0)?, spatial(1)?);
        same_srid(&left, &right)?;
        Ok((left, right))
    };
    Ok(match (name, arguments) {
        ("st_geomfromtext" | "st_geometryfromtext" | "st_geomfromewkt" | "st_geogfromtext" | "st_geographyfromtext", [_]) => {
            let mut parsed = spatial(0)?;
            if name.starts_with("st_geog") && parsed.srid.is_none() {
                parsed.srid = Some(4326);
            }
            geometry_value(&parsed)
        }
        ("st_geomfromtext" | "st_geometryfromtext" | "st_geomfromwkb", [_, srid]) => {
            let mut parsed = spatial(0)?;
            parsed.srid = srid_of(srid)?;
            geometry_value(&parsed)
        }
        ("st_geomfromwkb" | "st_geomfromewkb", [_]) => geometry_value(&spatial(0)?),
        ("st_astext", [_]) => Value::Text(eggdb_geo::to_wkt(&spatial(0)?, false)),
        ("st_asewkt", [_]) => Value::Text(eggdb_geo::to_wkt(&spatial(0)?, true)),
        ("st_asbinary", [_]) => Value::Bytea(eggdb_geo::to_wkb(&spatial(0)?, false)),
        ("st_asewkb", [_]) => Value::Bytea(eggdb_geo::to_wkb(&spatial(0)?, true)),
        ("st_asgeojson", [_, ..]) => Value::Text(eggdb_geo::to_geojson(&spatial(0)?.geometry)),
        ("st_geomfromgeojson", [Value::Text(text) | Value::Json(text) | Value::Jsonb(text)]) => {
            let node = parse_document(text).map_err(|error| SqlError::new(SqlState::INVALID_PARAMETER_VALUE, error.message))?;
            geometry_value(&Spatial { srid: Some(4326), geometry: from_geojson(&node, 0)? })
        }
        ("st_makepoint" | "st_point", [x, y]) => {
            if matches!(x, Value::Null) || matches!(y, Value::Null) {
                return Ok(Value::Null);
            }
            geometry_value(&Spatial { srid: None, geometry: Geometry::Point(Some(Coord { x: number(x)?, y: number(y)? })) })
        }
        ("st_point", [x, y, srid]) => geometry_value(&Spatial { srid: srid_of(srid)?, geometry: Geometry::Point(Some(Coord { x: number(x)?, y: number(y)? })) }),
        ("st_setsrid", [_, srid]) => {
            let mut changed = spatial(0)?;
            changed.srid = srid_of(srid)?;
            geometry_value(&changed)
        }
        ("st_srid", [_]) => Value::Integer(spatial(0)?.srid.unwrap_or(0)),
        ("st_x" | "st_y", [_]) => match spatial(0)?.geometry {
            Geometry::Point(Some(coord)) => Value::Double(if name == "st_x" { coord.x } else { coord.y }),
            Geometry::Point(None) => Value::Null,
            _ => return Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "Argument to ST_X() must have type POINT")),
        },
        ("st_distance", [_, _]) => {
            let (left, right) = pair()?;
            eggdb_geo::distance(&left.geometry, &right.geometry).map_or(Value::Null, Value::Double)
        }
        ("st_dwithin", [_, _, within]) => {
            let (left, right) = pair()?;
            Value::Boolean(eggdb_geo::distance(&left.geometry, &right.geometry).is_some_and(|distance| distance <= number(within).unwrap_or(-1.0)))
        }
        ("st_contains", [_, _]) => {
            let (left, right) = pair()?;
            Value::Boolean(eggdb_geo::contains(&left.geometry, &right.geometry))
        }
        ("st_within", [_, _]) => {
            let (left, right) = pair()?;
            Value::Boolean(eggdb_geo::contains(&right.geometry, &left.geometry))
        }
        ("st_intersects" | "st_disjoint", [_, _]) => {
            let (left, right) = pair()?;
            let meets = eggdb_geo::intersects(&left.geometry, &right.geometry);
            Value::Boolean(if name == "st_intersects" { meets } else { !meets })
        }
        ("st_equals", [_, _]) => {
            let (left, right) = pair()?;
            Value::Boolean(eggdb_geo::contains(&left.geometry, &right.geometry) && eggdb_geo::contains(&right.geometry, &left.geometry))
        }
        ("st_area", [_]) => Value::Double(eggdb_geo::area(&spatial(0)?.geometry)),
        ("st_length", [_]) => Value::Double(eggdb_geo::length(&spatial(0)?.geometry)),
        ("st_envelope", [_]) => {
            let source = spatial(0)?;
            match eggdb_geo::envelope(&source.geometry) {
                Some(geometry) => geometry_value(&Spatial { srid: source.srid, geometry }),
                None => geometry_value(&source),
            }
        }
        ("st_distancesphere", [_, _]) => {
            let (left, right) = pair()?;
            Value::Double(eggdb_geo::sphere_distance(&left.geometry, &right.geometry).map_err(geo_error)?)
        }
        ("st_npoints", [_]) => Value::Integer(i32::try_from(spatial(0)?.geometry.coords().len()).unwrap_or(i32::MAX)),
        ("st_geometrytype", [_]) => Value::Text(spatial(0)?.geometry.type_name().to_owned()),
        ("st_isempty", [_]) => Value::Boolean(spatial(0)?.geometry.is_empty()),
        _ => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function {name} does not take {} arguments", arguments.len()),
            ));
        }
    })
}
