use crate::{Coord, Geometry, area};

fn ring_length(ring: &[Coord]) -> f64 {
    ring.windows(2)
        .map(|pair| (pair[1].x - pair[0].x).hypot(pair[1].y - pair[0].y))
        .sum()
}

fn ring_signed_area(ring: &[Coord]) -> f64 {
    ring.windows(2)
        .map(|pair| pair[0].x * pair[1].y - pair[1].x * pair[0].y)
        .sum::<f64>()
        / 2.0
}

/// The length of a polygon's rings (all rings, holes included, as PostGIS measures it); 0 for
/// geometries without area.
pub fn perimeter(geometry: &Geometry) -> f64 {
    match geometry {
        Geometry::Polygon(rings) => rings.iter().map(|ring| ring_length(ring)).sum(),
        Geometry::MultiPolygon(polygons) => polygons
            .iter()
            .flatten()
            .map(|ring| ring_length(ring))
            .sum(),
        Geometry::Collection(items) => items.iter().map(perimeter).sum(),
        _ => 0.0,
    }
}

fn polygon_centroid(rings: &[Vec<Coord>]) -> Option<(f64, f64, f64)> {
    let mut weight = 0.0;
    let mut x = 0.0;
    let mut y = 0.0;
    for (at, ring) in rings.iter().enumerate() {
        let sign = if at == 0 { 1.0 } else { -1.0 };
        let signed = ring_signed_area(ring);
        if signed == 0.0 {
            continue;
        }
        let mut cx = 0.0;
        let mut cy = 0.0;
        for pair in ring.windows(2) {
            let cross = pair[0].x * pair[1].y - pair[1].x * pair[0].y;
            cx += (pair[0].x + pair[1].x) * cross;
            cy += (pair[0].y + pair[1].y) * cross;
        }
        let ring_area = signed.abs() * sign;
        x += cx / (6.0 * signed) * ring_area;
        y += cy / (6.0 * signed) * ring_area;
        weight += ring_area;
    }
    (weight != 0.0).then_some((x, y, weight))
}

fn line_centroid(line: &[Coord]) -> Option<(f64, f64, f64)> {
    let mut weight = 0.0;
    let mut x = 0.0;
    let mut y = 0.0;
    for pair in line.windows(2) {
        let length = (pair[1].x - pair[0].x).hypot(pair[1].y - pair[0].y);
        x += (pair[0].x + pair[1].x) / 2.0 * length;
        y += (pair[0].y + pair[1].y) / 2.0 * length;
        weight += length;
    }
    (weight != 0.0).then_some((x, y, weight))
}

fn merged(parts: impl Iterator<Item = Option<(f64, f64, f64)>>) -> Option<Coord> {
    let mut weight = 0.0;
    let mut x = 0.0;
    let mut y = 0.0;
    for (px, py, w) in parts.flatten() {
        x += px;
        y += py;
        weight += w;
    }
    (weight != 0.0).then(|| Coord {
        x: x / weight,
        y: y / weight,
    })
}

/// The geometry's centroid: area-weighted over its polygons when it has area, else
/// length-weighted over its lines, else the mean of its points, as PostGIS computes it from the
/// highest dimension present; `None` for an empty geometry.
pub fn centroid(geometry: &Geometry) -> Option<Coord> {
    if area(geometry) > 0.0 {
        let polygons = polygons_of(geometry);
        return merged(polygons.iter().map(|rings| polygon_centroid(rings)));
    }
    let lines = lines_of(geometry);
    if let Some(found) = merged(lines.iter().map(|line| line_centroid(line))) {
        return Some(found);
    }
    let points = geometry.coords();
    if points.is_empty() {
        return None;
    }
    let count = points.len() as f64;
    Some(Coord {
        x: points.iter().map(|point| point.x).sum::<f64>() / count,
        y: points.iter().map(|point| point.y).sum::<f64>() / count,
    })
}

fn polygons_of(geometry: &Geometry) -> Vec<Vec<Vec<Coord>>> {
    match geometry {
        Geometry::Polygon(rings) => vec![rings.clone()],
        Geometry::MultiPolygon(polygons) => polygons.clone(),
        Geometry::Collection(items) => items.iter().flat_map(polygons_of).collect(),
        _ => Vec::new(),
    }
}

fn lines_of(geometry: &Geometry) -> Vec<Vec<Coord>> {
    match geometry {
        Geometry::LineString(line) => vec![line.clone()],
        Geometry::MultiLineString(lines) => lines.clone(),
        Geometry::Polygon(rings) => rings.clone(),
        Geometry::MultiPolygon(polygons) => polygons.iter().flatten().cloned().collect(),
        Geometry::Collection(items) => items.iter().flat_map(lines_of).collect(),
        _ => Vec::new(),
    }
}

