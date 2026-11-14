//! Realtime channels in Supabase's style (D1182): `GET /realtime/v1/websocket` upgrades to a
//! WebSocket speaking the Phoenix channel protocol that `supabase-js`'s realtime client uses
//! (messages as JSON objects, `vsn=1.0.0`, or as arrays, `vsn=2.0.0`). A channel joined with
//! `postgres_changes` in its config receives each committed `INSERT`, `UPDATE` and `DELETE` of
//! the tables it names, with the row before and after, read from the engine's change capture
//! (D1030); `broadcast` messages come back to their sender when the channel asks for `self`;
//! `heartbeat` on `phoenix` is answered. Supabase is a trademark of its owners; EggDB is
//! compatible with its realtime client.

use crate::Endpoint;
use crate::request::identifier;
use crate::json_string;
use eggdb_http::{Reply, Request, WebSocket, WebSocketMessage, websocket_reply};
use eggdb_sql::KeyValueStore;
use eggdb_sql::json::{Json, Node, parse_document};
use std::io;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CHECK_INTERVAL: Duration = Duration::from_millis(250);
const QUICKEST_CHECK: Duration = Duration::from_millis(50);
const MOST_CHANNELS: usize = 100;
const MOST_COMMITS_A_CHECK: u64 = 256;
const CLOSE_BAD_MESSAGE: u16 = 1003;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Filter {
    column: String,
    operator: String,
    value: String,
}

#[derive(Debug, Clone)]
struct ChangeListener {
    id: u64,
    event: String,
    schema: String,
    table: String,
    filter: Option<Filter>,
}

struct Channel {
    topic: String,
    join_ref: Option<String>,
    listeners: Vec<ChangeListener>,
    broadcast_self: bool,
}

struct Wire {
    arrays: bool,
}

impl Wire {
    fn message(&self, join_ref: Option<&str>, reference: Option<&str>, topic: &str, event: &str, payload: &str) -> String {
        let quoted = |text: Option<&str>| text.map_or_else(|| "null".to_owned(), json_string);
        if self.arrays {
            format!(
                "[{},{},{},{},{payload}]",
                quoted(join_ref),
                quoted(reference),
                json_string(topic),
                json_string(event)
            )
        } else {
            let mut text = format!(
                "{{\"topic\":{},\"event\":{},\"payload\":{payload},\"ref\":{}",
                json_string(topic),
                json_string(event),
                quoted(reference)
            );
            if let Some(join_ref) = join_ref {
                text.push_str(&format!(",\"join_ref\":{}", json_string(join_ref)));
            }
            text.push('}');
            text
        }
    }
}

struct Incoming<'t> {
    join_ref: Option<String>,
    reference: Option<String>,
    topic: String,
    event: String,
    payload: Option<&'t Node>,
}

fn text_of(node: &Node) -> Option<String> {
    match &node.value {
        Json::String(text) | Json::Number(text) => Some(text.clone()),
        _ => None,
    }
}

fn member<'n>(members: &'n [(String, Node)], name: &str) -> Option<&'n Node> {
    members.iter().find(|(key, _)| key == name).map(|(_, node)| node)
}

fn incoming(document: &Node) -> Option<Incoming<'_>> {
    match &document.value {
        Json::Array(parts) => {
            let [join_ref, reference, topic, event, payload] = parts.as_slice() else {
                return None;
            };
            Some(Incoming {
                join_ref: text_of(join_ref),
                reference: text_of(reference),
                topic: text_of(topic)?,
                event: text_of(event)?,
                payload: Some(payload),
            })
        }
        Json::Object(members) => Some(Incoming {
            join_ref: member(members, "join_ref").and_then(text_of),
            reference: member(members, "ref").and_then(text_of),
            topic: member(members, "topic").and_then(text_of)?,
            event: member(members, "event").and_then(text_of)?,
            payload: member(members, "payload"),
        }),
        _ => None,
    }
}

fn parse_filter(text: &str) -> Option<Filter> {
    let (column, rest) = text.split_once('=')?;
    let (operator, value) = rest.split_once('.')?;
    matches!(operator, "eq" | "neq" | "lt" | "lte" | "gt" | "gte" | "in").then(|| Filter {
        column: column.trim().to_owned(),
        operator: operator.to_owned(),
        value: value.to_owned(),
    })
}

