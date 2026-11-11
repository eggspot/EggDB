use super::{
    Answer, Api, PageHit, SearchOutcome, SearchPage, error, json, member_of, query_flag,
};
use crate::json_out::escaped;
use eggdb_http::{Request, Response};
use eggdb_sql::json::{Json, Node, parse_document};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_SCROLL_CONTEXTS: usize = 500;
const MAX_KEEP_ALIVE: Duration = Duration::from_secs(24 * 3600);

struct ScrollContext {
    expires: Instant,
    keep_alive: Duration,
    page: SearchPage,
}

#[derive(Default)]
pub(super) struct Scrolls {
    contexts: Mutex<HashMap<String, ScrollContext>>,
}

impl std::fmt::Debug for Scrolls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scrolls").finish_non_exhaustive()
    }
}

pub(super) fn keep_alive(text: &str) -> Option<Duration> {
    let text = text.trim();
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let amount: u64 = text.get(..digits)?.parse().ok()?;
    let unit = match text.get(digits..)? {
        "nanos" => Duration::from_nanos(amount),
        "micros" => Duration::from_micros(amount),
        "ms" => Duration::from_millis(amount),
        "s" => Duration::from_secs(amount),
        "m" => Duration::from_secs(amount.checked_mul(60)?),
        "h" => Duration::from_secs(amount.checked_mul(3600)?),
        "d" => Duration::from_secs(amount.checked_mul(86_400)?),
        _ => return None,
    };
    Some(unit)
}

fn keep_alive_refused(text: &str) -> Response {
    error(
        400,
        "illegal_argument_exception",
        &format!(
            "failed to parse setting [scroll] with value [{text}] as a time value, or it is more than the maximum keep alive of [24h]"
        ),
    )
}

fn checked_keep_alive(text: &str) -> Result<Duration, Response> {
    keep_alive(text)
        .filter(|duration| *duration <= MAX_KEEP_ALIVE)
        .ok_or_else(|| keep_alive_refused(text))
}

fn render(page: &SearchPage, scroll_id: &str, hits: &[PageHit], tail: &str) -> String {
    let mut text = format!("{{\"_scroll_id\":{},", escaped(scroll_id));
    text.push_str(page.head.get(1..).unwrap_or_default());
    let mut first = true;
    for hit in hits {
        if let Some(rendered) = page.hit(hit) {
            if !first {
                text.push(',');
            }
            first = false;
            text.push_str(&rendered);
        }
    }
    text.push_str(tail);
    text
}

fn text_of(node: &Node) -> Option<String> {
    match &node.value {
        Json::String(text) => Some(text.clone()),
        _ => None,
    }
}

fn body_of(request: &Request) -> Result<Option<Node>, Response> {
    let text = String::from_utf8_lossy(&request.body).into_owned();
    if text.trim().is_empty() {
        return Ok(None);
    }
    parse_document(&text)
        .map(Some)
        .map_err(|syntax| error(400, "parse_exception", &syntax.message))
}

