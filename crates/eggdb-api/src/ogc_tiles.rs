//! OGC API - Tiles (OGC 20-057) for the OGC API's collections (D881): the `WebMercatorQuad`
//! tile matrix set (OGC 17-083r4), each collection's vector tileset, and its tiles as
//! Mapbox Vector Tiles built with `ST_AsMVT` and `ST_AsMVTGeom`.

use crate::ogc::{Collection, link};
use crate::request::{identifier, literal};
use crate::{Endpoint, failure, json, json_string, sql_failure};
use eggdb_http::Response;
use eggdb_sql::{KeyValueStore, Value};

pub(crate) const MATRIX_SET: &str = "WebMercatorQuad";
const MATRIX_SET_URI: &str = "http://www.opengis.net/def/tilematrixset/OGC/1.0/WebMercatorQuad";
const MVT: &str = "application/vnd.mapbox-vector-tile";
const MAX_ZOOM: u32 = 24;
const EXTENT: u32 = 4096;
const BUFFER: u32 = 64;
const ORIGIN: f64 = -20_037_508.342_789_244;
const STANDARD_PIXEL_METERS: f64 = 0.000_28;

pub(crate) const CONFORMANCE: [&str; 5] = [
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/core",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/tileset",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/tilesets-list",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/geodata-tilesets",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/mvt",
];

fn tile_matrix(zoom: u32) -> String {
    let tiles = 1u64 << zoom;
    let cell = 2.0 * -ORIGIN / (f64::from(EXTENT / 16) * f64::from(1u32 << zoom));
    format!(
        "{{\"id\":\"{zoom}\",\"scaleDenominator\":{},\"cellSize\":{cell},\"cornerOfOrigin\":\"topLeft\",\"pointOfOrigin\":[{ORIGIN},{}],\"tileWidth\":256,\"tileHeight\":256,\"matrixWidth\":{tiles},\"matrixHeight\":{tiles}}}",
        cell / STANDARD_PIXEL_METERS,
        -ORIGIN
    )
}

/// The `WebMercatorQuad` tile matrix set, in OGC 17-083r4's JSON encoding.
pub(crate) fn matrix_set_json() -> String {
    let matrices: Vec<String> = (0..=MAX_ZOOM).map(tile_matrix).collect();
    format!(
        "{{\"id\":\"{MATRIX_SET}\",\"title\":\"Google Maps Compatible for the World\",\"uri\":\"{MATRIX_SET_URI}\",\"crs\":\"http://www.opengis.net/def/crs/EPSG/0/3857\",\"orderedAxes\":[\"E\",\"N\"],\"wellKnownScaleSet\":\"http://www.opengis.net/def/wkss/OGC/1.0/GoogleMapsCompatible\",\"tileMatrices\":[{}]}}",
        matrices.join(",")
    )
}

/// The tile matrix sets the API offers.
pub(crate) fn matrix_sets_json() -> String {
    format!(
        "{{\"tileMatrixSets\":[{{\"id\":\"{MATRIX_SET}\",\"title\":\"Google Maps Compatible for the World\",\"uri\":\"{MATRIX_SET_URI}\",\"links\":[{}]}}]}}",
        link(&format!("/ogc/tileMatrixSets/{MATRIX_SET}"), "self", "application/json")
    )
}

fn tile_coordinates(zoom: &str, row: &str, column: &str) -> Option<(u32, u32, u32)> {
    let zoom: u32 = zoom.parse().ok().filter(|zoom| *zoom <= MAX_ZOOM)?;
    let size = 1u32.checked_shl(zoom)?;
    let row: u32 = row.parse().ok().filter(|row| *row < size)?;
    let column: u32 = column.trim_end_matches(".mvt").parse().ok().filter(|column| *column < size)?;
    Some((zoom, row, column))
}

