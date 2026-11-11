use crate::{Coord, Geometry};

const MOVE_TO: u32 = 1;
const LINE_TO: u32 = 2;
const CLOSE_PATH: u32 = 7;
const POINT: u64 = 1;
const LINESTRING: u64 = 2;
const POLYGON: u64 = 3;
const VARINT: u32 = 0;
const FIXED64: u32 = 1;
const LENGTH_DELIMITED: u32 = 2;

/// A property value of a vector tile feature.
#[derive(Debug, Clone, PartialEq)]
pub enum MvtValue {
    /// A string.
    String(String),
    /// A floating-point number.
    Double(f64),
    /// A signed whole number.
    Int(i64),
    /// A boolean.
    Bool(bool),
}

/// One feature of a vector tile layer: its id, its geometry in tile grid coordinates, and its
/// properties.
#[derive(Debug, Clone, PartialEq)]
pub struct MvtFeature {
    /// The feature's id, if it has one.
    pub id: Option<u64>,
    /// The geometry, in the tile's grid (as `ST_AsMVTGeom` returns it).
    pub geometry: Geometry,
    /// The properties, by name.
    pub properties: Vec<(String, MvtValue)>,
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn tag(out: &mut Vec<u8>, field: u32, wire: u32) {
    varint(out, u64::from((field << 3) | wire));
}

fn bytes_field(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    tag(out, field, LENGTH_DELIMITED);
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    tag(out, field, VARINT);
    varint(out, value);
}

fn packed_field(out: &mut Vec<u8>, field: u32, values: &[u32]) {
    if values.is_empty() {
        return;
    }
    let mut packed = Vec::new();
    for value in values {
        varint(&mut packed, u64::from(*value));
    }
    bytes_field(out, field, &packed);
}

fn zigzag(value: i64) -> u32 {
    ((value << 1) ^ (value >> 63)) as u32
}

fn command(id: u32, count: usize) -> u32 {
    (id & 0x7) | ((count as u32) << 3)
}

struct Cursor {
    x: i64,
    y: i64,
    commands: Vec<u32>,
}

impl Cursor {
    fn grid(coord: &Coord) -> (i64, i64) {
        (coord.x.round() as i64, coord.y.round() as i64)
    }

    fn step(&mut self, coord: &Coord) {
        let (x, y) = Self::grid(coord);
        self.commands.push(zigzag(x - self.x));
        self.commands.push(zigzag(y - self.y));
        self.x = x;
        self.y = y;
    }

    fn points(&mut self, points: &[Coord]) {
        if points.is_empty() {
            return;
        }
        self.commands.push(command(MOVE_TO, points.len()));
        for point in points {
            self.step(point);
        }
    }

    fn line(&mut self, line: &[Coord]) {
        if line.len() < 2 {
            return;
        }
        self.commands.push(command(MOVE_TO, 1));
        self.step(&line[0]);
        self.commands.push(command(LINE_TO, line.len() - 1));
        for point in &line[1..] {
            self.step(point);
        }
    }

