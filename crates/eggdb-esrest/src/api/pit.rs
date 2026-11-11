use super::scroll::keep_alive;
use super::{Answer, Api, error, json, member_of, missing_index, query_flag};
use crate::json_out::escaped;
use eggdb_http::{Request, Response};
use eggdb_sql::json::{Json, Node, parse_document};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_OPEN_PITS: usize = 500;
const MAX_KEEP_ALIVE: Duration = Duration::from_secs(24 * 3600);

type Documents = Arc<Vec<(String, String, String)>>;

struct PitContext {
    expires: Instant,
    documents: Documents,
}

#[derive(Default)]
pub(super) struct Pits {
    contexts: Mutex<HashMap<String, PitContext>>,
}

impl std::fmt::Debug for Pits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pits").finish_non_exhaustive()
    }
}

fn bounded_keep_alive(text: &str) -> Result<Duration, Response> {
    keep_alive(text)
        .filter(|duration| *duration <= MAX_KEEP_ALIVE)
        .ok_or_else(|| {
            error(
                400,
                "illegal_argument_exception",
                &format!(
                    "failed to parse [keep_alive] with value [{text}] as a time value, or it is more than the maximum keep alive of [24h]"
                ),
            )
        })
}

fn text_of(node: &Node) -> Option<String> {
    match &node.value {
        Json::String(text) => Some(text.clone()),
        _ => None,
    }
}

impl Api<'_> {
    pub(super) fn open_pit(&self, index: &str, request: &Request) -> Answer {
        let Some(requested) = query_flag(request, "keep_alive") else {
            return Ok(error(
                400,
                "action_request_validation_exception",
                "Validation Failed: 1: [keep_alive] is not specified;",
            ));
        };
        let keep = match bounded_keep_alive(&requested) {
            Ok(keep) => keep,
            Err(refusal) => return Ok(refusal),
        };
        let ignore_unavailable =
            query_flag(request, "ignore_unavailable").as_deref() == Some("true");
        let named: Vec<&str> = index.split(',').collect();
        let documents = match self.gather(Some(&named), ignore_unavailable)? {
            Ok(documents) => documents,
            Err(missing) => return Ok(missing_index(&missing)),
        };
        let now = Instant::now();
        let Ok(mut contexts) = self.pits.contexts.lock() else {
            return Ok(error(500, "eggdb_exception", "the point-in-time contexts are unavailable"));
        };
        contexts.retain(|_, context| context.expires > now);
        if contexts.len() >= MAX_OPEN_PITS {
            return Ok(error(
                429,
                "es_rejected_execution_exception",
                &format!(
                    "Trying to create too many Point In Time contexts. Must be less than or equal to: [{MAX_OPEN_PITS}]."
                ),
            ));
        }
        let id = self.new_id();
        contexts.insert(
            id.clone(),
            PitContext {
                expires: now + keep,
                documents: Arc::new(documents),
            },
        );
        Ok(json(200, format!("{{\"id\":{}}}", escaped(&id))))
    }

    pub(super) fn close_pit(&self, request: &Request) -> Answer {
        let text = String::from_utf8_lossy(&request.body).into_owned();
        let id = parse_document(&text)
            .ok()
            .and_then(|body| member_of(&body, "id"))
            .as_ref()
            .and_then(text_of);
        let Some(id) = id else {
            return Ok(error(
                400,
                "action_request_validation_exception",
                "Validation Failed: 1: [id] is not specified;",
            ));
        };
        let Ok(mut contexts) = self.pits.contexts.lock() else {
            return Ok(error(500, "eggdb_exception", "the point-in-time contexts are unavailable"));
        };
        let freed = usize::from(contexts.remove(&id).is_some());
        Ok(json(
            if freed == 0 { 404 } else { 200 },
            format!("{{\"succeeded\":true,\"num_freed\":{freed}}}"),
        ))
    }

    pub(super) fn pit_documents(&self, pit: &Node) -> Result<(Documents, Option<String>), Response> {
        let Some(id) = member_of(pit, "id").as_ref().and_then(text_of) else {
            return Err(error(
                400,
                "parsing_exception",
                "[pit] needs an [id]",
            ));
        };
        let renewed = match member_of(pit, "keep_alive").as_ref().and_then(text_of) {
            Some(text) => Some(bounded_keep_alive(&text)?),
            None => None,
        };
        let now = Instant::now();
        let Ok(mut contexts) = self.pits.contexts.lock() else {
            return Err(error(500, "eggdb_exception", "the point-in-time contexts are unavailable"));
        };
        contexts.retain(|_, context| context.expires > now);
        let Some(context) = contexts.get_mut(&id) else {
            return Err(error(
                404,
                "search_context_missing_exception",
                &format!("No search context found for id [{id}]"),
            ));
        };
        if let Some(keep) = renewed {
            context.expires = now + keep;
        }
        Ok((Arc::clone(&context.documents), Some(id)))
    }
}
