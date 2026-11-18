use crate::{Coord, GeoError, Geometry};

const EARTH_RADIUS_METRES: f64 = 6_371_008.8;

fn segments(geometry: &Geometry) -> Vec<(Coord, Coord)> {
    let of_line = |line: &[Coord]| {
        line.windows(2)
            .map(|pair| (pair[0], pair[1]))
            .collect::<Vec<_>>()
    };
    match geometry {
        Geometry::Point(_) | Geometry::MultiPoint(_) => Vec::new(),
        Geometry::LineString(line) => of_line(line),
        Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
            rings.iter().flat_map(|ring| of_line(ring)).collect()
        }
        Geometry::MultiPolygon(polygons) => polygons
            .iter()
            .flatten()
            .flat_map(|ring| of_line(ring))
            .collect(),
        Geometry::Collection(items) => items.iter().flat_map(segments).collect(),
    }
}

fn points(geometry: &Geometry) -> Vec<Coord> {
    match geometry {
        Geometry::Point(point) => point.iter().copied().collect(),
        Geometry::MultiPoint(points) => points.clone(),
        Geometry::Collection(items) => items.iter().flat_map(points).collect(),
        _ => Vec::new(),
    }
}

fn polygons(geometry: &Geometry) -> Vec<&Vec<Vec<Coord>>> {
    match geometry {
        Geometry::Polygon(rings) => vec![rings],
        Geometry::MultiPolygon(polygons) => polygons.iter().collect(),
        Geometry::Collection(items) => items.iter().flat_map(polygons).collect(),
        _ => Vec::new(),
    }
}

fn point_distance(a: Coord, b: Coord) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

fn point_segment(point: Coord, (a, b): (Coord, Coord)) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let length = dx * dx + dy * dy;
    if length == 0.0 {
        return point_distance(point, a);
    }
    let t = (((point.x - a.x) * dx + (point.y - a.y) * dy) / length).clamp(0.0, 1.0);
    point_distance(
        point,
        Coord {
            x: a.x + t * dx,
            y: a.y + t * dy,
        },
    )
}

fn orientation(a: Coord, b: Coord, c: Coord) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn on_segment(a: Coord, b: Coord, c: Coord) -> bool {
    c.x >= a.x.min(b.x) && c.x <= a.x.max(b.x) && c.y >= a.y.min(b.y) && c.y <= a.y.max(b.y)
}

fn segments_cross((a, b): (Coord, Coord), (c, d): (Coord, Coord)) -> bool {
    let (o1, o2, o3, o4) = (
        orientation(a, b, c),
        orientation(a, b, d),
        orientation(c, d, a),
        orientation(c, d, b),
    );
    if (o1 > 0.0) != (o2 > 0.0)
        && (o3 > 0.0) != (o4 > 0.0)
        && o1 != 0.0
        && o2 != 0.0
        && o3 != 0.0
        && o4 != 0.0
    {
        return true;
    }
    (o1 == 0.0 && on_segment(a, b, c))
        || (o2 == 0.0 && on_segment(a, b, d))
        || (o3 == 0.0 && on_segment(c, d, a))
        || (o4 == 0.0 && on_segment(c, d, b))
}

fn segment_distance(first: (Coord, Coord), second: (Coord, Coord)) -> f64 {
    if segments_cross(first, second) {
        return 0.0;
    }
    point_segment(first.0, second)
        .min(point_segment(first.1, second))
        .min(point_segment(second.0, first))
        .min(point_segment(second.1, first))
}

fn in_ring(point: Coord, ring: &[Coord]) -> bool {
    let mut inside = false;
    for pair in ring.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if (a.y > point.y) != (b.y > point.y)
            && point.x < (b.x - a.x) * (point.y - a.y) / (b.y - a.y) + a.x
        {
            inside = !inside;
        }
    }
    inside
}

fn on_boundary(point: Coord, rings: &[Vec<Coord>]) -> bool {
    rings.iter().any(|ring| {
        ring.windows(2)
            .any(|pair| point_segment(point, (pair[0], pair[1])) == 0.0)
    })
}

fn in_polygon(point: Coord, rings: &[Vec<Coord>]) -> bool {
    match rings.split_first() {
        Some((outer, holes)) => {
            in_ring(point, outer) && !holes.iter().any(|hole| in_ring(point, hole))
                || on_boundary(point, rings)
        }
        None => false,
    }
}

