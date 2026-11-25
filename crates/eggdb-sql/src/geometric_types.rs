//! PostgreSQL's geometric types `point`, `line`, `lseg`, `box`, `path`, `polygon` and `circle`
//! (D1331): read and printed in PostgreSQL's forms, kept as double-precision coordinates, with
//! the operators and functions that measure, move and compare them.

use crate::ast::BinaryOperator;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{DefinedType, DefinedValue, SqlType, Value};
use std::cmp::Ordering;

pub(crate) const POINT_OID: u32 = 600;
pub(crate) const LSEG_OID: u32 = 601;
pub(crate) const PATH_OID: u32 = 602;
pub(crate) const BOX_OID: u32 = 603;
pub(crate) const POLYGON_OID: u32 = 604;
pub(crate) const LINE_OID: u32 = 628;
pub(crate) const CIRCLE_OID: u32 = 718;

const EPSILON: f64 = 1.0e-6;

/// PostgreSQL's operator `~=`, with the function it calls.
pub(crate) const OPERATORS: [(&str, &str); 1] = [("~=", SAME_FUNCTION)];
const SAME_FUNCTION: &str = "eggdb_geometric_same";

/// Which geometric type a value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeometricKind {
    /// `point`: `(x,y)`.
    Point,
    /// `line`: the infinite line `Ax + By + C = 0`.
    Line,
    /// `lseg`: a line segment.
    Lseg,
    /// `box`: a rectangle kept as its upper right and lower left corners.
    Box,
    /// `path`: an open or closed list of points.
    Path,
    /// `polygon`: a closed list of points.
    Polygon,
    /// `circle`: a center and a radius.
    Circle,
}

/// A geometric value: its type, whether a path is closed, and its numbers (the coordinates
/// of its points, a line's `A`, `B` and `C`, a circle's center and radius).
#[derive(Debug, Clone, PartialEq)]
pub struct GeometricValue {
    /// Its type.
    pub kind: GeometricKind,
    /// Whether a path is closed; always true for polygons.
    pub closed: bool,
    /// Its numbers.
    pub numbers: Vec<f64>,
}

impl GeometricKind {
    pub(crate) const ALL: [Self; 7] =
        [Self::Point, Self::Line, Self::Lseg, Self::Box, Self::Path, Self::Polygon, Self::Circle];

    pub(crate) fn oid(self) -> u32 {
        match self {
            Self::Point => POINT_OID,
            Self::Line => LINE_OID,
            Self::Lseg => LSEG_OID,
            Self::Box => BOX_OID,
            Self::Path => PATH_OID,
            Self::Polygon => POLYGON_OID,
            Self::Circle => CIRCLE_OID,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Point => "point",
            Self::Line => "line",
            Self::Lseg => "lseg",
            Self::Box => "box",
            Self::Path => "path",
            Self::Polygon => "polygon",
            Self::Circle => "circle",
        }
    }

    pub(crate) fn of_oid(oid: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.oid() == oid)
    }

    pub(crate) fn index(self) -> u8 {
        Self::ALL.iter().position(|kind| *kind == self).unwrap_or(0) as u8
    }

    pub(crate) fn defined_type(self) -> DefinedType {
        DefinedType { kind: crate::types::DefinedKind::Base, oid: self.oid() }
    }
}

pub(crate) fn kind_of(sql_type: SqlType) -> Option<GeometricKind> {
    match sql_type {
        SqlType::Defined(defined) if defined.kind == crate::types::DefinedKind::Base => {
            GeometricKind::of_oid(defined.oid)
        }
        _ => None,
    }
}

fn invalid(kind: GeometricKind, text: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("invalid input syntax for type {}: \"{text}\"", kind.name()),
    )
}

fn numbers_in(text: &str, kind: GeometricKind) -> SqlResult<Vec<f64>> {
    let mut numbers = Vec::new();
    for piece in text.split(|character: char| "()[]{}<>,".contains(character) || character.is_whitespace()) {
        if piece.is_empty() {
            continue;
        }
        let number: f64 = piece.parse().map_err(|_| invalid(kind, text))?;
        numbers.push(number);
    }
    Ok(numbers)
}

fn line_through(x1: f64, y1: f64, x2: f64, y2: f64) -> Vec<f64> {
    if (x1 - x2).abs() < EPSILON {
        vec![-1.0, 0.0, x1]
    } else if (y1 - y2).abs() < EPSILON {
        vec![0.0, -1.0, y1]
    } else {
        let slope = (y2 - y1) / (x2 - x1);
        vec![slope, -1.0, y1 - slope * x1]
    }
}

fn boxed(x1: f64, y1: f64, x2: f64, y2: f64) -> Vec<f64> {
    vec![x1.max(x2), y1.max(y2), x1.min(x2), y1.min(y2)]
}