impl<S: KeyValueStore> Endpoint<'_, S> {
    pub(crate) fn tilesets(&self, collection: &Collection) -> Response {
        let base = format!("/ogc/collections/{}/tiles", collection.table);
        json(
            200,
            format!(
                "{{\"tilesets\":[{{\"title\":{},\"dataType\":\"vector\",\"crs\":\"http://www.opengis.net/def/crs/EPSG/0/3857\",\"tileMatrixSetURI\":\"{MATRIX_SET_URI}\",\"links\":[{},{}]}}]}}",
                json_string(&collection.table),
                link(&format!("{base}/{MATRIX_SET}"), "self", "application/json"),
                link(&format!("/ogc/tileMatrixSets/{MATRIX_SET}"), "http://www.opengis.net/def/rel/ogc/1.0/tiling-scheme", "application/json")
            ),
        )
    }

    pub(crate) fn tileset(&self, collection: &Collection) -> Response {
        let base = format!("/ogc/collections/{}/tiles/{MATRIX_SET}", collection.table);
        let fields: Vec<String> = collection
            .columns
            .iter()
            .filter(|(column, _)| *column != collection.geometry)
            .map(|(column, udt)| format!("{}:{}", json_string(column), json_string(udt)))
            .collect();
        json(
            200,
            format!(
                "{{\"title\":{title},\"dataType\":\"vector\",\"crs\":\"http://www.opengis.net/def/crs/EPSG/0/3857\",\"tileMatrixSetURI\":\"{MATRIX_SET_URI}\",\"layers\":[{{\"id\":{title},\"dataType\":\"vector\",\"propertiesSchema\":{{\"type\":\"object\",\"properties\":{{{}}}}}}}],\"links\":[{},{},{{\"href\":\"{base}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}\",\"rel\":\"item\",\"type\":\"{MVT}\",\"templated\":true}}]}}",
                fields.join(","),
                link(&base, "self", "application/json"),
                link(&format!("/ogc/tileMatrixSets/{MATRIX_SET}"), "http://www.opengis.net/def/rel/ogc/1.0/tiling-scheme", "application/json"),
                title = json_string(&collection.table)
            ),
        )
    }

    /// One tile: the collection's features in the tile, clipped and scaled into it.
    pub(crate) fn tile(&self, collection: &Collection, zoom: &str, row: &str, column: &str) -> Response {
        let Some((zoom, row, column)) = tile_coordinates(zoom, row, column) else {
            return failure(404, "OGC404", "no such tile in WebMercatorQuad");
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
        let envelope = format!("ST_TileEnvelope({zoom}, {column}, {row})");
        let selected = if properties.is_empty() { String::new() } else { format!(", {}", properties.join(", ")) };
        let sql = format!(
            "SELECT ST_AsMVT(q, {layer}, {EXTENT}, 'geom') FROM (SELECT ST_AsMVTGeom(ST_Transform(t.{geometry}, 3857), {envelope}, {EXTENT}, {BUFFER}, true) AS geom{selected} FROM \"{schema}\".{table} t WHERE t.{geometry} IS NOT NULL AND ST_Intersects(t.{geometry}, ST_Transform({envelope}, ST_SRID(t.{geometry})))) q WHERE q.geom IS NOT NULL",
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
        assert_eq!(tile_coordinates("2", "1", "3"), Some((2, 1, 3)));
        assert_eq!(tile_coordinates("2", "4", "0"), None);
        assert_eq!(tile_coordinates("25", "0", "0"), None);
        assert_eq!(tile_coordinates("1", "0", "1.mvt"), Some((1, 0, 1)));
        assert_eq!(tile_coordinates("x", "0", "0"), None);
    }

    #[test]
    fn the_matrix_set_lists_every_zoom_level() {
        let set = matrix_set_json();
        assert!(set.contains("\"id\":\"0\",\"scaleDenominator\":559082264.0287"), "{}", &set[..400.min(set.len())]);
        assert!(set.contains("\"id\":\"24\""));
        assert!(set.contains("\"matrixWidth\":16777216"));
    }
}