fn covered(point: Coord, geometry: &Geometry) -> bool {
    polygons(geometry)
        .iter()
        .any(|rings| in_polygon(point, rings))
        || segments(geometry)
            .iter()
            .any(|segment| point_segment(point, *segment) == 0.0)
        || points(geometry)
            .iter()
            .any(|other| point_distance(point, *other) == 0.0)
}

/// The least planar distance between the two geometries (0 when they meet), or `None` when one
/// is empty.
pub fn distance(left: &Geometry, right: &Geometry) -> Option<f64> {
    if left.is_empty() || right.is_empty() {
        return None;
    }
    if intersects(left, right) {
        return Some(0.0);
    }
    let (left_segments, right_segments) = (segments(left), segments(right));
    let (left_points, right_points) = (points(left), points(right));
    let mut best = f64::INFINITY;
    for a in &left_points {
        for b in &right_points {
            best = best.min(point_distance(*a, *b));
        }
        for segment in &right_segments {
            best = best.min(point_segment(*a, *segment));
        }
    }
    for b in &right_points {
        for segment in &left_segments {
            best = best.min(point_segment(*b, *segment));
        }
    }
    for a in &left_segments {
        for b in &right_segments {
            best = best.min(segment_distance(*a, *b));
        }
    }
    Some(best)
}

/// Whether the geometries share at least one position.
pub fn intersects(left: &Geometry, right: &Geometry) -> bool {
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let (left_segments, right_segments) = (segments(left), segments(right));
    if left_segments
        .iter()
        .any(|a| right_segments.iter().any(|b| segments_cross(*a, *b)))
    {
        return true;
    }
    left.coords().iter().any(|point| covered(*point, right))
        || right.coords().iter().any(|point| covered(*point, left))
}

/// Whether every position of `inner` lies in `outer` (its interior or boundary) and their
/// boundaries do not cross, as `ST_Contains` decides for the common cases.
pub fn contains(outer: &Geometry, inner: &Geometry) -> bool {
    if inner.is_empty()
        || outer.is_empty()
        || !inner.coords().iter().all(|point| covered(*point, outer))
    {
        return false;
    }
    let outer_polygons = polygons(outer);
    if outer_polygons.is_empty() {
        return true;
    }
    let edges = segments(outer);
    !segments(inner).iter().any(|segment| {
        let middle = Coord {
            x: (segment.0.x + segment.1.x) / 2.0,
            y: (segment.0.y + segment.1.y) / 2.0,
        };
        !covered(middle, outer) || edges.iter().any(|edge| crosses_properly(*segment, *edge))
    })
}

fn crosses_properly((a, b): (Coord, Coord), (c, d): (Coord, Coord)) -> bool {
    let (o1, o2, o3, o4) = (
        orientation(a, b, c),
        orientation(a, b, d),
        orientation(c, d, a),
        orientation(c, d, b),
    );
    o1 * o2 < 0.0 && o3 * o4 < 0.0
}

fn ring_area(ring: &[Coord]) -> f64 {
    ring.windows(2)
        .map(|pair| pair[0].x * pair[1].y - pair[1].x * pair[0].y)
        .sum::<f64>()
        .abs()
        / 2.0
}

/// The planar area of the polygons in the geometry, holes subtracted.
pub fn area(geometry: &Geometry) -> f64 {
    polygons(geometry)
        .iter()
        .map(|rings| match rings.split_first() {
            Some((outer, holes)) => {
                ring_area(outer) - holes.iter().map(|hole| ring_area(hole)).sum::<f64>()
            }
            None => 0.0,
        })
        .sum()
}

/// The planar length of the line strings in the geometry (polygons count 0, as in PostGIS).
pub fn length(geometry: &Geometry) -> f64 {
    match geometry {
        Geometry::LineString(_) | Geometry::MultiLineString(_) => segments(geometry)
            .iter()
            .map(|(a, b)| point_distance(*a, *b))
            .sum(),
        Geometry::Collection(items) => items.iter().map(length).sum(),
        _ => 0.0,
    }
}

