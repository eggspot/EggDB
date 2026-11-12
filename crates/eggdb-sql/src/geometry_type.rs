//! The stored `geometry` type, PostGIS-compatible (D845): a geometry kept as its extended
//! well-known binary (EWKB, with its SRID), written out as upper-case hex EWKB as PostGIS shows
//! it, read from hex EWKB, WKT or EWKT text, and sent in binary as the EWKB bytes.

use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{DefinedValue, Value};
use std::cmp::Ordering;

pub(crate) const GEOMETRY_OID: u32 = 9005;
pub(crate) const MOST_GEOMETRY_BYTES: usize = 1 << 28;

pub(crate) fn value(ewkb: Vec<u8>) -> Value {
    Value::Defined(Box::new(DefinedValue::Geometry(ewkb)))
}

pub(crate) fn geometry_of(value: &Value) -> Option<&[u8]> {
    match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Geometry(bytes) => Some(bytes),
            _ => None,
        },
        _ => None,
    }
}

fn invalid(text: &str, why: impl std::fmt::Display) -> SqlError {
    let shown: String = text.chars().take(64).collect();
    SqlError::new(SqlState::INTERNAL_ERROR, format!("parse error - invalid geometry \"{shown}\": {why}"))
}

fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
        .collect()
}

/// The canonical EWKB of `bytes` (WKB or EWKB), so equal geometries compare equal.
pub(crate) fn from_binary(bytes: &[u8]) -> SqlResult<Vec<u8>> {
    if bytes.len() > MOST_GEOMETRY_BYTES {
        return Err(SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "a geometry is larger than 256 MiB"));
    }
    let spatial = eggdb_geo::from_wkb(bytes).map_err(|error| {
        SqlError::new(SqlState::INVALID_BINARY_REPRESENTATION, format!("invalid geometry: {error}"))
    })?;
    Ok(eggdb_geo::to_wkb(&spatial, true))
}

pub(crate) fn parse_text(text: &str) -> SqlResult<Vec<u8>> {
    let trimmed = text.trim();
    if let Some(bytes) = hex_bytes(trimmed) {
        return from_binary(&bytes).map_err(|error| invalid(trimmed, error.message()));
    }
    let spatial = eggdb_geo::from_wkt(trimmed).map_err(|error| invalid(trimmed, error))?;
    Ok(eggdb_geo::to_wkb(&spatial, true))
}

pub(crate) fn to_text(ewkb: &[u8]) -> String {
    ewkb.iter().map(|byte| format!("{byte:02X}")).collect()
}

pub(crate) fn compare(left: &[u8], right: &[u8]) -> Ordering {
    left.cmp(right)
}

const SUBTYPES: [(&str, &str); 7] = [
    ("point", "ST_Point"),
    ("linestring", "ST_LineString"),
    ("polygon", "ST_Polygon"),
    ("multipoint", "ST_MultiPoint"),
    ("multilinestring", "ST_MultiLineString"),
    ("multipolygon", "ST_MultiPolygon"),
    ("geometrycollection", "ST_GeometryCollection"),
];