fn cross(origin: Coord, a: Coord, b: Coord) -> f64 {
    (a.x - origin.x) * (b.y - origin.y) - (a.y - origin.y) * (b.x - origin.x)
}

/// The smallest convex geometry holding every point (Andrew's monotone chain): a polygon, or a
/// line or point when the points are collinear or one.
pub fn convex_hull(geometry: &Geometry) -> Geometry {
    let mut points = geometry.coords();
    points.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    points.dedup_by(|a, b| a.x == b.x && a.y == b.y);
    match points.len() {
        0 => return Geometry::Collection(Vec::new()),
        1 => return Geometry::Point(Some(points[0])),
        _ => {}
    }
    let mut lower: Vec<Coord> = Vec::new();
    for &point in &points {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], point) <= 0.0 {
            lower.pop();
        }
        lower.push(point);
    }
    let mut upper: Vec<Coord> = Vec::new();
    for &point in points.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], point) <= 0.0 {
            upper.pop();
        }
        upper.push(point);
    }
    lower.pop();
    upper.pop();
    let mut hull = lower;
    hull.extend(upper);
    if hull.len() < 3 {
        return Geometry::LineString(hull);
    }
    hull.reverse();
    let first = hull[0];
    hull.push(first);
    Geometry::Polygon(vec![hull])
}