/// The bounding box as a polygon (a point for a single position), or `None` when empty.
pub fn envelope(geometry: &Geometry) -> Option<Geometry> {
    let coords = geometry.coords();
    let first = *coords.first()?;
    let (mut low, mut high) = (first, first);
    for coord in coords {
        low = Coord {
            x: low.x.min(coord.x),
            y: low.y.min(coord.y),
        };
        high = Coord {
            x: high.x.max(coord.x),
            y: high.y.max(coord.y),
        };
    }
    if low == high {
        return Some(Geometry::Point(Some(low)));
    }
    Some(Geometry::Polygon(vec![vec![
        low,
        Coord {
            x: low.x,
            y: high.y,
        },
        high,
        Coord {
            x: high.x,
            y: low.y,
        },
        low,
    ]]))
}

/// The great-circle distance in metres between two points given as longitude and latitude in
/// degrees, on a sphere of the mean Earth radius (`ST_DistanceSphere`).
pub fn sphere_distance(left: &Geometry, right: &Geometry) -> Result<f64, GeoError> {
    let (Geometry::Point(Some(a)), Geometry::Point(Some(b))) = (left, right) else {
        return Err(GeoError("ST_DistanceSphere takes two points".to_owned()));
    };
    let (phi1, phi2) = (a.y.to_radians(), b.y.to_radians());
    let (dphi, dlambda) = ((b.y - a.y).to_radians(), (b.x - a.x).to_radians());
    let h = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlambda / 2.0).sin().powi(2);
    Ok(2.0 * EARTH_RADIUS_METRES * h.sqrt().min(1.0).asin())
}

fn vertices(geometry: &Geometry) -> Vec<Coord> {
    match geometry {
        Geometry::Point(point) => point.iter().copied().collect(),
        Geometry::LineString(points) | Geometry::MultiPoint(points) => points.clone(),
        Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => rings.iter().flatten().copied().collect(),
        Geometry::MultiPolygon(polygons) => polygons.iter().flatten().flatten().copied().collect(),
        Geometry::Collection(items) => items.iter().flat_map(vertices).collect(),
    }
}

fn closest_on(point: Coord, (a, b): (Coord, Coord)) -> Coord {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let length = dx * dx + dy * dy;
    if length == 0.0 {
        return a;
    }
    let t = (((point.x - a.x) * dx + (point.y - a.y) * dy) / length).clamp(0.0, 1.0);
    Coord {
        x: a.x + t * dx,
        y: a.y + t * dy,
    }
}

fn crossing_point((a, b): (Coord, Coord), (c, d): (Coord, Coord)) -> Coord {
    let denominator = (b.x - a.x) * (d.y - c.y) - (b.y - a.y) * (d.x - c.x);
    if denominator == 0.0 {
        return [a, b].into_iter().find(|end| point_segment(*end, (c, d)) == 0.0).unwrap_or(c);
    }
    let t = ((c.x - a.x) * (d.y - c.y) - (c.y - a.y) * (d.x - c.x)) / denominator;
    Coord {
        x: a.x + t * (b.x - a.x),
        y: a.y + t * (b.y - a.y),
    }
}

fn nearest_pair(first: (Coord, Coord), second: (Coord, Coord)) -> (Coord, Coord) {
    if segments_cross(first, second) {
        let meeting = crossing_point(first, second);
        return (meeting, meeting);
    }
    [
        (first.0, closest_on(first.0, second)),
        (first.1, closest_on(first.1, second)),
        (closest_on(second.0, first), second.0),
        (closest_on(second.1, first), second.1),
    ]
    .into_iter()
    .min_by(|x, y| point_distance(x.0, x.1).total_cmp(&point_distance(y.0, y.1)))
    .unwrap_or((first.0, second.0))
}

/// The two nearest positions of the geometries, the first on `left` (`ST_ShortestLine`,
/// `ST_ClosestPoint`, D1273); the same position twice when they meet; `None` when one is empty.
pub fn shortest_line(left: &Geometry, right: &Geometry) -> Option<(Coord, Coord)> {
    if left.is_empty() || right.is_empty() {
        return None;
    }
    let mut best: Option<(f64, Coord, Coord)> = None;
    let mut offer = |a: Coord, b: Coord| {
        let found = point_distance(a, b);
        if best.is_none_or(|(known, _, _)| found < known) {
            best = Some((found, a, b));
        }
    };
    for vertex in vertices(left) {
        if polygons(right).iter().any(|rings| in_polygon(vertex, rings)) {
            offer(vertex, vertex);
        }
    }
    for vertex in vertices(right) {
        if polygons(left).iter().any(|rings| in_polygon(vertex, rings)) {
            offer(vertex, vertex);
        }
    }
    let (left_segments, right_segments) = (segments(left), segments(right));
    let (left_points, right_points) = (points(left), points(right));
    for a in &left_points {
        for b in &right_points {
            offer(*a, *b);
        }
        for segment in &right_segments {
            offer(*a, closest_on(*a, *segment));
        }
    }
    for b in &right_points {
        for segment in &left_segments {
            offer(closest_on(*b, *segment), *b);
        }
    }
    for first in &left_segments {
        for second in &right_segments {
            let (a, b) = nearest_pair(*first, *second);
            offer(a, b);
        }
    }
    best.map(|(_, a, b)| (a, b))
}