    fn ring(&mut self, ring: &[Coord]) {
        let open = match ring {
            [first, .., last] if first == last => &ring[..ring.len() - 1],
            _ => ring,
        };
        if open.len() < 3 {
            return;
        }
        self.commands.push(command(MOVE_TO, 1));
        self.step(&open[0]);
        self.commands.push(command(LINE_TO, open.len() - 1));
        for point in &open[1..] {
            self.step(point);
        }
        self.commands.push(command(CLOSE_PATH, 1));
    }
}

fn encoded_geometry(geometry: &Geometry) -> Option<(u64, Vec<u32>)> {
    let mut cursor = Cursor {
        x: 0,
        y: 0,
        commands: Vec::new(),
    };
    let kind = match geometry {
        Geometry::Point(Some(point)) => {
            cursor.points(std::slice::from_ref(point));
            POINT
        }
        Geometry::MultiPoint(points) => {
            cursor.points(points);
            POINT
        }
        Geometry::LineString(line) => {
            cursor.line(line);
            LINESTRING
        }
        Geometry::MultiLineString(lines) => {
            for line in lines {
                cursor.line(line);
            }
            LINESTRING
        }
        Geometry::Polygon(rings) => {
            for ring in rings {
                cursor.ring(ring);
            }
            POLYGON
        }
        Geometry::MultiPolygon(polygons) => {
            for ring in polygons.iter().flatten() {
                cursor.ring(ring);
            }
            POLYGON
        }
        Geometry::Point(None) | Geometry::Collection(_) => return None,
    };
    (!cursor.commands.is_empty()).then_some((kind, cursor.commands))
}

fn encoded_value(value: &MvtValue) -> Vec<u8> {
    let mut out = Vec::new();
    match value {
        MvtValue::String(text) => bytes_field(&mut out, 1, text.as_bytes()),
        MvtValue::Double(number) => {
            tag(&mut out, 3, FIXED64);
            out.extend_from_slice(&number.to_le_bytes());
        }
        MvtValue::Int(number) => varint_field(&mut out, 4, *number as u64),
        MvtValue::Bool(flag) => varint_field(&mut out, 7, u64::from(*flag)),
    }
    out
}

/// One Mapbox Vector Tile 2.1 layer, encoded as a tile holding only it (`ST_AsMVT`'s output):
/// the features whose geometry encodes, their properties as shared key and value tables, and
/// the grid's `extent`.
pub fn encode_layer(name: &str, extent: u32, features: &[MvtFeature]) -> Vec<u8> {
    let mut keys: Vec<String> = Vec::new();
    let mut values: Vec<Vec<u8>> = Vec::new();
    let mut layer = Vec::new();
    varint_field(&mut layer, 15, 2);
    bytes_field(&mut layer, 1, name.as_bytes());
    for feature in features {
        let Some((kind, geometry)) = encoded_geometry(&feature.geometry) else {
            continue;
        };
        let mut tags = Vec::new();
        for (key, value) in &feature.properties {
            let key_at = match keys.iter().position(|known| known == key) {
                Some(at) => at,
                None => {
                    keys.push(key.clone());
                    keys.len() - 1
                }
            };
            let encoded = encoded_value(value);
            let value_at = match values.iter().position(|known| *known == encoded) {
                Some(at) => at,
                None => {
                    values.push(encoded);
                    values.len() - 1
                }
            };
            tags.push(key_at as u32);
            tags.push(value_at as u32);
        }
        let mut encoded = Vec::new();
        if let Some(id) = feature.id {
            varint_field(&mut encoded, 1, id);
        }
        packed_field(&mut encoded, 2, &tags);
        varint_field(&mut encoded, 3, kind);
        packed_field(&mut encoded, 4, &geometry);
        bytes_field(&mut layer, 2, &encoded);
    }
    for key in &keys {
        bytes_field(&mut layer, 3, key.as_bytes());
    }
    for value in &values {
        bytes_field(&mut layer, 4, value);
    }
    varint_field(&mut layer, 5, u64::from(extent));
    let mut tile = Vec::new();
    bytes_field(&mut tile, 3, &layer);
    tile
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_feature_encodes_as_the_specification_example() {
        let tile = encode_layer(
            "points",
            4096,
            &[MvtFeature {
                id: Some(1),
                geometry: Geometry::Point(Some(Coord { x: 25.0, y: 17.0 })),
                properties: vec![("name".to_owned(), MvtValue::String("a".to_owned()))],
            }],
        );
        let feature_geometry = [0x22, 0x03, 0x09, 0x32, 0x22];
        assert!(tile.windows(feature_geometry.len()).any(|window| window == feature_geometry), "{tile:02x?}");
        assert_eq!(tile[0], 0x1a);
        let empty = encode_layer("none", 4096, &[]);
        assert!(!empty.is_empty());
    }

    #[test]
    fn polygons_drop_the_closing_point_and_close_the_path() {
        let square = Geometry::Polygon(vec![vec![
            Coord { x: 3.0, y: 6.0 },
            Coord { x: 8.0, y: 12.0 },
            Coord { x: 20.0, y: 34.0 },
            Coord { x: 3.0, y: 6.0 },
        ]]);
        let (kind, commands) = encoded_geometry(&square).unwrap();
        assert_eq!(kind, POLYGON);
        assert_eq!(commands, vec![9, 6, 12, 18, 10, 12, 24, 44, 15]);
    }
}
