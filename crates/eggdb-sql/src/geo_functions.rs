use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, Node, parse_document};
use crate::types::{SqlType, Value};
use eggdb_geo::{Coord, Geometry, Spatial};

const GEOGRAPHY_SRID: i32 = 4326;

/// The spatial functions EggDB answers, PostGIS's names (also MySQL's `ST_` functions).
pub(crate) const FUNCTIONS: [&str; 99] = [
    "st_issimple",
    "st_isring",
    "st_collect",
    "st_points",
    "st_isclosed",
    "st_iscollection",
    "st_dimension",
    "st_ndims",
    "st_coorddim",
    "st_numpoints",
    "st_closestpoint",
    "st_shortestline",
    "st_longestline",
    "st_maxdistance",
    "st_hausdorffdistance",
    "st_rotate",
    "st_affine",
    "st_snaptogrid",
    "st_removerepeatedpoints",
    "st_expand",
    "st_boundary",
    "st_segmentize",
    "st_multi",
    "st_force2d",
    "st_makepolygon",
    "st_addpoint",
    "st_removepoint",
    "st_setpoint",
    "st_azimuth",
    "sqlserver_stdistance",
    "sqlserver_stgeometrytype",
    "st_linelocatepoint",
    "st_lineinterpolatepoint",
    "st_linesubstring",
    "st_intersection",
    "st_centroid",
    "st_perimeter",
    "st_convexhull",
    "st_isvalid",
    "st_isvalidreason",
    "st_numgeometries",
    "st_geometryn",
    "st_startpoint",
    "st_endpoint",
    "st_pointn",
    "st_exteriorring",
    "st_numinteriorrings",
    "st_reverse",
    "st_flipcoordinates",
    "st_translate",
    "st_scale",
    "st_makeline",
    "st_makeenvelope",
    "st_xmin",
    "st_xmax",
    "st_ymin",
    "st_ymax",
    "st_simplify",
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
    "st_distancespheroid",
    "st_lengthspheroid",
    "st_npoints",
    "st_geometrytype",
    "st_isempty",
    "st_equals",
    "st_transform",
    "st_tileenvelope",
    "st_geohash",
    "st_asmvtgeom",
    "s2_cellidfrompoint",
    "s2_cellidtotoken",
];

/// The result type of a spatial function.
pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    Some(match name {
        "st_asbinary" | "st_asewkb" => SqlType::Bytea,
        "s2_cellidfrompoint" => SqlType::BigInt,
        "s2_cellidtotoken" => SqlType::Text,
        "st_x" | "st_y" | "st_distance" | "sqlserver_stdistance" | "st_area" | "st_length" | "st_distancesphere"
        | "st_distancespheroid" | "st_lengthspheroid"
        | "st_perimeter" | "st_xmin" | "st_xmax" | "st_ymin" | "st_ymax" | "st_linelocatepoint" | "st_azimuth" | "st_maxdistance" | "st_hausdorffdistance" => SqlType::Double,
        "st_srid" | "st_npoints" | "st_numgeometries" | "st_numinteriorrings" | "st_dimension" | "st_ndims"
        | "st_coorddim" | "st_numpoints" => {
            SqlType::Integer
        }
        "st_dwithin" | "st_contains" | "st_within" | "st_intersects" | "st_disjoint"
        | "st_isempty" | "st_equals" | "st_isvalid" | "st_isclosed" | "st_iscollection" | "st_issimple" | "st_isring" => SqlType::Boolean,
        name if FUNCTIONS.contains(&name) => SqlType::Text,
        _ => return None,
    })
}

fn argument_error(error: eggdb_geo::GeoError) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, error.0)
}

fn geo_error(error: eggdb_geo::GeoError) -> SqlError {
    SqlError::new(SqlState::INTERNAL_ERROR, error.0)
        .with_hint("\"POINT(1 2)\" and \"SRID=4326;POINT(1 2)\" are geometries")
}

fn spatial_of(value: &Value) -> SqlResult<Option<Spatial>> {
    Ok(match value {
        Value::Null => None,
        Value::Text(text) | Value::Bpchar(text) => {
            Some(eggdb_geo::from_wkt(text).map_err(geo_error)?)
        }
        Value::Bytea(bytes) => Some(eggdb_geo::from_wkb(bytes).map_err(geo_error)?),
        other if crate::geometry_type::geometry_of(other).is_some() => Some(
            eggdb_geo::from_wkb(crate::geometry_type::geometry_of(other).unwrap_or_default()).map_err(geo_error)?,
        ),
        other => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("a geometry is expected, not {}", other.type_name()),
            ));
        }
    })
}

/// The internal function `ST_AsMVT(row, ...)` becomes: `json_agg` of the rows, then the
/// layer name, extent, geometry column and feature id column.
pub(crate) const MVT_ENCODE: &str = "eggdb_mvt_encode";

fn mvt_property(value: &crate::json::Json) -> Option<eggdb_geo::MvtValue> {
    use crate::json::Json;
    Some(match value {
        Json::String(text) => eggdb_geo::MvtValue::String(text.clone()),
        Json::Boolean(flag) => eggdb_geo::MvtValue::Bool(*flag),
        Json::Number(text) => match text.parse::<i64>() {
            Ok(whole) => eggdb_geo::MvtValue::Int(whole),
            Err(_) => eggdb_geo::MvtValue::Double(text.parse().ok()?),
        },
        Json::Null => return None,
        Json::Array(_) | Json::Object(_) => {
            eggdb_geo::MvtValue::String(crate::json::normalized_value(value).ok()?)
        }
    })
}