/// The value of type `kind` written as `text`, in any of the forms PostgreSQL reads.
pub(crate) fn parse(text: &str, kind: GeometricKind) -> SqlResult<GeometricValue> {
    let numbers = numbers_in(text, kind)?;
    let trimmed = text.trim_start();
    let bad = || invalid(kind, text);
    let numbers = match kind {
        GeometricKind::Point if numbers.len() == 2 => numbers,
        GeometricKind::Lseg if numbers.len() == 4 => numbers,
        GeometricKind::Box if numbers.len() == 4 => boxed(numbers[0], numbers[1], numbers[2], numbers[3]),
        GeometricKind::Circle if numbers.len() == 3 => {
            if numbers[2] < 0.0 {
                return Err(bad());
            }
            numbers
        }
        GeometricKind::Line if trimmed.starts_with('{') && numbers.len() == 3 => {
            if numbers[0] == 0.0 && numbers[1] == 0.0 {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "invalid line specification: A and B cannot both be zero",
                ));
            }
            numbers
        }
        GeometricKind::Line if numbers.len() == 4 => {
            if numbers[0] == numbers[2] && numbers[1] == numbers[3] {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "invalid line specification: must be two distinct points",
                ));
            }
            line_through(numbers[0], numbers[1], numbers[2], numbers[3])
        }
        GeometricKind::Path | GeometricKind::Polygon if numbers.len() >= 2 && numbers.len() % 2 == 0 => numbers,
        _ => return Err(bad()),
    };
    let closed = match kind {
        GeometricKind::Path => !trimmed.starts_with('['),
        _ => true,
    };
    Ok(GeometricValue { kind, closed, numbers })
}

fn number_text(number: f64) -> String {
    Value::Double(number).to_text().unwrap_or_default()
}

fn points_text(numbers: &[f64]) -> String {
    numbers
        .chunks(2)
        .map(|pair| format!("({},{})", number_text(pair[0]), number_text(pair.get(1).copied().unwrap_or(0.0))))
        .collect::<Vec<_>>()
        .join(",")
}

/// The value as PostgreSQL prints it.
pub(crate) fn to_text(value: &GeometricValue) -> String {
    let numbers = &value.numbers;
    match value.kind {
        GeometricKind::Point => points_text(numbers),
        GeometricKind::Line => format!(
            "{{{}}}",
            numbers.iter().map(|number| number_text(*number)).collect::<Vec<_>>().join(",")
        ),
        GeometricKind::Lseg => format!("[{}]", points_text(numbers)),
        GeometricKind::Box => points_text(numbers),
        GeometricKind::Path if !value.closed => format!("[{}]", points_text(numbers)),
        GeometricKind::Path | GeometricKind::Polygon => format!("({})", points_text(numbers)),
        GeometricKind::Circle => format!(
            "<({},{}),{}>",
            number_text(numbers[0]),
            number_text(numbers[1]),
            number_text(numbers[2])
        ),
    }
}

pub(crate) fn value(geometric: GeometricValue) -> Value {
    Value::Defined(Box::new(DefinedValue::Geometric(geometric)))
}

pub(crate) fn geometric_of(value: &Value) -> Option<&GeometricValue> {
    match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Geometric(geometric) => Some(geometric),
            _ => None,
        },
        _ => None,
    }
}

fn point(x: f64, y: f64) -> GeometricValue {
    GeometricValue { kind: GeometricKind::Point, closed: true, numbers: vec![x, y] }
}

fn points(value: &GeometricValue) -> Vec<(f64, f64)> {
    value.numbers.chunks(2).map(|pair| (pair[0], pair.get(1).copied().unwrap_or(0.0))).collect()
}

fn shoelace(corners: &[(f64, f64)]) -> f64 {
    let mut twice = 0.0;
    for (index, (x1, y1)) in corners.iter().enumerate() {
        let (x2, y2) = corners[(index + 1) % corners.len()];
        twice += x1 * y2 - x2 * y1;
    }
    (twice / 2.0).abs()
}

fn area(value: &GeometricValue) -> Option<f64> {
    let numbers = &value.numbers;
    match value.kind {
        GeometricKind::Box => Some((numbers[0] - numbers[2]) * (numbers[1] - numbers[3])),
        GeometricKind::Circle => Some(std::f64::consts::PI * numbers[2] * numbers[2]),
        GeometricKind::Polygon => Some(shoelace(&points(value))),
        GeometricKind::Path if value.closed => Some(shoelace(&points(value))),
        _ => None,
    }
}

fn distance((x1, y1): (f64, f64), (x2, y2): (f64, f64)) -> f64 {
    (x1 - x2).hypot(y1 - y2)
}

fn length(value: &GeometricValue) -> Option<f64> {
    let corners = points(value);
    match value.kind {
        GeometricKind::Lseg => Some(distance(corners[0], corners[1])),
        GeometricKind::Path => {
            let mut total: f64 = corners.windows(2).map(|pair| distance(pair[0], pair[1])).sum();
            if value.closed && corners.len() > 1 {
                total += distance(corners[corners.len() - 1], corners[0]);
            }
            Some(total)
        }
        _ => None,
    }
}

fn center(value: &GeometricValue) -> Option<(f64, f64)> {
    let numbers = &value.numbers;
    match value.kind {
        GeometricKind::Point | GeometricKind::Circle => Some((numbers[0], numbers[1])),
        GeometricKind::Box => Some(((numbers[0] + numbers[2]) / 2.0, (numbers[1] + numbers[3]) / 2.0)),
        GeometricKind::Lseg => Some(((numbers[0] + numbers[2]) / 2.0, (numbers[1] + numbers[3]) / 2.0)),
        GeometricKind::Polygon | GeometricKind::Path => {
            let corners = points(value);
            let count = corners.len() as f64;
            Some((
                corners.iter().map(|(x, _)| x).sum::<f64>() / count,
                corners.iter().map(|(_, y)| y).sum::<f64>() / count,
            ))
        }
        GeometricKind::Line => None,
    }
}

