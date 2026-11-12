//! Linear referencing along line strings, where lines cross, and cutting lines where they
//! cross (D883), written from PostGIS's documented behaviour of `ST_LineLocatePoint`,
//! `ST_LineInterpolatePoint`, `ST_LineSubstring` and `ST_Intersection` of lines.

use crate::{Coord, GeoError, Geometry};

fn segment_length(start: Coord, end: Coord) -> f64 {
    (end.x - start.x).hypot(end.y - start.y)
}

fn line_of(geometry: &Geometry, function: &str) -> Result<Vec<Coord>, GeoError> {
    match geometry {
        Geometry::LineString(points) if points.len() >= 2 => Ok(points.clone()),
        _ => Err(GeoError(format!("{function}: the first argument must be a LINESTRING"))),
    }
}

fn fraction_in_range(fraction: f64, function: &str) -> Result<f64, GeoError> {
    if !(0.0..=1.0).contains(&fraction) {
        return Err(GeoError(format!("{function}: the fraction must be between 0 and 1")));
    }
    Ok(fraction)
}

fn total_length(points: &[Coord]) -> f64 {
    points.windows(2).map(|pair| segment_length(pair[0], pair[1])).sum()
}

/// Where along `line` (a fraction from 0 to 1 of its length) the point closest to `point`
/// lies.
pub fn line_locate_point(line: &Geometry, point: &Geometry) -> Result<f64, GeoError> {
    let points = line_of(line, "ST_LineLocatePoint")?;
    let Geometry::Point(Some(target)) = point else {
        return Err(GeoError("ST_LineLocatePoint: the second argument must be a POINT".to_owned()));
    };
    let total = total_length(&points);
    if total == 0.0 {
        return Ok(0.0);
    }
    let mut best = (f64::INFINITY, 0.0);
    let mut walked = 0.0;
    for pair in points.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let length = segment_length(start, end);
        let along = if length == 0.0 {
            0.0
        } else {
            (((target.x - start.x) * (end.x - start.x) + (target.y - start.y) * (end.y - start.y)) / (length * length))
                .clamp(0.0, 1.0)
        };
        let nearest = Coord {
            x: start.x + along * (end.x - start.x),
            y: start.y + along * (end.y - start.y),
        };
        let distance = segment_length(nearest, *target);
        if distance < best.0 {
            best = (distance, walked + along * length);
        }
        walked += length;
    }
    Ok((best.1 / total).clamp(0.0, 1.0))
}

fn point_at(points: &[Coord], distance: f64) -> (Coord, usize) {
    let mut walked = 0.0;
    for (at, pair) in points.windows(2).enumerate() {
        let length = segment_length(pair[0], pair[1]);
        if walked + length >= distance && length > 0.0 {
            let along = (distance - walked) / length;
            return (
                Coord {
                    x: pair[0].x + along * (pair[1].x - pair[0].x),
                    y: pair[0].y + along * (pair[1].y - pair[0].y),
                },
                at,
            );
        }
        walked += length;
    }
    let last = points.len().saturating_sub(1);
    (points.get(last).copied().unwrap_or(Coord { x: 0.0, y: 0.0 }), last.saturating_sub(1))
}

/// The point at `fraction` of `line`'s length.
pub fn line_interpolate_point(line: &Geometry, fraction: f64) -> Result<Geometry, GeoError> {
    let points = line_of(line, "ST_LineInterpolatePoint")?;
    let fraction = fraction_in_range(fraction, "ST_LineInterpolatePoint")?;
    Ok(Geometry::Point(Some(point_at(&points, fraction * total_length(&points)).0)))
}

/// The part of `line` between the fractions `from` and `to` of its length: a point when they
/// are equal.
pub fn line_substring(line: &Geometry, from: f64, to: f64) -> Result<Geometry, GeoError> {
    let points = line_of(line, "ST_LineSubstring")?;
    let from = fraction_in_range(from, "ST_LineSubstring")?;
    let to = fraction_in_range(to, "ST_LineSubstring")?;
    if from > to {
        return Err(GeoError("ST_LineSubstring: the second fraction must not be less than the first".to_owned()));
    }
    let total = total_length(&points);
    let (start, start_segment) = point_at(&points, from * total);
    let (end, end_segment) = point_at(&points, to * total);
    if from == to {
        return Ok(Geometry::Point(Some(start)));
    }
    let mut kept = vec![start];
    for point in points.iter().take(end_segment + 1).skip(start_segment + 1) {
        if kept.last() != Some(point) {
            kept.push(*point);
        }
    }
    if kept.last() != Some(&end) {
        kept.push(end);
    }
    Ok(Geometry::LineString(kept))
}

