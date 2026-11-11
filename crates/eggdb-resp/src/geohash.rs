use std::f64::consts::PI;

pub(crate) const LONGITUDE_MIN: f64 = -180.0;
pub(crate) const LONGITUDE_MAX: f64 = 180.0;
pub(crate) const LATITUDE_MIN: f64 = -85.051_128_78;
pub(crate) const LATITUDE_MAX: f64 = 85.051_128_78;
const STANDARD_LATITUDE_MIN: f64 = -90.0;
const STANDARD_LATITUDE_MAX: f64 = 90.0;
const STEPS: u32 = 26;
const EARTH_RADIUS_METERS: f64 = 6_372_797.560_856;
const DEGREES_TO_RADIANS: f64 = PI / 180.0;
const BASE32: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";
const GEOHASH_CHARACTERS: usize = 11;
const MERCATOR_MAX_METERS: f64 = 20_037_726.37;
const BOUNDS_MARGIN: f64 = 1.01;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Point {
    pub(crate) longitude: f64,
    pub(crate) latitude: f64,
}

impl Point {
    pub(crate) fn is_indexable(self) -> bool {
        (LONGITUDE_MIN..=LONGITUDE_MAX).contains(&self.longitude)
            && (LATITUDE_MIN..=LATITUDE_MAX).contains(&self.latitude)
    }
}