/// The two farthest vertices of the geometries, the first of `left` (`ST_LongestLine`,
/// `ST_MaxDistance`); `None` when one is empty.
pub fn longest_line(left: &Geometry, right: &Geometry) -> Option<(Coord, Coord)> {
    let right_vertices = vertices(right);
    vertices(left)
        .into_iter()
        .flat_map(|a| right_vertices.iter().map(move |b| (a, *b)))
        .max_by(|x, y| point_distance(x.0, x.1).total_cmp(&point_distance(y.0, y.1)))
}

/// The discrete Hausdorff distance: the farthest any vertex of one geometry is from the other
/// geometry, both ways (`ST_HausdorffDistance`); `None` when one is empty.
pub fn hausdorff_distance(left: &Geometry, right: &Geometry) -> Option<f64> {
    if left.is_empty() || right.is_empty() {
        return None;
    }
    let one_way = |from: &Geometry, to: &Geometry| -> Option<f64> {
        vertices(from)
            .into_iter()
            .map(|vertex| distance(&Geometry::Point(Some(vertex)), to))
            .try_fold(0.0_f64, |most, found| Some(most.max(found?)))
    };
    Some(one_way(left, right)?.max(one_way(right, left)?))
}

fn line_segments(points: &[Coord]) -> Vec<(Coord, Coord)> {
    points
        .windows(2)
        .map(|pair| (pair[0], pair[1]))
        .filter(|(a, b)| a != b)
        .collect()
}

fn line_is_simple(points: &[Coord]) -> bool {
    let segments = line_segments(points);
    let closed = points.len() >= 4 && points.first() == points.last();
    let last = segments.len().saturating_sub(1);
    for (i, first) in segments.iter().enumerate() {
        for (j, second) in segments.iter().enumerate().skip(i + 1) {
            if j == i + 1 {
                if point_segment(second.1, *first) == 0.0 || point_segment(first.0, *second) == 0.0 {
                    return false;
                }
                continue;
            }
            if closed && i == 0 && j == last {
                if point_segment(second.0, *first) == 0.0 || point_segment(first.1, *second) == 0.0 {
                    return false;
                }
                continue;
            }
            if segments_cross(*first, *second) {
                return false;
            }
        }
    }
    true
}

fn ends_of(points: &[Coord]) -> [Option<Coord>; 2] {
    [points.first().copied(), points.last().copied()]
}

/// Whether the geometry has no anomalous points in the Simple Features sense
/// (`ST_IsSimple`, D1276): a line that does not cross or touch itself except where a closed
/// line's ends meet, lines of a multiline meeting only at ends of both, and a multipoint
/// without repeated points. Polygons are simple.
pub fn is_simple(geometry: &Geometry) -> bool {
    match geometry {
        Geometry::LineString(points) => line_is_simple(points),
        Geometry::MultiPoint(points) => points.iter().enumerate().all(|(at, point)| !points[..at].contains(point)),
        Geometry::MultiLineString(lines) => {
            if !lines.iter().all(|line| line_is_simple(line)) {
                return false;
            }
            for (i, first) in lines.iter().enumerate() {
                for second in lines.iter().skip(i + 1) {
                    for a in line_segments(first) {
                        for b in line_segments(second) {
                            if !segments_cross(a, b) {
                                continue;
                            }
                            let meeting = crossing_point(a, b);
                            let at_ends = ends_of(first).contains(&Some(meeting)) && ends_of(second).contains(&Some(meeting));
                            if !at_ends {
                                return false;
                            }
                        }
                    }
                }
            }
            true
        }
        Geometry::Collection(items) => items.iter().all(is_simple),
        Geometry::Point(_) | Geometry::Polygon(_) | Geometry::MultiPolygon(_) => true,
    }
}