fn bounding(value: &GeometricValue) -> (f64, f64, f64, f64) {
    let numbers = &value.numbers;
    match value.kind {
        GeometricKind::Box => (numbers[0], numbers[1], numbers[2], numbers[3]),
        GeometricKind::Circle => {
            (numbers[0] + numbers[2], numbers[1] + numbers[2], numbers[0] - numbers[2], numbers[1] - numbers[2])
        }
        _ => {
            let corners = points(value);
            let xs = corners.iter().map(|(x, _)| *x);
            let ys = corners.iter().map(|(_, y)| *y);
            (
                xs.clone().fold(f64::MIN, f64::max),
                ys.clone().fold(f64::MIN, f64::max),
                xs.fold(f64::MAX, f64::min),
                ys.fold(f64::MAX, f64::min),
            )
        }
    }
}

fn to_segment((x, y): (f64, f64), (x1, y1): (f64, f64), (x2, y2): (f64, f64)) -> f64 {
    let (dx, dy) = (x2 - x1, y2 - y1);
    let squared = dx * dx + dy * dy;
    if squared == 0.0 {
        return distance((x, y), (x1, y1));
    }
    let along = (((x - x1) * dx + (y - y1) * dy) / squared).clamp(0.0, 1.0);
    distance((x, y), (x1 + along * dx, y1 + along * dy))
}

fn inside_polygon((x, y): (f64, f64), corners: &[(f64, f64)]) -> bool {
    let mut inside = false;
    for (index, &(x1, y1)) in corners.iter().enumerate() {
        let (x2, y2) = corners[(index + 1) % corners.len()];
        if to_segment((x, y), (x1, y1), (x2, y2)) < EPSILON {
            return true;
        }
        if (y1 > y) != (y2 > y) && x < (x2 - x1) * (y - y1) / (y2 - y1) + x1 {
            inside = !inside;
        }
    }
    inside
}

fn edges(corners: &[(f64, f64)], closed: bool) -> Vec<((f64, f64), (f64, f64))> {
    let mut found: Vec<_> = corners.windows(2).map(|pair| (pair[0], pair[1])).collect();
    if closed && corners.len() > 2 {
        found.push((corners[corners.len() - 1], corners[0]));
    }
    found
}

fn point_distance(at: (f64, f64), other: &GeometricValue) -> Option<f64> {
    let numbers = &other.numbers;
    Some(match other.kind {
        GeometricKind::Point => distance(at, (numbers[0], numbers[1])),
        GeometricKind::Circle => (distance(at, (numbers[0], numbers[1])) - numbers[2]).max(0.0),
        GeometricKind::Box => {
            let dx = (numbers[2] - at.0).max(0.0).max(at.0 - numbers[0]);
            let dy = (numbers[3] - at.1).max(0.0).max(at.1 - numbers[1]);
            dx.hypot(dy)
        }
        GeometricKind::Lseg => to_segment(at, (numbers[0], numbers[1]), (numbers[2], numbers[3])),
        GeometricKind::Line => {
            (numbers[0] * at.0 + numbers[1] * at.1 + numbers[2]).abs() / numbers[0].hypot(numbers[1])
        }
        GeometricKind::Polygon | GeometricKind::Path => {
            let corners = points(other);
            if other.kind == GeometricKind::Polygon && inside_polygon(at, &corners) {
                return Some(0.0);
            }
            edges(&corners, other.closed)
                .into_iter()
                .map(|(from, to)| to_segment(at, from, to))
                .fold(f64::MAX, f64::min)
        }
    })
}

/// `left <-> right`: the distance between two geometric values.
pub(crate) fn distance_between(left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    let (a, b) = (geometric_of(left)?, geometric_of(right)?);
    let found = match (a.kind, b.kind) {
        (GeometricKind::Point, _) => point_distance((a.numbers[0], a.numbers[1]), b),
        (_, GeometricKind::Point) => point_distance((b.numbers[0], b.numbers[1]), a),
        (GeometricKind::Circle, GeometricKind::Circle) => Some(
            (distance((a.numbers[0], a.numbers[1]), (b.numbers[0], b.numbers[1])) - a.numbers[2] - b.numbers[2])
                .max(0.0),
        ),
        (GeometricKind::Box, GeometricKind::Box) => center(a).zip(center(b)).map(|(p, q)| distance(p, q)),
        (GeometricKind::Polygon, GeometricKind::Polygon) => {
            if overlaps(a, b) {
                Some(0.0)
            } else {
                let from_a = points(a).into_iter().filter_map(|at| point_distance(at, b)).fold(f64::MAX, f64::min);
                let from_b = points(b).into_iter().filter_map(|at| point_distance(at, a)).fold(f64::MAX, f64::min);
                Some(from_a.min(from_b))
            }
        }
        _ => None,
    };
    Some(found.map(Value::Double).ok_or_else(|| no_operator("<->", a, b)))
}

fn no_operator(symbol: &str, a: &GeometricValue, b: &GeometricValue) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("operator does not exist: {} {symbol} {}", a.kind.name(), b.kind.name()),
    )
}

