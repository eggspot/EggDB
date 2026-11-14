//! HTTP/3 (RFC 9114) over a [`ServerConnection`]: the server's control stream with its
//! SETTINGS, requests on client-opened bidirectional streams read as HEADERS and DATA frames and
//! answered by the same handlers as EggDB's HTTP/1.1 listeners, and the client's control and
//! QPACK streams read and set aside.

use crate::QuicCodecError;
use crate::connection::ServerConnection;
use crate::qpack::{decode_field_section, encode_field_section};
use crate::varint::{Cursor, write_varint};
use eggdb_http::{Request, Response};
use std::collections::BTreeMap;

mod frame_type {
    pub(super) const DATA: u64 = 0x00;
    pub(super) const HEADERS: u64 = 0x01;
    pub(super) const SETTINGS: u64 = 0x04;
}

mod stream_type {
    pub(super) const CONTROL: u64 = 0x00;
}

/// The ALPN protocol of HTTP/3.
pub const ALPN: &[u8] = b"h3";
const SETTINGS_MAX_FIELD_SECTION_SIZE: u64 = 0x06;
const MOST_FIELD_SECTION_BYTES: u64 = 64 * 1024;
const MOST_REQUEST_BODY_BYTES: usize = 64 << 20;
const H3_GENERAL_PROTOCOL_ERROR: u64 = 0x0101;
const H3_FRAME_UNEXPECTED: u64 = 0x0105;
const H3_MESSAGE_ERROR: u64 = 0x010e;
const H3_EXCESSIVE_LOAD: u64 = 0x0107;

#[derive(Default)]
struct RequestStream {
    buffered: Vec<u8>,
    fields: Option<Vec<(String, String)>>,
    body: Vec<u8>,
    answered: bool,
}

/// The HTTP/3 state of one connection.
#[derive(Default)]
pub struct Http3Session {
    control_opened: bool,
    requests: BTreeMap<u64, RequestStream>,
}

impl std::fmt::Debug for Http3Session {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Http3Session")
            .field("requests", &self.requests.len())
            .finish_non_exhaustive()
    }
}

fn write_frame(out: &mut Vec<u8>, kind: u64, payload: &[u8]) -> Result<(), QuicCodecError> {
    write_varint(out, kind)?;
    write_varint(out, payload.len() as u64)?;
    out.extend_from_slice(payload);
    Ok(())
}

fn request_of(fields: &[(String, String)], body: Vec<u8>) -> Option<Request> {
    let field = |name: &str| fields.iter().find(|(field, _)| field == name).map(|(_, value)| value.clone());
    let method = field(":method")?;
    let target = field(":path")?;
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (target, None),
    };
    let mut headers: Vec<(String, String)> =
        fields.iter().filter(|(name, _)| !name.starts_with(':')).cloned().collect();
    if let Some(authority) = field(":authority") {
        headers.push(("host".to_owned(), authority));
    }
    Some(Request { method, path, query, headers, body })
}

fn response_bytes(response: &Response, head_only: bool) -> Result<Vec<u8>, QuicCodecError> {
    let status = response.status.to_string();
    let length = response.body.len().to_string();
    let mut fields: Vec<(&str, &str)> = vec![(":status", status.as_str())];
    if !response.content_type.is_empty() {
        fields.push(("content-type", response.content_type.as_str()));
    }
    fields.push(("content-length", length.as_str()));
    let lowered: Vec<(String, &str)> =
        response.headers.iter().map(|(name, value)| (name.to_ascii_lowercase(), value.as_str())).collect();
    for (name, value) in &lowered {
        fields.push((name.as_str(), value));
    }
    let mut out = Vec::new();
    write_frame(&mut out, frame_type::HEADERS, &encode_field_section(&fields))?;
    if !head_only && !response.body.is_empty() {
        write_frame(&mut out, frame_type::DATA, &response.body)?;
    }
    Ok(out)
}

impl Http3Session {
    /// Moves the connection along: opens the control stream once the handshake completed, reads
    /// every stream with data, and answers each complete request with `handler`.
    pub fn drive(&mut self, connection: &mut ServerConnection<'_>, handler: &(dyn Fn(&Request) -> Response + Sync)) {
        if !connection.is_established() || connection.closed().is_some() {
            return;
        }
        if !self.control_opened {
            let Some(control) = connection.open_unidirectional_stream() else {
                return connection.close_with(H3_GENERAL_PROTOCOL_ERROR, "no room for the control stream");
            };
            let mut settings = Vec::new();
            let mut opening = Vec::new();
            let written = write_varint(&mut settings, SETTINGS_MAX_FIELD_SECTION_SIZE)
                .and_then(|()| write_varint(&mut settings, MOST_FIELD_SECTION_BYTES))
                .and_then(|()| write_varint(&mut opening, stream_type::CONTROL))
                .and_then(|()| write_frame(&mut opening, frame_type::SETTINGS, &settings));
            if written.is_err() {
                return;
            }
            connection.write_stream(control, &opening, false);
            self.control_opened = true;
        }
        for stream_id in connection.readable_streams() {
            let Some((data, ended)) = connection.read_stream(stream_id) else {
                continue;
            };
            if stream_id & 0x03 == 0x02 {
                continue;
            }
            if let Err(code) = self.on_request_data(stream_id, data, ended, connection, handler) {
                return connection.close_with(code, "the request does not follow HTTP/3");
            }
        }
    }

