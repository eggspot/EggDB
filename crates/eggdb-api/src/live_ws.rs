//! Live queries and GraphQL subscriptions over WebSocket (D1178). `/graphql` speaks the
//! `graphql-transport-ws` subprotocol (graphql-ws's): `connection_init` and `connection_ack`,
//! then `subscribe` with an id and the operation, `next` with each changed result, `error`,
//! `complete` from either side, and `ping`/`pong`. `/realtime` speaks `eggdb-live`: a client
//! sends `{"type":"subscribe","id":..,"path":"/books?select=id,title&order=id"}` with a REST
//! read's path and query, and gets `{"type":"rows","id":..,"rows":[...]}` each time the rows
//! change, until it sends `{"type":"unsubscribe","id":..}`. Every subscription of a socket is
//! re-run only when the database committed a change (checked every interval), in the
//! signed-in user's session.

use crate::live::Polled;
use crate::request::{Query, identifier};
use crate::{Endpoint, json_string};
use eggdb_http::{Reply, Request, WebSocket, WebSocketMessage, websocket_reply};
use eggdb_sql::KeyValueStore;
use eggdb_sql::json::{Json, Node, parse_document};
use std::io;
use std::time::{Duration, Instant};

pub(crate) const GRAPHQL_TRANSPORT_WS: &str = "graphql-transport-ws";
pub(crate) const EGGDB_LIVE: &str = "eggdb-live";
const MOST_SUBSCRIPTIONS: usize = 64;
const INIT_TIMEOUT: Duration = Duration::from_secs(10);
const KEEP_ALIVE: Duration = Duration::from_secs(15);
const CHECK_INTERVAL: Duration = Duration::from_millis(250);
const CLOSE_UNAUTHORIZED: u16 = 4401;
const CLOSE_BAD_MESSAGE: u16 = 4400;
const CLOSE_INIT_TIMEOUT: u16 = 4408;
const CLOSE_DUPLICATE_ID: u16 = 4409;
const CLOSE_TOO_MANY_INITS: u16 = 4429;

struct Live<'p> {
    id: String,
    poll: Box<dyn FnMut() -> Polled + 'p>,
    last: Option<String>,
}

fn member<'n>(members: &'n [(String, Node)], name: &str) -> Option<&'n Node> {
    members.iter().find(|(key, _)| key == name).map(|(_, node)| node)
}

fn text_member(members: &[(String, Node)], name: &str) -> Option<String> {
    match member(members, name).map(|node| &node.value) {
        Some(Json::String(text)) => Some(text.clone()),
        _ => None,
    }
}

fn graphql_message(kind: &str, id: Option<&str>, payload: Option<&str>) -> String {
    let mut text = format!("{{\"type\":{}", json_string(kind));
    if let Some(id) = id {
        text.push_str(&format!(",\"id\":{}", json_string(id)));
    }
    if let Some(payload) = payload {
        text.push_str(&format!(",\"payload\":{payload}"));
    }
    text.push('}');
    text
}

fn live_message(kind: &str, id: &str, field: &str, value: &str) -> String {
    format!(
        "{{\"type\":{},\"id\":{},{}:{value}}}",
        json_string(kind),
        json_string(id),
        json_string(field)
    )
}

