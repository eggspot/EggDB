use crate::{Coord, GeoError, Geometry};

const MOST_ITERATIONS: usize = 200;
const CONVERGED: f64 = 1e-12;

/// An ellipsoid of revolution: its semi-major axis in metres and its flattening.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spheroid {
    /// The equatorial radius in metres.
    pub semi_major: f64,
    /// The flattening, `(a - b) / a`.
    pub flattening: f64,
}

impl Spheroid {
    /// The WGS 84 ellipsoid GPS and SRID 4326 use.
    pub const WGS84: Self = Self {
        semi_major: 6_378_137.0,
        flattening: 1.0 / 298.257_223_563,
    };

    /// Reads PostGIS's spheroid text, `SPHEROID["WGS 84",6378137,298.257223563]`: a name, the
    /// semi-major axis and the inverse flattening (0 for a sphere).
    ///
    /// # Errors
    ///
    /// Text that is not in that form, or a semi-major axis that is not positive.
    pub fn parse(text: &str) -> Result<Self, GeoError> {
        let invalid = || GeoError(format!("{text} is not a spheroid such as SPHEROID[\"WGS 84\",6378137,298.257223563]"));
        let inside = text
            .trim()
            .strip_prefix("SPHEROID")
            .and_then(|rest| rest.trim().strip_prefix('['))
            .and_then(|rest| rest.trim().strip_suffix(']'))
            .ok_or_else(invalid)?;
        let mut parts = inside.rsplitn(3, ',');
        let inverse: f64 = parts.next().and_then(|part| part.trim().parse().ok()).ok_or_else(invalid)?;
        let semi_major: f64 = parts.next().and_then(|part| part.trim().parse().ok()).ok_or_else(invalid)?;
        if !(semi_major > 0.0 && semi_major.is_finite() && inverse >= 0.0 && inverse.is_finite()) {
            return Err(invalid());
        }
        Ok(Self {
            semi_major,
            flattening: if inverse == 0.0 { 0.0 } else { 1.0 / inverse },
        })
    }
}

/// The distance in metres along the ellipsoid between two longitude and latitude points in
/// degrees, by Vincenty's inverse formula.
///
/// # Errors
///
/// Nearly antipodal points, for which the formula does not converge.
pub fn coord_spheroid_distance(from: Coord, to: Coord, spheroid: Spheroid) -> Result<f64, GeoError> {
    let a = spheroid.semi_major;
    let f = spheroid.flattening;
    let b = a * (1.0 - f);
    let difference = (to.x - from.x).to_radians();
    let reduced_from = ((1.0 - f) * from.y.to_radians().tan()).atan();
    let reduced_to = ((1.0 - f) * to.y.to_radians().tan()).atan();
    let (sin_from, cos_from) = reduced_from.sin_cos();
    let (sin_to, cos_to) = reduced_to.sin_cos();
    let mut lambda = difference;
    for _ in 0..MOST_ITERATIONS {
        let (sin_lambda, cos_lambda) = lambda.sin_cos();
        let sin_sigma = ((cos_to * sin_lambda).powi(2) + (cos_from * sin_to - sin_from * cos_to * cos_lambda).powi(2)).sqrt();
        if sin_sigma == 0.0 {
            return Ok(0.0);
        }
        let cos_sigma = sin_from * sin_to + cos_from * cos_to * cos_lambda;
        let sigma = sin_sigma.atan2(cos_sigma);
        let sin_alpha = cos_from * cos_to * sin_lambda / sin_sigma;
        let cos_squared_alpha = 1.0 - sin_alpha * sin_alpha;
        let cos_two_sigma_m = if cos_squared_alpha == 0.0 {
            0.0
        } else {
            cos_sigma - 2.0 * sin_from * sin_to / cos_squared_alpha
        };
        let c = f / 16.0 * cos_squared_alpha * (4.0 + f * (4.0 - 3.0 * cos_squared_alpha));
        let previous = lambda;
        lambda = difference
            + (1.0 - c)
                * f
                * sin_alpha
                * (sigma + c * sin_sigma * (cos_two_sigma_m + c * cos_sigma * (-1.0 + 2.0 * cos_two_sigma_m.powi(2))));
        if (lambda - previous).abs() < CONVERGED {
            let u_squared = cos_squared_alpha * (a * a - b * b) / (b * b);
            let big_a = 1.0 + u_squared / 16384.0 * (4096.0 + u_squared * (-768.0 + u_squared * (320.0 - 175.0 * u_squared)));
            let big_b = u_squared / 1024.0 * (256.0 + u_squared * (-128.0 + u_squared * (74.0 - 47.0 * u_squared)));
            let delta_sigma = big_b
                * sin_sigma
                * (cos_two_sigma_m
                    + big_b / 4.0
                        * (cos_sigma * (-1.0 + 2.0 * cos_two_sigma_m.powi(2))
                            - big_b / 6.0 * cos_two_sigma_m * (-3.0 + 4.0 * sin_sigma.powi(2)) * (-3.0 + 4.0 * cos_two_sigma_m.powi(2))));
            return Ok(b * big_a * (sigma - delta_sigma));
        }
    }
    Err(GeoError("the points are nearly antipodal: the distance on the spheroid does not converge".to_owned()))
}