fn contains(outer: &GeometricValue, inner: &GeometricValue) -> Option<bool> {
    let o = &outer.numbers;
    let i = &inner.numbers;
    Some(match (outer.kind, inner.kind) {
        (GeometricKind::Box, GeometricKind::Point) => {
            i[0] <= o[0] + EPSILON && i[0] >= o[2] - EPSILON && i[1] <= o[1] + EPSILON && i[1] >= o[3] - EPSILON
        }
        (GeometricKind::Box, GeometricKind::Box) => {
            i[0] <= o[0] + EPSILON && i[2] >= o[2] - EPSILON && i[1] <= o[1] + EPSILON && i[3] >= o[3] - EPSILON
        }
        (GeometricKind::Circle, GeometricKind::Point) => distance((o[0], o[1]), (i[0], i[1])) <= o[2] + EPSILON,
        (GeometricKind::Circle, GeometricKind::Circle) => {
            distance((o[0], o[1]), (i[0], i[1])) + i[2] <= o[2] + EPSILON
        }
        (GeometricKind::Polygon, GeometricKind::Point) => inside_polygon((i[0], i[1]), &points(outer)),
        (GeometricKind::Polygon, GeometricKind::Polygon | GeometricKind::Box) => {
            let corners = points(outer);
            let inner_points = if inner.kind == GeometricKind::Box {
                vec![(i[0], i[1]), (i[2], i[1]), (i[2], i[3]), (i[0], i[3])]
            } else {
                points(inner)
            };
            inner_points.into_iter().all(|at| inside_polygon(at, &corners))
        }
        (GeometricKind::Path, GeometricKind::Point) => points(outer).into_iter().any(|at| distance(at, (i[0], i[1])) < EPSILON),
        _ => return None,
    })
}

fn segments_cross((p1, p2): ((f64, f64), (f64, f64)), (q1, q2): ((f64, f64), (f64, f64))) -> bool {
    let side = |a: (f64, f64), b: (f64, f64), c: (f64, f64)| (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0);
    let (d1, d2) = (side(q1, q2, p1), side(q1, q2, p2));
    let (d3, d4) = (side(p1, p2, q1), side(p1, p2, q2));
    ((d1 > 0.0) != (d2 > 0.0) && (d3 > 0.0) != (d4 > 0.0))
        || to_segment(p1, q1, q2) < EPSILON
        || to_segment(p2, q1, q2) < EPSILON
        || to_segment(q1, p1, p2) < EPSILON
        || to_segment(q2, p1, p2) < EPSILON
}

fn overlaps(a: &GeometricValue, b: &GeometricValue) -> bool {
    let (ahx, ahy, alx, aly) = bounding(a);
    let (bhx, bhy, blx, bly) = bounding(b);
    let boxes_meet = ahx >= blx - EPSILON && bhx >= alx - EPSILON && ahy >= bly - EPSILON && bhy >= aly - EPSILON;
    match (a.kind, b.kind) {
        (GeometricKind::Box, GeometricKind::Box) => boxes_meet,
        (GeometricKind::Circle, GeometricKind::Circle) => {
            distance((a.numbers[0], a.numbers[1]), (b.numbers[0], b.numbers[1])) <= a.numbers[2] + b.numbers[2] + EPSILON
        }
        _ if !boxes_meet => false,
        _ => {
            let (pa, pb) = (points(a), points(b));
            pa.iter().any(|at| inside_polygon(*at, &pb))
                || pb.iter().any(|at| inside_polygon(*at, &pa))
                || edges(&pa, true).into_iter().any(|edge| edges(&pb, true).into_iter().any(|other| segments_cross(edge, other)))
        }
    }
}

fn same(a: &GeometricValue, b: &GeometricValue) -> bool {
    a.kind == b.kind
        && a.closed == b.closed
        && a.numbers.len() == b.numbers.len()
        && a.numbers.iter().zip(&b.numbers).all(|(x, y)| (x - y).abs() <= EPSILON)
}

/// `@>` (`contains` true) and `<@` between geometric values.
pub(crate) fn containment(contains_operator: bool, left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    let (a, b) = (geometric_of(left)?, geometric_of(right)?);
    let (outer, inner) = if contains_operator { (a, b) } else { (b, a) };
    Some(
        contains(outer, inner)
            .map(Value::Boolean)
            .ok_or_else(|| no_operator(if contains_operator { "@>" } else { "<@" }, a, b)),
    )
}

/// The operators `&&`, `<<`, `>>` and `~=` on geometric values, by the internal names the
/// parser gives them.
pub(crate) fn operator(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let [left, right] = values else {
        return None;
    };
    let (a, b) = (geometric_of(left)?, geometric_of(right)?);
    let (ahx, _, alx, _) = bounding(a);
    let (bhx, _, blx, _) = bounding(b);
    Some(Ok(Value::Boolean(match name {
        crate::ranges::OVERLAPS => overlaps(a, b),
        crate::ranges::LEFT_OF => ahx < blx,
        crate::ranges::RIGHT_OF => alx > bhx,
        SAME_FUNCTION => same(a, b),
        _ => return None,
    })))
}

fn translated(value: &GeometricValue, (dx, dy): (f64, f64)) -> GeometricValue {
    let mut moved = value.clone();
    match value.kind {
        GeometricKind::Circle => {
            moved.numbers[0] += dx;
            moved.numbers[1] += dy;
        }
        GeometricKind::Line => {}
        _ => {
            for pair in moved.numbers.chunks_mut(2) {
                pair[0] += dx;
                if let Some(y) = pair.get_mut(1) {
                    *y += dy;
                }
            }
        }
    }
    moved
}