/// The check a `geometry(subtype[, srid])` column carries: its values are of that subtype
/// (`geometry` takes any) and that SRID.
pub(crate) fn typmod_check(
    column: &crate::ast::Name,
    subtype: &str,
    srid: Option<i64>,
) -> SqlResult<Option<crate::ast::ConstraintSpec>> {
    use crate::ast::{BinaryOperator, ConstraintKind, ConstraintSpec, Expr, ExprKind, Literal};
    let lowered = subtype.to_ascii_lowercase();
    let wanted = match lowered.as_str() {
        "geometry" => None,
        other => Some(
            SUBTYPES
                .iter()
                .find(|(name, _)| *name == other)
                .map(|(_, shown)| *shown)
                .ok_or_else(|| {
                    SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("Invalid geometry type modifier: {subtype} (Z and M geometries are not supported yet)"),
                    )
                    .at(column.position)
                })?,
        ),
    };
    let position = column.position;
    let at = |kind: ExprKind| Expr { kind, position };
    let call = |name: &str| {
        at(ExprKind::Function {
            name: name.to_owned(),
            arguments: vec![at(ExprKind::Column {
                qualifier: None,
                name: column.text.clone(),
            })],
        })
    };
    let quoted = column.text.replace('"', "\"\"");
    let mut parts = Vec::new();
    let mut texts = Vec::new();
    if let Some(shown) = wanted {
        parts.push(at(ExprKind::Binary(
            BinaryOperator::Equal,
            Box::new(call("st_geometrytype")),
            Box::new(at(ExprKind::Literal(Literal::String(shown.to_owned())))),
        )));
        texts.push(format!("ST_GeometryType(\"{quoted}\") = '{shown}'"));
    }
    if let Some(srid) = srid.filter(|srid| *srid != 0) {
        parts.push(at(ExprKind::Binary(
            BinaryOperator::Equal,
            Box::new(call("st_srid")),
            Box::new(at(ExprKind::Literal(Literal::Integer(srid)))),
        )));
        texts.push(format!("ST_SRID(\"{quoted}\") = {srid}"));
    }
    let expr = match parts.len() {
        0 => return Ok(None),
        1 => parts.remove(0),
        _ => at(ExprKind::And(parts)),
    };
    Ok(Some(ConstraintSpec {
        name: None,
        kind: ConstraintKind::Check {
            expr,
            text: texts.join(" AND "),
        },
        position,
        column: Some(column.clone()),
        using_index: None,
        not_valid: false,
    }))
}

pub(crate) fn held_bytes(ewkb: &[u8]) -> usize {
    ewkb.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wkt_ewkt_and_hex_ewkb_read_to_one_canonical_form() {
        let from_wkt = parse_text("POINT(1 2)").unwrap();
        let shown = to_text(&from_wkt);
        assert!(shown.chars().all(|character| character.is_ascii_hexdigit() && !character.is_ascii_lowercase()));
        assert_eq!(parse_text(&shown).unwrap(), from_wkt);
        assert_eq!(parse_text(&shown.to_lowercase()).unwrap(), from_wkt);
        let with_srid = parse_text("SRID=4326;POINT(1 2)").unwrap();
        assert_ne!(with_srid, from_wkt);
        assert_eq!(from_binary(&with_srid).unwrap(), with_srid);
        assert!(parse_text("POINT(1").is_err());
        assert!(parse_text("ABC").is_err());
        assert!(from_binary(&[1, 2, 3]).is_err());
    }

    fn run(session: &mut crate::Session<'_>, sql: &str) -> Result<String, crate::SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(crate::tests::render(&result)))?;
        Ok(outputs.pop().unwrap_or_default())
    }

    #[test]
    fn a_geometry_column_stores_and_feeds_the_spatial_functions() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        for sql in [
            "CREATE TABLE places (id integer PRIMARY KEY, shape geometry)",
            "INSERT INTO places VALUES (1, 'POINT(3 4)'), (2, 'SRID=4326;LINESTRING(0 0, 1 1)')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT ST_X(shape) AS x FROM places WHERE id = 1").unwrap(), "x\n3\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT pg_typeof(shape) AS kind FROM places WHERE id = 2").unwrap(), "kind\ngeometry\nSELECT 1");
        assert!(run(&mut session, "INSERT INTO places VALUES (3, 'not a shape')").is_err());
        run(&mut session, "CREATE TABLE stops (id integer PRIMARY KEY, at geometry(Point, 4326))").unwrap();
        run(&mut session, "INSERT INTO stops VALUES (1, 'SRID=4326;POINT(1 2)')").unwrap();
        assert!(run(&mut session, "INSERT INTO stops VALUES (2, 'SRID=4326;LINESTRING(0 0, 1 1)')").is_err());
        assert!(run(&mut session, "INSERT INTO stops VALUES (3, 'POINT(1 2)')").is_err());
        assert!(run(&mut session, "CREATE TABLE bad (shape geometry(PointZ, 4326))").is_err());
    }
}