fn field_text(record: &str, column: &str) -> Option<String> {
    let document = parse_document(record).ok()?;
    let Json::Object(members) = &document.value else {
        return None;
    };
    let node = member(members, column)?;
    Some(match &node.value {
        Json::String(text) | Json::Number(text) => text.clone(),
        Json::Boolean(flag) => flag.to_string(),
        Json::Null => "null".to_owned(),
        _ => record[node.start..node.end].to_owned(),
    })
}

fn compared(found: &str, wanted: &str) -> std::cmp::Ordering {
    match (found.parse::<f64>(), wanted.parse::<f64>()) {
        (Ok(left), Ok(right)) => left.partial_cmp(&right).unwrap_or(std::cmp::Ordering::Equal),
        _ => found.cmp(wanted),
    }
}

fn passes(filter: &Filter, record: &str) -> bool {
    let Some(found) = field_text(record, &filter.column) else {
        return false;
    };
    use std::cmp::Ordering::{Equal, Greater, Less};
    match filter.operator.as_str() {
        "eq" => compared(&found, &filter.value) == Equal,
        "neq" => compared(&found, &filter.value) != Equal,
        "lt" => compared(&found, &filter.value) == Less,
        "lte" => compared(&found, &filter.value) != Greater,
        "gt" => compared(&found, &filter.value) == Greater,
        "gte" => compared(&found, &filter.value) != Less,
        "in" => filter
            .value
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(',')
            .any(|item| compared(&found, item.trim().trim_matches('"')) == Equal),
        _ => false,
    }
}

fn utc_now() -> String {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since.as_secs();
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60,
        since.subsec_millis()
    )
}