    fn on_request_data(
        &mut self,
        stream_id: u64,
        data: Vec<u8>,
        ended: bool,
        connection: &mut ServerConnection<'_>,
        handler: &(dyn Fn(&Request) -> Response + Sync),
    ) -> Result<(), u64> {
        let request = self.requests.entry(stream_id).or_default();
        if request.answered {
            return Ok(());
        }
        request.buffered.extend_from_slice(&data);
        loop {
            let mut cursor = Cursor::new(&request.buffered);
            let Ok(kind) = cursor.varint() else {
                break;
            };
            let Ok(length) = cursor.varint() else {
                break;
            };
            let length = usize::try_from(length).map_err(|_| H3_EXCESSIVE_LOAD)?;
            if cursor.remaining().len() < length {
                if length > MOST_REQUEST_BODY_BYTES {
                    return Err(H3_EXCESSIVE_LOAD);
                }
                break;
            }
            let payload = cursor.take(length).map_err(|_| H3_GENERAL_PROTOCOL_ERROR)?.to_vec();
            let consumed = cursor.position();
            request.buffered.drain(..consumed);
            match kind {
                frame_type::HEADERS if request.fields.is_none() => {
                    request.fields = Some(decode_field_section(&payload).map_err(|_| H3_MESSAGE_ERROR)?);
                }
                frame_type::HEADERS => {}
                frame_type::DATA if request.fields.is_some() => {
                    if request.body.len() + payload.len() > MOST_REQUEST_BODY_BYTES {
                        return Err(H3_EXCESSIVE_LOAD);
                    }
                    request.body.extend_from_slice(&payload);
                }
                frame_type::DATA | frame_type::SETTINGS => return Err(H3_FRAME_UNEXPECTED),
                _ => {}
            }
        }
        if !ended {
            return Ok(());
        }
        let fields = request.fields.take().ok_or(H3_MESSAGE_ERROR)?;
        let body = std::mem::take(&mut request.body);
        request.answered = true;
        let request = request_of(&fields, body).ok_or(H3_MESSAGE_ERROR)?;
        let response = handler(&request);
        let bytes = response_bytes(&response, request.method == "HEAD").map_err(|_| H3_GENERAL_PROTOCOL_ERROR)?;
        connection.write_stream(stream_id, &bytes, true);
        self.requests.remove(&stream_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_s_pseudo_fields_become_an_http_request() {
        let fields = vec![
            (":method".to_owned(), "POST".to_owned()),
            (":path".to_owned(), "/index/_search?size=1".to_owned()),
            (":authority".to_owned(), "db.example".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
        ];
        let request = request_of(&fields, b"{}".to_vec()).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/index/_search");
        assert_eq!(request.query.as_deref(), Some("size=1"));
        assert_eq!(request.header("host"), Some("db.example"));
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert!(request_of(&fields[2..], Vec::new()).is_none());
    }

    #[test]
    fn a_response_is_a_headers_frame_then_a_data_frame() {
        let response = Response {
            status: 200,
            content_type: "application/json".to_owned(),
            body: b"{\"ok\":true}".to_vec(),
            headers: vec![("X-Trace".to_owned(), "1".to_owned())],
        };
        let bytes = response_bytes(&response, false).unwrap();
        let mut cursor = Cursor::new(&bytes);
        assert_eq!(cursor.varint().unwrap(), frame_type::HEADERS);
        let length = cursor.length().unwrap();
        let fields = decode_field_section(cursor.take(length).unwrap()).unwrap();
        assert_eq!(fields[0], (":status".to_owned(), "200".to_owned()));
        assert!(fields.contains(&("x-trace".to_owned(), "1".to_owned())));
        assert_eq!(cursor.varint().unwrap(), frame_type::DATA);
        let length = cursor.length().unwrap();
        assert_eq!(cursor.take(length).unwrap(), b"{\"ok\":true}");
        assert!(response_bytes(&response, true).unwrap().len() < bytes.len());
    }
}