/// `+` and `-` with a point: moves a point, box, path or circle.
pub(crate) fn arithmetic(operator: BinaryOperator, left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    let (a, b) = (geometric_of(left)?, geometric_of(right)?);
    if b.kind != GeometricKind::Point
        || !matches!(a.kind, GeometricKind::Point | GeometricKind::Box | GeometricKind::Path | GeometricKind::Circle)
    {
        return Some(Err(no_operator(operator.symbol(), a, b)));
    }
    let offset = (b.numbers[0], b.numbers[1]);
    Some(match operator {
        BinaryOperator::Add => Ok(value(translated(a, offset))),
        BinaryOperator::Subtract => Ok(value(translated(a, (-offset.0, -offset.1)))),
        _ => Err(no_operator(operator.symbol(), a, b)),
    })
}

/// The type of `left operator right` when the left side is a geometric type.
pub(crate) fn arithmetic_type(operator: BinaryOperator, left: SqlType, right: SqlType) -> Option<SqlType> {
    let kind = kind_of(left)?;
    (matches!(operator, BinaryOperator::Add | BinaryOperator::Subtract) && kind_of(right) == Some(GeometricKind::Point))
        .then_some(left)
}

/// The order used to sort geometric values: boxes and circles by area, segments by length,
/// paths by their number of points, the rest by their numbers.
pub(crate) fn compare(a: &GeometricValue, b: &GeometricValue) -> Ordering {
    let measure = |value: &GeometricValue| match value.kind {
        GeometricKind::Box | GeometricKind::Circle => area(value),
        GeometricKind::Lseg => length(value),
        GeometricKind::Path => Some(value.numbers.len() as f64),
        _ => None,
    };
    a.kind.index().cmp(&b.kind.index()).then_with(|| match (measure(a), measure(b)) {
        (Some(x), Some(y)) => crate::types::compare_floats(x, y),
        _ => a
            .numbers
            .iter()
            .zip(&b.numbers)
            .map(|(x, y)| crate::types::compare_floats(*x, *y))
            .find(|order| order.is_ne())
            .unwrap_or_else(|| a.numbers.len().cmp(&b.numbers.len())),
    })
}

/// The bytes that equal values share, for hashing.
pub(crate) fn equality_key(value: &GeometricValue) -> Vec<u8> {
    let mut key = vec![value.kind.index()];
    let measured = match value.kind {
        GeometricKind::Box | GeometricKind::Circle => area(value).map(|found| vec![found]),
        GeometricKind::Lseg => length(value).map(|found| vec![found]),
        GeometricKind::Path => Some(vec![value.numbers.len() as f64]),
        _ => None,
    };
    for number in measured.unwrap_or_else(|| value.numbers.clone()) {
        key.extend_from_slice(&(if number == 0.0 { 0.0 } else { number }).to_bits().to_le_bytes());
    }
    key
}

/// The value as stored: whether it is closed, then its numbers.
pub(crate) fn encoded(value: &GeometricValue) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(1 + value.numbers.len() * 8);
    bytes.push(u8::from(value.closed));
    for number in &value.numbers {
        bytes.extend_from_slice(&number.to_le_bytes());
    }
    bytes
}

/// A stored value of type `kind`, or `None` when the bytes do not fit it.
pub(crate) fn decoded(kind: GeometricKind, bytes: &[u8]) -> Option<GeometricValue> {
    let (&closed, rest) = bytes.split_first()?;
    if rest.len() % 8 != 0 {
        return None;
    }
    let numbers: Vec<f64> = rest
        .chunks_exact(8)
        .map(|chunk| f64::from_le_bytes(chunk.try_into().unwrap_or([0; 8])))
        .collect();
    let fits = match kind {
        GeometricKind::Point => numbers.len() == 2,
        GeometricKind::Line | GeometricKind::Circle => numbers.len() == 3,
        GeometricKind::Lseg | GeometricKind::Box => numbers.len() == 4,
        GeometricKind::Path | GeometricKind::Polygon => numbers.len() >= 2 && numbers.len() % 2 == 0,
    };
    fits.then_some(GeometricValue { kind, closed: closed == 1, numbers })
}

/// The value in PostgreSQL's binary form.
pub(crate) fn binary(value: &GeometricValue) -> Vec<u8> {
    let mut bytes = Vec::new();
    let count = (value.numbers.len() / 2) as i32;
    match value.kind {
        GeometricKind::Path => {
            bytes.push(u8::from(value.closed));
            bytes.extend_from_slice(&count.to_be_bytes());
        }
        GeometricKind::Polygon => bytes.extend_from_slice(&count.to_be_bytes()),
        _ => {}
    }
    for number in &value.numbers {
        bytes.extend_from_slice(&number.to_be_bytes());
    }
    bytes
}

/// The text of a value of type `kind` sent in PostgreSQL's binary form.
pub(crate) fn text_from_binary(kind: GeometricKind, bytes: &[u8]) -> Option<String> {
    let (closed, rest) = match kind {
        GeometricKind::Path => {
            let (&closed, rest) = bytes.split_first()?;
            (closed == 1, rest.get(4..)?)
        }
        GeometricKind::Polygon => (true, bytes.get(4..)?),
        _ => (true, bytes),
    };
    if rest.len() % 8 != 0 {
        return None;
    }
    let numbers: Vec<f64> = rest
        .chunks_exact(8)
        .map(|chunk| f64::from_be_bytes(chunk.try_into().unwrap_or([0; 8])))
        .collect();
    let mut little = vec![u8::from(closed)];
    for number in &numbers {
        little.extend_from_slice(&number.to_le_bytes());
    }
    decoded(kind, &little).map(|found| to_text(&found))
}