/// `ST_AsMVT`'s encoding: each aggregated row's geometry column (named, or the first value
/// that reads as a geometry) becomes a feature, the other non-NULL columns its properties.
pub(crate) fn mvt_encode(values: &[Value]) -> SqlResult<Value> {
    use crate::json::Json;
    let text_option = |at: usize| match values.get(at) {
        Some(Value::Text(text)) => Some(text.clone()),
        _ => None,
    };
    let rows = match values.first() {
        Some(Value::Json(text) | Value::Text(text)) => crate::json::parse(text, false)?,
        _ => return Ok(Value::Bytea(Vec::new())),
    };
    let name = text_option(1).unwrap_or_else(|| "default".to_owned());
    let extent = match values.get(2) {
        None | Some(Value::Null) => 4096,
        Some(value) => u32::try_from(value.as_integer().unwrap_or(4096)).map_err(|_| {
            SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "ST_AsMVT: the extent must be positive")
        })?,
    };
    let geometry_column = text_option(3);
    let id_column = text_option(4);
    let Json::Array(items) = rows.value else {
        return Ok(Value::Bytea(Vec::new()));
    };
    let mut features = Vec::with_capacity(items.len());
    for item in items {
        let Json::Object(members) = item.value else {
            continue;
        };
        let geometry_at = members.iter().position(|(key, value)| match (&geometry_column, &value.value) {
            (Some(wanted), _) => key == wanted,
            (None, Json::String(text)) => eggdb_geo::from_wkt(text).is_ok(),
            _ => false,
        });
        let Some(geometry_at) = geometry_at else {
            continue;
        };
        let Json::String(geometry_text) = &members[geometry_at].1.value else {
            continue;
        };
        let geometry = eggdb_geo::from_wkt(geometry_text).map_err(geo_error)?.geometry;
        let mut id = None;
        let mut properties = Vec::new();
        for (at, (key, value)) in members.iter().enumerate() {
            if at == geometry_at {
                continue;
            }
            if id_column.as_deref() == Some(key.as_str()) {
                id = match &value.value {
                    Json::Number(text) => text.parse::<u64>().ok(),
                    _ => None,
                };
                continue;
            }
            if let Some(property) = mvt_property(&value.value) {
                properties.push((key.clone(), property));
            }
        }
        features.push(eggdb_geo::MvtFeature {
            id,
            geometry,
            properties,
        });
    }
    Ok(Value::Bytea(eggdb_geo::encode_layer(&name, extent, &features)))
}

fn geometry_value(spatial: &Spatial) -> Value {
    Value::Text(eggdb_geo::to_wkt(spatial, true))
}

fn spheroid_argument(value: Option<&Value>) -> SqlResult<eggdb_geo::Spheroid> {
    match value {
        None | Some(Value::Null) => Ok(eggdb_geo::Spheroid::WGS84),
        Some(value) => {
            let text = value.to_text().unwrap_or_default();
            eggdb_geo::Spheroid::parse(&text).map_err(argument_error)
        }
    }
}

/// `ST_Dump`: a geometry's parts with their paths (D1274).
pub(crate) const DUMP: &str = "st_dump";
/// `ST_DumpPoints`: a geometry's vertices with their paths.
pub(crate) const DUMP_POINTS: &str = "st_dumppoints";
/// The columns `ST_Dump` and `ST_DumpPoints` return.
pub(crate) const DUMP_COLUMNS: [(&str, SqlType); 2] = [
    ("path", SqlType::Array(crate::arrays::ArrayElement::Integer)),
    ("geom", SqlType::Text),
];

fn deeper(path: &[i32], at: usize) -> Vec<i32> {
    let mut deeper = path.to_vec();
    deeper.push(i32::try_from(at + 1).unwrap_or(i32::MAX));
    deeper
}

fn dumped(geometry: &Geometry, path: &[i32], out: &mut Vec<(Vec<i32>, Geometry)>) {
    match geometry {
        Geometry::Collection(items) => {
            for (at, item) in items.iter().enumerate() {
                dumped(item, &deeper(path, at), out);
            }
        }
        Geometry::MultiPoint(_) | Geometry::MultiLineString(_) | Geometry::MultiPolygon(_) => {
            for (at, part) in eggdb_geo::parts(geometry).into_iter().enumerate() {
                out.push((deeper(path, at), part));
            }
        }
        single => out.push((path.to_vec(), single.clone())),
    }
}

fn dumped_line(points: &[Coord], path: &[i32], out: &mut Vec<(Vec<i32>, Geometry)>) {
    for (at, point) in points.iter().enumerate() {
        out.push((deeper(path, at), Geometry::Point(Some(*point))));
    }
}

fn dumped_points(geometry: &Geometry, path: &[i32], out: &mut Vec<(Vec<i32>, Geometry)>) {
    match geometry {
        Geometry::Point(Some(point)) => out.push((path.to_vec(), Geometry::Point(Some(*point)))),
        Geometry::Point(None) => {}
        Geometry::LineString(points) | Geometry::MultiPoint(points) => dumped_line(points, path, out),
        Geometry::Polygon(lines) | Geometry::MultiLineString(lines) => {
            for (at, line) in lines.iter().enumerate() {
                dumped_line(line, &deeper(path, at), out);
            }
        }
        Geometry::MultiPolygon(polygons) => {
            for (at, rings) in polygons.iter().enumerate() {
                let polygon_path = deeper(path, at);
                for (ring_at, ring) in rings.iter().enumerate() {
                    dumped_line(ring, &deeper(&polygon_path, ring_at), out);
                }
            }
        }
        Geometry::Collection(items) => {
            for (at, item) in items.iter().enumerate() {
                dumped_points(item, &deeper(path, at), out);
            }
        }
    }
}

