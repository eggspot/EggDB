//! Sync over HTTP (D1106): a client posts the changes it logged after its `sent` cursor to
//! `POST /sync?received=<n>` with `Authorization: Bearer <token>`, and the answer carries the
//! server's changes after `received` and, in `X-EggDB-Sync-Received`, the cursor after them.
//! Each signed-in user syncs with a space of their own on the server, so one user's devices
//! share rows and no user sees another's.

use crate::{Cursors, Replica, SyncError, decode, encode};
use eggdb_http::{Request, Response};
use eggdb_keyspace::Keyspace;
use std::time::Duration;

/// The header carrying the server's cursor after the changes in an answer.
pub const RECEIVED_HEADER: &str = "x-eggdb-sync-received";

/// The most changes an answer carries.
pub const CHANGES_PER_ANSWER: usize = 1_000;

const CONTENT_TYPE: &str = "application/vnd.eggdb.sync";

fn received_of(query: Option<&str>) -> Option<u64> {
    let query = query.unwrap_or("received=0");
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("received="))
        .and_then(|text| text.parse().ok())
}

/// A sync server: who a bearer token signs in, and where each user's space is kept.
pub struct SyncEndpoint<'k> {
    /// The keyspace holding every user's space, database `n` for user `n`.
    pub keyspace: &'k (dyn Keyspace + Sync),
    /// The server replica's id among the replicas that sync with it.
    pub server_id: u64,
    /// The user a bearer token signs in, or `None` for a token that signs nobody in.
    pub authenticate: &'k (dyn Fn(&str) -> Option<u32> + Sync),
}

impl std::fmt::Debug for SyncEndpoint<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncEndpoint").field("server_id", &self.server_id).finish_non_exhaustive()
    }
}

impl SyncEndpoint<'_> {
    /// Answers one HTTP request.
    pub fn answer(&self, request: &Request) -> Response {
        if request.path != "/sync" {
            return Response::text(404, "the sync endpoint is POST /sync");
        }
        if request.method != "POST" {
            return Response::text(405, "send POST /sync").with_header("Allow", "POST");
        }
        let token = request
            .header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim);
        let Some(user) = token.and_then(|token| (self.authenticate)(token)) else {
            return Response::text(401, "sign in and send the session token as Authorization: Bearer <token>")
                .with_header("WWW-Authenticate", "Bearer");
        };
        let Some(received) = received_of(request.query.as_deref()) else {
            return Response::text(400, "received= must be a whole number");
        };
        let incoming = match decode(&request.body) {
            Ok(changes) => changes,
            Err(error) => return Response::text(400, &error.to_string()),
        };
        let space = Replica::new(self.keyspace, user, self.server_id);
        if let Err(error) = space.apply(&incoming) {
            return Response::text(500, &error.to_string());
        }
        match space.changes_after(received, CHANGES_PER_ANSWER) {
            Ok((outgoing, next)) => {
                let mut body = Vec::new();
                encode(&outgoing, &mut body);
                Response::ok(CONTENT_TYPE, body).with_header("X-EggDB-Sync-Received", &next.to_string())
            }
            Err(error) => Response::text(500, &error.to_string()),
        }
    }
}

/// Syncs `client` with the server at `url` (such as `http://host:7600`) as the user `token`
/// signs in, sending at most `batch` changes and taking in the server's answer, and moves
/// `cursors` past what each side took in; returns how many rows changed on the client. Run it
/// again until it changes nothing to catch up fully. A dropped connection only repeats work.
pub fn sync_over_http(
    client: &Replica<'_>,
    cursors: &mut Cursors,
    url: &str,
    token: &str,
    batch: usize,
    timeout: Duration,
) -> Result<usize, SyncError> {
    let target = eggdb_http::client::parse_http_url(url).map_err(SyncError::Storage)?;
    let (outgoing, sent) = client.changes_after(cursors.sent, batch)?;
    let mut body = Vec::new();
    encode(&outgoing, &mut body);
    let headers = vec![
        ("Authorization".to_owned(), format!("Bearer {token}")),
        ("Content-Type".to_owned(), CONTENT_TYPE.to_owned()),
    ];
    let path = format!("/sync?received={}", cursors.received);
    let answer = eggdb_http::client::request(&target, "POST", &path, &headers, &body, timeout, 256 << 20)
        .map_err(|error| SyncError::Storage(format!("cannot reach {url}: {error}")))?;
    if answer.status != 200 {
        return Err(SyncError::Storage(format!(
            "{url} answered {}: {}",
            answer.status,
            String::from_utf8_lossy(&answer.body).trim()
        )));
    }
    let next = answer
        .headers
        .iter()
        .find(|(name, _)| name == RECEIVED_HEADER)
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
        .ok_or(SyncError::Malformed("the answer has no received cursor"))?;
    let incoming = decode(&answer.body)?;
    cursors.sent = sent;
    let changed = client.apply(&incoming)?;
    cursors.received = next;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::MemoryKeyspace;

    fn request(token: Option<&str>, query: &str, body: Vec<u8>) -> Request {
        let mut headers = Vec::new();
        if let Some(token) = token {
            headers.push(("authorization".to_owned(), format!("Bearer {token}")));
        }
        Request { method: "POST".to_owned(), path: "/sync".to_owned(), query: Some(query.to_owned()), headers, body }
    }

    fn signed_in(token: &str) -> Option<u32> {
        match token {
            "ada" => Some(1),
            "bob" => Some(2),
            _ => None,
        }
    }

    #[test]
    fn each_user_syncs_with_a_space_of_their_own() {
        let server = MemoryKeyspace::default();
        let endpoint = SyncEndpoint { keyspace: &server, server_id: 0, authenticate: &signed_in };
        let phone = MemoryKeyspace::default();
        let ada = Replica::new(&phone, 0, 11);
        ada.write(b"note", Some(b"from ada"), 1_000).unwrap();
        let (changes, _) = ada.changes_after(0, 100).unwrap();
        let mut body = Vec::new();
        encode(&changes, &mut body);
        let answer = endpoint.answer(&request(Some("ada"), "received=0", body));
        assert_eq!(answer.status, 200);
        let bob_answer = endpoint.answer(&request(Some("bob"), "received=0", {
            let mut empty = Vec::new();
            encode(&[], &mut empty);
            empty
        }));
        assert_eq!(decode(&bob_answer.body).unwrap(), Vec::new());
        let ada_answer = endpoint.answer(&request(Some("ada"), "received=0", {
            let mut empty = Vec::new();
            encode(&[], &mut empty);
            empty
        }));
        assert_eq!(decode(&ada_answer.body).unwrap().len(), 1);
    }

    #[test]
    fn a_request_without_a_valid_token_is_refused() {
        let server = MemoryKeyspace::default();
        let endpoint = SyncEndpoint { keyspace: &server, server_id: 0, authenticate: &signed_in };
        assert_eq!(endpoint.answer(&request(None, "received=0", Vec::new())).status, 401);
        assert_eq!(endpoint.answer(&request(Some("mallory"), "received=0", Vec::new())).status, 401);
        assert_eq!(endpoint.answer(&request(Some("ada"), "received=x", Vec::new())).status, 400);
        assert_eq!(endpoint.answer(&request(Some("ada"), "received=0", vec![1, 2, 3])).status, 400);
    }
}
