use crate::{Coord, GeoError, Geometry, MAX_DEPTH, MAX_POINTS, Spatial};

const SRID_FLAG: u32 = 0x2000_0000;
const Z_FLAG: u32 = 0x8000_0000;
const M_FLAG: u32 = 0x4000_0000;

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
    little: bool,
    points: usize,
}

fn bad(message: &str) -> GeoError {
    GeoError(format!("invalid WKB: {message}"))
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], GeoError> {
        let raw = self.bytes.get(self.at..self.at + N).ok_or_else(|| bad("it ends early"))?;
        self.at += N;
        let mut out = [0u8; N];
        out.copy_from_slice(raw);
        Ok(out)
    }

    fn u32(&mut self) -> Result<u32, GeoError> {
        let raw = self.take::<4>()?;
        Ok(if self.little { u32::from_le_bytes(raw) } else { u32::from_be_bytes(raw) })
    }

    fn f64(&mut self) -> Result<f64, GeoError> {
        let raw = self.take::<8>()?;
        Ok(if self.little { f64::from_le_bytes(raw) } else { f64::from_be_bytes(raw) })
    }

    fn count(&mut self, each: usize) -> Result<usize, GeoError> {
        let count = self.u32()? as usize;
        if count.saturating_mul(each) > self.bytes.len() - self.at {
            return Err(bad("a count is larger than the bytes left"));
        }
        Ok(count)
    }

    fn coord(&mut self) -> Result<Coord, GeoError> {
        self.points += 1;
        if self.points > MAX_POINTS {
            return Err(bad("too many points"));
        }
        Ok(Coord { x: self.f64()?, y: self.f64()? })
    }

    fn coords(&mut self) -> Result<Vec<Coord>, GeoError> {
        let count = self.count(16)?;
        (0..count).map(|_| self.coord()).collect()
    }

    fn rings(&mut self) -> Result<Vec<Vec<Coord>>, GeoError> {
        let count = self.count(4)?;
        (0..count).map(|_| self.coords()).collect()
    }

    fn geometry(&mut self, depth: usize) -> Result<(Option<i32>, Geometry), GeoError> {
        if depth > MAX_DEPTH {
            return Err(bad("collections nested too deeply"));
        }
        self.little = match self.take::<1>()?[0] {
            0 => false,
            1 => true,
            _ => return Err(bad("unknown byte order")),
        };
        let kind = self.u32()?;
        if kind & (Z_FLAG | M_FLAG) != 0 || (kind & 0xffff) > 1000 {
            return Err(GeoError("only two-dimensional geometries are supported yet".to_owned()));
        }
        let srid = if kind & SRID_FLAG != 0 { Some(self.u32()? as i32) } else { None };
        let geometry = match kind & 0xff {
            1 => {
                let coord = self.coord()?;
                Geometry::Point((!(coord.x.is_nan() && coord.y.is_nan())).then_some(coord))
            }
            2 => Geometry::LineString(self.coords()?),
            3 => Geometry::Polygon(self.rings()?),
            4 | 5 | 6 | 7 => {
                let count = self.count(5)?;
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(self.geometry(depth + 1)?.1);
                }
                let wanted = kind & 0xff;
                match wanted {
                    4 => Geometry::MultiPoint(items.into_iter().map(|item| match item {
                        Geometry::Point(Some(coord)) => Ok(coord),
                        _ => Err(bad("a multipoint holds something other than points")),
                    }).collect::<Result<_, _>>()?),
                    5 => Geometry::MultiLineString(items.into_iter().map(|item| match item {
                        Geometry::LineString(line) => Ok(line),
                        _ => Err(bad("a multilinestring holds something other than line strings")),
                    }).collect::<Result<_, _>>()?),
                    6 => Geometry::MultiPolygon(items.into_iter().map(|item| match item {
                        Geometry::Polygon(rings) => Ok(rings),
                        _ => Err(bad("a multipolygon holds something other than polygons")),
                    }).collect::<Result<_, _>>()?),
                    _ => Geometry::Collection(items),
                }
            }
            _ => return Err(bad("unknown geometry type")),
        };
        Ok((srid, geometry))
    }
}

/// Reads WKB or PostGIS's EWKB (with an SRID).
pub fn from_wkb(bytes: &[u8]) -> Result<Spatial, GeoError> {
    let mut reader = Reader { bytes, at: 0, little: true, points: 0 };
    let (srid, geometry) = reader.geometry(0)?;
    if reader.at != bytes.len() {
        return Err(bad("bytes follow the geometry"));
    }
    Ok(Spatial { srid: srid.filter(|srid| *srid != 0), geometry })
}

fn put_coords(out: &mut Vec<u8>, coords: &[Coord]) {
    out.extend_from_slice(&(coords.len() as u32).to_le_bytes());
    for coord in coords {
        out.extend_from_slice(&coord.x.to_le_bytes());
        out.extend_from_slice(&coord.y.to_le_bytes());
    }
}

fn put_rings(out: &mut Vec<u8>, rings: &[Vec<Coord>]) {
    out.extend_from_slice(&(rings.len() as u32).to_le_bytes());
    for ring in rings {
        put_coords(out, ring);
    }
}

fn put(out: &mut Vec<u8>, geometry: &Geometry, srid: Option<i32>) {
    out.push(1);
    let kind: u32 = match geometry {
        Geometry::Point(_) => 1,
        Geometry::LineString(_) => 2,
        Geometry::Polygon(_) => 3,
        Geometry::MultiPoint(_) => 4,
        Geometry::MultiLineString(_) => 5,
        Geometry::MultiPolygon(_) => 6,
        Geometry::Collection(_) => 7,
    };
    out.extend_from_slice(&(kind | if srid.is_some() { SRID_FLAG } else { 0 }).to_le_bytes());
    if let Some(srid) = srid {
        out.extend_from_slice(&srid.to_le_bytes());
    }
    match geometry {
        Geometry::Point(point) => {
            let coord = point.unwrap_or(Coord { x: f64::NAN, y: f64::NAN });
            out.extend_from_slice(&coord.x.to_le_bytes());
            out.extend_from_slice(&coord.y.to_le_bytes());
        }
        Geometry::LineString(coords) => put_coords(out, coords),
        Geometry::Polygon(rings) => put_rings(out, rings),
        Geometry::MultiPoint(coords) => {
            out.extend_from_slice(&(coords.len() as u32).to_le_bytes());
            for coord in coords {
                put(out, &Geometry::Point(Some(*coord)), None);
            }
        }
        Geometry::MultiLineString(lines) => {
            out.extend_from_slice(&(lines.len() as u32).to_le_bytes());
            for line in lines {
                put(out, &Geometry::LineString(line.clone()), None);
            }
        }
        Geometry::MultiPolygon(polygons) => {
            out.extend_from_slice(&(polygons.len() as u32).to_le_bytes());
            for rings in polygons {
                put(out, &Geometry::Polygon(rings.clone()), None);
            }
        }
        Geometry::Collection(items) => {
            out.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for item in items {
                put(out, item, None);
            }
        }
    }
}

/// Writes little-endian WKB; with `extended`, EWKB carrying the SRID.
pub fn to_wkb(spatial: &Spatial, extended: bool) -> Vec<u8> {
    let mut out = Vec::new();
    put(&mut out, &spatial.geometry, if extended { spatial.srid } else { None });
    out
}
