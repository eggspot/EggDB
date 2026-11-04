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
    let latitude_offset =
        (point.latitude - latitude_min) / (latitude_max - latitude_min) * cells;
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
    let longitude_high = edge(longitude_cell.wrapping_add(1), LONGITUDE_MIN, longitude_scale);
    Point {
        longitude: ((longitude_low + longitude_high) / 2.0).clamp(LONGITUDE_MIN, LONGITUDE_MAX),
        latitude: ((latitude_low + latitude_high) / 2.0).clamp(LATITUDE_MIN, LATITUDE_MAX),
    }
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

pub(crate) fn distance_within_radius(center: Point, point: Point, radius_meters: f64) -> Option<f64> {
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
