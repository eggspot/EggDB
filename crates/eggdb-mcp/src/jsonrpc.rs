use crate::json_out::{JsonOut, encoded_string};
use eggdb_sql::json::{Json, Node, parse_document};
use std::io::{self, Read};

/// The largest message the server reads: 4 MiB. A longer line is refused and skipped.
pub const MAX_MESSAGE_BYTES: usize = 4 << 20;
const READ_CHUNK_BYTES: usize = 64 * 1024;

pub(crate) const PARSE_ERROR: i64 = -32700;
pub(crate) const INVALID_REQUEST: i64 = -32600;
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
pub(crate) const INVALID_PARAMS: i64 = -32602;
pub(crate) const INTERNAL_ERROR: i64 = -32603;
pub(crate) const RESOURCE_NOT_FOUND: i64 = -32002;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Incoming {
    Line(Vec<u8>),
    TooLong,
    End,
}

pub(crate) struct MessageReader<'r> {
    input: &'r mut dyn Read,
    pending: Vec<u8>,
    searched: usize,
    discarding: bool,
    ended: bool,
}

impl<'r> MessageReader<'r> {
    pub(crate) fn new(input: &'r mut dyn Read) -> Self {
        Self {
            input,
            pending: Vec::new(),
            searched: 0,
            discarding: false,
            ended: false,
        }
    }

    pub(crate) fn next_message(&mut self) -> io::Result<Incoming> {
        loop {
            if let Some(offset) = self.pending[self.searched..]
                .iter()
                .position(|&byte| byte == b'\n')
            {
                let at = self.searched + offset;
                let mut line: Vec<u8> = self.pending.drain(..=at).collect();
                self.searched = 0;
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if std::mem::take(&mut self.discarding) || line.len() > MAX_MESSAGE_BYTES {
                    return Ok(Incoming::TooLong);
                }
                return Ok(Incoming::Line(line));
            }
            if self.discarding {
                self.pending.clear();
            } else if self.pending.len() > MAX_MESSAGE_BYTES {
                self.pending.clear();
                self.discarding = true;
            }
            self.searched = self.pending.len();
            if self.ended {
                if std::mem::take(&mut self.discarding) {
                    return Ok(Incoming::TooLong);
                }
                if self.pending.is_empty() {
                    return Ok(Incoming::End);
                }
                self.searched = 0;
                return Ok(Incoming::Line(std::mem::take(&mut self.pending)));
            }
            let start = self.pending.len();
            self.pending.resize(start + READ_CHUNK_BYTES, 0);
            let read = loop {
                match self.input.read(&mut self.pending[start..]) {
                    Ok(read) => break read,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        self.pending.truncate(start);
                        return Err(error);
                    }
                }
            };
            self.pending.truncate(start + read);
            if read == 0 {
                self.ended = true;
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum Message {
    Request {
        id: String,
        method: String,
        params: Option<Node>,
    },
    Notification,
    Response,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) id: Option<String>,
    pub(crate) code: i64,
    pub(crate) message: &'static str,
    pub(crate) method: Option<String>,
}

impl Refusal {
    fn new(id: Option<String>, code: i64, message: &'static str) -> Self {
        Self {
            id,
            code,
            message,
            method: None,
        }
    }
}

pub(crate) fn parse_message(bytes: &[u8]) -> Result<Message, Refusal> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Refusal::new(None, PARSE_ERROR, "the message is not UTF-8 text"))?;
    if too_many_values(text) {
        return Err(Refusal::new(
            None,
            INVALID_REQUEST,
            "the message holds more than 100,000 JSON values",
        ));
    }
    let document = parse_document(text)
        .map_err(|_| Refusal::new(None, PARSE_ERROR, "the message is not valid JSON"))?;
    let method = match &document.value {
        Json::Object(members) => members.iter().find_map(|(name, node)| match &node.value {
            Json::String(method) if name == "method" => Some(method.clone()),
            _ => None,
        }),
        _ => None,
    };
    request_of(document).map_err(|mut refusal| {
        refusal.method = method;
        refusal
    })
}

