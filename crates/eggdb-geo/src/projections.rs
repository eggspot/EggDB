use crate::{Coord, GeoError, Geometry};

const WGS84_A: f64 = 6_378_137.0;
const WGS84_F: f64 = 1.0 / 298.257_223_563;
const GRS80_F: f64 = 1.0 / 298.257_222_101;
const UTM_SCALE: f64 = 0.9996;
const UTM_FALSE_EASTING: f64 = 500_000.0;
const UTM_SOUTH_FALSE_NORTHING: f64 = 10_000_000.0;
const MERCATOR_ITERATIONS: usize = 15;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Projection {
    Geographic,
    WebMercator,
    WorldMercator,
    TransverseMercator { flattening: f64, zone: u32, south: bool },
}

fn projection(srid: i32) -> Option<Projection> {
    Some(match srid {
        4326 | 4258 | 4269 | 4979 => Projection::Geographic,
        3857 | 900_913 => Projection::WebMercator,
        3395 => Projection::WorldMercator,
        32_601..=32_660 => Projection::TransverseMercator { flattening: WGS84_F, zone: (srid - 32_600) as u32, south: false },
        32_701..=32_760 => Projection::TransverseMercator { flattening: WGS84_F, zone: (srid - 32_700) as u32, south: true },
        25_828..=25_838 => Projection::TransverseMercator { flattening: GRS80_F, zone: (srid - 25_800) as u32, south: false },
        26_901..=26_923 => Projection::TransverseMercator { flattening: GRS80_F, zone: (srid - 26_900) as u32, south: false },
        _ => return None,
    })
}

/// Whether [`transform`] knows the coordinate system `srid`: geographic WGS 84, ETRS89 and NAD83
/// (4326, 4258, 4269, 4979), Web Mercator (3857), World Mercator (3395), and the UTM zones of
/// WGS 84 (32601–32660, 32701–32760), ETRS89 (25828–25838) and NAD83 (26901–26923).
pub fn knows_srid(srid: i32) -> bool {
    projection(srid).is_some()
}

fn meridian_arc(phi: f64, e2: f64) -> f64 {
    let e4 = e2 * e2;
    let e6 = e4 * e2;
    WGS84_A
        * ((1.0 - e2 / 4.0 - 3.0 * e4 / 64.0 - 5.0 * e6 / 256.0) * phi
            - (3.0 * e2 / 8.0 + 3.0 * e4 / 32.0 + 45.0 * e6 / 1024.0) * (2.0 * phi).sin()
            + (15.0 * e4 / 256.0 + 45.0 * e6 / 1024.0) * (4.0 * phi).sin()
            - (35.0 * e6 / 3072.0) * (6.0 * phi).sin())
}

fn central_meridian(zone: u32) -> f64 {
    (f64::from(zone) * 6.0 - 183.0).to_radians()
}

fn utm_forward(longitude: f64, latitude: f64, flattening: f64, zone: u32, south: bool) -> Coord {
    let e2 = flattening * (2.0 - flattening);
    let ep2 = e2 / (1.0 - e2);
    let (phi, lambda) = (latitude.to_radians(), longitude.to_radians());
    let n = WGS84_A / (1.0 - e2 * phi.sin().powi(2)).sqrt();
    let t = phi.tan().powi(2);
    let c = ep2 * phi.cos().powi(2);
    let a = (lambda - central_meridian(zone)) * phi.cos();
    let m = meridian_arc(phi, e2);
    let x = UTM_SCALE
        * n
        * (a + (1.0 - t + c) * a.powi(3) / 6.0 + (5.0 - 18.0 * t + t * t + 72.0 * c - 58.0 * ep2) * a.powi(5) / 120.0);
    let y = UTM_SCALE
        * (m + n
            * phi.tan()
            * (a * a / 2.0
                + (5.0 - t + 9.0 * c + 4.0 * c * c) * a.powi(4) / 24.0
                + (61.0 - 58.0 * t + t * t + 600.0 * c - 330.0 * ep2) * a.powi(6) / 720.0));
    Coord {
        x: x + UTM_FALSE_EASTING,
        y: if south { y + UTM_SOUTH_FALSE_NORTHING } else { y },
    }
}

fn utm_inverse(easting: f64, northing: f64, flattening: f64, zone: u32, south: bool) -> Coord {
    let e2 = flattening * (2.0 - flattening);
    let ep2 = e2 / (1.0 - e2);
    let e4 = e2 * e2;
    let e6 = e4 * e2;
    let x = easting - UTM_FALSE_EASTING;
    let m = (if south { northing - UTM_SOUTH_FALSE_NORTHING } else { northing }) / UTM_SCALE;
    let mu = m / (WGS84_A * (1.0 - e2 / 4.0 - 3.0 * e4 / 64.0 - 5.0 * e6 / 256.0));
    let e1 = (1.0 - (1.0 - e2).sqrt()) / (1.0 + (1.0 - e2).sqrt());
    let phi1 = mu
        + (3.0 * e1 / 2.0 - 27.0 * e1.powi(3) / 32.0) * (2.0 * mu).sin()
        + (21.0 * e1 * e1 / 16.0 - 55.0 * e1.powi(4) / 32.0) * (4.0 * mu).sin()
        + (151.0 * e1.powi(3) / 96.0) * (6.0 * mu).sin()
        + (1097.0 * e1.powi(4) / 512.0) * (8.0 * mu).sin();
    let c1 = ep2 * phi1.cos().powi(2);
    let t1 = phi1.tan().powi(2);
    let sin2 = phi1.sin().powi(2);
    let n1 = WGS84_A / (1.0 - e2 * sin2).sqrt();
    let r1 = WGS84_A * (1.0 - e2) / (1.0 - e2 * sin2).powf(1.5);
    let d = x / (n1 * UTM_SCALE);
    let phi = phi1
        - (n1 * phi1.tan() / r1)
            * (d * d / 2.0 - (5.0 + 3.0 * t1 + 10.0 * c1 - 4.0 * c1 * c1 - 9.0 * ep2) * d.powi(4) / 24.0
                + (61.0 + 90.0 * t1 + 298.0 * c1 + 45.0 * t1 * t1 - 252.0 * ep2 - 3.0 * c1 * c1) * d.powi(6) / 720.0);
    let lambda = central_meridian(zone)
        + (d - (1.0 + 2.0 * t1 + c1) * d.powi(3) / 6.0
            + (5.0 - 2.0 * c1 + 28.0 * t1 - 3.0 * c1 * c1 + 8.0 * ep2 + 24.0 * t1 * t1) * d.powi(5) / 120.0)
            / phi1.cos();
    Coord { x: lambda.to_degrees(), y: phi.to_degrees() }
}