/// The functions on geometric values, by name.
pub(crate) const FUNCTIONS: [&str; 22] = [
    "area", "center", "diameter", "radius", "height", "width", "length", "npoints", "isclosed", "isopen", "popen",
    "pclose", "point", "box", "circle", "lseg", "line", "path", "polygon", "slope", "bound_box", "diagonal",
];

fn number(value: &Value) -> Option<f64> {
    value.as_float()
}

/// A geometric function's answer, or `None` when `name` is not one or its arguments are not
/// geometric values.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) || crate::constraints::current_dialect() != crate::Dialect::PostgreSql {
        return None;
    }
    let takes_geometric = values.iter().any(|found| geometric_of(found).is_some());
    if values.iter().any(Value::is_null) && (takes_geometric || matches!(name, "point" | "polygon")) {
        return Some(Ok(Value::Null));
    }
    let first = values.first().and_then(geometric_of);
    let second = values.get(1).and_then(geometric_of);
    let made = |kind: GeometricKind, numbers: Vec<f64>| Some(Ok(value(GeometricValue { kind, closed: true, numbers })));
    let double = |found: Option<f64>| Some(Ok(found.map_or(Value::Null, Value::Double)));
    match (name, values.len(), first, second) {
        ("area", 1, Some(found), _) => double(area(found)),
        ("center", 1, Some(found), _) => center(found).map(|(x, y)| Ok(value(point(x, y)))),
        ("radius", 1, Some(found), _) if found.kind == GeometricKind::Circle => double(Some(found.numbers[2])),
        ("diameter", 1, Some(found), _) if found.kind == GeometricKind::Circle => double(Some(found.numbers[2] * 2.0)),
        ("height", 1, Some(found), _) if found.kind == GeometricKind::Box => double(Some(found.numbers[1] - found.numbers[3])),
        ("width", 1, Some(found), _) if found.kind == GeometricKind::Box => double(Some(found.numbers[0] - found.numbers[2])),
        ("length", 1, Some(found), _) => double(length(found)),
        ("npoints", 1, Some(found), _) if matches!(found.kind, GeometricKind::Path | GeometricKind::Polygon) => {
            Some(Ok(Value::Integer((found.numbers.len() / 2) as i32)))
        }
        ("isclosed" | "isopen", 1, Some(found), _) if found.kind == GeometricKind::Path => {
            Some(Ok(Value::Boolean(found.closed == (name == "isclosed"))))
        }
        ("popen" | "pclose", 1, Some(found), _) if found.kind == GeometricKind::Path => {
            Some(Ok(value(GeometricValue { closed: name == "pclose", ..found.clone() })))
        }
        ("point", 2, None, None) => {
            let (x, y) = (number(&values[0])?, number(&values[1])?);
            Some(Ok(value(point(x, y))))
        }
        ("point", 1, Some(found), _) => center(found).map(|(x, y)| Ok(value(point(x, y)))),
        ("box", 2, Some(a), Some(b)) if a.kind == GeometricKind::Point && b.kind == GeometricKind::Point => {
            made(GeometricKind::Box, boxed(a.numbers[0], a.numbers[1], b.numbers[0], b.numbers[1]))
        }
        ("box", 1, Some(found), _) if found.kind == GeometricKind::Point => {
            made(GeometricKind::Box, boxed(found.numbers[0], found.numbers[1], found.numbers[0], found.numbers[1]))
        }
        ("box", 1, Some(found), _) if found.kind == GeometricKind::Circle => {
            let half = found.numbers[2] / std::f64::consts::SQRT_2;
            let (x, y) = (found.numbers[0], found.numbers[1]);
            made(GeometricKind::Box, vec![x + half, y + half, x - half, y - half])
        }
        ("box" | "bound_box", _, Some(found), other) => {
            let (hx, hy, lx, ly) = bounding(found);
            let (hx, hy, lx, ly) = match other {
                Some(other) => {
                    let (ohx, ohy, olx, oly) = bounding(other);
                    (hx.max(ohx), hy.max(ohy), lx.min(olx), ly.min(oly))
                }
                None => (hx, hy, lx, ly),
            };
            made(GeometricKind::Box, vec![hx, hy, lx, ly])
        }
        ("circle", 2, Some(found), None) if found.kind == GeometricKind::Point => {
            let radius = number(&values[1])?;
            made(GeometricKind::Circle, vec![found.numbers[0], found.numbers[1], radius])
        }
        ("circle", 1, Some(found), _) if matches!(found.kind, GeometricKind::Box | GeometricKind::Polygon) => {
            let (x, y) = center(found)?;
            let radius = if found.kind == GeometricKind::Box {
                distance((x, y), (found.numbers[0], found.numbers[1]))
            } else {
                let corners = points(found);
                corners.iter().map(|at| distance(*at, (x, y))).sum::<f64>() / corners.len() as f64
            };
            made(GeometricKind::Circle, vec![x, y, radius])
        }
        ("lseg" | "line", 2, Some(a), Some(b)) if a.kind == GeometricKind::Point && b.kind == GeometricKind::Point => {
            let (x1, y1, x2, y2) = (a.numbers[0], a.numbers[1], b.numbers[0], b.numbers[1]);
            if name == "lseg" {
                made(GeometricKind::Lseg, vec![x1, y1, x2, y2])
            } else {
                made(GeometricKind::Line, line_through(x1, y1, x2, y2))
            }
        }
        ("lseg" | "diagonal", 1, Some(found), _) if found.kind == GeometricKind::Box => made(GeometricKind::Lseg, found.numbers.clone()),
        ("path", 1, Some(found), _) if found.kind == GeometricKind::Polygon => {
            Some(Ok(value(GeometricValue { kind: GeometricKind::Path, closed: true, numbers: found.numbers.clone() })))
        }
        ("polygon", 1, Some(found), _) if found.kind == GeometricKind::Box => {
            let n = &found.numbers;
            made(GeometricKind::Polygon, vec![n[2], n[3], n[2], n[1], n[0], n[1], n[0], n[3]])
        }
        ("polygon", 1, Some(found), _) if found.kind == GeometricKind::Path => {
            if !found.closed {
                return Some(Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "open path cannot be converted to polygon",
                )));
            }
            made(GeometricKind::Polygon, found.numbers.clone())
        }
        ("polygon", 2, None, Some(found)) if found.kind == GeometricKind::Circle => {
            let count = values[0].as_integer().filter(|count| *count >= 1)?;
            let (x, y, radius) = (found.numbers[0], found.numbers[1], found.numbers[2]);
            let step = 2.0 * std::f64::consts::PI / count as f64;
            let numbers = (0..count)
                .flat_map(|index| {
                    let angle = step * index as f64;
                    [x - radius * angle.cos(), y + radius * angle.sin()]
                })
                .collect();
            made(GeometricKind::Polygon, numbers)
        }
        ("slope", 2, Some(a), Some(b)) if a.kind == GeometricKind::Point && b.kind == GeometricKind::Point => {
            let (dx, dy) = (b.numbers[0] - a.numbers[0], b.numbers[1] - a.numbers[1]);
            double(Some(if dx == 0.0 { f64::INFINITY } else { dy / dx }))
        }
        _ => None,
    }
}