impl<'s, S: KeyValueStore> Endpoint<'s, S> {
    /// Switches `/realtime/v1/websocket` to a Phoenix-channel WebSocket.
    pub(crate) fn realtime_channels(&self, request: &Request) -> Reply<'s> {
        let endpoint = self.copy();
        let arrays = request
            .query
            .as_deref()
            .unwrap_or_default()
            .split('&')
            .any(|pair| pair == "vsn=2.0.0");
        websocket_reply(request, &[], move |socket| endpoint.channels_session(socket, &Wire { arrays }))
    }

    fn may_read(&self, schema: &str, table: &str) -> Result<(), String> {
        let (Some(schema), Some(table)) = (identifier(schema), identifier(table)) else {
            return Err("the schema or table cannot be named in SQL".to_owned());
        };
        self.run(&format!("SELECT 1 FROM {schema}.{table} LIMIT 0"))
            .map(drop)
            .map_err(|error| error.to_string())
    }

    fn join(&self, channel_id: &mut u64, incoming: &Incoming<'_>) -> Result<(Channel, String), String> {
        let config = incoming
            .payload
            .and_then(|payload| match &payload.value {
                Json::Object(members) => member(members, "config"),
                _ => None,
            })
            .and_then(|config| match &config.value {
                Json::Object(members) => Some(members),
                _ => None,
            });
        let mut listeners = Vec::new();
        let mut broadcast_self = false;
        if let Some(config) = config {
            if let Some(Json::Object(broadcast)) = member(config, "broadcast").map(|node| &node.value) {
                broadcast_self = matches!(member(broadcast, "self").map(|node| &node.value), Some(Json::Boolean(true)));
            }
            if let Some(Json::Array(changes)) = member(config, "postgres_changes").map(|node| &node.value) {
                for change in changes {
                    let Json::Object(fields) = &change.value else {
                        return Err("each postgres_changes entry is an object".to_owned());
                    };
                    let event = member(fields, "event").and_then(text_of).unwrap_or_else(|| "*".to_owned());
                    if !matches!(event.as_str(), "*" | "INSERT" | "UPDATE" | "DELETE") {
                        return Err(format!("'{event}' is not a postgres_changes event"));
                    }
                    let schema = member(fields, "schema").and_then(text_of).unwrap_or_else(|| "public".to_owned());
                    let table = member(fields, "table").and_then(text_of).unwrap_or_default();
                    if table.is_empty() || table == "*" {
                        return Err("postgres_changes names one table".to_owned());
                    }
                    let filter = match member(fields, "filter").and_then(text_of) {
                        Some(text) => Some(parse_filter(&text).ok_or_else(|| format!("'{text}' is not a filter"))?),
                        None => None,
                    };
                    self.may_read(&schema, &table)?;
                    *channel_id += 1;
                    listeners.push(ChangeListener {
                        id: *channel_id,
                        event,
                        schema,
                        table,
                        filter,
                    });
                }
            }
        }
        let response: Vec<String> = listeners
            .iter()
            .map(|listener| {
                let filter = listener
                    .filter
                    .as_ref()
                    .map_or_else(String::new, |filter| {
                        format!(
                            ",\"filter\":{}",
                            json_string(&format!("{}={}.{}", filter.column, filter.operator, filter.value))
                        )
                    });
                format!(
                    "{{\"id\":{},\"event\":{},\"schema\":{},\"table\":{}{filter}}}",
                    listener.id,
                    json_string(&listener.event),
                    json_string(&listener.schema),
                    json_string(&listener.table)
                )
            })
            .collect();
        Ok((
            Channel {
                topic: incoming.topic.clone(),
                join_ref: incoming.join_ref.clone().or_else(|| incoming.reference.clone()),
                listeners,
                broadcast_self,
            },
            format!("{{\"postgres_changes\":[{}]}}", response.join(",")),
        ))
    }

    fn channels_session(&self, socket: &mut WebSocket<'_>, wire: &Wire) -> io::Result<()> {
        let mut channels: Vec<Channel> = Vec::new();
        let mut channel_id = 0u64;
        let mut position = eggdb_sql::cdc::latest_capture_sequence(self.store).unwrap_or_default();
        let mut last_check = Instant::now();
        let mut seen_generation = eggdb_sql::commit_generation();
        loop {
            match socket.poll_message()? {
                Some(WebSocketMessage::Closed(_)) => return Ok(()),
                Some(WebSocketMessage::Binary(_)) => return socket.close(CLOSE_BAD_MESSAGE),
                Some(WebSocketMessage::Text(text)) => {
                    let Ok(document) = parse_document(&text) else {
                        return socket.close(CLOSE_BAD_MESSAGE);
                    };
                    let Some(message) = incoming(&document) else {
                        return socket.close(CLOSE_BAD_MESSAGE);
                    };
                    let reply = |status: &str, response: &str| {
                        wire.message(
                            message.join_ref.as_deref(),
                            message.reference.as_deref(),
                            &message.topic,
                            "phx_reply",
                            &format!("{{\"status\":{},\"response\":{response}}}", json_string(status)),
                        )
                    };
                    match message.event.as_str() {
                        "heartbeat" => socket.send_text(&reply("ok", "{}"))?,
                        "phx_join" => {
                            if channels.len() >= MOST_CHANNELS {
                                socket.send_text(&reply("error", "{\"reason\":\"too many channels on one socket\"}"))?;
                                continue;
                            }
                            match self.join(&mut channel_id, &message) {
                                Ok((channel, response)) => {
                                    socket.send_text(&reply("ok", &response))?;
                                    if !channel.listeners.is_empty() {
                                        let subtopic = channel.topic.strip_prefix("realtime:").unwrap_or(&channel.topic);
                                        socket.send_text(&wire.message(
                                            channel.join_ref.as_deref(),
                                            None,
                                            &channel.topic,
                                            "system",
                                            &format!(
                                                "{{\"channel\":{},\"extension\":\"postgres_changes\",\"message\":\"Subscribed to PostgreSQL\",\"status\":\"ok\"}}",
                                                json_string(subtopic)
                                            ),
                                        ))?;
                                    }
                                    socket.send_text(&wire.message(channel.join_ref.as_deref(), None, &channel.topic, "presence_state", "{}"))?;
                                    channels.retain(|known| known.topic != channel.topic);
                                    channels.push(channel);
                                }
                                Err(reason) => {
                                    socket.send_text(&reply("error", &format!("{{\"reason\":{}}}", json_string(&reason))))?;
                                }
                            }
                        }
                        "phx_leave" => {
                            channels.retain(|known| known.topic != message.topic);
                            socket.send_text(&reply("ok", "{}"))?;
                        }
                        "broadcast" => {
                            let payload = message.payload.map_or("{}", |payload| &text[payload.start..payload.end]);
                            if let Some(channel) = channels.iter().find(|known| known.topic == message.topic)
                                && channel.broadcast_self
                            {
                                socket.send_text(&wire.message(channel.join_ref.as_deref(), None, &channel.topic, "broadcast", payload))?;
                            }
                            if message.reference.is_some() {
                                socket.send_text(&reply("ok", "{}"))?;
                            }
                        }
                        "access_token" | "presence" => socket.send_text(&reply("ok", "{}"))?,
                        _ => socket.send_text(&reply("error", "{\"reason\":\"unknown event\"}"))?,
                    }
                }
                None => {}
            }
            let generation = eggdb_sql::commit_generation();
            let elapsed = last_check.elapsed();
            let due = elapsed >= CHECK_INTERVAL || (generation != seen_generation && elapsed >= QUICKEST_CHECK);
            if due && channels.iter().any(|channel| !channel.listeners.is_empty()) {
                last_check = Instant::now();
                seen_generation = generation;
                let Ok((last, changes)) = eggdb_sql::cdc::captured_changes_after(self.store, position, MOST_COMMITS_A_CHECK) else {
                    continue;
                };
                position = last;
                for change in &changes {
                    self.send_change(socket, wire, &channels, change)?;
                }
            }
        }
    }

    fn send_change(
        &self,
        socket: &mut WebSocket<'_>,
        wire: &Wire,
        channels: &[Channel],
        change: &eggdb_sql::cdc::CapturedChange,
    ) -> io::Result<()> {
        let record = change.after.as_deref();
        let old = change.before.as_deref();
        for channel in channels {
            let ids: Vec<String> = channel
                .listeners
                .iter()
                .filter(|listener| {
                    listener.schema == change.schema
                        && listener.table == change.table
                        && (listener.event == "*" || listener.event == change.kind)
                        && listener
                            .filter
                            .as_ref()
                            .is_none_or(|filter| record.or(old).is_some_and(|row| passes(filter, row)))
                })
                .map(|listener| listener.id.to_string())
                .collect();
            if ids.is_empty() {
                continue;
            }
            let payload = format!(
                "{{\"ids\":[{}],\"data\":{{\"schema\":{},\"table\":{},\"commit_timestamp\":{},\"type\":{},\"eventType\":{},\"record\":{},\"old_record\":{},\"columns\":[],\"errors\":null}}}}",
                ids.join(","),
                json_string(&change.schema),
                json_string(&change.table),
                json_string(&utc_now()),
                json_string(change.kind),
                json_string(change.kind),
                record.unwrap_or("{}"),
                old.unwrap_or("{}")
            );
            socket.send_text(&wire.message(channel.join_ref.as_deref(), None, &channel.topic, "postgres_changes", &payload))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_follow_supabase_realtime() {
        let filter = parse_filter("id=gte.10").unwrap();
        assert!(passes(&filter, "{\"id\":12,\"name\":\"a\"}"));
        assert!(!passes(&filter, "{\"id\":9}"));
        let listed = parse_filter("name=in.(egg,\"hen\")").unwrap();
        assert!(passes(&listed, "{\"name\":\"hen\"}"));
        assert!(parse_filter("id=like.5").is_none());
        assert_eq!(utc_now().len(), 24);
    }

    #[test]
    fn messages_are_objects_or_arrays_by_protocol_version() {
        let objects = Wire { arrays: false };
        assert_eq!(
            objects.message(None, Some("1"), "phoenix", "phx_reply", "{}"),
            "{\"topic\":\"phoenix\",\"event\":\"phx_reply\",\"payload\":{},\"ref\":\"1\"}"
        );
        let arrays = Wire { arrays: true };
        assert_eq!(
            arrays.message(Some("3"), None, "realtime:x", "broadcast", "{}"),
            "[\"3\",null,\"realtime:x\",\"broadcast\",{}]"
        );
    }
}
