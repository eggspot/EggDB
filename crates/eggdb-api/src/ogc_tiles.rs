//! OGC API - Tiles (OGC 20-057) for the OGC API's collections (D881, D906): the
//! `WebMercatorQuad` and `WorldCRS84Quad` tile matrix sets (OGC 17-083r4), each collection's
//! vector tilesets, and its tiles as Mapbox Vector Tiles built with `ST_AsMVT` and `ST_AsMVTGeom`.

use crate::ogc::{Collection, link};
use crate::request::{identifier, literal};
use crate::{Endpoint, failure, json, json_string, sql_failure};
use eggdb_http::Response;
use eggdb_sql::{KeyValueStore, Value};

const MVT: &str = "application/vnd.mapbox-vector-tile";
const MAX_ZOOM: u32 = 24;
const EXTENT: u32 = 4096;
const BUFFER: u32 = 64;
const MERCATOR_HALF_WIDTH: f64 = 20_037_508.342_789_244;
const STANDARD_PIXEL_METERS: f64 = 0.000_28;
const METERS_PER_DEGREE: f64 = 6_378_137.0 * std::f64::consts::PI / 180.0;
const TILE_PIXELS: u32 = 256;

/// A tile matrix set the API offers (OGC 17-083r4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatrixSet {
    WebMercatorQuad,
    WorldCrs84Quad,
}

impl MatrixSet {
    pub(crate) const ALL: [Self; 2] = [Self::WebMercatorQuad, Self::WorldCrs84Quad];

    pub(crate) fn named(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|set| set.id() == id)
    }

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::WebMercatorQuad => "WebMercatorQuad",
            Self::WorldCrs84Quad => "WorldCRS84Quad",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::WebMercatorQuad => "Google Maps Compatible for the World",
            Self::WorldCrs84Quad => "CRS84 for the World",
        }
    }

    fn uri(self) -> String {
        format!("http://www.opengis.net/def/tilematrixset/OGC/1.0/{}", self.id())
    }

    fn crs(self) -> &'static str {
        match self {
            Self::WebMercatorQuad => "http://www.opengis.net/def/crs/EPSG/0/3857",
            Self::WorldCrs84Quad => "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        }
    }

    fn srid(self) -> u32 {
        match self {
            Self::WebMercatorQuad => 3857,
            Self::WorldCrs84Quad => 4326,
        }
    }

    fn columns(self, zoom: u32) -> u64 {
        match self {
            Self::WebMercatorQuad => 1u64 << zoom,
            Self::WorldCrs84Quad => 2u64 << zoom,
        }
    }

    fn rows(self, zoom: u32) -> u64 {
        1u64 << zoom
    }

    fn cell_size(self, zoom: u32) -> f64 {
        match self {
            Self::WebMercatorQuad => 2.0 * MERCATOR_HALF_WIDTH / (f64::from(TILE_PIXELS) * f64::from(1u32 << zoom)),
            Self::WorldCrs84Quad => 180.0 / (f64::from(TILE_PIXELS) * f64::from(1u32 << zoom)),
        }
    }

    fn scale_denominator(self, zoom: u32) -> f64 {
        let meters = match self {
            Self::WebMercatorQuad => self.cell_size(zoom),
            Self::WorldCrs84Quad => self.cell_size(zoom) * METERS_PER_DEGREE,
        };
        meters / STANDARD_PIXEL_METERS
    }

    fn origin(self) -> (f64, f64) {
        match self {
            Self::WebMercatorQuad => (-MERCATOR_HALF_WIDTH, MERCATOR_HALF_WIDTH),
            Self::WorldCrs84Quad => (-180.0, 90.0),
        }
    }

    fn tile_matrix(self, zoom: u32) -> String {
        let (x, y) = self.origin();
        format!(
            "{{\"id\":\"{zoom}\",\"scaleDenominator\":{},\"cellSize\":{},\"cornerOfOrigin\":\"topLeft\",\"pointOfOrigin\":[{x},{y}],\"tileWidth\":{TILE_PIXELS},\"tileHeight\":{TILE_PIXELS},\"matrixWidth\":{},\"matrixHeight\":{}}}",
            self.scale_denominator(zoom),
            self.cell_size(zoom),
            self.columns(zoom),
            self.rows(zoom)
        )
    }

    fn envelope_sql(self, zoom: u32, row: u32, column: u32) -> String {
        match self {
            Self::WebMercatorQuad => format!("ST_TileEnvelope({zoom}, {column}, {row})"),
            Self::WorldCrs84Quad => {
                let size = 180.0 / f64::from(1u32 << zoom);
                let west = -180.0 + f64::from(column) * size;
                let north = 90.0 - f64::from(row) * size;
                format!("ST_MakeEnvelope({west}, {}, {}, {north}, 4326)", north - size, west + size)
            }
        }
    }

    fn axes(self) -> &'static str {
        match self {
            Self::WebMercatorQuad => "[\"E\",\"N\"]",
            Self::WorldCrs84Quad => "[\"Lon\",\"Lat\"]",
        }
    }

    fn scale_set(self) -> &'static str {
        match self {
            Self::WebMercatorQuad => "http://www.opengis.net/def/wkss/OGC/1.0/GoogleMapsCompatible",
            Self::WorldCrs84Quad => "http://www.opengis.net/def/wkss/OGC/1.0/GoogleCRS84Quad",
        }
    }
}