impl Api<'_> {
    fn purge_expired(contexts: &mut HashMap<String, ScrollContext>, now: Instant) {
        contexts.retain(|_, context| context.expires > now);
    }

    pub(super) fn open_scroll(&self, indices: Option<&[&str]>, request: &Request) -> Answer {
        let scroll = query_flag(request, "scroll").unwrap_or_default();
        let keep = match checked_keep_alive(&scroll) {
            Ok(keep) => keep,
            Err(refusal) => return Ok(refusal),
        };
        let mut page = match self.search_page(indices, request, false, true)? {
            SearchOutcome::Whole(response) => return Ok(response),
            SearchOutcome::Page(page) => page,
        };
        let now = Instant::now();
        let Ok(mut contexts) = self.scrolls.contexts.lock() else {
            return Ok(error(500, "eggdb_exception", "the scroll contexts are unavailable"));
        };
        Self::purge_expired(&mut contexts, now);
        if contexts.len() >= MAX_SCROLL_CONTEXTS {
            return Ok(error(
                429,
                "es_rejected_execution_exception",
                &format!(
                    "Trying to create too many scroll contexts. Must be less than or equal to: [{MAX_SCROLL_CONTEXTS}]."
                ),
            ));
        }
        let id = self.new_id();
        let shown: Vec<PageHit> = page.hits.drain(..page.size.min(page.hits.len())).collect();
        let tail = page.tail.clone();
        let body = render(&page, &id, &shown, &tail);
        page.tail = "]}}".to_owned();
        contexts.insert(
            id,
            ScrollContext {
                expires: now + keep,
                keep_alive: keep,
                page,
            },
        );
        Ok(json(200, body))
    }

    pub(super) fn continue_scroll(&self, request: &Request, path_id: Option<&str>) -> Answer {
        let body = match body_of(request) {
            Ok(body) => body,
            Err(refusal) => return Ok(refusal),
        };
        let member = |name: &str| body.as_ref().and_then(|body| member_of(body, name));
        let id = path_id
            .map(str::to_owned)
            .or_else(|| member("scroll_id").as_ref().and_then(text_of))
            .or_else(|| query_flag(request, "scroll_id"));
        let Some(id) = id else {
            return Ok(error(
                400,
                "action_request_validation_exception",
                "Validation Failed: 1: scrollId is missing;",
            ));
        };
        let requested = member("scroll")
            .as_ref()
            .and_then(text_of)
            .or_else(|| query_flag(request, "scroll"));
        let keep = match requested.as_deref().map(checked_keep_alive) {
            Some(Err(refusal)) => return Ok(refusal),
            Some(Ok(keep)) => Some(keep),
            None => None,
        };
        let now = Instant::now();
        let Ok(mut contexts) = self.scrolls.contexts.lock() else {
            return Ok(error(500, "eggdb_exception", "the scroll contexts are unavailable"));
        };
        Self::purge_expired(&mut contexts, now);
        let Some(context) = contexts.get_mut(&id) else {
            return Ok(error(
                404,
                "search_context_missing_exception",
                &format!("No search context found for id [{id}]"),
            ));
        };
        if let Some(keep) = keep {
            context.keep_alive = keep;
        }
        context.expires = now + context.keep_alive;
        let page = &mut context.page;
        let shown: Vec<PageHit> = page.hits.drain(..page.size.min(page.hits.len())).collect();
        let body = render(page, &id, &shown, "]}}");
        Ok(json(200, body))
    }

    pub(super) fn clear_scroll(&self, request: &Request, path_id: Option<&str>) -> Answer {
        let body = match body_of(request) {
            Ok(body) => body,
            Err(refusal) => return Ok(refusal),
        };
        let mut ids: Vec<String> = path_id
            .map(|ids| ids.split(',').map(str::to_owned).collect())
            .unwrap_or_default();
        if let Some(listed) = body.as_ref().and_then(|body| member_of(body, "scroll_id")) {
            match &listed.value {
                Json::String(id) => ids.push(id.clone()),
                Json::Array(items) => ids.extend(items.iter().filter_map(text_of)),
                _ => {}
            }
        }
        let Ok(mut contexts) = self.scrolls.contexts.lock() else {
            return Ok(error(500, "eggdb_exception", "the scroll contexts are unavailable"));
        };
        let freed = if ids.iter().any(|id| id == "_all") {
            let count = contexts.len();
            contexts.clear();
            count
        } else {
            ids.iter()
                .filter(|id| contexts.remove(id.as_str()).is_some())
                .count()
        };
        let status = if freed == 0 && !ids.is_empty() { 404 } else { 200 };
        Ok(json(
            status,
            format!("{{\"succeeded\":true,\"num_freed\":{freed}}}"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_alive_units_are_read_and_bounded() {
        assert_eq!(keep_alive("1m"), Some(Duration::from_secs(60)));
        assert_eq!(keep_alive("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(keep_alive("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(keep_alive("m"), None);
        assert_eq!(keep_alive("5y"), None);
        assert!(checked_keep_alive("2d").is_err());
        assert!(checked_keep_alive("24h").is_ok());
    }
}
