//! Kafka Connect's REST API, the paths and answers its documentation gives, so tools that
//! manage Connect clusters manage EggDB's worker the same way.

use crate::connectors::{PLUGINS, plugin_for, validate};
use crate::json::{Json, object, parse};
use crate::store::{Connectors, State, Stored};
use eggdb_http::{Request, Response};
use std::collections::BTreeMap;

const JSON: &str = "application/json";
const VERSION: &str = "3.7.0";
const WORKER_ID: &str = "eggdb:8083";

fn answer(status: u16, body: Json) -> Response {
    let mut response = Response::ok(JSON, body.write());
    response.status = status;
    response
}

fn empty(status: u16) -> Response {
    let mut response = Response::ok(JSON, Vec::new());
    response.status = status;
    response
}

fn failure(status: u16, message: &str) -> Response {
    answer(status, object([("error_code".to_owned(), Json::Number(f64::from(status))), ("message".to_owned(), Json::Text(message.to_owned()))]))
}

fn text(value: &str) -> Json {
    Json::Text(value.to_owned())
}

fn config_json(config: &BTreeMap<String, String>) -> Json {
    object(config.iter().map(|(field, value)| (field.clone(), text(value))))
}

fn config_of(body: &Json) -> Result<BTreeMap<String, String>, String> {
    let Json::Object(fields) = body else {
        return Err("a connector configuration is a JSON object".to_owned());
    };
    fields
        .iter()
        .map(|(field, value)| value.config_text().map(|value| (field.clone(), value)).ok_or_else(|| format!("the configuration value of {field} is not a string")))
        .collect()
}

fn kind_of(stored: &Stored) -> &'static str {
    stored.config.get("connector.class").and_then(|class| plugin_for(class)).map_or("unknown", |plugin| plugin.kind)
}

fn status_json(stored: &Stored) -> Json {
    let mut connector = vec![("state".to_owned(), text(stored.state.name())), ("worker_id".to_owned(), text(WORKER_ID))];
    if let State::Failed(trace) = &stored.state {
        connector.push(("trace".to_owned(), text(trace)));
    }
    let mut task = vec![("id".to_owned(), Json::Number(0.0)), ("state".to_owned(), text(stored.state.name())), ("worker_id".to_owned(), text(WORKER_ID))];
    if let State::Failed(trace) = &stored.state {
        task.push(("trace".to_owned(), text(trace)));
    }
    object([
        ("name".to_owned(), text(&stored.name)),
        ("connector".to_owned(), object(connector)),
        ("tasks".to_owned(), Json::Array(vec![object(task)])),
        ("type".to_owned(), text(kind_of(stored))),
    ])
}

fn info_json(stored: &Stored) -> Json {
    object([
        ("name".to_owned(), text(&stored.name)),
        ("config".to_owned(), config_json(&stored.config)),
        ("tasks".to_owned(), Json::Array(vec![object([("connector".to_owned(), text(&stored.name)), ("task".to_owned(), Json::Number(0.0))])])),
        ("type".to_owned(), text(kind_of(stored))),
    ])
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.contains('/') && !name.chars().any(char::is_control)
}

fn create_or_update(connectors: &Connectors<'_>, name: &str, mut config: BTreeMap<String, String>) -> Response {
    if !valid_name(name) {
        return failure(400, "a connector name is 1 to 255 characters without '/'");
    }
    config.insert("name".to_owned(), name.to_owned());
    let problems = validate(&config);
    if let Some((field, message)) = problems.first() {
        return failure(400, &format!("Connector configuration is invalid and contains the following {} error(s):\n{field}: {message}", problems.len()));
    }
    match connectors.put(name, &config).and_then(|created| connectors.get(name).map(|stored| (created, stored))) {
        Ok((created, Some(stored))) => answer(if created { 201 } else { 200 }, info_json(&stored)),
        Ok((_, None)) => failure(500, "the connector vanished while it was stored"),
        Err(message) => failure(500, &message),
    }
}

