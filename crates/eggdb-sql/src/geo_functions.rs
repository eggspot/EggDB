use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, Node, parse_document};
use crate::types::{SqlType, Value};
use eggdb_geo::{Coord, Geometry, Spatial};

/// The spatial functions EggDB answers, PostGIS's names (also MySQL's `ST_` functions).
pub(crate) const FUNCTIONS: [&str; 60] = [
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
    "st_npoints",
    "st_geometrytype",
    "st_isempty",
    "st_equals",
    "st_transform",
    "st_tileenvelope",
    "st_geohash",
    "st_asmvtgeom",
];

/// The result type of a spatial function.
pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    Some(match name {
        "st_asbinary" | "st_asewkb" => SqlType::Bytea,
        "st_x" | "st_y" | "st_distance" | "st_area" | "st_length" | "st_distancesphere"
        | "st_perimeter" | "st_xmin" | "st_xmax" | "st_ymin" | "st_ymax" => SqlType::Double,
        "st_srid" | "st_npoints" | "st_numgeometries" | "st_numinteriorrings" => {
            SqlType::Integer
        }
        "st_dwithin" | "st_contains" | "st_within" | "st_intersects" | "st_disjoint"
        | "st_isempty" | "st_equals" | "st_isvalid" => SqlType::Boolean,
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
        ("st_npoints", [_]) => {
            Value::Integer(i32::try_from(spatial(0)?.geometry.coords().len()).unwrap_or(i32::MAX))
        }
        ("st_geometrytype", [_]) => Value::Text(spatial(0)?.geometry.type_name().to_owned()),
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
                            "ST_Transform from SRID {from} to {} is not supported yet; EggDB transforms between 4326 and 3857",
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
