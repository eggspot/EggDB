//! Geometry for EggDB (D204), written from the OGC Simple Features specification (WKT and WKB),
//! PostGIS's documented EWKT and EWKB extensions and RFC 7946 (GeoJSON): two-dimensional points,
//! line strings, polygons, their multi forms and collections, with planar measures and
//! predicates and a great-circle distance for longitude and latitude.

mod algorithms;
mod geojson;
mod wkb;
mod wkt;

pub use algorithms::{area, contains, distance, envelope, intersects, length, sphere_distance};
pub use geojson::to_geojson;
pub use wkb::{from_wkb, to_wkb};
pub use wkt::{from_wkt, to_wkt};

/// The deepest nesting of collections read.
pub const MAX_DEPTH: usize = 32;
/// The most coordinates a geometry may hold.
pub const MAX_POINTS: usize = 1 << 22;

/// A position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coord {
    /// The x (longitude) value.
    pub x: f64,
    /// The y (latitude) value.
    pub y: f64,
}

/// A geometry of the Simple Features model.
#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    /// A point, or the empty point.
    Point(Option<Coord>),
    /// A line string.
    LineString(Vec<Coord>),
    /// A polygon: its outer ring, then its holes.
    Polygon(Vec<Vec<Coord>>),
    /// Points.
    MultiPoint(Vec<Coord>),
    /// Line strings.
    MultiLineString(Vec<Vec<Coord>>),
    /// Polygons.
    MultiPolygon(Vec<Vec<Vec<Coord>>>),
    /// Any geometries.
    Collection(Vec<Geometry>),
}

/// A geometry with its spatial reference id, as EWKT and EWKB carry it.
#[derive(Debug, Clone, PartialEq)]
pub struct Spatial {
    /// The spatial reference id, such as 4326, or `None` (PostGIS's 0).
    pub srid: Option<i32>,
    /// The geometry.
    pub geometry: Geometry,
}

/// Why a geometry could not be read or combined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeoError(pub String);

impl std::fmt::Display for GeoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GeoError {}

impl Geometry {
    /// The type's name as `ST_GeometryType` writes it, such as `ST_Polygon`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Geometry::Point(_) => "ST_Point",
            Geometry::LineString(_) => "ST_LineString",
            Geometry::Polygon(_) => "ST_Polygon",
            Geometry::MultiPoint(_) => "ST_MultiPoint",
            Geometry::MultiLineString(_) => "ST_MultiLineString",
            Geometry::MultiPolygon(_) => "ST_MultiPolygon",
            Geometry::Collection(_) => "ST_GeometryCollection",
        }
    }

    /// Every position, in order.
    pub fn coords(&self) -> Vec<Coord> {
        match self {
            Geometry::Point(point) => point.iter().copied().collect(),
            Geometry::LineString(line) | Geometry::MultiPoint(line) => line.clone(),
            Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => rings.concat(),
            Geometry::MultiPolygon(polygons) => polygons.iter().flat_map(|rings| rings.concat()).collect(),
            Geometry::Collection(items) => items.iter().flat_map(Geometry::coords).collect(),
        }
    }

    /// Whether the geometry holds no position.
    pub fn is_empty(&self) -> bool {
        self.coords().is_empty()
    }
}

#[cfg(test)]
mod tests;