pub(crate) const CONFORMANCE: [&str; 5] = [
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/core",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/tileset",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/tilesets-list",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/geodata-tilesets",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/mvt",
];

/// A tile matrix set, in OGC 17-083r4's JSON encoding.
pub(crate) fn matrix_set_json(set: MatrixSet) -> String {
    let matrices: Vec<String> = (0..=MAX_ZOOM).map(|zoom| set.tile_matrix(zoom)).collect();
    format!(
        "{{\"id\":\"{}\",\"title\":\"{}\",\"uri\":\"{}\",\"crs\":\"{}\",\"orderedAxes\":{},\"wellKnownScaleSet\":\"{}\",\"tileMatrices\":[{}]}}",
        set.id(),
        set.title(),
        set.uri(),
        set.crs(),
        set.axes(),
        set.scale_set(),
        matrices.join(",")
    )
}

/// The tile matrix sets the API offers.
pub(crate) fn matrix_sets_json() -> String {
    let listed: Vec<String> = MatrixSet::ALL
        .iter()
        .map(|set| {
            format!(
                "{{\"id\":\"{}\",\"title\":\"{}\",\"uri\":\"{}\",\"links\":[{}]}}",
                set.id(),
                set.title(),
                set.uri(),
                link(&format!("/ogc/tileMatrixSets/{}", set.id()), "self", "application/json")
            )
        })
        .collect();
    format!("{{\"tileMatrixSets\":[{}]}}", listed.join(","))
}

fn tile_coordinates(set: MatrixSet, zoom: &str, row: &str, column: &str) -> Option<(u32, u32, u32)> {
    let zoom: u32 = zoom.parse().ok().filter(|zoom| *zoom <= MAX_ZOOM)?;
    let row: u32 = row.parse().ok().filter(|row| u64::from(*row) < set.rows(zoom))?;
    let column: u32 = column.trim_end_matches(".mvt").parse().ok().filter(|column| u64::from(*column) < set.columns(zoom))?;
    Some((zoom, row, column))
}

