use super::{Answer, Api, error, json, read, write};
use crate::json_out::Out;
use crate::script::Script;
use eggdb_http::{Request, Response};
use eggdb_sql::json::{Json, Node, parse_document};

pub(super) const STORED_SCRIPT_RECORD: u8 = 4;
const MAX_SCRIPT_ID_BYTES: usize = 512;
const MAX_STORED_SCRIPT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StoredScript {
    pub(super) lang: String,
    pub(super) source: String,
}

impl StoredScript {
    fn encode(&self) -> Vec<u8> {
        let mut out = self.lang.as_bytes().to_vec();
        out.push(0);
        out.extend_from_slice(self.source.as_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let split = bytes.iter().position(|&byte| byte == 0)?;
        Some(Self {
            lang: String::from_utf8(bytes[..split].to_vec()).ok()?,
            source: String::from_utf8(bytes[split + 1..].to_vec()).ok()?,
        })
    }
}

fn script_key(id: &str) -> Vec<u8> {
    let mut key = vec![STORED_SCRIPT_RECORD];
    key.extend_from_slice(id.as_bytes());
    key
}

fn valid_script_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_SCRIPT_ID_BYTES && !id.contains('#')
}

fn node(value: Json) -> Node {
    Node {
        value,
        start: 0,
        end: 0,
    }
}

fn not_found(id: &str) -> Response {
    error(
        404,
        "resource_not_found_exception",
        &format!("unable to find script [{id}] in cluster state"),
    )
}

fn stored_script_of(body: &[u8]) -> Result<StoredScript, Response> {
    let text = String::from_utf8_lossy(body);
    let parsed = parse_document(&text)
        .map_err(|_| error(400, "parse_exception", "the stored script request is not JSON"))?;
    let Some(script) = super::member_of(&parsed, "script") else {
        return Err(error(400, "illegal_argument_exception", "must specify [script] for stored script"));
    };
    let Json::Object(members) = &script.value else {
        return Err(error(400, "parse_exception", "[script] must be an object"));
    };
    let mut lang = None;
    let mut source = None;
    for (key, value) in members {
        match (key.as_str(), &value.value) {
            ("lang", Json::String(name)) => lang = Some(name.clone()),
            ("source" | "code", Json::String(text)) => source = Some(text.clone()),
            ("source" | "code", Json::Object(_)) => source = Some(crate::mustache::json_of(value)),
            ("options" | "params", _) => {}
            (other, _) => {
                return Err(error(400, "parse_exception", &format!("unexpected field [{other}] in the stored script")));
            }
        }
    }
    let lang = lang.ok_or_else(|| error(400, "illegal_argument_exception", "must specify lang for stored script"))?;
    let source = source.ok_or_else(|| error(400, "illegal_argument_exception", "must specify source for stored script"))?;
    if source.len() > MAX_STORED_SCRIPT_BYTES {
        return Err(error(400, "illegal_argument_exception", "the stored script is too large"));
    }
    match lang.as_str() {
        "mustache" => {
            crate::mustache::render(&source, &node(Json::Object(Vec::new())))
                .map_err(|reason| error(400, "script_exception", &format!("compile error: {reason}")))?;
        }
        "painless" => {
            Script::from_node(&node(Json::String(source.clone())))
                .map_err(|reason| error(400, "script_exception", &format!("compile error: {reason}")))?;
        }
        other => {
            return Err(error(400, "illegal_argument_exception", &format!("script_lang not supported [{other}]")));
        }
    }
    Ok(StoredScript { lang, source })
}