/// Answers one request of the Connect REST API.
pub(crate) fn handle(connectors: &Connectors<'_>, request: &Request) -> Response {
    let parts: Vec<&str> = request.path.trim_matches('/').split('/').filter(|part| !part.is_empty()).collect();
    let body = || if request.body.is_empty() { Ok(Json::Object(BTreeMap::new())) } else { parse(&request.body) };
    let decoded: Vec<String> = parts.iter().map(|part| eggdb_http::percent_decoded(part).unwrap_or_else(|| (*part).to_owned())).collect();
    let parts: Vec<&str> = decoded.iter().map(String::as_str).collect();
    let stored = |name: &str| connectors.get(name);
    match (request.method.as_str(), parts.as_slice()) {
        ("GET", []) => answer(200, object([("version".to_owned(), text(VERSION)), ("commit".to_owned(), text("eggdb")), ("kafka_cluster_id".to_owned(), text("eggdb"))])),
        ("GET", ["connector-plugins"]) => answer(
            200,
            Json::Array(PLUGINS.iter().map(|plugin| object([("class".to_owned(), text(plugin.class)), ("type".to_owned(), text(plugin.kind)), ("version".to_owned(), text(VERSION))])).collect()),
        ),
        ("PUT", ["connector-plugins", _, "config", "validate"]) => {
            let config = match body().and_then(|json| config_of(&json)) {
                Ok(config) => config,
                Err(message) => return failure(400, &message),
            };
            let problems = validate(&config);
            let fields: Vec<Json> = config
                .keys()
                .chain(problems.iter().map(|(field, _)| field))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|field| {
                    let errors: Vec<Json> = problems.iter().filter(|(problem, _)| problem == field).map(|(_, message)| text(message)).collect();
                    object([("value".to_owned(), object([("name".to_owned(), text(field)), ("value".to_owned(), config.get(field).map_or(Json::Null, |value| text(value))), ("errors".to_owned(), Json::Array(errors))]))])
                })
                .collect();
            answer(200, object([
                ("name".to_owned(), text(config.get("connector.class").map_or("", String::as_str))),
                ("error_count".to_owned(), Json::Number(problems.len() as f64)),
                ("groups".to_owned(), Json::Array(Vec::new())),
                ("configs".to_owned(), Json::Array(fields)),
            ]))
        }
        ("GET", ["connectors"]) => {
            let names = match connectors.names() {
                Ok(names) => names,
                Err(message) => return failure(500, &message),
            };
            let expand: Vec<&str> = request.query.as_deref().unwrap_or_default().split('&').filter_map(|pair| pair.strip_prefix("expand=")).collect();
            if expand.is_empty() {
                return answer(200, Json::Array(names.iter().map(|name| text(name)).collect()));
            }
            let mut all = BTreeMap::new();
            for name in names {
                let Ok(Some(found)) = stored(&name) else { continue };
                let mut entry = BTreeMap::new();
                if expand.contains(&"status") {
                    entry.insert("status".to_owned(), status_json(&found));
                }
                if expand.contains(&"info") {
                    entry.insert("info".to_owned(), info_json(&found));
                }
                all.insert(name, Json::Object(entry));
            }
            answer(200, Json::Object(all))
        }
        ("POST", ["connectors"]) => {
            let json = match body() {
                Ok(json) => json,
                Err(message) => return failure(400, &message),
            };
            let Some(name) = json.get("name").and_then(Json::text).map(str::to_owned) else {
                return failure(400, "a connector needs a name");
            };
            if matches!(stored(&name), Ok(Some(_))) {
                return failure(409, &format!("Connector {name} already exists"));
            }
            match json.get("config").map(config_of) {
                Some(Ok(config)) => create_or_update(connectors, &name, config),
                Some(Err(message)) => failure(400, &message),
                None => failure(400, "a connector needs a config"),
            }
        }
        (method, ["connectors", name, rest @ ..]) => {
            let found = match stored(name) {
                Ok(found) => found,
                Err(message) => return failure(500, &message),
            };
            if method == "PUT" && rest == ["config"] {
                return match body().and_then(|json| config_of(&json)) {
                    Ok(config) => create_or_update(connectors, name, config),
                    Err(message) => failure(400, &message),
                };
            }
            let Some(found) = found else {
                return failure(404, &format!("Connector {name} not found"));
            };
            let set = |state: State| connectors.set_state(name, &state).map_or_else(|message| failure(500, &message), |()| empty(202));
            match (method, rest) {
                ("GET", []) => answer(200, info_json(&found)),
                ("GET", ["config"]) => answer(200, config_json(&found.config)),
                ("GET", ["status"]) => answer(200, status_json(&found)),
                ("GET", ["tasks"]) => answer(200, Json::Array(vec![object([("id".to_owned(), object([("connector".to_owned(), text(name)), ("task".to_owned(), Json::Number(0.0))])), ("config".to_owned(), config_json(&found.config))])])),
                ("GET", ["tasks", "0", "status"]) => answer(200, object([("id".to_owned(), Json::Number(0.0)), ("state".to_owned(), text(found.state.name())), ("worker_id".to_owned(), text(WORKER_ID))])),
                ("PUT", ["pause"]) => set(State::Paused),
                ("PUT", ["resume"]) => set(State::Running),
                ("POST", ["restart"]) | ("POST", ["tasks", "0", "restart"]) => set(State::Running),
                ("DELETE", []) => connectors.delete(name).map_or_else(|message| failure(500, &message), |_| empty(204)),
                ("GET", ["offsets"]) => answer(200, object([("offsets".to_owned(), Json::Array(vec![object([("partition".to_owned(), object([])), ("offset".to_owned(), object([("position".to_owned(), Json::Number(found.position as f64))]))])]))])),
                _ => failure(404, "Not found"),
            }
        }
        _ => failure(404, "Not found"),
    }
}