/// The rows of `ST_Dump(geometry)` or `ST_DumpPoints(geometry)`: each part or vertex with its
/// path (1-based positions down the geometry's nesting, empty for a single geometry dumped
/// whole), keeping the SRID.
pub(crate) fn dump_records(name: &str, values: &[Value], sink: &mut crate::json_functions::RecordSink<'_>) -> SqlResult<()> {
    let Some(source) = values.first().map(spatial_of).transpose()?.flatten() else {
        return Ok(());
    };
    let mut found = Vec::new();
    if name == DUMP_POINTS {
        dumped_points(&source.geometry, &[], &mut found);
    } else {
        dumped(&source.geometry, &[], &mut found);
    }
    for (path, geometry) in found {
        let path = Value::Array(Box::new(crate::arrays::ArrayValue::new(
            crate::arrays::ArrayElement::Integer,
            path.into_iter().map(Value::Integer).collect(),
        )));
        sink(vec![path, geometry_value(&Spatial { srid: source.srid, geometry })])?;
    }
    Ok(())
}

fn collected(geometries: Vec<Geometry>) -> Geometry {
    if geometries.iter().all(|geometry| matches!(geometry, Geometry::Point(Some(_)))) {
        return Geometry::MultiPoint(geometries.iter().flat_map(Geometry::coords).collect());
    }
    if geometries.iter().all(|geometry| matches!(geometry, Geometry::LineString(_))) {
        return Geometry::MultiLineString(
            geometries.into_iter().filter_map(|geometry| match geometry {
                Geometry::LineString(points) => Some(points),
                _ => None,
            }).collect(),
        );
    }
    if geometries.iter().all(|geometry| matches!(geometry, Geometry::Polygon(_))) {
        return Geometry::MultiPolygon(
            geometries.into_iter().filter_map(|geometry| match geometry {
                Geometry::Polygon(rings) => Some(rings),
                _ => None,
            }).collect(),
        );
    }
    Geometry::Collection(geometries)
}

fn line_closed(points: &[Coord]) -> bool {
    points.len() >= 2 && points.first() == points.last()
}

fn closed(geometry: &Geometry) -> bool {
    match geometry {
        Geometry::LineString(points) => line_closed(points),
        Geometry::MultiLineString(lines) => !lines.is_empty() && lines.iter().all(|line| line_closed(line)),
        Geometry::Collection(items) => items.iter().all(closed),
        _ => true,
    }
}

fn dimension(geometry: &Geometry) -> i32 {
    match geometry {
        Geometry::Point(_) | Geometry::MultiPoint(_) => 0,
        Geometry::LineString(_) | Geometry::MultiLineString(_) => 1,
        Geometry::Polygon(_) | Geometry::MultiPolygon(_) => 2,
        Geometry::Collection(items) => items.iter().map(dimension).max().unwrap_or(0),
    }
}

fn point_at(spatial: &Spatial) -> SqlResult<Coord> {
    match spatial.geometry {
        Geometry::Point(Some(point)) => Ok(point),
        _ => Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "a point is expected")),
    }
}

fn number(value: &Value) -> SqlResult<f64> {
    value
        .as_float()
        .filter(|number| number.is_finite())
        .ok_or_else(|| {
            SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "a finite number is expected",
            )
        })
}

fn srid_of(value: &Value) -> SqlResult<Option<i32>> {
    let srid = value
        .as_integer()
        .and_then(|srid| i32::try_from(srid).ok())
        .ok_or_else(|| SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "an SRID is an integer"))?;
    Ok((srid != 0).then_some(srid))
}

fn same_srid(left: &Spatial, right: &Spatial) -> SqlResult<()> {
    if left.srid == right.srid {
        return Ok(());
    }
    Err(SqlError::new(
        SqlState::INTERNAL_ERROR,
        format!(
            "Operation on mixed SRID geometries ({} != {})",
            left.srid.unwrap_or(0),
            right.srid.unwrap_or(0)
        ),
    ))
}

fn coord_of(node: &Node) -> SqlResult<Coord> {
    let invalid = || {
        SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "invalid GeoJSON coordinates",
        )
    };
    let Json::Array(items) = &node.value else {
        return Err(invalid());
    };
    let value = |at: usize| match items.get(at).map(|item| &item.value) {
        Some(Json::Number(text)) => text
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .ok_or_else(invalid),
        _ => Err(invalid()),
    };
    Ok(Coord {
        x: value(0)?,
        y: value(1)?,
    })
}

fn list<T>(node: &Node, each: impl Fn(&Node) -> SqlResult<T>) -> SqlResult<Vec<T>> {
    match &node.value {
        Json::Array(items) => items.iter().map(each).collect(),
        _ => Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "invalid GeoJSON coordinates",
        )),
    }
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn from_geojson(node: &Node, depth: usize) -> SqlResult<Geometry> {
    let invalid =
        |message: &str| SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message.to_owned());
    if depth > eggdb_geo::MAX_DEPTH {
        return Err(invalid("GeoJSON collections are nested too deeply"));
    }
    let kind = match member(node, "type").map(|node| &node.value) {
        Some(Json::String(kind)) => kind.as_str(),
        _ => return Err(invalid("GeoJSON has no type")),
    };
    if kind == "GeometryCollection" {
        let items = member(node, "geometries")
            .ok_or_else(|| invalid("a GeometryCollection has no geometries"))?;
        return Ok(Geometry::Collection(list(items, |item| {
            from_geojson(item, depth + 1)
        })?));
    }
    let coordinates =
        member(node, "coordinates").ok_or_else(|| invalid("GeoJSON has no coordinates"))?;
    Ok(match kind {
        "Point" => Geometry::Point(match &coordinates.value {
            Json::Array(items) if items.is_empty() => None,
            _ => Some(coord_of(coordinates)?),
        }),
        "LineString" => Geometry::LineString(list(coordinates, coord_of)?),
        "MultiPoint" => Geometry::MultiPoint(list(coordinates, coord_of)?),
        "Polygon" => Geometry::Polygon(list(coordinates, |ring| list(ring, coord_of))?),
        "MultiLineString" => {
            Geometry::MultiLineString(list(coordinates, |line| list(line, coord_of))?)
        }
        "MultiPolygon" => Geometry::MultiPolygon(list(coordinates, |polygon| {
            list(polygon, |ring| list(ring, coord_of))
        })?),
        other => return Err(invalid(&format!("unknown GeoJSON type {other}"))),
    })
}

