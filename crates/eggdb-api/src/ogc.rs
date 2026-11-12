use crate::request::{identifier, literal, percent_decode};
use crate::{Endpoint, failure, json, json_string, sql_failure};
use eggdb_http::{Request, Response};
use eggdb_sql::{KeyValueStore, Value};

const DEFAULT_LIMIT: u64 = 10;
const MOST_LIMIT: u64 = 10_000;
const GEOJSON: &str = "application/geo+json";
const GEOMETRY_NAMES: [&str; 3] = ["geom", "geometry", "the_geom"];
const CONFORMANCE: [&str; 14] = [
    "http://www.opengis.net/spec/ogcapi-features-2/1.0/conf/crs",
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core",
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson",
    "http://www.opengis.net/spec/ogcapi-features-3/1.0/conf/filter",
    "http://www.opengis.net/spec/ogcapi-features-3/1.0/conf/features-filter",
    "http://www.opengis.net/spec/ogcapi-features-3/1.0/conf/queryables",
    "http://www.opengis.net/spec/cql2/1.0/conf/basic-cql2",
    "http://www.opengis.net/spec/cql2/1.0/conf/advanced-comparison-operators",
    "http://www.opengis.net/spec/cql2/1.0/conf/case-insensitive-comparison",
    "http://www.opengis.net/spec/cql2/1.0/conf/basic-spatial-functions",
    "http://www.opengis.net/spec/cql2/1.0/conf/spatial-functions",
    "http://www.opengis.net/spec/cql2/1.0/conf/temporal-functions",
    "http://www.opengis.net/spec/cql2/1.0/conf/cql2-text",
    "http://www.opengis.net/spec/cql2/1.0/conf/cql2-json",
];
const CRS84_URI: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
const EPSG_PREFIX: &str = "http://www.opengis.net/def/crs/EPSG/0/";
const LISTED_EPSG: [u32; 3] = [4326, 3857, 3395];

/// A CRS URI as EggDB's SRID and axis order: `CRS84` is longitude first, EPSG's geographic
/// systems latitude first, and projected systems easting first. Only the systems
/// `ST_Transform` knows are accepted.
fn crs_of(uri: &str) -> Option<crate::cql2::LiteralCrs> {
    let uri = uri.trim().trim_start_matches('[').trim_end_matches(']');
    let uri = uri.strip_prefix("https://").map_or_else(|| uri.to_owned(), |rest| format!("http://{rest}"));
    if uri == CRS84_URI {
        return Some(crate::cql2::LiteralCrs::CRS84);
    }
    let code: u32 = uri.strip_prefix(EPSG_PREFIX)?.parse().ok()?;
    let geographic = matches!(code, 4326 | 4258 | 4269);
    let known = geographic
        || matches!(code, 3857 | 3395 | 32601..=32660 | 32701..=32760 | 25828..=25838 | 26901..=26923);
    known.then_some(crate::cql2::LiteralCrs {
        srid: code,
        flipped: geographic,
    })
}

fn crs_uri(crs: crate::cql2::LiteralCrs) -> String {
    if crs == crate::cql2::LiteralCrs::CRS84 {
        CRS84_URI.to_owned()
    } else {
        format!("{EPSG_PREFIX}{}", crs.srid)
    }
}

fn listed_crs() -> String {
    let mut listed = vec![json_string(CRS84_URI)];
    listed.extend(LISTED_EPSG.iter().map(|code| json_string(&format!("{EPSG_PREFIX}{code}"))));
    listed.join(",")
}

pub(crate) struct Collection {
    pub(crate) table: String,
    pub(crate) geometry: String,
    id: Option<String>,
    pub(crate) columns: Vec<(String, String)>,
}

fn decoded_parameter(request: &Request, name: &str) -> Option<String> {
    request.query.as_deref().unwrap_or_default().split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (percent_decode(key) == name).then(|| percent_decode(value))
    })
}

fn percent_encoded(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn json_schema_type(udt: &str) -> &'static str {
    match udt {
        "int2" | "int4" | "int8" => "integer",
        "float4" | "float8" | "numeric" => "number",
        "bool" => "boolean",
        _ => "string",
    }
}

fn parameter(request: &Request, name: &str) -> Option<String> {
    request.query.as_deref().unwrap_or_default().split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.replace("%2C", ",").replace("%2c", ",").replace('+', " "))
    })
}

pub(crate) fn link(href: &str, rel: &str, kind: &str) -> String {
    format!("{{\"href\":{},\"rel\":{},\"type\":{}}}", json_string(href), json_string(rel), json_string(kind))
}