impl<'s, S: KeyValueStore> Endpoint<'s, S> {
    /// Switches a `/graphql` request to a `graphql-transport-ws` WebSocket.
    pub(crate) fn graphql_websocket(&self, request: &Request) -> Reply<'s> {
        let endpoint = self.copy();
        websocket_reply(request, &[GRAPHQL_TRANSPORT_WS], move |socket| endpoint.graphql_session(socket))
    }

    /// Switches a `/realtime` request to an `eggdb-live` WebSocket.
    pub(crate) fn live_websocket(&self, request: &Request) -> Reply<'s> {
        let endpoint = self.copy();
        websocket_reply(request, &[EGGDB_LIVE], move |socket| endpoint.live_session(socket))
    }

    fn graphql_session(&self, socket: &mut WebSocket<'_>) -> io::Result<()> {
        let opened = Instant::now();
        let mut acknowledged = false;
        let mut live: Vec<Live<'s>> = Vec::new();
        let mut last_check = Instant::now();
        let mut quiet_since = Instant::now();
        loop {
            match socket.poll_message()? {
                Some(WebSocketMessage::Closed(_)) => return Ok(()),
                Some(WebSocketMessage::Binary(_)) => return socket.close(CLOSE_BAD_MESSAGE),
                Some(WebSocketMessage::Text(text)) => {
                    quiet_since = Instant::now();
                    let Ok(document) = parse_document(&text) else {
                        return socket.close(CLOSE_BAD_MESSAGE);
                    };
                    let Json::Object(members) = &document.value else {
                        return socket.close(CLOSE_BAD_MESSAGE);
                    };
                    let kind = text_member(members, "type").unwrap_or_default();
                    let id = text_member(members, "id");
                    match kind.as_str() {
                        "connection_init" => {
                            if acknowledged {
                                return socket.close(CLOSE_TOO_MANY_INITS);
                            }
                            acknowledged = true;
                            socket.send_text(&graphql_message("connection_ack", None, None))?;
                        }
                        "ping" => socket.send_text(&graphql_message("pong", None, None))?,
                        "pong" => {}
                        "subscribe" => {
                            if !acknowledged {
                                return socket.close(CLOSE_UNAUTHORIZED);
                            }
                            let Some(id) = id else {
                                return socket.close(CLOSE_BAD_MESSAGE);
                            };
                            if live.iter().any(|known| known.id == id) {
                                return socket.close(CLOSE_DUPLICATE_ID);
                            }
                            let Some(payload) = member(members, "payload") else {
                                return socket.close(CLOSE_BAD_MESSAGE);
                            };
                            if live.len() >= MOST_SUBSCRIPTIONS {
                                let errors = format!("[{{\"message\":\"at most {MOST_SUBSCRIPTIONS} subscriptions on one socket\"}}]");
                                socket.send_text(&graphql_message("error", Some(&id), Some(&errors)))?;
                                continue;
                            }
                            match self.graphql_poller(&text[payload.start..payload.end]) {
                                Ok(poll) => {
                                    live.push(Live { id, poll, last: None });
                                    last_check = Instant::now().checked_sub(CHECK_INTERVAL).unwrap_or_else(Instant::now);
                                }
                                Err(errors) => socket.send_text(&graphql_message("error", Some(&id), Some(&errors)))?,
                            }
                        }
                        "complete" => {
                            if let Some(id) = id {
                                live.retain(|known| known.id != id);
                            }
                        }
                        _ => return socket.close(CLOSE_BAD_MESSAGE),
                    }
                }
                None => {}
            }
            if !acknowledged && opened.elapsed() >= INIT_TIMEOUT {
                return socket.close(CLOSE_INIT_TIMEOUT);
            }
            if last_check.elapsed() >= CHECK_INTERVAL {
                last_check = Instant::now();
                let mut finished = Vec::new();
                for subscription in &mut live {
                    match (subscription.poll)() {
                        Polled::Result(payload) => {
                            if subscription.last.as_deref() != Some(payload.as_str()) {
                                socket.send_text(&graphql_message("next", Some(&subscription.id), Some(&payload)))?;
                                subscription.last = Some(payload);
                                quiet_since = Instant::now();
                            }
                        }
                        Polled::Failed(payload) => {
                            socket.send_text(&graphql_message("next", Some(&subscription.id), Some(&payload)))?;
                            socket.send_text(&graphql_message("complete", Some(&subscription.id), None))?;
                            finished.push(subscription.id.clone());
                        }
                        Polled::Unchanged => {}
                    }
                }
                live.retain(|subscription| !finished.contains(&subscription.id));
            }
            if quiet_since.elapsed() >= KEEP_ALIVE {
                socket.send_text(&graphql_message("ping", None, None))?;
                quiet_since = Instant::now();
            }
        }
    }

    fn live_session(&self, socket: &mut WebSocket<'_>) -> io::Result<()> {
        let mut live: Vec<Live<'s>> = Vec::new();
        let mut last_check = Instant::now();
        let mut quiet_since = Instant::now();
        loop {
            match socket.poll_message()? {
                Some(WebSocketMessage::Closed(_)) => return Ok(()),
                Some(WebSocketMessage::Binary(_)) => return socket.close(CLOSE_BAD_MESSAGE),
                Some(WebSocketMessage::Text(text)) => {
                    quiet_since = Instant::now();
                    let Ok(document) = parse_document(&text) else {
                        return socket.close(CLOSE_BAD_MESSAGE);
                    };
                    let Json::Object(members) = &document.value else {
                        return socket.close(CLOSE_BAD_MESSAGE);
                    };
                    let kind = text_member(members, "type").unwrap_or_default();
                    let id = text_member(members, "id").unwrap_or_default();
                    match kind.as_str() {
                        "subscribe" => {
                            if live.iter().any(|known| known.id == id) {
                                return socket.close(CLOSE_DUPLICATE_ID);
                            }
                            if live.len() >= MOST_SUBSCRIPTIONS {
                                let error = json_string(&format!("at most {MOST_SUBSCRIPTIONS} subscriptions on one socket"));
                                socket.send_text(&live_message("error", &id, "message", &error))?;
                                continue;
                            }
                            let path = text_member(members, "path").unwrap_or_default();
                            match self.path_poller(&path) {
                                Ok(poll) => {
                                    live.push(Live { id, poll, last: None });
                                    last_check = Instant::now().checked_sub(CHECK_INTERVAL).unwrap_or_else(Instant::now);
                                }
                                Err(message) => {
                                    socket.send_text(&live_message("error", &id, "message", &json_string(&message)))?;
                                }
                            }
                        }
                        "unsubscribe" => live.retain(|known| known.id != id),
                        "ping" => socket.send_text("{\"type\":\"pong\"}")?,
                        _ => return socket.close(CLOSE_BAD_MESSAGE),
                    }
                }
                None => {}
            }
            if last_check.elapsed() >= CHECK_INTERVAL {
                last_check = Instant::now();
                let mut finished = Vec::new();
                for subscription in &mut live {
                    match (subscription.poll)() {
                        Polled::Result(rows) => {
                            if subscription.last.as_deref() != Some(rows.as_str()) {
                                socket.send_text(&live_message("rows", &subscription.id, "rows", &rows))?;
                                subscription.last = Some(rows);
                                quiet_since = Instant::now();
                            }
                        }
                        Polled::Failed(error) => {
                            socket.send_text(&live_message("error", &subscription.id, "error", &error))?;
                            finished.push(subscription.id.clone());
                        }
                        Polled::Unchanged => {}
                    }
                }
                live.retain(|subscription| !finished.contains(&subscription.id));
            }
            if quiet_since.elapsed() >= KEEP_ALIVE {
                socket.ping()?;
                quiet_since = Instant::now();
            }
        }
    }

    fn path_poller(&self, path: &str) -> Result<Box<dyn FnMut() -> Polled + 's>, String> {
        let (table, query) = path.trim_start_matches('/').split_once('?').unwrap_or((path.trim_start_matches('/'), ""));
        let quoted = identifier(table).ok_or_else(|| format!("no table named {table}"))?;
        let query = Query::parse(query)?;
        let target = format!(
            "{}.{quoted}",
            identifier(&self.schema).unwrap_or_else(|| "public".to_owned())
        );
        let sql = self
            .read_sql(&target, table, &query)
            .map_err(|response| String::from_utf8(response.body).unwrap_or_default())?;
        Ok(Box::new(self.rows_poller(sql)))
    }
}
