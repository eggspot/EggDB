use crate::{Coord, Geometry};

/// A tile's bounds in the geometry's coordinates and its grid: `extent` cells a side and
/// `buffer` cells kept beyond each edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileGrid {
    /// The lowest x of the tile.
    pub min_x: f64,
    /// The lowest y of the tile.
    pub min_y: f64,
    /// The highest x of the tile.
    pub max_x: f64,
    /// The highest y of the tile.
    pub max_y: f64,
    /// Cells along each side of the tile.
    pub extent: u32,
    /// Cells kept outside each edge.
    pub buffer: u32,
}

impl TileGrid {
    fn cell(&self, coord: Coord) -> Coord {
        let extent = f64::from(self.extent);
        Coord {
            x: ((coord.x - self.min_x) * extent / (self.max_x - self.min_x)).round(),
            y: ((self.max_y - coord.y) * extent / (self.max_y - self.min_y)).round(),
        }
    }

    fn low(&self) -> f64 {
        -f64::from(self.buffer)
    }

    fn high(&self) -> f64 {
        f64::from(self.extent) + f64::from(self.buffer)
    }

    fn inside(&self, coord: Coord) -> bool {
        (self.low()..=self.high()).contains(&coord.x) && (self.low()..=self.high()).contains(&coord.y)
    }
}

fn deduplicated(coords: Vec<Coord>) -> Vec<Coord> {
    let mut out: Vec<Coord> = Vec::with_capacity(coords.len());
    for coord in coords {
        if out.last() != Some(&coord) {
            out.push(coord);
        }
    }
    out
}

fn clip_segment(grid: &TileGrid, from: Coord, to: Coord) -> Option<(Coord, Coord)> {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let (mut enter, mut leave) = (0.0f64, 1.0f64);
    for (p, q) in [
        (-dx, from.x - grid.low()),
        (dx, grid.high() - from.x),
        (-dy, from.y - grid.low()),
        (dy, grid.high() - from.y),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
            continue;
        }
        let t = q / p;
        if p < 0.0 {
            enter = enter.max(t);
        } else {
            leave = leave.min(t);
        }
        if enter > leave {
            return None;
        }
    }
    let at = |t: f64| Coord {
        x: from.x + t * dx,
        y: from.y + t * dy,
    };
    Some((at(enter), at(leave)))
}

fn clip_line(grid: &TileGrid, line: &[Coord]) -> Vec<Vec<Coord>> {
    let mut pieces: Vec<Vec<Coord>> = Vec::new();
    let mut current: Vec<Coord> = Vec::new();
    for pair in line.windows(2) {
        match clip_segment(grid, pair[0], pair[1]) {
            Some((start, end)) => {
                if current.last() != Some(&start) {
                    if current.len() >= 2 {
                        pieces.push(std::mem::take(&mut current));
                    }
                    current.clear();
                    current.push(start);
                }
                current.push(end);
            }
            None => {
                if current.len() >= 2 {
                    pieces.push(std::mem::take(&mut current));
                }
                current.clear();
            }
        }
    }
    if current.len() >= 2 {
        pieces.push(current);
    }
    pieces
}

fn clip_ring(grid: &TileGrid, ring: &[Coord]) -> Vec<Coord> {
    let edges: [(&dyn Fn(Coord) -> bool, &dyn Fn(Coord, Coord) -> Coord); 4] = [
        (&|c: Coord| c.x >= grid.low(), &|a: Coord, b: Coord| cross_x(a, b, grid.low())),
        (&|c: Coord| c.x <= grid.high(), &|a: Coord, b: Coord| cross_x(a, b, grid.high())),
        (&|c: Coord| c.y >= grid.low(), &|a: Coord, b: Coord| cross_y(a, b, grid.low())),
        (&|c: Coord| c.y <= grid.high(), &|a: Coord, b: Coord| cross_y(a, b, grid.high())),
    ];
    let mut points: Vec<Coord> = ring.to_vec();
    if points.len() > 1 && points.first() == points.last() {
        points.pop();
    }
    for (inside, crossing) in edges {
        if points.is_empty() {
            break;
        }
        let mut kept = Vec::with_capacity(points.len() + 4);
        for index in 0..points.len() {
            let current = points[index];
            let previous = points[(index + points.len() - 1) % points.len()];
            match (inside(previous), inside(current)) {
                (true, true) => kept.push(current),
                (true, false) => kept.push(crossing(previous, current)),
                (false, true) => {
                    kept.push(crossing(previous, current));
                    kept.push(current);
                }
                (false, false) => {}
            }
        }
        points = kept;
    }
    points
}

fn cross_x(a: Coord, b: Coord, x: f64) -> Coord {
    let t = (x - a.x) / (b.x - a.x);
    Coord { x, y: a.y + t * (b.y - a.y) }
}

fn cross_y(a: Coord, b: Coord, y: f64) -> Coord {
    let t = (y - a.y) / (b.y - a.y);
    Coord { x: a.x + t * (b.x - a.x), y }
}

fn signed_area(ring: &[Coord]) -> f64 {
    ring.windows(2).map(|pair| pair[0].x * pair[1].y - pair[1].x * pair[0].y).sum::<f64>() / 2.0
}

fn closed_ring(points: Vec<Coord>, exterior: bool) -> Option<Vec<Coord>> {
    let mut ring = deduplicated(points);
    if ring.len() > 1 && ring.first() == ring.last() {
        ring.pop();
    }
    if ring.len() < 3 {
        return None;
    }
    if let Some(first) = ring.first().copied() {
        ring.push(first);
    }
    let area = signed_area(&ring);
    if area == 0.0 {
        return None;
    }
    if (area > 0.0) != exterior {
        ring.reverse();
    }
    Some(ring)
}