fn segments_cross(a: Coord, b: Coord, c: Coord, d: Coord) -> bool {
    let d1 = cross(c, d, a);
    let d2 = cross(c, d, b);
    let d3 = cross(a, b, c);
    let d4 = cross(a, b, d);
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

fn ring_problem(ring: &[Coord]) -> Option<String> {
    if ring.len() < 4 {
        return Some("Too few points in geometry component".to_owned());
    }
    if ring.first() != ring.last() {
        return Some("Ring is not closed".to_owned());
    }
    let segments: Vec<(Coord, Coord)> = ring.windows(2).map(|pair| (pair[0], pair[1])).collect();
    for (at, first) in segments.iter().enumerate() {
        for second in segments.iter().skip(at + 2) {
            if segments_cross(first.0, first.1, second.0, second.1) {
                return Some(format!("Self-intersection[{} {}]", second.0.x, second.0.y));
            }
        }
    }
    None
}

/// Why the geometry is not valid in the OGC sense, `None` when it is: rings closed, with four or
/// more points and without proper self-crossings, and lines with two or more points. Holes inside
/// their shell and polygons that touch are not checked yet.
pub fn invalidity(geometry: &Geometry) -> Option<String> {
    match geometry {
        Geometry::LineString(line) if line.len() == 1 => {
            Some("Too few points in geometry component".to_owned())
        }
        Geometry::MultiLineString(lines) => lines
            .iter()
            .find(|line| line.len() == 1)
            .map(|_| "Too few points in geometry component".to_owned()),
        Geometry::Polygon(rings) => rings.iter().find_map(|ring| ring_problem(ring)),
        Geometry::MultiPolygon(polygons) => polygons.iter().flatten().find_map(|ring| ring_problem(ring)),
        Geometry::Collection(items) => items.iter().find_map(invalidity),
        _ => None,
    }
}

fn mapped_coords(geometry: &Geometry, change: &dyn Fn(Coord) -> Coord) -> Geometry {
    let line = |line: &Vec<Coord>| line.iter().map(|coord| change(*coord)).collect::<Vec<_>>();
    match geometry {
        Geometry::Point(point) => Geometry::Point(point.map(change)),
        Geometry::LineString(points) => Geometry::LineString(line(points)),
        Geometry::MultiPoint(points) => Geometry::MultiPoint(line(points)),
        Geometry::Polygon(rings) => Geometry::Polygon(rings.iter().map(line).collect()),
        Geometry::MultiLineString(lines) => Geometry::MultiLineString(lines.iter().map(line).collect()),
        Geometry::MultiPolygon(polygons) => Geometry::MultiPolygon(
            polygons
                .iter()
                .map(|rings| rings.iter().map(line).collect())
                .collect(),
        ),
        Geometry::Collection(items) => Geometry::Collection(
            items.iter().map(|item| mapped_coords(item, change)).collect(),
        ),
    }
}

/// The geometry moved by `dx`, `dy`.
pub fn translated(geometry: &Geometry, dx: f64, dy: f64) -> Geometry {
    mapped_coords(geometry, &|coord| Coord {
        x: coord.x + dx,
        y: coord.y + dy,
    })
}

/// The geometry with its coordinates multiplied by `sx`, `sy`.
pub fn scaled(geometry: &Geometry, sx: f64, sy: f64) -> Geometry {
    mapped_coords(geometry, &|coord| Coord {
        x: coord.x * sx,
        y: coord.y * sy,
    })
}

/// The geometry with x and y swapped.
pub fn flipped(geometry: &Geometry) -> Geometry {
    mapped_coords(geometry, &|coord| Coord {
        x: coord.y,
        y: coord.x,
    })
}

/// The geometry with the order of its points reversed in every line and ring.
pub fn reversed(geometry: &Geometry) -> Geometry {
    let line = |line: &Vec<Coord>| line.iter().rev().copied().collect::<Vec<_>>();
    match geometry {
        Geometry::LineString(points) => Geometry::LineString(line(points)),
        Geometry::MultiPoint(points) => Geometry::MultiPoint(line(points)),
        Geometry::Polygon(rings) => Geometry::Polygon(rings.iter().map(line).collect()),
        Geometry::MultiLineString(lines) => Geometry::MultiLineString(lines.iter().map(line).collect()),
        Geometry::MultiPolygon(polygons) => Geometry::MultiPolygon(
            polygons
                .iter()
                .map(|rings| rings.iter().map(line).collect())
                .collect(),
        ),
        Geometry::Collection(items) => Geometry::Collection(items.iter().map(reversed).collect()),
        point => point.clone(),
    }
}

fn perpendicular(point: Coord, start: Coord, end: Coord) -> f64 {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let length = dx.hypot(dy);
    if length == 0.0 {
        return (point.x - start.x).hypot(point.y - start.y);
    }
    ((point.x - start.x) * dy - (point.y - start.y) * dx).abs() / length
}

fn douglas_peucker(points: &[Coord], tolerance: f64, keep: &mut Vec<bool>, from: usize, to: usize) {
    if to <= from + 1 {
        return;
    }
    let mut farthest = from;
    let mut distance = 0.0;
    for at in from + 1..to {
        let found = perpendicular(points[at], points[from], points[to]);
        if found > distance {
            distance = found;
            farthest = at;
        }
    }
    if distance > tolerance {
        keep[farthest] = true;
        douglas_peucker(points, tolerance, keep, from, farthest);
        douglas_peucker(points, tolerance, keep, farthest, to);
    }
}

fn simplified_line(points: &[Coord], tolerance: f64, minimum: usize) -> Vec<Coord> {
    if points.len() <= minimum {
        return points.to_vec();
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    let last = points.len() - 1;
    keep[last] = true;
    douglas_peucker(points, tolerance, &mut keep, 0, last);
    let kept: Vec<Coord> = points
        .iter()
        .zip(&keep)
        .filter(|(_, kept)| **kept)
        .map(|(point, _)| *point)
        .collect();
    if kept.len() < minimum {
        points.to_vec()
    } else {
        kept
    }
}

/// The geometry simplified by the Douglas-Peucker algorithm with `tolerance`; rings keep at least
/// four points, as PostGIS keeps them.
pub fn simplified(geometry: &Geometry, tolerance: f64) -> Geometry {
    let ring = |ring: &Vec<Coord>| simplified_line(ring, tolerance, 4);
    let line = |line: &Vec<Coord>| simplified_line(line, tolerance, 2);
    match geometry {
        Geometry::LineString(points) => Geometry::LineString(line(points)),
        Geometry::Polygon(rings) => Geometry::Polygon(rings.iter().map(ring).collect()),
        Geometry::MultiLineString(lines) => Geometry::MultiLineString(lines.iter().map(line).collect()),
        Geometry::MultiPolygon(polygons) => Geometry::MultiPolygon(
            polygons
                .iter()
                .map(|rings| rings.iter().map(ring).collect())
                .collect(),
        ),
        Geometry::Collection(items) => Geometry::Collection(
            items.iter().map(|item| simplified(item, tolerance)).collect(),
        ),
        other => other.clone(),
    }
}

/// The parts of a multi-geometry or collection, or the geometry itself as its only part.
pub fn parts(geometry: &Geometry) -> Vec<Geometry> {
    match geometry {
        Geometry::MultiPoint(points) => points.iter().map(|point| Geometry::Point(Some(*point))).collect(),
        Geometry::MultiLineString(lines) => lines.iter().cloned().map(Geometry::LineString).collect(),
        Geometry::MultiPolygon(polygons) => polygons.iter().cloned().map(Geometry::Polygon).collect(),
        Geometry::Collection(items) => items.clone(),
        single => vec![single.clone()],
    }
}