impl<S: KeyValueStore> Endpoint<'_, S> {
    pub(crate) fn tilesets(&self, collection: &Collection) -> Response {
        let base = format!("/ogc/collections/{}/tiles", collection.table);
        let listed: Vec<String> = MatrixSet::ALL
            .iter()
            .map(|set| {
                format!(
                    "{{\"title\":{},\"dataType\":\"vector\",\"crs\":\"{}\",\"tileMatrixSetURI\":\"{}\",\"links\":[{},{}]}}",
                    json_string(&collection.table),
                    set.crs(),
                    set.uri(),
                    link(&format!("{base}/{}", set.id()), "self", "application/json"),
                    link(&format!("/ogc/tileMatrixSets/{}", set.id()), "http://www.opengis.net/def/rel/ogc/1.0/tiling-scheme", "application/json")
                )
            })
            .collect();
        json(200, format!("{{\"tilesets\":[{}]}}", listed.join(",")))
    }

    pub(crate) fn tileset(&self, collection: &Collection, set: MatrixSet) -> Response {
        let base = format!("/ogc/collections/{}/tiles/{}", collection.table, set.id());
        let fields: Vec<String> = collection
            .columns
            .iter()
            .filter(|(column, _)| *column != collection.geometry)
            .map(|(column, udt)| format!("{}:{}", json_string(column), json_string(udt)))
            .collect();
        json(
            200,
            format!(
                "{{\"title\":{title},\"dataType\":\"vector\",\"crs\":\"{crs}\",\"tileMatrixSetURI\":\"{uri}\",\"layers\":[{{\"id\":{title},\"dataType\":\"vector\",\"propertiesSchema\":{{\"type\":\"object\",\"properties\":{{{}}}}}}}],\"links\":[{},{},{{\"href\":\"{base}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}\",\"rel\":\"item\",\"type\":\"{MVT}\",\"templated\":true}}]}}",
                fields.join(","),
                link(&base, "self", "application/json"),
                link(&format!("/ogc/tileMatrixSets/{}", set.id()), "http://www.opengis.net/def/rel/ogc/1.0/tiling-scheme", "application/json"),
                title = json_string(&collection.table),
                crs = set.crs(),
                uri = set.uri()
            ),
        )
    }

    /// One tile: the collection's features in the tile, clipped and scaled into it.
    pub(crate) fn tile(&self, collection: &Collection, set: MatrixSet, zoom: &str, row: &str, column: &str) -> Response {
        let Some((zoom, row, column)) = tile_coordinates(set, zoom, row, column) else {
            return failure(404, "OGC404", &format!("no such tile in {}", set.id()));
        };
        let (Some(table), Some(geometry)) = (identifier(&collection.table), identifier(&collection.geometry)) else {
            return failure(400, "OGC400", "the collection's name is not a plain identifier");
        };
        let properties: Vec<String> = collection
            .columns
            .iter()
            .filter(|(name, _)| *name != collection.geometry)
            .filter_map(|(name, _)| identifier(name).map(|quoted| format!("t.{quoted}")))
            .collect();
        let envelope = set.envelope_sql(zoom, row, column);
        let srid = set.srid();
        let selected = if properties.is_empty() { String::new() } else { format!(", {}", properties.join(", ")) };
        let sql = format!(
            "SELECT ST_AsMVT(q, {layer}, {EXTENT}, 'geom') FROM (SELECT ST_AsMVTGeom(ST_Transform(t.{geometry}, {srid}), {envelope}, {EXTENT}, {BUFFER}, true) AS geom{selected} FROM \"{schema}\".{table} t WHERE t.{geometry} IS NOT NULL AND ST_Intersects(t.{geometry}, ST_Transform({envelope}, ST_SRID(t.{geometry})))) q WHERE q.geom IS NOT NULL",
            layer = literal(&collection.table),
            schema = self.schema.replace('"', "\"\""),
        );
        match self.run(&sql) {
            Ok(results) => {
                let body = results
                    .iter()
                    .rev()
                    .find(|result| result.columns.is_some())
                    .and_then(|result| result.rows.first())
                    .and_then(|row| row.first())
                    .map(|value| match value {
                        Value::Bytea(bytes) => bytes.clone(),
                        _ => Vec::new(),
                    })
                    .unwrap_or_default();
                if body.is_empty() {
                    return Response {
                        status: 204,
                        content_type: MVT.to_owned(),
                        body,
                        headers: Vec::new(),
                    };
                }
                Response {
                    status: 200,
                    content_type: MVT.to_owned(),
                    body,
                    headers: Vec::new(),
                }
            }
            Err(error) => sql_failure(&error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_coordinates_stay_inside_the_matrix() {
        let mercator = MatrixSet::WebMercatorQuad;
        assert_eq!(tile_coordinates(mercator, "2", "1", "3"), Some((2, 1, 3)));
        assert_eq!(tile_coordinates(mercator, "2", "4", "0"), None);
        assert_eq!(tile_coordinates(mercator, "25", "0", "0"), None);
        assert_eq!(tile_coordinates(mercator, "1", "0", "1.mvt"), Some((1, 0, 1)));
        assert_eq!(tile_coordinates(mercator, "x", "0", "0"), None);
        assert_eq!(tile_coordinates(MatrixSet::WorldCrs84Quad, "0", "0", "1"), Some((0, 0, 1)));
        assert_eq!(tile_coordinates(MatrixSet::WorldCrs84Quad, "0", "1", "0"), None);
    }

    #[test]
    fn the_matrix_sets_list_every_zoom_level() {
        let set = matrix_set_json(MatrixSet::WebMercatorQuad);
        assert!(set.contains("\"id\":\"0\",\"scaleDenominator\":559082264.0287"), "{}", &set[..400.min(set.len())]);
        assert!(set.contains("\"id\":\"24\""));
        assert!(set.contains("\"matrixWidth\":16777216"));
        let crs84 = matrix_set_json(MatrixSet::WorldCrs84Quad);
        assert!(crs84.contains("\"id\":\"0\",\"scaleDenominator\":279541132.01435"), "{}", &crs84[..400.min(crs84.len())]);
        assert!(crs84.contains("\"cellSize\":0.703125"));
        assert!(crs84.contains("\"matrixWidth\":2,\"matrixHeight\":1"));
        assert_eq!(MatrixSet::named("WorldCRS84Quad"), Some(MatrixSet::WorldCrs84Quad));
        assert_eq!(MatrixSet::WorldCrs84Quad.envelope_sql(1, 0, 3), "ST_MakeEnvelope(90, 0, 180, 90, 4326)");
    }
}
