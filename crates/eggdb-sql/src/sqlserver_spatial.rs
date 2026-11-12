//! SQL Server's `geometry` and `geography` types (D1004): their static methods
//! (`geometry::STGeomFromText(...)`, `geography::Point(...)`) and instance methods and
//! properties (`g.STDistance(h)`, `g.STX`) as calls of the PostGIS-compatible spatial functions,
//! on the one stored `geometry` type (D845).

use crate::ast::{Expr, ExprKind, Literal};
use crate::error::{SqlError, SqlResult, SqlState};

const GEOGRAPHY_SRID: i64 = 4326;

const METHODS: [(&str, &str); 24] = [
    ("stastext", "st_astext"),
    ("tostring", "st_astext"),
    ("astextzm", "st_astext"),
    ("stasbinary", "st_asbinary"),
    ("starea", "st_area"),
    ("stlength", "st_length"),
    ("stenvelope", "st_envelope"),
    ("stcentroid", "st_centroid"),
    ("stnumpoints", "st_npoints"),
    ("stisvalid", "st_isvalid"),
    ("stisempty", "st_isempty"),
    ("stnumgeometries", "st_numgeometries"),
    ("ststartpoint", "st_startpoint"),
    ("stendpoint", "st_endpoint"),
    ("stexteriorring", "st_exteriorring"),
    ("stnuminteriorring", "st_numinteriorrings"),
    ("stconvexhull", "st_convexhull"),
    ("stgeometrytype", "sqlserver_stgeometrytype"),
    ("stdistance", "sqlserver_stdistance"),
    ("stintersects", "st_intersects"),
    ("stcontains", "st_contains"),
    ("stwithin", "st_within"),
    ("stequals", "st_equals"),
    ("stdisjoint", "st_disjoint"),
];

const MORE_METHODS: [(&str, &str); 4] = [
    ("stintersection", "st_intersection"),
    ("stpointn", "st_pointn"),
    ("stgeometryn", "st_geometryn"),
    ("reduce", "st_simplify"),
];

const PROPERTIES: [(&str, &str); 5] = [("stx", "st_x"), ("sty", "st_y"), ("lat", "st_y"), ("long", "st_x"), ("stsrid", "st_srid")];

fn call(name: &str, arguments: Vec<Expr>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Function {
            name: name.to_owned(),
            arguments,
        },
        position,
    }
}

fn integer(value: i64, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Literal(Literal::Integer(value)),
        position,
    }
}

/// The spatial function an instance method `name` calls, its target first.
pub(crate) fn method_function(name: &str) -> Option<&'static str> {
    let lowered = name.to_ascii_lowercase();
    METHODS
        .iter()
        .chain(MORE_METHODS.iter())
        .find(|(method, _)| *method == lowered)
        .map(|(_, function)| *function)
}

/// Whether `name` is one of the properties read without parentheses (`STX`, `STY`, `STSrid`,
/// and `Lat` and `Long` of a geography).
pub(crate) fn property_function(name: &str) -> Option<&'static str> {
    let lowered = name.to_ascii_lowercase();
    PROPERTIES.iter().find(|(property, _)| *property == lowered).map(|(_, function)| *function)
}

/// Whether `type_name` is `geometry` or `geography`.
pub(crate) fn is_spatial_type(type_name: &str) -> bool {
    type_name.eq_ignore_ascii_case("geometry") || type_name.eq_ignore_ascii_case("geography")
}

/// The expression a static method `type_name::method(arguments)` stands for.
pub(crate) fn static_call(type_name: &str, method: &str, mut arguments: Vec<Expr>, position: usize) -> SqlResult<Expr> {
    let geography = type_name.eq_ignore_ascii_case("geography");
    let default_srid = if geography { GEOGRAPHY_SRID } else { 0 };
    let lowered = method.to_ascii_lowercase();
    let from_text = lowered.starts_with("st") && lowered.ends_with("fromtext");
    let from_binary = lowered.starts_with("st") && lowered.ends_with("fromwkb");
    let wrong = || {
        SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("{type_name}::{method} takes other arguments"),
        )
        .with_sqlserver_code(6506)
        .at(position)
    };
    if from_text || from_binary || lowered == "parse" {
        if lowered == "parse" {
            arguments.push(integer(default_srid, position));
        }
        if arguments.len() != 2 {
            return Err(wrong());
        }
        let function = if from_binary { "st_geomfromwkb" } else { "st_geomfromtext" };
        return Ok(call(function, arguments, position));
    }
    if lowered == "point" {
        let [first, second, srid]: [Expr; 3] = arguments.try_into().map_err(|_| wrong())?;
        let (x, y) = if geography { (second, first) } else { (first, second) };
        return Ok(call("st_setsrid", vec![call("st_makepoint", vec![x, y], position), srid], position));
    }
    Err(SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("{type_name}::{method} is not supported yet"),
    )
    .at(position))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_and_static_calls_map_to_spatial_functions() {
        assert_eq!(method_function("STDistance"), Some("sqlserver_stdistance"));
        assert_eq!(property_function("Lat"), Some("st_y"));
        let point = static_call("geography", "Point", vec![integer(47, 0), integer(-122, 0), integer(4326, 0)], 0).unwrap();
        let ExprKind::Function { name, arguments } = point.kind else {
            panic!("a function");
        };
        assert_eq!(name, "st_setsrid");
        let ExprKind::Function { arguments: coordinates, .. } = &arguments[0].kind else {
            panic!("st_makepoint");
        };
        assert_eq!(coordinates[0].kind, ExprKind::Literal(Literal::Integer(-122)));
        assert!(static_call("geometry", "STGeomFromText", vec![integer(1, 0)], 0).is_err());
    }
}