impl Api<'_> {
    pub(super) fn stored_script(&self, id: &str) -> Result<Option<StoredScript>, String> {
        read(self.keyspace, |keys| {
            Ok(keys.get(&script_key(id))?.and_then(|bytes| StoredScript::decode(&bytes)))
        })
    }

    pub(super) fn put_stored_script(&self, id: &str, request: &Request) -> Answer {
        if !valid_script_id(id) {
            return Ok(error(400, "illegal_argument_exception", &format!("invalid id [{id}]")));
        }
        let script = match stored_script_of(&request.body) {
            Ok(script) => script,
            Err(refusal) => return Ok(refusal),
        };
        write(self.keyspace, |keys| keys.put(&script_key(id), &script.encode()))?;
        Ok(json(200, "{\"acknowledged\":true}".to_owned()))
    }

    pub(super) fn get_stored_script(&self, id: &str) -> Answer {
        Ok(match self.stored_script(id)? {
            Some(script) => {
                let mut out = Out::default();
                out.begin_object()
                    .key("_id")
                    .string(id)
                    .key("found")
                    .boolean(true)
                    .key("script")
                    .begin_object()
                    .key("lang")
                    .string(&script.lang)
                    .key("source")
                    .string(&script.source)
                    .end_object()
                    .end_object();
                json(200, out.finish())
            }
            None => {
                let mut out = Out::default();
                out.begin_object().key("_id").string(id).key("found").boolean(false).end_object();
                json(404, out.finish())
            }
        })
    }

    pub(super) fn delete_stored_script(&self, id: &str) -> Answer {
        let removed = write(self.keyspace, |keys| keys.delete(&script_key(id)))?;
        Ok(if removed {
            json(200, "{\"acknowledged\":true}".to_owned())
        } else {
            not_found(id)
        })
    }

    pub(super) fn stored_template(&self, id: &str) -> Result<String, Response> {
        match self.stored_script(id) {
            Ok(Some(script)) if script.lang == "mustache" => Ok(script.source),
            Ok(Some(_)) => Err(error(
                400,
                "illegal_argument_exception",
                &format!("the stored script [{id}] is not a search template"),
            )),
            Ok(None) => Err(not_found(id)),
            Err(reason) => Err(error(500, "exception", &reason)),
        }
    }

    /// The `script` member with a stored script's `id` replaced by its source, so it compiles
    /// as an inline script; any other script as it is.
    pub(super) fn resolved_script(&self, script: &Node) -> Result<Node, Response> {
        let Json::Object(members) = &script.value else {
            return Ok(script.clone());
        };
        let Some(id) = members.iter().find_map(|(key, value)| match (key.as_str(), &value.value) {
            ("id", Json::String(id)) => Some(id.clone()),
            _ => None,
        }) else {
            return Ok(script.clone());
        };
        let stored = match self.stored_script(&id) {
            Ok(Some(stored)) => stored,
            Ok(None) => return Err(not_found(&id)),
            Err(reason) => return Err(error(500, "exception", &reason)),
        };
        if stored.lang != "painless" {
            return Err(error(
                400,
                "illegal_argument_exception",
                &format!("the stored script [{id}] is not a painless script"),
            ));
        }
        let mut resolved = vec![
            ("source".to_owned(), node(Json::String(stored.source))),
            ("lang".to_owned(), node(Json::String(stored.lang))),
        ];
        resolved.extend(
            members
                .iter()
                .filter(|(key, _)| key == "params")
                .cloned(),
        );
        Ok(node(Json::Object(resolved)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_scripts_round_trip_and_bad_ones_are_refused() {
        let script = stored_script_of(br#"{"script":{"lang":"mustache","source":{"query":{"match":{"title":"{{q}}"}}}}}"#).unwrap();
        assert_eq!(script.lang, "mustache");
        assert_eq!(StoredScript::decode(&script.encode()), Some(script));
        let painless = stored_script_of(br#"{"script":{"lang":"painless","source":"ctx._source.n += params.by"}}"#).unwrap();
        assert_eq!(painless.lang, "painless");
        assert!(stored_script_of(br#"{"script":{"lang":"expression","source":"1"}}"#).is_err());
        assert!(stored_script_of(br#"{"script":{"source":"1"}}"#).is_err());
        assert!(stored_script_of(br#"{"nothing":1}"#).is_err());
        assert!(!valid_script_id(""));
    }
}