const MOST_VALUES: usize = 100_000;

fn too_many_values(text: &str) -> bool {
    let mut values = 1usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in text.bytes() {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b',' | b'[' | b'{' | b':' => {
                values += 1;
                if values > MOST_VALUES {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn request_of(document: Node) -> Result<Message, Refusal> {
    if has_duplicate_keys(&document) {
        return Err(Refusal::new(
            None,
            INVALID_REQUEST,
            "the message has an object with a key given twice",
        ));
    }
    let members = match document.value {
        Json::Object(members) => members,
        Json::Array(_) => {
            return Err(Refusal::new(
                None,
                INVALID_REQUEST,
                "JSON-RPC batches are not supported: send one message per line",
            ));
        }
        _ => {
            return Err(Refusal::new(
                None,
                INVALID_REQUEST,
                "a JSON-RPC message is an object",
            ));
        }
    };
    let mut version = None;
    let mut id = None;
    let mut method = None;
    let mut params = None;
    let mut is_response = false;
    for (name, value) in members {
        match name.as_str() {
            "jsonrpc" => version = Some(value),
            "id" => id = Some(value),
            "method" => method = Some(value),
            "params" => params = Some(value),
            "result" | "error" => is_response = true,
            _ => {}
        }
    }
    let id = match id {
        None => None,
        Some(node) => Some(request_id(&node).ok_or_else(|| {
            Refusal::new(
                None,
                INVALID_REQUEST,
                "a request id is a string or an integer",
            )
        })?),
    };
    if !matches!(version.map(|node| node.value), Some(Json::String(version)) if version == "2.0") {
        return Err(Refusal::new(
            id,
            INVALID_REQUEST,
            "the message needs \"jsonrpc\": \"2.0\"",
        ));
    }
    let method = match method.map(|node| node.value) {
        Some(Json::String(method)) => method,
        Some(_) => {
            return Err(Refusal::new(
                id,
                INVALID_REQUEST,
                "a method name is a string",
            ));
        }
        None if is_response => return Ok(Message::Response),
        None => {
            return Err(Refusal::new(
                id,
                INVALID_REQUEST,
                "the message has no method",
            ));
        }
    };
    let Some(id) = id else {
        return Ok(Message::Notification);
    };
    if let Some(node) = &params
        && !matches!(node.value, Json::Object(_))
    {
        return Err(Refusal::new(
            Some(id),
            INVALID_PARAMS,
            "params is an object",
        ));
    }
    Ok(Message::Request { id, method, params })
}

fn request_id(node: &Node) -> Option<String> {
    match &node.value {
        Json::String(text) => Some(encoded_string(text)),
        Json::Number(text)
            if text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
                && text.len() <= 40 =>
        {
            Some(text.clone())
        }
        _ => None,
    }
}

fn has_duplicate_keys(node: &Node) -> bool {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        match &node.value {
            Json::Object(members) => {
                let mut names: Vec<&str> = members.iter().map(|(name, _)| name.as_str()).collect();
                names.sort_unstable();
                if names.windows(2).any(|pair| pair[0] == pair[1]) {
                    return true;
                }
                stack.extend(members.iter().map(|(_, value)| value));
            }
            Json::Array(items) => stack.extend(items.iter()),
            _ => {}
        }
    }
    false
}

pub(crate) fn result_line(id: &str, result: &str) -> String {
    let mut out = JsonOut::new();
    out.begin_object()
        .key("jsonrpc")
        .string("2.0")
        .key("id")
        .encoded(id)
        .key("result")
        .encoded(result)
        .end_object();
    out.finish()
}

pub(crate) fn error_line(id: Option<&str>, code: i64, message: &str, data: Option<&str>) -> String {
    let mut out = JsonOut::new();
    out.begin_object().key("jsonrpc").string("2.0").key("id");
    match id {
        Some(id) => out.encoded(id),
        None => out.null(),
    };
    out.key("error")
        .begin_object()
        .key("code")
        .signed(code)
        .key("message")
        .string(message);
    if let Some(data) = data {
        out.key("data").encoded(data);
    }
    out.end_object().end_object();
    out.finish()
}

#[cfg(test)]
mod tests {
    use super::{
        INVALID_PARAMS, INVALID_REQUEST, Incoming, MAX_MESSAGE_BYTES, Message, MessageReader,
        PARSE_ERROR, parse_message,
    };

    fn read_all(input: &[u8]) -> Vec<Incoming> {
        let mut input = input;
        let mut reader = MessageReader::new(&mut input);
        let mut messages = Vec::new();
        loop {
            let message = reader.next_message().unwrap();
            if message == Incoming::End {
                return messages;
            }
            messages.push(message);
        }
    }

    #[test]
    fn lines_split_on_newlines_and_drop_a_carriage_return() {
        assert_eq!(
            read_all(b"a\r\nb\n\nc"),
            [
                Incoming::Line(b"a".to_vec()),
                Incoming::Line(b"b".to_vec()),
                Incoming::Line(Vec::new()),
                Incoming::Line(b"c".to_vec()),
            ]
        );
    }

    #[test]
    fn an_oversized_line_is_one_refusal_and_the_next_line_still_reads() {
        let mut input = vec![b'x'; MAX_MESSAGE_BYTES + 100];
        input.extend_from_slice(b"\nok\n");
        input.extend(vec![b'y'; MAX_MESSAGE_BYTES * 2]);
        assert_eq!(
            read_all(&input),
            [
                Incoming::TooLong,
                Incoming::Line(b"ok".to_vec()),
                Incoming::TooLong
            ]
        );
    }

    #[test]
    fn requests_notifications_and_responses_are_told_apart() {
        let request = parse_message(br#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#).unwrap();
        assert!(matches!(request, Message::Request { ref id, .. } if id == "7"));
        let named =
            parse_message(br#"{"jsonrpc":"2.0","id":"a\u0001","method":"ping","params":{}}"#)
                .unwrap();
        assert!(matches!(named, Message::Request { ref id, .. } if id == "\"a\\u0001\""));
        assert!(matches!(
            parse_message(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap(),
            Message::Notification
        ));
        assert!(matches!(
            parse_message(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#).unwrap(),
            Message::Response
        ));
    }

    #[test]
    fn malformed_messages_are_refused_with_the_right_code() {
        for (text, code, has_id) in [
            (&b"{"[..], PARSE_ERROR, false),
            (b"\xff", PARSE_ERROR, false),
            (
                br#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
                INVALID_REQUEST,
                false,
            ),
            (br#""ping""#, INVALID_REQUEST, false),
            (
                br#"{"jsonrpc":"2.0","id":1,"id":2,"method":"ping"}"#,
                INVALID_REQUEST,
                false,
            ),
            (
                br#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
                INVALID_REQUEST,
                false,
            ),
            (
                br#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#,
                INVALID_REQUEST,
                false,
            ),
            (
                br#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
                INVALID_REQUEST,
                true,
            ),
            (
                br#"{"jsonrpc":"2.0","id":1,"method":3}"#,
                INVALID_REQUEST,
                true,
            ),
            (br#"{"jsonrpc":"2.0","id":1}"#, INVALID_REQUEST, true),
            (
                br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":[]}"#,
                INVALID_PARAMS,
                true,
            ),
        ] {
            let refusal = parse_message(text).unwrap_err();
            assert_eq!(refusal.code, code, "{}", String::from_utf8_lossy(text));
            assert_eq!(
                refusal.id.is_some(),
                has_id,
                "{}",
                String::from_utf8_lossy(text)
            );
        }
    }
}