fn spread(half: u32) -> u64 {
    let mut wide = u64::from(half);
    wide = (wide | (wide << 16)) & 0x0000_FFFF_0000_FFFF;
    wide = (wide | (wide << 8)) & 0x00FF_00FF_00FF_00FF;
    wide = (wide | (wide << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    wide = (wide | (wide << 2)) & 0x3333_3333_3333_3333;
    (wide | (wide << 1)) & 0x5555_5555_5555_5555
}

fn squeeze(interleaved: u64) -> u32 {
    let mut wide = interleaved & 0x5555_5555_5555_5555;
    wide = (wide | (wide >> 1)) & 0x3333_3333_3333_3333;
    wide = (wide | (wide >> 2)) & 0x0F0F_0F0F_0F0F_0F0F;
    wide = (wide | (wide >> 4)) & 0x00FF_00FF_00FF_00FF;
    wide = (wide | (wide >> 8)) & 0x0000_FFFF_0000_FFFF;
    ((wide | (wide >> 16)) & 0x0000_0000_FFFF_FFFF) as u32
}

fn interleaved_in(point: Point, latitude_min: f64, latitude_max: f64) -> u64 {
    let cells = (1_u64 << STEPS) as f64;
    let latitude_offset = (point.latitude - latitude_min) / (latitude_max - latitude_min) * cells;
    let longitude_offset =
        (point.longitude - LONGITUDE_MIN) / (LONGITUDE_MAX - LONGITUDE_MIN) * cells;
    spread(latitude_offset as u32) | (spread(longitude_offset as u32) << 1)
}

pub(crate) fn encode(point: Point) -> u64 {
    interleaved_in(point, LATITUDE_MIN, LATITUDE_MAX)
}

pub(crate) fn score_of(point: Point) -> f64 {
    encode(point) as f64
}

pub(crate) fn decode(bits: u64) -> Point {
    let cells = (1_u64 << STEPS) as f64;
    let latitude_cell = squeeze(bits);
    let longitude_cell = squeeze(bits >> 1);
    let latitude_scale = LATITUDE_MAX - LATITUDE_MIN;
    let longitude_scale = LONGITUDE_MAX - LONGITUDE_MIN;
    let edge = |cell: u32, min: f64, scale: f64| min + (f64::from(cell) / cells) * scale;
    let latitude_low = edge(latitude_cell, LATITUDE_MIN, latitude_scale);
    let latitude_high = edge(latitude_cell.wrapping_add(1), LATITUDE_MIN, latitude_scale);
    let longitude_low = edge(longitude_cell, LONGITUDE_MIN, longitude_scale);
    let longitude_high = edge(
        longitude_cell.wrapping_add(1),
        LONGITUDE_MIN,
        longitude_scale,
    );
    Point {
        longitude: ((longitude_low + longitude_high) / 2.0).clamp(LONGITUDE_MIN, LONGITUDE_MAX),
        latitude: ((latitude_low + latitude_high) / 2.0).clamp(LATITUDE_MIN, LATITUDE_MAX),
    }
}

fn estimate_steps(range_meters: f64, latitude: f64) -> u32 {
    if !(range_meters > 0.0) {
        return STEPS;
    }
    let mut step: i32 = 1;
    let mut range = range_meters;
    while range < MERCATOR_MAX_METERS {
        range *= 2.0;
        step += 1;
    }
    step -= 2;
    if latitude.abs() > 66.0 {
        step -= 1;
        if latitude.abs() > 80.0 {
            step -= 1;
        }
    }
    step.clamp(1, STEPS as i32) as u32
}

/// The score ranges of the geohash cell holding `center` and its eight neighbours, at the finest
/// step whose three-by-three block still covers the search area's bounding box; merged and
/// ascending, each `[low, high)`. `None` when the box reaches a pole or the antimeridian, where
/// the caller scans every member instead.
pub(crate) fn search_ranges(
    center: Point,
    half_width_meters: f64,
    half_height_meters: f64,
) -> Option<Vec<(u64, u64)>> {
    if !center.is_indexable() || !half_width_meters.is_finite() || !half_height_meters.is_finite() {
        return None;
    }
    let latitude_delta = half_height_meters / EARTH_RADIUS_METERS / DEGREES_TO_RADIANS * BOUNDS_MARGIN;
    let north = center.latitude + latitude_delta;
    let south = center.latitude - latitude_delta;
    if north >= LATITUDE_MAX || south <= LATITUDE_MIN {
        return None;
    }
    let widest = north.abs().max(south.abs()) * DEGREES_TO_RADIANS;
    let longitude_delta =
        half_width_meters / (EARTH_RADIUS_METERS * widest.cos()) / DEGREES_TO_RADIANS * BOUNDS_MARGIN;
    let east = center.longitude + longitude_delta;
    let west = center.longitude - longitude_delta;
    if !(east < LONGITUDE_MAX && west > LONGITUDE_MIN) {
        return None;
    }
    let mut step = estimate_steps(half_width_meters.hypot(half_height_meters), center.latitude);
    let (latitude_cell, longitude_cell) = loop {
        let cells = (1_u64 << step) as f64;
        let latitude_size = (LATITUDE_MAX - LATITUDE_MIN) / cells;
        let longitude_size = (LONGITUDE_MAX - LONGITUDE_MIN) / cells;
        let latitude_cell = ((center.latitude - LATITUDE_MIN) / latitude_size).floor().clamp(0.0, cells - 1.0);
        let longitude_cell = ((center.longitude - LONGITUDE_MIN) / longitude_size).floor().clamp(0.0, cells - 1.0);
        let covered = LATITUDE_MIN + (latitude_cell - 1.0) * latitude_size <= south
            && LATITUDE_MIN + (latitude_cell + 2.0) * latitude_size >= north
            && LONGITUDE_MIN + (longitude_cell - 1.0) * longitude_size <= west
            && LONGITUDE_MIN + (longitude_cell + 2.0) * longitude_size >= east;
        if covered {
            break (latitude_cell as i64, longitude_cell as i64);
        }
        if step <= 1 {
            return None;
        }
        step -= 1;
    };
    let cells = 1_i64 << step;
    let shift = 2 * (STEPS - step);
    let mut ranges = Vec::with_capacity(9);
    for latitude in latitude_cell - 1..=latitude_cell + 1 {
        for longitude in longitude_cell - 1..=longitude_cell + 1 {
            if !(0..cells).contains(&latitude) || !(0..cells).contains(&longitude) {
                continue;
            }
            let hash = spread(latitude as u32) | (spread(longitude as u32) << 1);
            ranges.push((hash << shift, (hash + 1) << shift));
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (low, high) in ranges {
        match merged.last_mut() {
            Some(last) if last.1 >= low => last.1 = last.1.max(high),
            _ => merged.push((low, high)),
        }
    }
    Some(merged)
}

pub(crate) fn decode_score(score: f64) -> Point {
    decode(score as u64)
}

fn latitude_distance(from_latitude: f64, to_latitude: f64) -> f64 {
    EARTH_RADIUS_METERS
        * (to_latitude * DEGREES_TO_RADIANS - from_latitude * DEGREES_TO_RADIANS).abs()
}

pub(crate) fn distance_meters(from: Point, to: Point) -> f64 {
    let from_longitude = from.longitude * DEGREES_TO_RADIANS;
    let to_longitude = to.longitude * DEGREES_TO_RADIANS;
    let v = ((to_longitude - from_longitude) / 2.0).sin();
    if v == 0.0 {
        return latitude_distance(from.latitude, to.latitude);
    }
    let from_latitude = from.latitude * DEGREES_TO_RADIANS;
    let to_latitude = to.latitude * DEGREES_TO_RADIANS;
    let u = ((to_latitude - from_latitude) / 2.0).sin();
    let a = u * u + from_latitude.cos() * to_latitude.cos() * v * v;
    2.0 * EARTH_RADIUS_METERS * a.sqrt().asin()
}

pub(crate) fn distance_within_radius(
    center: Point,
    point: Point,
    radius_meters: f64,
) -> Option<f64> {
    let distance = distance_meters(center, point);
    (distance <= radius_meters).then_some(distance)
}

pub(crate) fn distance_within_box(
    center: Point,
    point: Point,
    width_meters: f64,
    height_meters: f64,
) -> Option<f64> {
    if latitude_distance(point.latitude, center.latitude) > height_meters / 2.0 {
        return None;
    }
    let same_latitude = Point {
        longitude: center.longitude,
        latitude: point.latitude,
    };
    if distance_meters(point, same_latitude) > width_meters / 2.0 {
        return None;
    }
    Some(distance_meters(center, point))
}

pub(crate) fn standard_geohash(score: f64) -> String {
    let bits = interleaved_in(
        decode_score(score),
        STANDARD_LATITUDE_MIN,
        STANDARD_LATITUDE_MAX,
    );
    (0..GEOHASH_CHARACTERS)
        .map(|index| {
            let symbol = if index + 1 == GEOHASH_CHARACTERS {
                0
            } else {
                (bits >> (52 - (index + 1) * 5)) & 0x1F
            };
            char::from(BASE32.get(symbol as usize).copied().unwrap_or(b'0'))
        })
        .collect()
}
