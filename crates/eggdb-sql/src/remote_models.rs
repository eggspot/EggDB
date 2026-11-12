use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, Node, parse_document};
use crate::types::Value;
use std::sync::OnceLock;

pub(crate) const FUNCTIONS: [&str; 2] = ["eggdb_remote_embed", "eggdb_remote_complete"];
const MOST_INPUT_BYTES: usize = 1 << 20;

/// One call to a remote model: the endpoint's URL, the JSON request body, the API key if any,
/// and the role asking (`None` for a superuser).
#[derive(Debug, Clone, Copy)]
pub struct ModelRequest<'a> {
    /// The endpoint, a URL such as `https://api.example.com/v1/embeddings`.
    pub endpoint: &'a str,
    /// The JSON body to post.
    pub body: &'a str,
    /// The bearer key, if the caller gave one.
    pub api_key: Option<&'a str>,
    /// The role that called, or `None` when a superuser did.
    pub role: Option<&'a str>,
}

/// Posts a request to a remote model and returns the response body, or a failure.
pub type ModelProvider = Box<dyn Fn(&ModelRequest<'_>) -> Result<String, crate::ForeignFailure> + Send + Sync>;

static PROVIDER: OnceLock<ModelProvider> = OnceLock::new();

/// Sets the process's caller of remote models. The first registration wins. Until one is set,
/// `eggdb_remote_embed` and `eggdb_remote_complete` answer SQLSTATE `0A000`, so SQL sends nothing
/// out of the process unless the program running it chose to allow it.
pub fn set_model_provider(provider: ModelProvider) {
    let _ = PROVIDER.set(provider);
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn member<'a>(node: &'a Node, name: &str) -> Option<&'a Node> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, value)| value),
        _ => None,
    }
}

fn element(node: &Node, index: usize) -> Option<&Node> {
    match &node.value {
        Json::Array(items) => items.get(index),
        _ => None,
    }
}

fn post(endpoint: &str, body: &str, api_key: Option<&str>) -> SqlResult<String> {
    let Some(provider) = PROVIDER.get() else {
        return Err(SqlError::not_supported("remote models are off: the program running this SQL allows none (eggdb reads EGGDB_MODEL_ENDPOINTS)"));
    };
    let role = crate::constraints::checked_role();
    let request = ModelRequest { endpoint, body, api_key, role: role.as_deref() };
    provider(&request).map_err(|failure| match failure {
        crate::ForeignFailure::Denied(message) => SqlError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("permission denied to call {message}")),
        crate::ForeignFailure::Failed(message) => SqlError::new(SqlState::FDW_ERROR, format!("{endpoint}: {message}")),
    })
}

fn embed(endpoint: &str, model: &str, input: &str, api_key: Option<&str>) -> SqlResult<String> {
    let answer = post(endpoint, &format!("{{\"model\": {}, \"input\": {}}}", json_string(model), json_string(input)), api_key)?;
    let document = parse_document(&answer).map_err(|_| SqlError::new(SqlState::FDW_ERROR, format!("{endpoint}: the answer is not JSON")))?;
    let numbers = member(&document, "data")
        .and_then(|data| element(data, 0))
        .and_then(|first| member(first, "embedding"))
        .or_else(|| member(&document, "embedding"));
    let Some(Node { value: Json::Array(items), .. }) = numbers else {
        return Err(SqlError::new(SqlState::FDW_ERROR, format!("{endpoint}: the answer has no data[0].embedding")));
    };
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        match &item.value {
            Json::Number(number) => parts.push(number.clone()),
            _ => return Err(SqlError::new(SqlState::FDW_ERROR, format!("{endpoint}: the embedding holds a non-number"))),
        }
    }
    Ok(format!("[{}]", parts.join(",")))
}

fn complete(endpoint: &str, model: &str, prompt: &str, api_key: Option<&str>) -> SqlResult<String> {
    let body = format!("{{\"model\": {}, \"messages\": [{{\"role\": \"user\", \"content\": {}}}]}}", json_string(model), json_string(prompt));
    let answer = post(endpoint, &body, api_key)?;
    let document = parse_document(&answer).map_err(|_| SqlError::new(SqlState::FDW_ERROR, format!("{endpoint}: the answer is not JSON")))?;
    let content = member(&document, "choices")
        .and_then(|choices| element(choices, 0))
        .and_then(|choice| member(choice, "message"))
        .and_then(|message| member(message, "content"))
        .or_else(|| member(&document, "message").and_then(|message| member(message, "content")));
    match content {
        Some(Node { value: Json::String(text), .. }) => Ok(text.clone()),
        _ => Err(SqlError::new(SqlState::FDW_ERROR, format!("{endpoint}: the answer has no choices[0].message.content"))),
    }
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    let texts: Vec<Option<String>> = values.iter().map(Value::to_text).collect();
    let (endpoint, model, input) = match texts.as_slice() {
        [Some(endpoint), Some(model), Some(input)] | [Some(endpoint), Some(model), Some(input), _] => (endpoint, model, input),
        [_, _, _] | [_, _, _, _] => return Some(Ok(Value::Null)),
        _ => return Some(Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("{name} takes an endpoint, a model, the text and optionally an API key")))),
    };
    if input.len() > MOST_INPUT_BYTES {
        return Some(Err(SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, format!("{name} sends at most 1 MiB of text"))));
    }
    let api_key = texts.get(3).and_then(Option::as_deref);
    Some(if name == "eggdb_remote_embed" {
        embed(endpoint, model, input, api_key).map(Value::Text)
    } else {
        complete(endpoint, model, input, api_key).map(Value::Text)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_stay_off_until_a_program_allows_them() {
        let refused = function("eggdb_remote_embed", &[Value::Text("https://models.example/v1/embeddings".to_owned()), Value::Text("m".to_owned()), Value::Text("hi".to_owned())]);
        if PROVIDER.get().is_none() {
            assert_eq!(refused.unwrap().unwrap_err().state().code(), "0A000");
        }
        assert_eq!(json_string("a\"b\n"), "\"a\\\"b\\u000a\"");
    }
}