fn world_mercator_forward(longitude: f64, latitude: f64) -> Coord {
    let e = (WGS84_F * (2.0 - WGS84_F)).sqrt();
    let phi = latitude.to_radians();
    let ratio = ((1.0 - e * phi.sin()) / (1.0 + e * phi.sin())).powf(e / 2.0);
    Coord {
        x: WGS84_A * longitude.to_radians(),
        y: WGS84_A * ((std::f64::consts::FRAC_PI_4 + phi / 2.0).tan() * ratio).ln(),
    }
}

fn world_mercator_inverse(x: f64, y: f64) -> Coord {
    let e = (WGS84_F * (2.0 - WGS84_F)).sqrt();
    let t = (-y / WGS84_A).exp();
    let mut phi = std::f64::consts::FRAC_PI_2 - 2.0 * t.atan();
    for _ in 0..MERCATOR_ITERATIONS {
        let ratio = ((1.0 - e * phi.sin()) / (1.0 + e * phi.sin())).powf(e / 2.0);
        phi = std::f64::consts::FRAC_PI_2 - 2.0 * (t * ratio).atan();
    }
    Coord { x: (x / WGS84_A).to_degrees(), y: phi.to_degrees() }
}

fn to_geographic(coord: Coord, from: Projection) -> Result<Coord, GeoError> {
    Ok(match from {
        Projection::Geographic => coord,
        Projection::WebMercator => {
            let latitude = 2.0 * (coord.y / crate::WEB_MERCATOR_RADIUS).exp().atan() - std::f64::consts::FRAC_PI_2;
            Coord { x: (coord.x / crate::WEB_MERCATOR_RADIUS).to_degrees(), y: latitude.to_degrees() }
        }
        Projection::WorldMercator => world_mercator_inverse(coord.x, coord.y),
        Projection::TransverseMercator { flattening, zone, south } => utm_inverse(coord.x, coord.y, flattening, zone, south),
    })
}

fn from_geographic(coord: Coord, to: Projection) -> Result<Coord, GeoError> {
    if !(coord.x.is_finite() && coord.y.is_finite() && coord.y.abs() <= 90.0) {
        return Err(GeoError(format!("POINT({} {}) is not a longitude and latitude", coord.x, coord.y)));
    }
    Ok(match to {
        Projection::Geographic => coord,
        Projection::WebMercator | Projection::WorldMercator if coord.y.abs() >= 89.999_999 => {
            return Err(GeoError("a pole has no Mercator coordinates".to_owned()));
        }
        Projection::WebMercator => Coord {
            x: crate::WEB_MERCATOR_RADIUS * coord.x.to_radians(),
            y: crate::WEB_MERCATOR_RADIUS * (std::f64::consts::FRAC_PI_4 + coord.y.to_radians() / 2.0).tan().ln(),
        },
        Projection::WorldMercator => world_mercator_forward(coord.x, coord.y),
        Projection::TransverseMercator { flattening, zone, south } => utm_forward(coord.x, coord.y, flattening, zone, south),
    })
}

/// `geometry` in the coordinate system `to` (an SRID [`knows_srid`] accepts), from `from`:
/// through longitude and latitude, with the datums of WGS 84, ETRS89 and NAD83 taken as the same.
pub fn transform(geometry: &Geometry, from: i32, to: i32) -> Result<Geometry, GeoError> {
    let (Some(source), Some(target)) = (projection(from), projection(to)) else {
        return Err(GeoError(format!("transforming from SRID {from} to {to} is not supported yet")));
    };
    crate::tiles::map_coords(geometry, &|coord| {
        let geographic = to_geographic(coord, source)?;
        let projected = from_geographic(geographic, target)?;
        if projected.x.is_finite() && projected.y.is_finite() {
            Ok(projected)
        } else {
            Err(GeoError(format!("POINT({} {}) has no coordinates in SRID {to}", coord.x, coord.y)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utm_projects_and_returns_to_the_same_place() {
        let origin = utm_forward(3.0, 0.0, WGS84_F, 31, false);
        assert!((origin.x - 500_000.0).abs() < 1e-6 && origin.y.abs() < 1e-6);
        let point = Coord { x: 106.7, y: 10.78 };
        let projected = from_geographic(point, Projection::TransverseMercator { flattening: WGS84_F, zone: 48, south: false }).unwrap();
        let back = to_geographic(projected, Projection::TransverseMercator { flattening: WGS84_F, zone: 48, south: false }).unwrap();
        assert!((back.x - point.x).abs() < 1e-7 && (back.y - point.y).abs() < 1e-7, "{back:?}");
        let mercator = world_mercator_forward(10.0, 50.0);
        let returned = world_mercator_inverse(mercator.x, mercator.y);
        assert!((returned.y - 50.0).abs() < 1e-9);
    }
}
