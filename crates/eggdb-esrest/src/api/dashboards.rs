use super::{Answer, Api, Failure, VERSION, index_key, indices_of, json, missing_index, read, resolve_indices, settings_key, write};
use crate::json_out::{escaped, written};
use eggdb_http::{Request, Response};
use eggdb_sql::json::{Json, Node, parse_document};

const NODE_ID: &str = "eggdb";
const KIBANA_7_VERSION: &str = "7.10.2";
const AGGREGATABLE_TYPES: [&str; 16] = [
    "keyword",
    "constant_keyword",
    "long",
    "integer",
    "short",
    "byte",
    "double",
    "float",
    "half_float",
    "scaled_float",
    "unsigned_long",
    "date",
    "date_nanos",
    "boolean",
    "ip",
    "geo_point",
];
const METADATA_FIELDS: [(&str, &str, bool, bool); 4] = [
    ("_id", "_id", true, true),
    ("_index", "_index", true, true),
    ("_source", "_source", false, false),
    ("_type", "_type", true, true),
];

/// The version EggDB answers a client with: OpenSearch's for every client but Kibana 7, which
/// refuses to start unless the cluster answers its own version, as an Elasticsearch 7.10 cluster
/// does for it.
pub(super) fn version_for(request: &Request) -> &'static str {
    let agent = request.header("user-agent");
    let kibana_seven = agent.is_some_and(|agent| agent.starts_with("Kibana/7."))
        || (request.header("x-elastic-product-origin") == Some("kibana")
            && agent.is_none_or(|agent| !agent.starts_with("OpenSearch")));
    if kibana_seven { KIBANA_7_VERSION } else { VERSION }
}

fn members(node: &Node) -> Vec<(String, Node)> {
    match &node.value {
        Json::Object(members) => members.clone(),
        _ => Vec::new(),
    }
}

fn object(members: &[(String, String)]) -> String {
    let written: Vec<String> = members
        .iter()
        .map(|(name, value)| format!("{}:{value}", escaped(name)))
        .collect();
    format!("{{{}}}", written.join(","))
}

fn parsed_body(request: &Request) -> Result<Option<Node>, String> {
    let body = String::from_utf8_lossy(&request.body);
    if body.trim().is_empty() {
        return Ok(None);
    }
    parse_document(&body)
        .map(Some)
        .map_err(|_| "the request body is not JSON".to_owned())
}

fn merged_mappings(stored: &str, change: &Node) -> Result<String, String> {
    let stored = parse_document(stored).map_err(|_| "the stored mappings are not JSON".to_owned())?;
    let mut merged: Vec<(String, String)> = members(&stored)
        .iter()
        .map(|(name, value)| (name.clone(), written(value)))
        .collect();
    for (name, value) in members(change) {
        let value = if name == "properties" {
            let earlier = members(&stored)
                .into_iter()
                .find(|(member, _)| *member == "properties")
                .map(|(_, properties)| members(&properties))
                .unwrap_or_default();
            let mut properties: Vec<(String, String)> =
                earlier.iter().map(|(field, mapping)| (field.clone(), written(mapping))).collect();
            for (field, mapping) in members(&value) {
                let mapping = written(&mapping);
                match properties.iter_mut().find(|(known, _)| *known == field) {
                    Some(known) => known.1 = mapping,
                    None => properties.push((field, mapping)),
                }
            }
            object(&properties)
        } else {
            written(&value)
        };
        match merged.iter_mut().find(|(known, _)| *known == name) {
            Some(known) => known.1 = value,
            None => merged.push((name, value)),
        }
    }
    Ok(object(&merged))
}

fn field_type(mapping: &Node) -> String {
    members(mapping)
        .into_iter()
        .find(|(name, _)| name == "type")
        .and_then(|(_, kind)| match kind.value {
            Json::String(kind) => Some(kind),
            _ => None,
        })
        .unwrap_or_else(|| "object".to_owned())
}

fn collect_fields(properties: &Node, prefix: &str, out: &mut Vec<(String, String)>) {
    for (name, mapping) in members(properties) {
        let path = if prefix.is_empty() { name } else { format!("{prefix}.{name}") };
        let kind = field_type(&mapping);
        out.push((path.clone(), kind));
        for (member, value) in members(&mapping) {
            match member.as_str() {
                "properties" => collect_fields(&value, &path, out),
                "fields" => {
                    for (sub, sub_mapping) in members(&value) {
                        out.push((format!("{path}.{sub}"), field_type(&sub_mapping)));
                    }
                }
                _ => {}
            }
        }
    }
}