/// Runs a spatial function. Geometries are EWKT text (or WKB bytes) in and EWKT text out.
pub(crate) fn function(name: &str, arguments: &[Value]) -> SqlResult<Value> {
    if arguments
        .iter()
        .any(|argument| matches!(argument, Value::Null))
        && name != "st_makepoint"
    {
        return Ok(Value::Null);
    }
    let spatial = |at: usize| -> SqlResult<Spatial> {
        arguments
            .get(at)
            .map(spatial_of)
            .transpose()?
            .flatten()
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!("function {name} needs a geometry argument"),
                )
            })
    };
    let pair = || -> SqlResult<(Spatial, Spatial)> {
        let (left, right) = (spatial(0)?, spatial(1)?);
        same_srid(&left, &right)?;
        Ok((left, right))
    };
    Ok(match (name, arguments) {
        (
            "st_geomfromtext"
            | "st_geometryfromtext"
            | "st_geomfromewkt"
            | "st_geogfromtext"
            | "st_geographyfromtext",
            [_],
        ) => {
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
            let node = parse_document(text)
                .map_err(|error| SqlError::new(SqlState::INVALID_PARAMETER_VALUE, error.message))?;
            geometry_value(&Spatial {
                srid: Some(4326),
                geometry: from_geojson(&node, 0)?,
            })
        }
        ("st_makepoint" | "st_point", [x, y]) => {
            if matches!(x, Value::Null) || matches!(y, Value::Null) {
                return Ok(Value::Null);
            }
            geometry_value(&Spatial {
                srid: None,
                geometry: Geometry::Point(Some(Coord {
                    x: number(x)?,
                    y: number(y)?,
                })),
            })
        }
        ("st_point", [x, y, srid]) => geometry_value(&Spatial {
            srid: srid_of(srid)?,
            geometry: Geometry::Point(Some(Coord {
                x: number(x)?,
                y: number(y)?,
            })),
        }),
        ("st_setsrid", [_, srid]) => {
            let mut changed = spatial(0)?;
            changed.srid = srid_of(srid)?;
            geometry_value(&changed)
        }
        ("st_srid", [_]) => Value::Integer(spatial(0)?.srid.unwrap_or(0)),
        ("st_x" | "st_y", [_]) => match spatial(0)?.geometry {
            Geometry::Point(Some(coord)) => {
                Value::Double(if name == "st_x" { coord.x } else { coord.y })
            }
            Geometry::Point(None) => Value::Null,
            _ => {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "Argument to ST_X() must have type POINT",
                ));
            }
        },
        ("st_distance", [_, _]) => {
            let (left, right) = pair()?;
            eggdb_geo::distance(&left.geometry, &right.geometry).map_or(Value::Null, Value::Double)
        }
        ("st_dwithin", [_, _, within]) => {
            let (left, right) = pair()?;
            Value::Boolean(
                eggdb_geo::distance(&left.geometry, &right.geometry)
                    .is_some_and(|distance| distance <= number(within).unwrap_or(-1.0)),
            )
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
            Value::Boolean(if name == "st_intersects" {
                meets
            } else {
                !meets
            })
        }
        ("st_equals", [_, _]) => {
            let (left, right) = pair()?;
            Value::Boolean(
                eggdb_geo::contains(&left.geometry, &right.geometry)
                    && eggdb_geo::contains(&right.geometry, &left.geometry),
            )
        }
        ("st_area", [_]) => Value::Double(eggdb_geo::area(&spatial(0)?.geometry)),
        ("st_length", [_]) => Value::Double(eggdb_geo::length(&spatial(0)?.geometry)),
        ("st_envelope", [_]) => {
            let source = spatial(0)?;
            match eggdb_geo::envelope(&source.geometry) {
                Some(geometry) => geometry_value(&Spatial {
                    srid: source.srid,
                    geometry,
                }),
                None => geometry_value(&source),
            }
        }
        ("st_distancesphere", [_, _]) => {
            let (left, right) = pair()?;
            Value::Double(
                eggdb_geo::sphere_distance(&left.geometry, &right.geometry).map_err(geo_error)?,
            )
        }
        ("st_distancespheroid", [_, _] | [_, _, _]) => {
            let (left, right) = pair()?;
            let spheroid = spheroid_argument(arguments.get(2))?;
            Value::Double(eggdb_geo::spheroid_distance(&left.geometry, &right.geometry, spheroid).map_err(argument_error)?)
        }
        ("st_lengthspheroid", [_] | [_, _]) => {
            let spheroid = spheroid_argument(arguments.get(1))?;
            Value::Double(eggdb_geo::spheroid_length(&spatial(0)?.geometry, spheroid).map_err(argument_error)?)
        }
        ("st_collect", [Value::Array(array)]) => {
            let mut found: Vec<Spatial> = Vec::new();
            for item in array.items.iter().filter(|item| !matches!(item, Value::Null)) {
                if let Some(spatial) = spatial_of(item)? {
                    if let Some(first) = found.first() {
                        same_srid(first, &spatial)?;
                    }
                    found.push(spatial);
                }
            }
            match found.first().map(|first| first.srid) {
                None => Value::Null,
                Some(srid) => geometry_value(&Spatial { srid, geometry: collected(found.into_iter().map(|spatial| spatial.geometry).collect()) }),
            }
        }
        ("st_collect", [_, _]) => {
            let (left, right) = pair()?;
            geometry_value(&Spatial { srid: left.srid, geometry: collected(vec![left.geometry, right.geometry]) })
        }
        ("st_points", [_]) => {
            let source = spatial(0)?;
            geometry_value(&Spatial { srid: source.srid, geometry: Geometry::MultiPoint(source.geometry.coords()) })
        }
        ("st_isclosed", [_]) => Value::Boolean(closed(&spatial(0)?.geometry)),
        ("st_issimple", [_]) => Value::Boolean(eggdb_geo::is_simple(&spatial(0)?.geometry)),
        ("st_isring", [_]) => match spatial(0)?.geometry {
            Geometry::LineString(points) => {
                let line = Geometry::LineString(points);
                Value::Boolean(closed(&line) && eggdb_geo::is_simple(&line))
            }
            _ => return Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "ST_IsRing() should only be called on a linear feature")),
        },
        ("st_iscollection", [_]) => Value::Boolean(matches!(
            spatial(0)?.geometry,
            Geometry::MultiPoint(_) | Geometry::MultiLineString(_) | Geometry::MultiPolygon(_) | Geometry::Collection(_)
        )),
        ("st_dimension", [_]) => Value::Integer(dimension(&spatial(0)?.geometry)),
        ("st_ndims" | "st_coorddim", [_]) => {
            spatial(0)?;
            Value::Integer(2)
        }
        ("st_numpoints", [_]) => match spatial(0)?.geometry {
            Geometry::LineString(points) => Value::Integer(i32::try_from(points.len()).unwrap_or(i32::MAX)),
            _ => Value::Null,
        },
        ("st_npoints", [_]) => {
            Value::Integer(i32::try_from(spatial(0)?.geometry.coords().len()).unwrap_or(i32::MAX))
        }
        ("st_geometrytype", [_]) => Value::Text(spatial(0)?.geometry.type_name().to_owned()),
        ("sqlserver_stgeometrytype", [_]) => {
            let name = spatial(0)?.geometry.type_name().to_owned();
            Value::Text(name.strip_prefix("ST_").unwrap_or(&name).to_owned())
        }
        ("sqlserver_stdistance", [_, _]) => {
            let (left, right) = pair()?;
            if left.srid == Some(GEOGRAPHY_SRID) {
                Value::Double(eggdb_geo::sphere_distance(&left.geometry, &right.geometry).map_err(geo_error)?)
            } else {
                eggdb_geo::distance(&left.geometry, &right.geometry).map_or(Value::Null, Value::Double)
            }
        }
        ("st_isempty", [_]) => Value::Boolean(spatial(0)?.geometry.is_empty()),
        ("st_transform", [_, target]) => {
            let source = spatial(0)?;
            let geometry = match (source.srid, srid_of(target)?) {
                (from, to) if from == to => source.geometry,
                (Some(4326), Some(3857)) => {
                    eggdb_geo::to_web_mercator(&source.geometry).map_err(argument_error)?
                }
                (Some(3857), Some(4326)) => {
                    eggdb_geo::from_web_mercator(&source.geometry).map_err(argument_error)?
                }
                (Some(from), Some(to)) if eggdb_geo::knows_srid(from) && eggdb_geo::knows_srid(to) => {
                    eggdb_geo::transform(&source.geometry, from, to).map_err(argument_error)?
                }
                (None, _) => {
                    return Err(SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "ST_Transform: Input geometry has unknown (0) SRID",
                    ));
                }
                (Some(from), to) => {
                    return Err(SqlError::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!(
                            "ST_Transform from SRID {from} to {} is not supported yet; EggDB transforms between 4326, 4258, 4269, 3857, 3395 and the WGS 84, ETRS89 and NAD83 UTM zones",
                            to.unwrap_or(0)
                        ),
                    ));
                }
            };
            geometry_value(&Spatial {
                srid: srid_of(target)?,
                geometry,
            })
        }
        ("s2_cellidfrompoint", [_] | [_, _]) => {
            let Geometry::Point(Some(point)) = spatial(0)?.geometry else {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "S2_CELLIDFROMPOINT needs a point",
                ));
            };
            let level = match arguments.get(1) {
                None => 30,
                Some(value) => match value.as_integer() {
                    Some(level @ 0..=30) => level as u32,
                    _ => {
                        return Err(SqlError::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            "S2_CELLIDFROMPOINT: the level must be from 0 to 30",
                        ));
                    }
                },
            };
            Value::BigInt(eggdb_geo::s2_cell_id(point.y, point.x, level) as i64)
        }
        ("s2_cellidtotoken", [id]) => match id.as_integer() {
            Some(id) => Value::Text(eggdb_geo::s2_token(id as u64)),
            None => {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "S2_CELLIDTOTOKEN needs a cell id",
                ));
            }
        },
        ("st_tileenvelope", [zoom, x, y]) => {
            let whole = |value: &Value, what: &str| -> SqlResult<u64> {
                let number = number(value)?;
                if number.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&number) {
                    Ok(number as u64)
                } else {
                    Err(SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("ST_TileEnvelope: {what} must be a non-negative whole number"),
                    ))
                }
            };
            let zoom = u32::try_from(whole(zoom, "the zoom")?).unwrap_or(u32::MAX);
            let geometry = eggdb_geo::tile_envelope(zoom, whole(x, "x")?, whole(y, "y")?)
                .map_err(argument_error)?;
            geometry_value(&Spatial {
                srid: Some(3857),
                geometry,
            })
        }
        ("st_asmvtgeom", [_, _, ..]) if arguments.len() <= 5 => {
            let source = spatial(0)?;
            let bounds = spatial(1)?;
            let Some(Geometry::Polygon(rings)) = eggdb_geo::envelope(&bounds.geometry) else {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "ST_AsMVTGeom: the tile bounds must cover an area",
                ));
            };
            let corners = rings.first().cloned().unwrap_or_default();
            let fold = |pick: fn(&eggdb_geo::Coord) -> f64, low: bool| {
                corners.iter().map(pick).fold(if low { f64::INFINITY } else { f64::NEG_INFINITY }, |held, value| {
                    if low { held.min(value) } else { held.max(value) }
                })
            };
            let whole = |at: usize, default: u32, what: &str| -> SqlResult<u32> {
                match arguments.get(at) {
                    None | Some(Value::Null) => Ok(default),
                    Some(value) => {
                        let found = number(value)?;
                        if found.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&found) {
                            Ok(found as u32)
                        } else {
                            Err(SqlError::new(
                                SqlState::INVALID_PARAMETER_VALUE,
                                format!("ST_AsMVTGeom: {what} must be a non-negative whole number"),
                            ))
                        }
                    }
                }
            };
            let grid = eggdb_geo::TileGrid {
                min_x: fold(|coord| coord.x, true),
                min_y: fold(|coord| coord.y, true),
                max_x: fold(|coord| coord.x, false),
                max_y: fold(|coord| coord.y, false),
                extent: whole(2, 4096, "the extent")?,
                buffer: whole(3, 256, "the buffer")?,
            };
            let clip = !matches!(arguments.get(4), Some(Value::Boolean(false)));
            match eggdb_geo::tile_geometry(&source.geometry, &grid, clip) {
                Some(geometry) => geometry_value(&Spatial { srid: None, geometry }),
                None => Value::Null,
            }
        }
        ("st_geohash", [_, ..]) if arguments.len() <= 2 => {
            let precision = match arguments.get(1) {
                Some(value) => number(value)? as usize,
                None => 20,
            };
            match spatial(0)?.geometry {
                Geometry::Point(Some(coord)) => {
                    Value::Text(eggdb_geo::geohash(coord, precision).map_err(argument_error)?)
                }
                _ => {
                    return Err(SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "ST_GeoHash takes a point here",
                    ));
                }
            }
        }
        ("st_centroid", [_]) => {
            let source = spatial(0)?;
            geometry_value(&Spatial {
                srid: source.srid,
                geometry: Geometry::Point(eggdb_geo::centroid(&source.geometry)),
            })
        }
        ("st_perimeter", [_]) => Value::Double(eggdb_geo::perimeter(&spatial(0)?.geometry)),
        ("st_convexhull", [_]) => {
            let source = spatial(0)?;
            geometry_value(&Spatial {
                srid: source.srid,
                geometry: eggdb_geo::convex_hull(&source.geometry),
            })
        }
        ("st_isvalid", [_]) => {
            Value::Boolean(eggdb_geo::invalidity(&spatial(0)?.geometry).is_none())
        }
        ("st_isvalidreason", [_]) => Value::Text(
            eggdb_geo::invalidity(&spatial(0)?.geometry)
                .unwrap_or_else(|| "Valid Geometry".to_owned()),
        ),
        ("st_numgeometries", [_]) => {
            let source = spatial(0)?;
            Value::Integer(if source.geometry.is_empty() {
                0
            } else {
                eggdb_geo::parts(&source.geometry).len() as i32
            })
        }
        ("st_geometryn", [_, at]) => {
            let source = spatial(0)?;
            let parts = eggdb_geo::parts(&source.geometry);
            match at
                .as_integer()
                .and_then(|at| usize::try_from(at).ok())
                .filter(|at| *at >= 1)
                .and_then(|at| parts.get(at - 1))
            {
                Some(part) => geometry_value(&Spatial {
                    srid: source.srid,
                    geometry: part.clone(),
                }),
                None => Value::Null,
            }
        }
        ("st_startpoint" | "st_endpoint" | "st_pointn", [_, rest @ ..]) => {
            let source = spatial(0)?;
            let Geometry::LineString(points) = &source.geometry else {
                return Ok(Value::Null);
            };
            let picked = match (name, rest.first()) {
                ("st_startpoint", None) => points.first(),
                ("st_endpoint", None) => points.last(),
                ("st_pointn", Some(at)) => match at.as_integer() {
                    Some(at) if at >= 1 => usize::try_from(at - 1).ok().and_then(|at| points.get(at)),
                    Some(at) if at < 0 => usize::try_from(-at)
                        .ok()
                        .and_then(|back| points.len().checked_sub(back))
                        .and_then(|at| points.get(at)),
                    _ => None,
                },
                _ => None,
            };
            match picked {
                Some(point) => geometry_value(&Spatial {
                    srid: source.srid,
                    geometry: Geometry::Point(Some(*point)),
                }),
                None => Value::Null,
            }
        }
        ("st_rotate", [_, angle, origin @ ..]) => {
            let source = spatial(0)?;
            let origin = match origin {
                [] => Coord { x: 0.0, y: 0.0 },
                [x, y] => Coord { x: number(x)?, y: number(y)? },
                [_] => point_at(&spatial(2)?)?,
                _ => return Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "ST_Rotate takes a geometry, an angle and an optional origin")),
            };
            geometry_value(&Spatial { srid: source.srid, geometry: eggdb_geo::rotated(&source.geometry, number(angle)?, origin) })
        }
        ("st_affine", [_, a, b, d, e, x_offset, y_offset]) => {
            let source = spatial(0)?;
            let matrix = [number(a)?, number(b)?, number(d)?, number(e)?, number(x_offset)?, number(y_offset)?];
            geometry_value(&Spatial { srid: source.srid, geometry: eggdb_geo::affine(&source.geometry, matrix) })
        }
        ("st_snaptogrid", [_, size]) => {
            let source = spatial(0)?;
            geometry_value(&Spatial { srid: source.srid, geometry: eggdb_geo::snapped_to_grid(&source.geometry, number(size)?) })
        }
        ("st_removerepeatedpoints", [_]) => {
            let source = spatial(0)?;
            geometry_value(&Spatial { srid: source.srid, geometry: eggdb_geo::without_repeated_points(&source.geometry) })
        }
        ("st_expand", [_, amount]) => {
            let source = spatial(0)?;
            match eggdb_geo::expanded(&source.geometry, number(amount)?) {
                Some(geometry) => geometry_value(&Spatial { srid: source.srid, geometry }),
                None => geometry_value(&source),
            }
        }
        ("st_boundary", [_]) => {
            let source = spatial(0)?;
            geometry_value(&Spatial { srid: source.srid, geometry: eggdb_geo::boundary(&source.geometry) })
        }
        ("st_segmentize", [_, most]) => {
            let source = spatial(0)?;
            let geometry = eggdb_geo::segmentized(&source.geometry, number(most)?).map_err(argument_error)?;
            geometry_value(&Spatial { srid: source.srid, geometry })
        }
        ("st_multi", [_]) => {
            let source = spatial(0)?;
            geometry_value(&Spatial { srid: source.srid, geometry: eggdb_geo::as_multi(&source.geometry) })
        }
        ("st_force2d", [_]) => geometry_value(&spatial(0)?),
        ("st_makepolygon", [_]) => {
            let source = spatial(0)?;
            let Geometry::LineString(points) = source.geometry else {
                return Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "ST_MakePolygon takes a line string"));
            };
            if points.len() < 4 || points.first() != points.last() {
                return Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "shell must have at least 4 points and be closed"));
            }
            geometry_value(&Spatial { srid: source.srid, geometry: Geometry::Polygon(vec![points]) })
        }
        ("st_addpoint" | "st_removepoint" | "st_setpoint", [_, rest @ ..]) => {
            let source = spatial(0)?;
            let Geometry::LineString(mut points) = source.geometry else {
                return Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("{} takes a line string", name.to_uppercase())));
            };
            let out_of_range = || SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "point index is out of range");
            let index_of = |value: &Value, size: usize| -> SqlResult<usize> {
                let index = value.as_integer().ok_or_else(out_of_range)?;
                let index = if index < 0 { i64::try_from(size).unwrap_or(i64::MAX) + index } else { index };
                usize::try_from(index).ok().filter(|index| *index < size).ok_or_else(out_of_range)
            };
            match (name, rest) {
                ("st_addpoint", [_]) => points.push(point_at(&spatial(1)?)?),
                ("st_addpoint", [_, position]) => {
                    let point = point_at(&spatial(1)?)?;
                    match position.as_integer() {
                        Some(-1) => points.push(point),
                        _ => {
                            let at = index_of(position, points.len() + 1)?;
                            points.insert(at, point);
                        }
                    }
                }
                ("st_removepoint", [index]) => {
                    if points.len() < 3 {
                        return Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "can't remove points from a single segment line"));
                    }
                    let at = index_of(index, points.len())?;
                    points.remove(at);
                }
                ("st_setpoint", [index, _]) => {
                    let at = index_of(index, points.len())?;
                    points[at] = point_at(&spatial(2)?)?;
                }
                _ => return Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("function {name} does not take these arguments"))),
            }
            geometry_value(&Spatial { srid: source.srid, geometry: Geometry::LineString(points) })
        }
        ("st_closestpoint" | "st_shortestline" | "st_longestline", [_, _]) => {
            let (left, right) = pair()?;
            let found = if name == "st_longestline" {
                eggdb_geo::longest_line(&left.geometry, &right.geometry)
            } else {
                eggdb_geo::shortest_line(&left.geometry, &right.geometry)
            };
            match found {
                None => Value::Null,
                Some((near, _)) if name == "st_closestpoint" => geometry_value(&Spatial { srid: left.srid, geometry: Geometry::Point(Some(near)) }),
                Some((from, to)) => geometry_value(&Spatial { srid: left.srid, geometry: Geometry::LineString(vec![from, to]) }),
            }
        }
        ("st_maxdistance", [_, _]) => {
            let (left, right) = pair()?;
            match eggdb_geo::longest_line(&left.geometry, &right.geometry) {
                Some((from, to)) => Value::Double((to.x - from.x).hypot(to.y - from.y)),
                None => Value::Null,
            }
        }
        ("st_hausdorffdistance", [_, _]) => {
            let (left, right) = pair()?;
            eggdb_geo::hausdorff_distance(&left.geometry, &right.geometry).map_or(Value::Null, Value::Double)
        }
        ("st_azimuth", [_, _]) => {
            let (from, to) = pair()?;
            match eggdb_geo::azimuth(point_at(&from)?, point_at(&to)?) {
                Some(angle) => Value::Double(angle),
                None => Value::Null,
            }
        }
        ("st_exteriorring", [_]) => {
            let source = spatial(0)?;
            match &source.geometry {
                Geometry::Polygon(rings) => geometry_value(&Spatial {
                    srid: source.srid,
                    geometry: Geometry::LineString(rings.first().cloned().unwrap_or_default()),
                }),
                _ => Value::Null,
            }
        }
        ("st_numinteriorrings", [_]) => match spatial(0)?.geometry {
            Geometry::Polygon(rings) => Value::Integer(rings.len().saturating_sub(1) as i32),
            _ => Value::Null,
        },
        ("st_reverse" | "st_flipcoordinates", [_]) => {
            let source = spatial(0)?;
            let geometry = if name == "st_reverse" {
                eggdb_geo::reversed(&source.geometry)
            } else {
                eggdb_geo::flipped(&source.geometry)
            };
            geometry_value(&Spatial {
                srid: source.srid,
                geometry,
            })
        }
        ("st_translate" | "st_scale", [_, x, y]) => {
            let source = spatial(0)?;
            let (x, y) = (number(x)?, number(y)?);
            let geometry = if name == "st_translate" {
                eggdb_geo::translated(&source.geometry, x, y)
            } else {
                eggdb_geo::scaled(&source.geometry, x, y)
            };
            geometry_value(&Spatial {
                srid: source.srid,
                geometry,
            })
        }
        ("st_simplify", [_, tolerance, ..]) => {
            let source = spatial(0)?;
            let tolerance = number(tolerance)?;
            geometry_value(&Spatial {
                srid: source.srid,
                geometry: eggdb_geo::simplified(&source.geometry, tolerance),
            })
        }
        ("st_linelocatepoint", [_, _]) => {
            let (line, point) = pair()?;
            Value::Double(eggdb_geo::line_locate_point(&line.geometry, &point.geometry).map_err(argument_error)?)
        }
        ("st_lineinterpolatepoint", [_, fraction]) => {
            let source = spatial(0)?;
            let geometry = eggdb_geo::line_interpolate_point(&source.geometry, number(fraction)?).map_err(argument_error)?;
            geometry_value(&Spatial {
                srid: source.srid,
                geometry,
            })
        }
        ("st_linesubstring", [_, from, to]) => {
            let source = spatial(0)?;
            let geometry = eggdb_geo::line_substring(&source.geometry, number(from)?, number(to)?).map_err(argument_error)?;
            geometry_value(&Spatial {
                srid: source.srid,
                geometry,
            })
        }
        ("st_intersection", [_, _]) => {
            let (left, right) = pair()?;
            let geometry = match (&left.geometry, &right.geometry) {
                (Geometry::Point(Some(point)), other) | (other, Geometry::Point(Some(point))) => {
                    if eggdb_geo::intersects(&Geometry::Point(Some(*point)), other) {
                        Geometry::Point(Some(*point))
                    } else {
                        Geometry::Point(None)
                    }
                }
                (
                    Geometry::LineString(_) | Geometry::MultiLineString(_),
                    Geometry::LineString(_) | Geometry::MultiLineString(_),
                ) => match eggdb_geo::line_crossings(&left.geometry, &right.geometry).as_slice() {
                    [] => Geometry::Point(None),
                    [only] => Geometry::Point(Some(*only)),
                    many => Geometry::MultiPoint(many.to_vec()),
                },
                _ => {
                    return Err(SqlError::not_supported(
                        "ST_Intersection of areas (polygon overlays); points and lines are supported",
                    ));
                }
            };
            geometry_value(&Spatial {
                srid: left.srid,
                geometry,
            })
        }
        ("st_makeline", [_, _]) => {
            let (left, right) = pair()?;
            let mut points = left.geometry.coords();
            points.extend(right.geometry.coords());
            geometry_value(&Spatial {
                srid: left.srid,
                geometry: Geometry::LineString(points),
            })
        }
        ("st_makeenvelope", [xmin, ymin, xmax, ymax, rest @ ..]) => {
            let (xmin, ymin, xmax, ymax) = (number(xmin)?, number(ymin)?, number(xmax)?, number(ymax)?);
            let srid = match rest.first() {
                Some(srid) => srid_of(srid)?,
                None => None,
            };
            let corner = |x: f64, y: f64| Coord { x, y };
            geometry_value(&Spatial {
                srid,
                geometry: Geometry::Polygon(vec![vec![
                    corner(xmin, ymin),
                    corner(xmin, ymax),
                    corner(xmax, ymax),
                    corner(xmax, ymin),
                    corner(xmin, ymin),
                ]]),
            })
        }
        ("st_xmin" | "st_xmax" | "st_ymin" | "st_ymax", [_]) => {
            let points = spatial(0)?.geometry.coords();
            let pick = |values: &mut dyn Iterator<Item = f64>, largest: bool| {
                values.fold(None::<f64>, |found, value| {
                    Some(match found {
                        None => value,
                        Some(found) if largest => found.max(value),
                        Some(found) => found.min(value),
                    })
                })
            };
            let found = match name {
                "st_xmin" => pick(&mut points.iter().map(|point| point.x), false),
                "st_xmax" => pick(&mut points.iter().map(|point| point.x), true),
                "st_ymin" => pick(&mut points.iter().map(|point| point.y), false),
                _ => pick(&mut points.iter().map(|point| point.y), true),
            };
            found.map_or(Value::Null, Value::Double)
        }
        _ => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!(
                    "function {name} does not take {} arguments",
                    arguments.len()
                ),
            ));
        }
    })
}