impl<S: KeyValueStore> Endpoint<'_, S> {
    fn collections(&self) -> Result<Vec<Collection>, Response> {
        let sql = format!(
            "SELECT table_name, column_name, udt_name FROM information_schema.columns WHERE table_schema = {} ORDER BY table_name, ordinal_position",
            literal(&self.schema)
        );
        let results = self.run(&sql).map_err(|error| sql_failure(&error))?;
        let mut found: Vec<Collection> = Vec::new();
        let mut all: Vec<(String, String, String)> = Vec::new();
        for row in results.iter().filter(|result| result.columns.is_some()).flat_map(|result| result.rows.iter()) {
            let text = |at: usize| row.get(at).and_then(Value::to_text).unwrap_or_default();
            let (table, column, udt) = (text(0), text(1), text(2));
            let is_geometry = matches!(udt.as_str(), "geometry" | "geography") || GEOMETRY_NAMES.contains(&column.as_str());
            if is_geometry && !found.iter().any(|collection| collection.table == table) {
                found.push(Collection {
                    table: table.clone(),
                    geometry: column.clone(),
                    id: None,
                    columns: Vec::new(),
                });
            }
            all.push((table, column, udt));
        }
        for collection in &mut found {
            collection.columns = all
                .iter()
                .filter(|(table, _, _)| *table == collection.table)
                .map(|(_, column, udt)| (column.clone(), udt.clone()))
                .collect();
            collection.id = collection.columns.iter().find(|(column, _)| column == "id").map(|(column, _)| column.clone());
        }
        Ok(found)
    }

    fn collection_json(&self, collection: &Collection) -> String {
        let base = format!("/ogc/collections/{}", collection.table);
        format!(
            "{{\"id\":{},\"title\":{},\"itemType\":\"feature\",\"crs\":[{}],\"storageCrs\":{},\"links\":[{},{}]}}",
            json_string(&collection.table),
            json_string(&collection.table),
            listed_crs(),
            json_string(CRS84_URI),
            link(&base, "self", "application/json"),
            link(&format!("{base}/items"), "items", GEOJSON)
        )
    }

    fn items(&self, collection: &Collection, request: &Request, one: Option<&str>) -> Response {
        let (Some(table), Some(geometry)) = (identifier(&collection.table), identifier(&collection.geometry)) else {
            return failure(400, "OGC400", "the collection's name is not a plain identifier");
        };
        let limit = parameter(request, "limit").and_then(|text| text.parse::<u64>().ok()).unwrap_or(DEFAULT_LIMIT).clamp(1, MOST_LIMIT);
        let offset = parameter(request, "offset").and_then(|text| text.parse::<u64>().ok()).unwrap_or(0);
        let mut conditions = vec![format!("t.{geometry} IS NOT NULL")];
        let output_crs = match decoded_parameter(request, "crs") {
            None => crate::cql2::LiteralCrs::CRS84,
            Some(uri) => match crs_of(&uri) {
                Some(crs) => crs,
                None => return failure(400, "OGC400", &format!("crs {uri} is not supported")),
            },
        };
        if let Some(bbox) = parameter(request, "bbox") {
            let bbox_crs = match decoded_parameter(request, "bbox-crs") {
                None => crate::cql2::LiteralCrs::CRS84,
                Some(uri) => match crs_of(&uri) {
                    Some(crs) => crs,
                    None => return failure(400, "OGC400", &format!("bbox-crs {uri} is not supported")),
                },
            };
            let corners: Vec<f64> = bbox.split(',').filter_map(|part| part.trim().parse().ok()).collect();
            let [first_low, second_low, first_high, second_high] = corners.as_slice() else {
                return failure(400, "OGC400", "bbox is four numbers: west,south,east,north");
            };
            if [first_low, second_low, first_high, second_high].iter().any(|value| !value.is_finite()) {
                return failure(400, "OGC400", "bbox holds a number that is not finite");
            }
            let (west, south, east, north) = if bbox_crs.flipped {
                (second_low, first_low, second_high, first_high)
            } else {
                (first_low, second_low, first_high, second_high)
            };
            let envelope = format!("ST_MakeEnvelope({west}, {south}, {east}, {north}, {})", bbox_crs.srid);
            let envelope = if bbox_crs == crate::cql2::LiteralCrs::CRS84 {
                format!("ST_MakeEnvelope({west}, {south}, {east}, {north}, ST_SRID(t.{geometry}))")
            } else {
                format!("ST_Transform({envelope}, ST_SRID(t.{geometry}))")
            };
            conditions.push(format!("ST_Intersects(t.{geometry}, {envelope})"));
        }
        let filter = match self.filter_condition(collection, request) {
            Ok(filter) => filter,
            Err(message) => return failure(400, "OGC400", &message),
        };
        conditions.extend(filter);
        let mut kept = String::new();
        for name in ["bbox", "bbox-crs", "crs", "filter", "filter-lang", "filter-crs"] {
            if let Some(value) = decoded_parameter(request, name) {
                kept.push_str(&format!("&{name}={}", percent_encoded(&value)));
            }
        }
        let id_column = collection.id.as_deref().and_then(identifier);
        if let Some(wanted) = one {
            let Some(id) = &id_column else {
                return failure(404, "OGC404", "this collection has no id column");
            };
            conditions.push(format!("t.{id}::text = {}", literal(wanted)));
        }
        let id_expression = id_column.as_ref().map_or_else(|| "NULL".to_owned(), |id| format!("t.{id}::text"));
        let output = if output_crs == crate::cql2::LiteralCrs::CRS84 {
            format!("t.{geometry}")
        } else {
            let transformed = format!("ST_Transform(t.{geometry}, {})", output_crs.srid);
            if output_crs.flipped { format!("ST_FlipCoordinates({transformed})") } else { transformed }
        };
        let order = id_column.as_ref().map_or_else(String::new, |id| format!(" ORDER BY t.{id}"));
        let sql = format!(
            "SELECT ST_AsGeoJSON({output}) AS geometry, (to_jsonb(t) - {}) AS properties, {id_expression} AS id FROM \"{}\".{table} t WHERE {}{order} LIMIT {} OFFSET {offset}",
            literal(&collection.geometry),
            self.schema.replace('"', "\"\""),
            conditions.join(" AND "),
            if one.is_some() { 1 } else { limit + 1 }
        );
        let results = match self.run(&sql) {
            Ok(results) => results,
            Err(error) => return sql_failure(&error),
        };
        let rows: Vec<&Vec<Value>> = results.iter().filter(|result| result.columns.is_some()).flat_map(|result| result.rows.iter()).collect();
        let feature = |row: &Vec<Value>| {
            let text = |at: usize| row.get(at).and_then(Value::to_text);
            format!(
                "{{\"type\":\"Feature\",\"id\":{},\"geometry\":{},\"properties\":{}}}",
                text(2).map_or_else(|| "null".to_owned(), |id| json_string(&id)),
                text(0).unwrap_or_else(|| "null".to_owned()),
                text(1).unwrap_or_else(|| "{}".to_owned())
            )
        };
        let mut response = if one.is_some() {
            match rows.first() {
                Some(row) => json(200, feature(row)),
                None => return failure(404, "OGC404", "no feature has this id"),
            }
        } else {
            let more = rows.len() as u64 > limit;
            let shown: Vec<String> = rows.iter().take(limit as usize).map(|row| feature(row)).collect();
            let base = format!("/ogc/collections/{}/items", collection.table);
            let mut links = vec![link(&format!("{base}?limit={limit}&offset={offset}{kept}"), "self", GEOJSON)];
            if more {
                links.push(link(&format!("{base}?limit={limit}&offset={}{kept}", offset + limit), "next", GEOJSON));
            }
            json(
                200,
                format!(
                    "{{\"type\":\"FeatureCollection\",\"numberReturned\":{},\"features\":[{}],\"links\":[{}]}}",
                    shown.len(),
                    shown.join(","),
                    links.join(",")
                ),
            )
        };
        response.content_type = GEOJSON.to_owned();
        response.headers.push(("Content-Crs".to_owned(), format!("<{}>", crs_uri(output_crs))));
        response
    }

    fn filter_condition(&self, collection: &Collection, request: &Request) -> Result<Option<String>, String> {
        let Some(text) = decoded_parameter(request, "filter") else {
            return Ok(None);
        };
        let crs = match decoded_parameter(request, "filter-crs") {
            None => crate::cql2::LiteralCrs::CRS84,
            Some(uri) => crs_of(&uri).ok_or_else(|| format!("filter-crs {uri} is not supported"))?,
        };
        let columns = |name: &str| -> Result<String, String> {
            let column = if (name == "geometry" || name == "geom")
                && !collection.columns.iter().any(|(column, _)| column == name)
            {
                collection.geometry.as_str()
            } else {
                collection
                    .columns
                    .iter()
                    .find(|(column, _)| column == name)
                    .map(|(column, _)| column.as_str())
                    .ok_or_else(|| format!("'{name}' is not a queryable of {}", collection.table))?
            };
            identifier(column)
                .map(|quoted| format!("t.{quoted}"))
                .ok_or_else(|| format!("'{name}' cannot be queried"))
        };
        let language = decoded_parameter(request, "filter-lang").unwrap_or_else(|| "cql2-text".to_owned());
        match language.as_str() {
            "cql2-text" => crate::cql2::text_filter(&text, &columns, crs).map(Some),
            "cql2-json" => {
                let document = eggdb_sql::json::parse_document(&text).map_err(|_| "the filter is not JSON".to_owned())?;
                crate::cql2::json_filter(&document, &columns, crs).map(Some)
            }
            other => Err(format!("filter-lang {other} is not supported; use cql2-text or cql2-json")),
        }
    }

    fn queryables(&self, collection: &Collection) -> Response {
        let mut properties: Vec<String> = Vec::new();
        for (column, udt) in &collection.columns {
            let schema = if *column == collection.geometry {
                "{\"$ref\":\"https://geojson.org/schema/Geometry.json\"}".to_owned()
            } else {
                format!("{{\"type\":\"{}\"}}", json_schema_type(udt))
            };
            properties.push(format!("{}:{schema}", json_string(column)));
        }
        let mut response = json(
            200,
            format!(
                "{{\"$schema\":\"https://json-schema.org/draft/2020-12/schema\",\"$id\":{},\"type\":\"object\",\"title\":{},\"properties\":{{{}}},\"additionalProperties\":false}}",
                json_string(&format!("/ogc/collections/{}/queryables", collection.table)),
                json_string(&collection.table),
                properties.join(",")
            ),
        );
        response.content_type = "application/schema+json".to_owned();
        response
    }

    pub(crate) fn ogc(&self, rest: &str, request: &Request) -> Response {
        if request.method != "GET" {
            return failure(405, "OGC405", "OGC API - Features is read with GET");
        }
        let parts: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
        if parts.is_empty() {
            return json(200, format!(
                "{{\"title\":\"EggDB\",\"links\":[{},{},{},{}]}}",
                link("/ogc", "self", "application/json"),
                link("/ogc/conformance", "conformance", "application/json"),
                link("/ogc/collections", "data", "application/json"),
                link("/ogc/tileMatrixSets", "http://www.opengis.net/def/rel/ogc/1.0/tiling-schemes", "application/json")
            ));
        }
        if parts == ["conformance"] {
            let classes: Vec<String> = CONFORMANCE
                .iter()
                .chain(crate::ogc_tiles::CONFORMANCE.iter())
                .map(|class| json_string(class))
                .collect();
            return json(200, format!("{{\"conformsTo\":[{}]}}", classes.join(",")));
        }
        if parts == ["tileMatrixSets"] {
            return json(200, crate::ogc_tiles::matrix_sets_json());
        }
        if parts == ["tileMatrixSets", crate::ogc_tiles::MATRIX_SET] {
            return json(200, crate::ogc_tiles::matrix_set_json());
        }
        let collections = match self.collections() {
            Ok(collections) => collections,
            Err(response) => return response,
        };
        match parts.as_slice() {
            ["collections"] => {
                let listed: Vec<String> = collections.iter().map(|collection| self.collection_json(collection)).collect();
                json(200, format!("{{\"collections\":[{}],\"links\":[{}]}}", listed.join(","), link("/ogc/collections", "self", "application/json")))
            }
            ["collections", name, tail @ ..] => {
                let Some(collection) = collections.iter().find(|collection| collection.table == *name) else {
                    return failure(404, "OGC404", &format!("there is no collection {name}"));
                };
                match tail {
                    [] => json(200, self.collection_json(collection)),
                    ["items"] => self.items(collection, request, None),
                    ["queryables"] => self.queryables(collection),
                    ["tiles"] => self.tilesets(collection),
                    ["tiles", crate::ogc_tiles::MATRIX_SET] => self.tileset(collection),
                    ["tiles", crate::ogc_tiles::MATRIX_SET, zoom, row, column] => self.tile(collection, zoom, row, column),
                    ["items", id] => self.items(collection, request, Some(id)),
                    _ => failure(404, "OGC404", "no such resource"),
                }
            }
            _ => failure(404, "OGC404", "no such resource"),
        }
    }
}