fn mapped_fields(mappings: &str) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    if let Ok(parsed) = parse_document(mappings)
        && let Some((_, properties)) = members(&parsed).into_iter().find(|(name, _)| name == "properties")
    {
        collect_fields(&properties, "", &mut fields);
    }
    fields
}

fn capability(kind: &str, searchable: bool, aggregatable: bool) -> String {
    format!(
        "{{{}:{{\"type\":{},\"searchable\":{searchable},\"aggregatable\":{aggregatable}}}}}",
        escaped(kind),
        escaped(kind)
    )
}

fn written_names(names: &[String]) -> String {
    let written: Vec<String> = names.iter().map(|name| escaped(name)).collect();
    format!("[{}]", written.join(","))
}

impl Api<'_> {
    pub(super) fn nodes_info(&self, request: &Request) -> Response {
        json(
            200,
            format!(
                "{{\"_nodes\":{{\"total\":1,\"successful\":1,\"failed\":0}},\"cluster_name\":\"eggdb\",\"nodes\":{{\"{NODE_ID}\":{{\"name\":\"eggdb\",\"version\":\"{}\",\"ip\":\"127.0.0.1\",\"host\":\"127.0.0.1\",\"roles\":[\"data\",\"ingest\",\"cluster_manager\"],\"http\":{{\"publish_address\":\"127.0.0.1:9200\"}}}}}}}}",
                version_for(request)
            ),
        )
    }

    pub(super) fn cluster_settings(&self) -> Response {
        json(200, "{\"persistent\":{},\"transient\":{}}".to_owned())
    }

    pub(super) fn put_cluster_settings(&self, request: &Request) -> Answer {
        let body = parsed_body(request).map_err(Failure::BadRequest)?;
        let part = |name: &str| {
            body.as_ref()
                .and_then(|body| members(body).into_iter().find(|(member, _)| member == name))
                .map_or_else(|| "{}".to_owned(), |(_, value)| written(&value))
        };
        Ok(json(
            200,
            format!(
                "{{\"acknowledged\":true,\"persistent\":{},\"transient\":{}}}",
                part("persistent"),
                part("transient")
            ),
        ))
    }

    fn resolved(&self, written: Option<&str>) -> Result<Result<Vec<String>, String>, String> {
        read(self.keyspace, |keys| match written {
            None | Some("_all" | "*") => indices_of(keys).map(Ok),
            Some(written) => {
                let parts: Vec<&str> = written.split(',').collect();
                resolve_indices(keys, &parts, false)
            }
        })
    }

    pub(super) fn get_mapping(&self, index: Option<&str>) -> Answer {
        let indices = match self.resolved(index)? {
            Ok(indices) => indices,
            Err(missing) => return Ok(missing_index(&missing)),
        };
        let answer = read(self.keyspace, |keys| {
            let mut out = Vec::new();
            for name in &indices {
                if let Some(mappings) = keys.get(&index_key(name))? {
                    out.push((name.clone(), format!("{{\"mappings\":{}}}", String::from_utf8_lossy(&mappings))));
                }
            }
            Ok(object(&out))
        })?;
        Ok(json(200, answer))
    }

    pub(super) fn put_mapping(&self, index: &str, request: &Request) -> Answer {
        let Some(change) = parsed_body(request).map_err(Failure::BadRequest)? else {
            return Ok(super::error(400, "action_request_validation_exception", "mapping source is required"));
        };
        let indices = match self.resolved(Some(index))? {
            Ok(indices) => indices,
            Err(missing) => return Ok(missing_index(&missing)),
        };
        write(self.keyspace, |keys| {
            for name in &indices {
                let stored = keys
                    .get(&index_key(name))?
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_else(|| "{}".to_owned());
                let merged = merged_mappings(&stored, &change)?;
                keys.put(&index_key(name), merged.as_bytes())?;
            }
            Ok(())
        })?;
        Ok(json(200, "{\"acknowledged\":true}".to_owned()))
    }

    pub(super) fn get_settings(&self, index: Option<&str>) -> Answer {
        let indices = match self.resolved(index)? {
            Ok(indices) => indices,
            Err(missing) => return Ok(missing_index(&missing)),
        };
        let answer = read(self.keyspace, |keys| {
            let mut out = Vec::new();
            for name in &indices {
                let stored = keys
                    .get(&settings_key(name))?
                    .and_then(|bytes| parse_document(&String::from_utf8_lossy(&bytes)).ok());
                let mut settings: Vec<(String, String)> = vec![
                    ("number_of_shards".to_owned(), "\"1\"".to_owned()),
                    ("number_of_replicas".to_owned(), "\"0\"".to_owned()),
                    ("provided_name".to_owned(), escaped(name)),
                ];
                let stored_members = stored.as_ref().map(members).unwrap_or_default();
                let nested = stored_members
                    .iter()
                    .find(|(member, _)| member == "index")
                    .map(|(_, index)| members(index));
                let flattened = nested.unwrap_or(stored_members);
                for (member, value) in flattened {
                    let value = match &value.value {
                        Json::Number(text) => escaped(text),
                        Json::Boolean(flag) => escaped(&flag.to_string()),
                        _ => written(&value),
                    };
                    match settings.iter_mut().find(|(known, _)| *known == member) {
                        Some(known) => known.1 = value,
                        None => settings.push((member, value)),
                    }
                }
                out.push((
                    name.clone(),
                    format!("{{\"settings\":{{\"index\":{}}}}}", object(&settings)),
                ));
            }
            Ok(object(&out))
        })?;
        Ok(json(200, answer))
    }

    pub(super) fn field_caps(&self, index: Option<&str>) -> Answer {
        let indices = match self.resolved(index)? {
            Ok(indices) => indices,
            Err(missing) => return Ok(missing_index(&missing)),
        };
        let fields = read(self.keyspace, |keys| {
            let mut fields: Vec<(String, String)> = Vec::new();
            for name in &indices {
                if let Some(mappings) = keys.get(&index_key(name))? {
                    for field in mapped_fields(&String::from_utf8_lossy(&mappings)) {
                        if !fields.iter().any(|(known, _)| *known == field.0) {
                            fields.push(field);
                        }
                    }
                }
            }
            Ok(fields)
        })?;
        let mut written_fields: Vec<(String, String)> = METADATA_FIELDS
            .iter()
            .map(|(name, kind, searchable, aggregatable)| ((*name).to_owned(), capability(kind, *searchable, *aggregatable)))
            .collect();
        for (name, kind) in fields {
            let searchable = kind != "object" && kind != "nested";
            let aggregatable = AGGREGATABLE_TYPES.contains(&kind.as_str());
            written_fields.push((name, capability(&kind, searchable, aggregatable)));
        }
        Ok(json(
            200,
            format!(
                "{{\"indices\":{},\"fields\":{}}}",
                written_names(&indices),
                object(&written_fields)
            ),
        ))
    }

    pub(super) fn resolve_index(&self, pattern: &str) -> Answer {
        let indices = match self.resolved(Some(pattern))? {
            Ok(indices) => indices,
            Err(_) => Vec::new(),
        };
        let listed: Vec<String> = indices
            .iter()
            .map(|name| format!("{{\"name\":{},\"attributes\":[\"open\"]}}", escaped(name)))
            .collect();
        Ok(json(
            200,
            format!("{{\"indices\":[{}],\"aliases\":[],\"data_streams\":[]}}", listed.join(",")),
        ))
    }

    pub(super) fn empty_cat(&self, request: &Request) -> Response {
        let wants_json = super::query_flag(request, "format").is_some_and(|format| format == "json");
        if wants_json {
            json(200, "[]".to_owned())
        } else {
            Response {
                status: 200,
                content_type: "text/plain; charset=UTF-8".to_owned(),
                body: Vec::new(),
                headers: Vec::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mappings_merge_new_fields_and_list_their_capabilities() {
        let stored = "{\"properties\":{\"title\":{\"type\":\"text\",\"fields\":{\"keyword\":{\"type\":\"keyword\"}}}}}";
        let change = parse_document("{\"properties\":{\"views\":{\"type\":\"long\"}},\"_meta\":{\"v\":1}}").unwrap();
        let merged = merged_mappings(stored, &change).unwrap();
        assert_eq!(
            merged,
            "{\"properties\":{\"title\":{\"type\":\"text\",\"fields\":{\"keyword\":{\"type\":\"keyword\"}}},\"views\":{\"type\":\"long\"}},\"_meta\":{\"v\":1}}"
        );
        let fields = mapped_fields(&merged);
        assert_eq!(
            fields,
            [
                ("title".to_owned(), "text".to_owned()),
                ("title.keyword".to_owned(), "keyword".to_owned()),
                ("views".to_owned(), "long".to_owned()),
            ]
        );
    }
}
