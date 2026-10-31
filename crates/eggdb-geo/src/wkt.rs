use crate::{Coord, GeoError, Geometry, MAX_DEPTH, MAX_POINTS, Spatial};
use std::fmt::Write;

struct Reader<'t> {
    text: &'t str,
    at: usize,
    points: usize,
}

impl Reader<'_> {
    fn fail<T>(&self, message: &str) -> Result<T, GeoError> {
        Err(GeoError(format!("parse error - invalid geometry: {message} at character {}", self.at)))
    }

    fn skip_space(&mut self) {
        while let Some(space) = self.text[self.at..].chars().next().filter(|character| character.is_whitespace()) {
            self.at += space.len_utf8();
        }
    }

    fn word(&mut self) -> String {
        self.skip_space();
        let rest = &self.text[self.at..];
        let length = rest.find(|character: char| !character.is_ascii_alphabetic()).unwrap_or(rest.len());
        self.at += length;
        rest[..length].to_ascii_uppercase()
    }

    fn symbol(&mut self, expected: char) -> bool {
        self.skip_space();
        if self.text[self.at..].starts_with(expected) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: char) -> Result<(), GeoError> {
        if self.symbol(expected) { Ok(()) } else { self.fail(&format!("expected '{expected}'")) }
    }

    fn number(&mut self) -> Result<f64, GeoError> {
        self.skip_space();
        let rest = &self.text[self.at..];
        let length = rest.find(|character: char| !(character.is_ascii_digit() || "+-.eE".contains(character))).unwrap_or(rest.len());
        let value: f64 = rest[..length].parse().or_else(|_| self.fail("expected a number"))?;
        if !value.is_finite() {
            return self.fail("expected a finite number");
        }
        self.at += length;
        Ok(value)
    }

    fn coord(&mut self) -> Result<Coord, GeoError> {
        self.points += 1;
        if self.points > MAX_POINTS {
            return self.fail("too many points");
        }
        let x = self.number()?;
        let y = self.number()?;
        Ok(Coord { x, y })
    }

    fn empty(&mut self) -> bool {
        let saved = self.at;
        if self.word() == "EMPTY" {
            true
        } else {
            self.at = saved;
            false
        }
    }

    fn coords(&mut self) -> Result<Vec<Coord>, GeoError> {
        if self.empty() {
            return Ok(Vec::new());
        }
        self.expect('(')?;
        let mut coords = Vec::new();
        loop {
            let parenthesized = self.symbol('(');
            coords.push(self.coord()?);
            if parenthesized {
                self.expect(')')?;
            }
            if !self.symbol(',') {
                break;
            }
        }
        self.expect(')')?;
        Ok(coords)
    }

    fn rings(&mut self) -> Result<Vec<Vec<Coord>>, GeoError> {
        if self.empty() {
            return Ok(Vec::new());
        }
        self.expect('(')?;
        let mut rings = vec![self.coords()?];
        while self.symbol(',') {
            rings.push(self.coords()?);
        }
        self.expect(')')?;
        Ok(rings)
    }

    fn geometry(&mut self, depth: usize) -> Result<Geometry, GeoError> {
        if depth > MAX_DEPTH {
            return self.fail("collections nested too deeply");
        }
        let kind = self.word();
        let saved = self.at;
        let dimension = self.word();
        if !dimension.is_empty() && dimension != "EMPTY" {
            return self.fail("only two-dimensional geometries are supported yet");
        }
        self.at = saved;
        Ok(match kind.as_str() {
            "POINT" => {
                if self.empty() {
                    Geometry::Point(None)
                } else {
                    self.expect('(')?;
                    let coord = self.coord()?;
                    self.expect(')')?;
                    Geometry::Point(Some(coord))
                }
            }
            "LINESTRING" => Geometry::LineString(self.coords()?),
            "POLYGON" => Geometry::Polygon(self.rings()?),
            "MULTIPOINT" => Geometry::MultiPoint(self.coords()?),
            "MULTILINESTRING" => Geometry::MultiLineString(self.rings()?),
            "MULTIPOLYGON" => {
                if self.empty() {
                    Geometry::MultiPolygon(Vec::new())
                } else {
                    self.expect('(')?;
                    let mut polygons = vec![self.rings()?];
                    while self.symbol(',') {
                        polygons.push(self.rings()?);
                    }
                    self.expect(')')?;
                    Geometry::MultiPolygon(polygons)
                }
            }
            "GEOMETRYCOLLECTION" => {
                if self.empty() {
                    Geometry::Collection(Vec::new())
                } else {
                    self.expect('(')?;
                    let mut items = vec![self.geometry(depth + 1)?];
                    while self.symbol(',') {
                        items.push(self.geometry(depth + 1)?);
                    }
                    self.expect(')')?;
                    Geometry::Collection(items)
                }
            }
            _ => return self.fail("unknown geometry type"),
        })
    }
}

