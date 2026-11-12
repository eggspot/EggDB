use crate::request::{identifier, literal};
use crate::{Endpoint, failure, json, json_string, sql_failure};
use eggdb_http::{Request, Response};
use eggdb_sql::{KeyValueStore, Value};

const DEFAULT_LIMIT: u64 = 10;
const MOST_LIMIT: u64 = 10_000;
const GEOJSON: &str = "application/geo+json";
const GEOMETRY_NAMES: [&str; 3] = ["geom", "geometry", "the_geom"];
const CONFORMANCE: [&str; 2] = [
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core",
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson",
];

struct Collection {
    table: String,
    geometry: String,
    id: Option<String>,
}

fn parameter(request: &Request, name: &str) -> Option<String> {
    request.query.as_deref().unwrap_or_default().split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.replace("%2C", ",").replace("%2c", ",").replace('+', " "))
    })
}

fn link(href: &str, rel: &str, kind: &str) -> String {
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
        let mut ids: Vec<(String, String)> = Vec::new();
        for row in results.iter().filter(|result| result.columns.is_some()).flat_map(|result| result.rows.iter()) {
            let text = |at: usize| row.get(at).and_then(Value::to_text).unwrap_or_default();
            let (table, column, udt) = (text(0), text(1), text(2));
            if column == "id" {
                ids.push((table.clone(), column.clone()));
            }
            let is_geometry = matches!(udt.as_str(), "geometry" | "geography") || GEOMETRY_NAMES.contains(&column.as_str());
            if is_geometry && !found.iter().any(|collection| collection.table == table) {
                found.push(Collection { table, geometry: column, id: None });
            }
        }
        for collection in &mut found {
            collection.id = ids.iter().find(|(table, _)| *table == collection.table).map(|(_, column)| column.clone());
        }
        Ok(found)
    }

    fn collection_json(&self, collection: &Collection) -> String {
        let base = format!("/ogc/collections/{}", collection.table);
        format!(
            "{{\"id\":{},\"title\":{},\"itemType\":\"feature\",\"links\":[{},{}]}}",
            json_string(&collection.table),
            json_string(&collection.table),
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
        if let Some(bbox) = parameter(request, "bbox") {
            let corners: Vec<f64> = bbox.split(',').filter_map(|part| part.trim().parse().ok()).collect();
            let [west, south, east, north] = corners.as_slice() else {
                return failure(400, "OGC400", "bbox is four numbers: west,south,east,north");
            };
            if [west, south, east, north].iter().any(|value| !value.is_finite()) {
                return failure(400, "OGC400", "bbox holds a number that is not finite");
            }
            conditions.push(format!("ST_Intersects(t.{geometry}, ST_MakeEnvelope({west}, {south}, {east}, {north}, ST_SRID(t.{geometry})))"));
        }
        let id_column = collection.id.as_deref().and_then(identifier);
        if let Some(wanted) = one {
            let Some(id) = &id_column else {
                return failure(404, "OGC404", "this collection has no id column");
            };
            conditions.push(format!("t.{id}::text = {}", literal(wanted)));
        }
        let id_expression = id_column.as_ref().map_or_else(|| "NULL".to_owned(), |id| format!("t.{id}::text"));
        let order = id_column.as_ref().map_or_else(String::new, |id| format!(" ORDER BY t.{id}"));
        let sql = format!(
            "SELECT ST_AsGeoJSON(t.{geometry}) AS geometry, (to_jsonb(t) - {}) AS properties, {id_expression} AS id FROM \"{}\".{table} t WHERE {}{order} LIMIT {} OFFSET {offset}",
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
            let mut links = vec![link(&format!("{base}?limit={limit}&offset={offset}"), "self", GEOJSON)];
            if more {
                links.push(link(&format!("{base}?limit={limit}&offset={}", offset + limit), "next", GEOJSON));
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
        response
    }

    pub(crate) fn ogc(&self, rest: &str, request: &Request) -> Response {
        if request.method != "GET" {
            return failure(405, "OGC405", "OGC API - Features is read with GET");
        }
        let parts: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
        if parts.is_empty() {
            return json(200, format!(
                "{{\"title\":\"EggDB\",\"links\":[{},{},{}]}}",
                link("/ogc", "self", "application/json"),
                link("/ogc/conformance", "conformance", "application/json"),
                link("/ogc/collections", "data", "application/json")
            ));
        }
        if parts == ["conformance"] {
            let classes: Vec<String> = CONFORMANCE.iter().map(|class| json_string(class)).collect();
            return json(200, format!("{{\"conformsTo\":[{}]}}", classes.join(",")));
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
                    ["items", id] => self.items(collection, request, Some(id)),
                    _ => failure(404, "OGC404", "no such resource"),
                }
            }
            _ => failure(404, "OGC404", "no such resource"),
        }
    }
}