/// The type a geometric function returns.
pub(crate) fn result_type(name: &str, first: Option<SqlType>) -> Option<SqlType> {
    if !FUNCTIONS.contains(&name) || crate::constraints::current_dialect() != crate::Dialect::PostgreSql {
        return None;
    }
    let geometric = |kind: GeometricKind| Some(SqlType::Defined(kind.defined_type()));
    let first_kind = first.and_then(kind_of);
    match name {
        "length" if first_kind.is_none() => None,
        "area" | "diameter" | "radius" | "height" | "width" | "length" | "slope" => Some(SqlType::Double),
        "npoints" => Some(SqlType::Integer),
        "isclosed" | "isopen" => Some(SqlType::Boolean),
        "popen" | "pclose" | "path" => geometric(GeometricKind::Path),
        "center" | "point" => geometric(GeometricKind::Point),
        "box" | "bound_box" => geometric(GeometricKind::Box),
        "circle" => geometric(GeometricKind::Circle),
        "lseg" | "diagonal" => geometric(GeometricKind::Lseg),
        "line" => geometric(GeometricKind::Line),
        "polygon" => geometric(GeometricKind::Polygon),
        _ => None,
    }
}

/// Casts between geometric types that PostgreSQL allows, by calling the function of the
/// target type's name.
pub(crate) fn convert(found: &GeometricValue, target: GeometricKind) -> Option<SqlResult<Value>> {
    if found.kind == target {
        return Some(Ok(value(found.clone())));
    }
    function(target.name(), &[value(found.clone())])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(text: &str, kind: GeometricKind) -> String {
        to_text(&parse(text, kind).unwrap())
    }

    #[test]
    fn values_read_and_print_as_postgresql_does() {
        assert_eq!(shown("(1,2)", GeometricKind::Point), "(1,2)");
        assert_eq!(shown(" 1.5 , -2 ", GeometricKind::Point), "(1.5,-2)");
        assert_eq!(shown("(0,0),(2,3)", GeometricKind::Box), "(2,3),(0,0)");
        assert_eq!(shown("[(0,0),(1,1)]", GeometricKind::Lseg), "[(0,0),(1,1)]");
        assert_eq!(shown("[(0,0),(1,1),(2,0)]", GeometricKind::Path), "[(0,0),(1,1),(2,0)]");
        assert_eq!(shown("((0,0),(1,1),(2,0))", GeometricKind::Path), "((0,0),(1,1),(2,0))");
        assert_eq!(shown("<(1,1),2>", GeometricKind::Circle), "<(1,1),2>");
        assert_eq!(shown("{1,-1,0}", GeometricKind::Line), "{1,-1,0}");
        assert_eq!(shown("[(0,0),(2,2)]", GeometricKind::Line), "{1,-1,0}");
        assert!(parse("(1,2,3)", GeometricKind::Point).is_err());
        assert!(parse("<(1,1),-2>", GeometricKind::Circle).is_err());
        assert!(parse("{0,0,1}", GeometricKind::Line).is_err());
        assert!(parse("(a,b)", GeometricKind::Point).is_err());
    }

    #[test]
    fn measures_distances_and_containment() {
        let of = |text: &str, kind| value(parse(text, kind).unwrap());
        let call = |name: &str, values: &[Value]| function(name, values).unwrap().unwrap().to_text().unwrap_or_default();
        assert_eq!(call("area", &[of("(0,0),(2,3)", GeometricKind::Box)]), "6");
        assert_eq!(call("center", &[of("(0,0),(2,4)", GeometricKind::Box)]), "(1,2)");
        assert_eq!(call("length", &[of("[(0,0),(3,4)]", GeometricKind::Lseg)]), "5");
        assert_eq!(call("npoints", &[of("((0,0),(1,1),(2,0))", GeometricKind::Polygon)]), "3");
        assert_eq!(call("area", &[of("((0,0),(4,0),(4,3),(0,3))", GeometricKind::Polygon)]), "12");
        assert_eq!(call("point", &[Value::Double(1.0), Value::Double(2.0)]), "(1,2)");
        assert_eq!(call("box", &[of("(1,1)", GeometricKind::Point), of("(0,0)", GeometricKind::Point)]), "(1,1),(0,0)");
        let distance = distance_between(&of("(0,0)", GeometricKind::Point), &of("(3,4)", GeometricKind::Point));
        assert_eq!(distance.unwrap().unwrap(), Value::Double(5.0));
        let inside = containment(true, &of("(0,0),(2,2)", GeometricKind::Box), &of("(1,1)", GeometricKind::Point));
        assert_eq!(inside.unwrap().unwrap(), Value::Boolean(true));
        let inside = containment(false, &of("(5,5)", GeometricKind::Point), &of("<(0,0),1>", GeometricKind::Circle));
        assert_eq!(inside.unwrap().unwrap(), Value::Boolean(false));
        let triangle = of("((0,0),(4,0),(0,4))", GeometricKind::Polygon);
        assert_eq!(containment(true, &triangle, &of("(1,1)", GeometricKind::Point)).unwrap().unwrap(), Value::Boolean(true));
        assert_eq!(containment(true, &triangle, &of("(3,3)", GeometricKind::Point)).unwrap().unwrap(), Value::Boolean(false));
        let overlapping = operator(
            crate::ranges::OVERLAPS,
            &[of("(0,0),(2,2)", GeometricKind::Box), of("(1,1),(3,3)", GeometricKind::Box)],
        );
        assert_eq!(overlapping.unwrap().unwrap(), Value::Boolean(true));
        let moved = arithmetic(BinaryOperator::Add, &of("(0,0),(1,1)", GeometricKind::Box), &of("(2,3)", GeometricKind::Point));
        assert_eq!(moved.unwrap().unwrap().to_text().unwrap_or_default(), "(3,4),(2,3)");
    }

    #[test]
    fn stored_and_binary_forms_round_trip() {
        for (text, kind) in [
            ("(1.5,2)", GeometricKind::Point),
            ("[(0,0),(1,1),(2,0)]", GeometricKind::Path),
            ("((0,0),(1,1),(2,0))", GeometricKind::Polygon),
            ("<(1,1),2>", GeometricKind::Circle),
        ] {
            let found = parse(text, kind).unwrap();
            assert_eq!(decoded(kind, &encoded(&found)), Some(found.clone()));
            assert_eq!(text_from_binary(kind, &binary(&found)).as_deref(), Some(to_text(&found).as_str()));
        }
    }

    fn answer(session: &mut crate::Session<'_>, sql: &str) -> String {
        let mut outputs = Vec::new();
        session
            .execute(sql, &mut |result| outputs.push(crate::tests::render(&result)))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        outputs.pop().unwrap_or_default()
    }

    #[test]
    fn columns_store_shapes_and_queries_measure_them() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        answer(
            &mut session,
            "CREATE TABLE shapes (id integer PRIMARY KEY, at point, area_box box, ring circle, outline polygon);
             INSERT INTO shapes VALUES (1, '(1,1)', '(0,0),(2,2)', '<(0,0),1>', '((0,0),(4,0),(0,4))'),
                                       (2, '(5,5)', '(3,3),(4,4)', '<(5,5),2>', '((10,10),(11,10),(10,11))')",
        );
        assert_eq!(
            answer(&mut session, "SELECT id, at, area_box, ring, outline FROM shapes WHERE id = 1"),
            "id|at|area_box|ring|outline\n1|(1,1)|(2,2),(0,0)|<(0,0),1>|((0,0),(4,0),(0,4))\nSELECT 1"
        );
        assert_eq!(
            answer(&mut session, "SELECT id FROM shapes WHERE area_box @> at ORDER BY id"),
            "id\n1\nSELECT 1"
        );
        assert_eq!(
            answer(&mut session, "SELECT id, at <-> point(0, 0) AS d FROM shapes ORDER BY at <-> point(0, 0)"),
            "id|d\n1|1.4142135623731\n2|7.07106781186548\nSELECT 2"
                .replace("1.4142135623731", &Value::Double(2f64.sqrt()).to_text().unwrap_or_default())
                .replace("7.07106781186548", &Value::Double(50f64.sqrt()).to_text().unwrap_or_default())
        );
        assert_eq!(
            answer(&mut session, "SELECT area(area_box), radius(ring), npoints(outline), area_box && box '(1,1),(3,3)' AS o FROM shapes WHERE id = 1"),
            "area|radius|npoints|o\n4|1|3|t\nSELECT 1"
        );
        assert_eq!(
            answer(&mut session, "SELECT at + point '(1,2)' AS moved, at ~= point '(1,1)' AS same FROM shapes WHERE id = 1"),
            "moved|same\n(2,3)|t\nSELECT 1"
        );
    }
}
