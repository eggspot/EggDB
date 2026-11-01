use crate::{Coord, GeoError, Geometry};

/// The radius of the sphere Web Mercator (EPSG:3857) projects, in metres.
pub const WEB_MERCATOR_RADIUS: f64 = 6_378_137.0;
const HALF_WORLD: f64 = WEB_MERCATOR_RADIUS * std::f64::consts::PI;
const MAX_LATITUDE: f64 = 85.051_128_779_806_6;
const MAX_ZOOM: u32 = 30;
const GEOHASH_ALPHABET: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";

fn map_coords(
    geometry: &Geometry,
    map: &dyn Fn(Coord) -> Result<Coord, GeoError>,
) -> Result<Geometry, GeoError> {
    let line = |coords: &Vec<Coord>| {
        coords
            .iter()
            .map(|coord| map(*coord))
            .collect::<Result<Vec<_>, _>>()
    };
    let rings = |rings: &Vec<Vec<Coord>>| rings.iter().map(line).collect::<Result<Vec<_>, _>>();
    Ok(match geometry {
        Geometry::Point(coord) => Geometry::Point(coord.map(map).transpose()?),
        Geometry::LineString(coords) => Geometry::LineString(line(coords)?),
        Geometry::MultiPoint(coords) => Geometry::MultiPoint(line(coords)?),
        Geometry::Polygon(polygon) => Geometry::Polygon(rings(polygon)?),
        Geometry::MultiLineString(lines) => Geometry::MultiLineString(rings(lines)?),
        Geometry::MultiPolygon(polygons) => {
            Geometry::MultiPolygon(polygons.iter().map(rings).collect::<Result<Vec<_>, _>>()?)
        }
        Geometry::Collection(members) => Geometry::Collection(
            members
                .iter()
                .map(|member| map_coords(member, map))
                .collect::<Result<Vec<_>, _>>()?,
        ),
    })
}

/// Projects longitude and latitude (EPSG:4326) onto Web Mercator (EPSG:3857); latitudes beyond
/// ±85.0511° have no Web Mercator coordinates.
pub fn to_web_mercator(geometry: &Geometry) -> Result<Geometry, GeoError> {
    map_coords(geometry, &|coord| {
        if !(coord.y.abs() <= MAX_LATITUDE && coord.x.abs() <= 180.0) {
            return Err(GeoError(format!(
                "POINT({} {}) is outside Web Mercator's longitudes ±180 and latitudes ±85.0511",
                coord.x, coord.y
            )));
        }
        let latitude = coord.y.to_radians();
        Ok(Coord {
            x: WEB_MERCATOR_RADIUS * coord.x.to_radians(),
            y: WEB_MERCATOR_RADIUS * (std::f64::consts::FRAC_PI_4 + latitude / 2.0).tan().ln(),
        })
    })
}

/// Projects Web Mercator (EPSG:3857) back to longitude and latitude (EPSG:4326).
pub fn from_web_mercator(geometry: &Geometry) -> Result<Geometry, GeoError> {
    map_coords(geometry, &|coord| {
        if !(coord.x.is_finite() && coord.y.is_finite()) {
            return Err(GeoError(
                "a Web Mercator coordinate is not finite".to_owned(),
            ));
        }
        let latitude =
            2.0 * (coord.y / WEB_MERCATOR_RADIUS).exp().atan() - std::f64::consts::FRAC_PI_2;
        Ok(Coord {
            x: (coord.x / WEB_MERCATOR_RADIUS).to_degrees(),
            y: latitude.to_degrees(),
        })
    })
}

/// The Web Mercator square of map tile `x`, `y` at `zoom` (the XYZ scheme web maps use, with
/// y growing southwards), as a polygon.
pub fn tile_envelope(zoom: u32, x: u64, y: u64) -> Result<Geometry, GeoError> {
    if zoom > MAX_ZOOM {
        return Err(GeoError(format!("tile zoom {zoom} is above {MAX_ZOOM}")));
    }
    let tiles = 1u64 << zoom;
    if x >= tiles || y >= tiles {
        return Err(GeoError(format!(
            "tile {x}/{y} is outside zoom {zoom}, which has {tiles} tiles a side"
        )));
    }
    let size = 2.0 * HALF_WORLD / tiles as f64;
    let (west, north) = (-HALF_WORLD + x as f64 * size, HALF_WORLD - y as f64 * size);
    let (east, south) = (west + size, north - size);
    Ok(Geometry::Polygon(vec![vec![
        Coord { x: west, y: south },
        Coord { x: west, y: north },
        Coord { x: east, y: north },
        Coord { x: east, y: south },
        Coord { x: west, y: south },
    ]]))
}

/// The geohash of a longitude and latitude, `precision` characters long (1 to 20).
pub fn geohash(coord: Coord, precision: usize) -> Result<String, GeoError> {
    if !(1..=20).contains(&precision) {
        return Err(GeoError(format!(
            "geohash precision {precision} is not from 1 to 20"
        )));
    }
    if !(coord.x.abs() <= 180.0 && coord.y.abs() <= 90.0) {
        return Err(GeoError(format!(
            "POINT({} {}) is not a longitude and latitude",
            coord.x, coord.y
        )));
    }
    let (mut longitude, mut latitude) = ((-180.0f64, 180.0f64), (-90.0f64, 90.0f64));
    let mut out = String::with_capacity(precision);
    let mut even = true;
    for _ in 0..precision {
        let mut index = 0usize;
        for _ in 0..5 {
            let (range, value) = if even {
                (&mut longitude, coord.x)
            } else {
                (&mut latitude, coord.y)
            };
            let middle = (range.0 + range.1) / 2.0;
            index <<= 1;
            if value >= middle {
                index |= 1;
                range.0 = middle;
            } else {
                range.1 = middle;
            }
            even = !even;
        }
        out.push(char::from(GEOHASH_ALPHABET[index]));
    }
    Ok(out)
}