/// Reads WKT, or EWKT with a `SRID=n;` prefix.
pub fn from_wkt(text: &str) -> Result<Spatial, GeoError> {
    let (srid, body) = match text.trim_start().strip_prefix("SRID=").or_else(|| text.trim_start().strip_prefix("srid=")) {
        Some(rest) => {
            let (number, body) = rest.split_once(';').ok_or_else(|| GeoError("parse error - invalid geometry: SRID without ';'".to_owned()))?;
            let srid: i32 = number.trim().parse().map_err(|_| GeoError("parse error - invalid geometry: bad SRID".to_owned()))?;
            (Some(srid).filter(|srid| *srid != 0), body)
        }
        None => (None, text),
    };
    let mut reader = Reader { text: body, at: 0, points: 0 };
    let geometry = reader.geometry(0)?;
    reader.skip_space();
    if reader.at != body.len() {
        return reader.fail("unexpected text after the geometry");
    }
    Ok(Spatial { srid, geometry })
}

fn write_coords(out: &mut String, coords: &[Coord]) {
    if coords.is_empty() {
        out.push_str(" EMPTY");
        return;
    }
    out.push('(');
    for (at, coord) in coords.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        let _ = write!(out, "{} {}", coord.x, coord.y);
    }
    out.push(')');
}

fn write_rings(out: &mut String, rings: &[Vec<Coord>]) {
    if rings.is_empty() {
        out.push_str(" EMPTY");
        return;
    }
    out.push('(');
    for (at, ring) in rings.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        write_coords(out, ring);
    }
    out.push(')');
}

fn write_geometry(out: &mut String, geometry: &Geometry) {
    match geometry {
        Geometry::Point(None) => out.push_str("POINT EMPTY"),
        Geometry::Point(Some(coord)) => {
            let _ = write!(out, "POINT({} {})", coord.x, coord.y);
        }
        Geometry::LineString(coords) => {
            out.push_str("LINESTRING");
            write_coords(out, coords);
        }
        Geometry::Polygon(rings) => {
            out.push_str("POLYGON");
            write_rings(out, rings);
        }
        Geometry::MultiPoint(coords) => {
            out.push_str("MULTIPOINT");
            write_coords(out, coords);
        }
        Geometry::MultiLineString(lines) => {
            out.push_str("MULTILINESTRING");
            write_rings(out, lines);
        }
        Geometry::MultiPolygon(polygons) => {
            out.push_str("MULTIPOLYGON");
            if polygons.is_empty() {
                out.push_str(" EMPTY");
                return;
            }
            out.push('(');
            for (at, rings) in polygons.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                write_rings(out, rings);
            }
            out.push(')');
        }
        Geometry::Collection(items) => {
            out.push_str("GEOMETRYCOLLECTION");
            if items.is_empty() {
                out.push_str(" EMPTY");
                return;
            }
            out.push('(');
            for (at, item) in items.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                write_geometry(out, item);
            }
            out.push(')');
        }
    }
}

/// Writes WKT, as `ST_AsText` does; with `extended`, EWKT with the SRID first.
pub fn to_wkt(spatial: &Spatial, extended: bool) -> String {
    let mut out = String::new();
    if extended && let Some(srid) = spatial.srid {
        let _ = write!(out, "SRID={srid};");
    }
    write_geometry(&mut out, &spatial.geometry);
    out
}