fn segment_crossing(a: Coord, b: Coord, c: Coord, d: Coord) -> Option<Coord> {
    let denominator = (b.x - a.x) * (d.y - c.y) - (b.y - a.y) * (d.x - c.x);
    if denominator == 0.0 {
        return None;
    }
    let t = ((c.x - a.x) * (d.y - c.y) - (c.y - a.y) * (d.x - c.x)) / denominator;
    let u = ((c.x - a.x) * (b.y - a.y) - (c.y - a.y) * (b.x - a.x)) / denominator;
    ((0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u)).then(|| Coord {
        x: a.x + t * (b.x - a.x),
        y: a.y + t * (b.y - a.y),
    })
}

fn lines(geometry: &Geometry) -> Vec<Vec<Coord>> {
    match geometry {
        Geometry::LineString(points) => vec![points.clone()],
        Geometry::MultiLineString(lines) => lines.clone(),
        Geometry::Polygon(rings) => rings.clone(),
        Geometry::MultiPolygon(polygons) => polygons.iter().flatten().cloned().collect(),
        Geometry::Collection(members) => members.iter().flat_map(lines).collect(),
        _ => Vec::new(),
    }
}

/// The points where the lines (and rings) of `left` and `right` cross or touch, each once, in
/// order of x then y. Collinear overlaps are not reported.
pub fn line_crossings(left: &Geometry, right: &Geometry) -> Vec<Coord> {
    let mut found: Vec<Coord> = Vec::new();
    for first in lines(left) {
        for second in lines(right) {
            for a in first.windows(2) {
                for b in second.windows(2) {
                    if let Some(point) = segment_crossing(a[0], a[1], b[0], b[1])
                        && !found.iter().any(|known| segment_length(*known, point) < 1e-12)
                    {
                        found.push(point);
                    }
                }
            }
        }
    }
    found.sort_by(|left, right| left.x.total_cmp(&right.x).then(left.y.total_cmp(&right.y)));
    found
}

/// `line` cut at `cuts` (points on or within `tolerance` of it), as its pieces in order along
/// it; pieces shorter than `tolerance` are dropped.
pub fn line_split(line: &Geometry, cuts: &[Coord], tolerance: f64) -> Result<Vec<Geometry>, GeoError> {
    let points = line_of(line, "line_split")?;
    let total = total_length(&points);
    let mut fractions: Vec<f64> = vec![0.0, 1.0];
    for cut in cuts {
        let fraction = line_locate_point(line, &Geometry::Point(Some(*cut)))?;
        let (on_line, _) = point_at(&points, fraction * total);
        if segment_length(on_line, *cut) <= tolerance.max(1e-12) {
            fractions.push(fraction);
        }
    }
    fractions.sort_by(f64::total_cmp);
    fractions.dedup_by(|later, earlier| (*later - *earlier) * total <= tolerance.max(1e-12));
    if fractions.last().is_some_and(|last| *last < 1.0) {
        fractions.push(1.0);
    }
    let mut pieces = Vec::new();
    for pair in fractions.windows(2) {
        let (from, to) = (pair[0], pair[1].min(1.0));
        if (to - from) * total <= tolerance.max(1e-12) {
            continue;
        }
        pieces.push(line_substring(line, from, to)?);
    }
    Ok(pieces)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(points: &[(f64, f64)]) -> Geometry {
        Geometry::LineString(points.iter().map(|&(x, y)| Coord { x, y }).collect())
    }

    #[test]
    fn locate_interpolate_and_substring_agree() {
        let path = line(&[(0.0, 0.0), (2.0, 0.0), (2.0, 2.0)]);
        assert_eq!(line_locate_point(&path, &Geometry::Point(Some(Coord { x: 1.0, y: 1.0 }))).unwrap(), 0.25);
        assert_eq!(
            line_interpolate_point(&path, 0.75).unwrap(),
            Geometry::Point(Some(Coord { x: 2.0, y: 1.0 }))
        );
        assert_eq!(
            line_substring(&path, 0.25, 0.75).unwrap(),
            line(&[(1.0, 0.0), (2.0, 0.0), (2.0, 1.0)])
        );
        assert!(line_substring(&path, 0.8, 0.2).is_err());
        assert!(line_interpolate_point(&path, 1.5).is_err());
    }

    #[test]
    fn crossing_lines_are_cut_where_they_cross() {
        let across = line(&[(0.0, 1.0), (4.0, 1.0)]);
        let down = line(&[(2.0, 0.0), (2.0, 4.0)]);
        let crossings = line_crossings(&across, &down);
        assert_eq!(crossings, vec![Coord { x: 2.0, y: 1.0 }]);
        let pieces = line_split(&across, &crossings, 0.0).unwrap();
        assert_eq!(pieces, vec![line(&[(0.0, 1.0), (2.0, 1.0)]), line(&[(2.0, 1.0), (4.0, 1.0)])]);
        assert!(line_crossings(&across, &line(&[(0.0, 2.0), (4.0, 2.0)])).is_empty());
    }
}