fn tile_polygon(grid: &TileGrid, polygon: &[Vec<Coord>], clip: bool) -> Option<Vec<Vec<Coord>>> {
    let mut rings = Vec::with_capacity(polygon.len());
    for (index, ring) in polygon.iter().enumerate() {
        let cells: Vec<Coord> = ring.iter().map(|coord| grid.cell(*coord)).collect();
        let cells = if clip { clip_ring(grid, &cells) } else { cells };
        match closed_ring(cells, index == 0) {
            Some(ring) => rings.push(ring),
            None if index == 0 => return None,
            None => {}
        }
    }
    Some(rings)
}

fn tile_lines(grid: &TileGrid, line: &[Coord], clip: bool) -> Vec<Vec<Coord>> {
    let cells: Vec<Coord> = line.iter().map(|coord| grid.cell(*coord)).collect();
    let pieces = if clip { clip_line(grid, &cells) } else { vec![cells] };
    pieces
        .into_iter()
        .map(|piece| deduplicated(piece.into_iter().map(|coord| Coord { x: coord.x.round(), y: coord.y.round() }).collect()))
        .filter(|piece| piece.len() >= 2)
        .collect()
}

fn single_or_multi<T>(mut items: Vec<T>, single: fn(T) -> Geometry, multi: fn(Vec<T>) -> Geometry) -> Option<Geometry> {
    match items.len() {
        0 => None,
        1 => items.pop().map(single),
        _ => Some(multi(items)),
    }
}

/// A geometry in a vector tile's grid, as PostGIS's `ST_AsMVTGeom` returns it: coordinates
/// scaled to the tile's `extent` with y pointing down, snapped to whole cells, clipped to the
/// tile and its buffer when `clip`, polygons' exterior rings with positive area and holes with
/// negative (the Mapbox Vector Tile 2.1 winding); `None` when nothing is left.
pub fn tile_geometry(geometry: &Geometry, grid: &TileGrid, clip: bool) -> Option<Geometry> {
    if grid.max_x <= grid.min_x || grid.max_y <= grid.min_y || grid.extent == 0 {
        return None;
    }
    let points = |coords: &[Coord]| -> Vec<Coord> {
        deduplicated(
            coords
                .iter()
                .map(|coord| grid.cell(*coord))
                .filter(|coord| !clip || grid.inside(*coord))
                .collect(),
        )
    };
    match geometry {
        Geometry::Point(None) => None,
        Geometry::Point(Some(coord)) => single_or_multi(points(&[*coord]), |coord| Geometry::Point(Some(coord)), Geometry::MultiPoint),
        Geometry::MultiPoint(coords) => single_or_multi(points(coords), |coord| Geometry::Point(Some(coord)), Geometry::MultiPoint),
        Geometry::LineString(line) => single_or_multi(tile_lines(grid, line, clip), Geometry::LineString, Geometry::MultiLineString),
        Geometry::MultiLineString(lines) => single_or_multi(
            lines.iter().flat_map(|line| tile_lines(grid, line, clip)).collect(),
            Geometry::LineString,
            Geometry::MultiLineString,
        ),
        Geometry::Polygon(polygon) => single_or_multi(
            tile_polygon(grid, polygon, clip).into_iter().collect(),
            Geometry::Polygon,
            Geometry::MultiPolygon,
        ),
        Geometry::MultiPolygon(polygons) => single_or_multi(
            polygons.iter().filter_map(|polygon| tile_polygon(grid, polygon, clip)).collect(),
            Geometry::Polygon,
            Geometry::MultiPolygon,
        ),
        Geometry::Collection(members) => {
            let kept: Vec<Geometry> = members.iter().filter_map(|member| tile_geometry(member, grid, clip)).collect();
            single_or_multi(kept, |geometry| geometry, Geometry::Collection)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> TileGrid {
        TileGrid {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 100.0,
            max_y: 100.0,
            extent: 4096,
            buffer: 0,
        }
    }

    #[test]
    fn coordinates_scale_flip_and_clip() {
        let point = Geometry::Point(Some(Coord { x: 25.0, y: 75.0 }));
        assert_eq!(tile_geometry(&point, &grid(), true), Some(Geometry::Point(Some(Coord { x: 1024.0, y: 1024.0 }))));
        let outside = Geometry::Point(Some(Coord { x: 150.0, y: 50.0 }));
        assert_eq!(tile_geometry(&outside, &grid(), true), None);
        let line = Geometry::LineString(vec![Coord { x: -50.0, y: 50.0 }, Coord { x: 50.0, y: 50.0 }]);
        assert_eq!(
            tile_geometry(&line, &grid(), true),
            Some(Geometry::LineString(vec![Coord { x: 0.0, y: 2048.0 }, Coord { x: 2048.0, y: 2048.0 }]))
        );
        let square = Geometry::Polygon(vec![vec![
            Coord { x: -10.0, y: -10.0 },
            Coord { x: 50.0, y: -10.0 },
            Coord { x: 50.0, y: 50.0 },
            Coord { x: -10.0, y: 50.0 },
            Coord { x: -10.0, y: -10.0 },
        ]]);
        let Some(Geometry::Polygon(rings)) = tile_geometry(&square, &grid(), true) else {
            panic!("the square was dropped");
        };
        assert!(signed_area(&rings[0]) > 0.0);
        assert!(rings[0].iter().all(|coord| (0.0..=4096.0).contains(&coord.x) && (0.0..=4096.0).contains(&coord.y)));
    }
}