/// The distance in metres on `spheroid` between two longitude and latitude points
/// (`ST_DistanceSpheroid`).
///
/// # Errors
///
/// Arguments that are not two points, or nearly antipodal points.
pub fn spheroid_distance(left: &Geometry, right: &Geometry, spheroid: Spheroid) -> Result<f64, GeoError> {
    let (Geometry::Point(Some(a)), Geometry::Point(Some(b))) = (left, right) else {
        return Err(GeoError("ST_DistanceSpheroid takes two points".to_owned()));
    };
    coord_spheroid_distance(*a, *b, spheroid)
}

/// The length in metres on `spheroid` of the line strings of a geometry whose coordinates are
/// longitude and latitude (`ST_LengthSpheroid`); points and polygons have no length.
///
/// # Errors
///
/// A segment between nearly antipodal points.
pub fn spheroid_length(geometry: &Geometry, spheroid: Spheroid) -> Result<f64, GeoError> {
    let line = |coords: &[Coord]| -> Result<f64, GeoError> {
        coords
            .windows(2)
            .map(|pair| match pair {
                [from, to] => coord_spheroid_distance(*from, *to, spheroid),
                _ => Ok(0.0),
            })
            .sum()
    };
    match geometry {
        Geometry::LineString(coords) => line(coords),
        Geometry::MultiLineString(lines) => lines.iter().map(|coords| line(coords)).sum(),
        Geometry::Collection(parts) => parts.iter().map(|part| spheroid_length(part, spheroid)).sum(),
        _ => Ok(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn degrees(whole: f64, minutes: f64, seconds: f64) -> f64 {
        whole.signum() * (whole.abs() + minutes / 60.0 + seconds / 3600.0)
    }

    #[test]
    fn vincentys_own_example_and_the_equator() {
        let flinders_peak = Coord {
            x: degrees(144.0, 25.0, 29.524_40),
            y: degrees(-37.0, 57.0, 3.720_30),
        };
        let buninyong = Coord {
            x: degrees(143.0, 55.0, 35.383_90),
            y: degrees(-37.0, 39.0, 10.156_10),
        };
        let distance = coord_spheroid_distance(flinders_peak, buninyong, Spheroid::WGS84).unwrap();
        assert!((distance - 54_972.271).abs() < 0.01, "{distance}");
        let equator = coord_spheroid_distance(Coord { x: 0.0, y: 0.0 }, Coord { x: 1.0, y: 0.0 }, Spheroid::WGS84).unwrap();
        assert!((equator - 111_319.490_8).abs() < 0.001, "{equator}");
        assert_eq!(coord_spheroid_distance(buninyong, buninyong, Spheroid::WGS84).unwrap(), 0.0);
        assert!(coord_spheroid_distance(Coord { x: 0.0, y: 0.0 }, Coord { x: 179.7, y: 0.5 }, Spheroid::WGS84).is_err());
    }

    #[test]
    fn spheroids_parse_and_lines_sum_their_segments() {
        assert_eq!(Spheroid::parse("SPHEROID[\"WGS 84\",6378137,298.257223563]").unwrap(), Spheroid::WGS84);
        let sphere = Spheroid::parse("SPHEROID[\"sphere\",6371000,0]").unwrap();
        assert_eq!(sphere.flattening, 0.0);
        assert!(Spheroid::parse("ELLIPSOID[1,2]").is_err());
        assert!(Spheroid::parse("SPHEROID[\"x\",-1,300]").is_err());
        let line = Geometry::LineString(vec![Coord { x: 0.0, y: 0.0 }, Coord { x: 1.0, y: 0.0 }, Coord { x: 2.0, y: 0.0 }]);
        let length = spheroid_length(&line, Spheroid::WGS84).unwrap();
        assert!((length - 2.0 * 111_319.490_8).abs() < 0.01, "{length}");
        assert_eq!(spheroid_length(&Geometry::Point(None), Spheroid::WGS84).unwrap(), 0.0);
    }
}
