//! The OPC UA client (D1071): the TCP binary transport's `HEL`/`ACK`, a secure channel with the
//! `None` security policy (renewed before its token runs out), a session activated anonymously
//! or with a user name, one subscription with a monitored item per node, and `Publish` requests
//! whose data changes are handed back as they arrive.

use crate::binary::{NodeId, Reader, Writer};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const PROTOCOL_VERSION: u32 = 0;
const BUFFER_SIZE: u32 = 1 << 16;
const MOST_MESSAGE: usize = 16 << 20;
const MOST_CHUNKS: usize = 4_096;
const POLICY_NONE: &str = "http://opcfoundation.org/UA/SecurityPolicy#None";
const SECURITY_MODE_NONE: u32 = 1;
const TOKEN_ANONYMOUS: u32 = 0;
const TOKEN_USER_NAME: u32 = 1;
const REQUESTED_LIFETIME_MS: u32 = 3_600_000;
const SESSION_TIMEOUT_MS: f64 = 120_000.0;
const READ_TIMEOUT: Duration = Duration::from_secs(60);

const OPEN_SECURE_CHANNEL_REQUEST: u32 = 446;
const OPEN_SECURE_CHANNEL_RESPONSE: u32 = 449;
const CLOSE_SECURE_CHANNEL_REQUEST: u32 = 452;
const CREATE_SESSION_REQUEST: u32 = 461;
const CREATE_SESSION_RESPONSE: u32 = 464;
const ACTIVATE_SESSION_REQUEST: u32 = 467;
const ACTIVATE_SESSION_RESPONSE: u32 = 470;
const CLOSE_SESSION_REQUEST: u32 = 473;
const CREATE_SUBSCRIPTION_REQUEST: u32 = 787;
const CREATE_SUBSCRIPTION_RESPONSE: u32 = 790;
const CREATE_MONITORED_ITEMS_REQUEST: u32 = 751;
const CREATE_MONITORED_ITEMS_RESPONSE: u32 = 754;
const PUBLISH_REQUEST: u32 = 826;
const PUBLISH_RESPONSE: u32 = 829;
const SERVICE_FAULT: u32 = 397;
const ANONYMOUS_IDENTITY_TOKEN: u32 = 321;
const USER_NAME_IDENTITY_TOKEN: u32 = 324;
const DATA_CHANGE_NOTIFICATION: u32 = 811;
const STATUS_CHANGE_NOTIFICATION: u32 = 820;

/// Where and how to connect: `opc.tcp://[user:password@]host:port[/path]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The URL sent to the server, without the user and password.
    pub url: String,
    /// The `host:port` to connect to.
    pub address: String,
    /// The user name and password, when the URL gave them.
    pub login: Option<(String, String)>,
}

impl Endpoint {
    /// Reads an `opc.tcp://` URL, taking a `user:password@` part out of it.
    pub fn parse(text: &str) -> Result<Self, String> {
        let rest = text.strip_prefix("opc.tcp://").ok_or_else(|| format!("{text} is not an opc.tcp:// URL"))?;
        let (authority, path) = rest.split_once('/').map_or((rest, ""), |(authority, path)| (authority, path));
        let (login, host) = match authority.rsplit_once('@') {
            Some((login, host)) => {
                let (user, password) = login.split_once(':').ok_or_else(|| format!("{text}: write user:password@"))?;
                (Some((user.to_owned(), password.to_owned())), host)
            }
            None => (None, authority),
        };
        if host.is_empty() {
            return Err(format!("{text} names no host"));
        }
        let address = if host.contains(':') && !host.ends_with(']') { host.to_owned() } else { format!("{host}:4840") };
        let url = if path.is_empty() { format!("opc.tcp://{host}") } else { format!("opc.tcp://{host}/{path}") };
        Ok(Self { url, address, login })
    }
}

/// One value change a subscription delivered: the monitored item's handle (the node's index in
/// the subscribed list) and its DataValue as `opcua_decode` gives it.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// The index of the node in the list given to [`Client::subscribe`].
    pub handle: u32,
    /// The DataValue as JSON.
    pub data_value: String,
}

/// A connected, signed-in OPC UA session.
#[derive(Debug)]
pub struct Client {
    stream: TcpStream,
    url: String,
    channel_id: u32,
    token_id: u32,
    token_lifetime: Duration,
    token_since: Instant,
    sequence: u32,
    request_id: u32,
    authentication: NodeId,
    subscription: Option<u32>,
    acknowledgements: Vec<(u32, u32)>,
    remote_buffer: usize,
}

fn header(kind: &[u8; 3], chunk: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(kind);
    out.push(chunk);
    out.extend_from_slice(&u32::try_from(body.len() + 8).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(body);
    out
}

fn type_of(node: &NodeId) -> u32 {
    match node {
        NodeId::Numeric(0, id) => *id,
        _ => 0,
    }
}

impl Client {
    fn read_frame(&mut self) -> Result<([u8; 3], u8, Vec<u8>), String> {
        let mut head = [0u8; 8];
        self.stream.read_exact(&mut head).map_err(|error| error.to_string())?;
        let size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        if !(8..=MOST_MESSAGE).contains(&size) {
            return Err(format!("the server sent a message of {size} bytes"));
        }
        let mut body = vec![0u8; size - 8];
        self.stream.read_exact(&mut body).map_err(|error| error.to_string())?;
        let kind = [head[0], head[1], head[2]];
        if &kind == b"ERR" {
            let mut reader = Reader::new(&body);
            let code = reader.u32().unwrap_or(0);
            let reason = reader.string().ok().flatten().unwrap_or_default();
            return Err(format!("the server closed the connection with 0x{code:08x}: {reason}"));
        }
        Ok((kind, head[3], body))
    }

    /// Connects to `endpoint`, opens a secure channel and signs a session in.
    pub fn connect(endpoint: &Endpoint) -> Result<Self, String> {
        let stream = TcpStream::connect(&endpoint.address).map_err(|error| format!("cannot reach {}: {error}", endpoint.address))?;
        stream.set_read_timeout(Some(READ_TIMEOUT)).map_err(|error| error.to_string())?;
        stream.set_nodelay(true).map_err(|error| error.to_string())?;
        let mut client = Self {
            stream,
            url: endpoint.url.clone(),
            channel_id: 0,
            token_id: 0,
            token_lifetime: Duration::from_millis(u64::from(REQUESTED_LIFETIME_MS)),
            token_since: Instant::now(),
            sequence: 0,
            request_id: 0,
            authentication: NodeId::NULL,
            subscription: None,
            acknowledgements: Vec::new(),
            remote_buffer: BUFFER_SIZE as usize,
        };
        client.hello()?;
        client.open_channel(false)?;
        let policies = client.create_session()?;
        client.activate_session(endpoint.login.as_ref(), &policies)?;
        Ok(client)
    }

    fn hello(&mut self) -> Result<(), String> {
        let mut body = Writer::default();
        body.u32(PROTOCOL_VERSION).u32(BUFFER_SIZE).u32(BUFFER_SIZE).u32(MOST_MESSAGE as u32).u32(MOST_CHUNKS as u32).string(Some(&self.url));
        self.stream.write_all(&header(b"HEL", b'F', &body.bytes)).map_err(|error| error.to_string())?;
        let (kind, _, body) = self.read_frame()?;
        if &kind != b"ACK" {
            return Err("the server did not acknowledge the hello".to_owned());
        }
        let mut reader = Reader::new(&body);
        reader.u32()?;
        let receive_buffer = reader.u32()? as usize;
        self.remote_buffer = receive_buffer.clamp(8_192, MOST_MESSAGE);
        Ok(())
    }

    fn next_ids(&mut self) -> (u32, u32) {
        self.sequence = self.sequence.wrapping_add(1).max(1);
        self.request_id = self.request_id.wrapping_add(1).max(1);
        (self.sequence, self.request_id)
    }

    fn request_header(&mut self, writer: &mut Writer, handle: u32, timeout_ms: u32) {
        let token = self.authentication.clone();
        writer.node_id(&token).now().u32(handle).u32(0).string(None).u32(timeout_ms).no_extension_object();
    }

    fn open_channel(&mut self, renew: bool) -> Result<(), String> {
        let (sequence, request_id) = self.next_ids();
        let mut body = Writer::default();
        body.u32(self.channel_id).string(Some(POLICY_NONE)).byte_string(None).byte_string(None).u32(sequence).u32(request_id);
        body.node_id(&NodeId::Numeric(0, OPEN_SECURE_CHANNEL_REQUEST));
        self.request_header(&mut body, request_id, 10_000);
        body.u32(PROTOCOL_VERSION).u32(u32::from(renew)).u32(SECURITY_MODE_NONE).byte_string(None).u32(REQUESTED_LIFETIME_MS);
        self.stream.write_all(&header(b"OPN", b'F', &body.bytes)).map_err(|error| error.to_string())?;
        let (kind, _, response) = self.read_frame()?;
        if &kind != b"OPN" {
            return Err("the server did not answer the secure channel request".to_owned());
        }
        let mut reader = Reader::new(&response);
        self.channel_id = reader.u32()?;
        reader.string()?;
        reader.byte_string()?;
        reader.byte_string()?;
        reader.u32()?;
        reader.u32()?;
        let type_id = type_of(&reader.node_id()?);
        if type_id != OPEN_SECURE_CHANNEL_RESPONSE {
            return Err(format!("the server answered the secure channel request with type {type_id}"));
        }
        let result = reader.response_header()?;
        if result != 0 {
            return Err(format!("the server refused the secure channel: 0x{result:08x}"));
        }
        reader.u32()?;
        self.channel_id = reader.u32()?;
        self.token_id = reader.u32()?;
        reader.date_time()?;
        let lifetime = reader.u32()?.max(10_000);
        self.token_lifetime = Duration::from_millis(u64::from(lifetime));
        self.token_since = Instant::now();
        Ok(())
    }

    fn call(&mut self, request_type: u32, body: &[u8], response_type: u32) -> Result<Vec<u8>, String> {
        if self.token_since.elapsed() > self.token_lifetime.mul_f64(0.75) {
            self.open_channel(true)?;
        }
        let (sequence, request_id) = self.next_ids();
        let mut message = Writer::default();
        message.u32(self.channel_id).u32(self.token_id).u32(sequence).u32(request_id).node_id(&NodeId::Numeric(0, request_type));
        message.bytes.extend_from_slice(body);
        if message.bytes.len() + 8 > self.remote_buffer {
            return Err("a request is larger than the server's receive buffer".to_owned());
        }
        self.stream.write_all(&header(b"MSG", b'F', &message.bytes)).map_err(|error| error.to_string())?;
        let mut whole = Vec::new();
        let mut chunks = 0;
        loop {
            let (kind, chunk, frame) = self.read_frame()?;
            if &kind != b"MSG" {
                continue;
            }
            let mut reader = Reader::new(&frame);
            reader.take(16)?;
            if chunk == b'A' {
                return Err("the server aborted its response".to_owned());
            }
            chunks += 1;
            if chunks > MOST_CHUNKS || whole.len() + frame.len() > MOST_MESSAGE {
                return Err("the server's response is too large".to_owned());
            }
            whole.extend_from_slice(&frame[16..]);
            if chunk == b'F' {
                break;
            }
        }
        let mut reader = Reader::new(&whole);
        let type_id = type_of(&reader.node_id()?);
        if type_id == SERVICE_FAULT {
            let result = reader.response_header()?;
            return Err(format!("the server answered with the fault 0x{result:08x}"));
        }
        if type_id != response_type {
            return Err(format!("the server answered type {type_id}, not {response_type}"));
        }
        Ok(whole[reader.at..].to_vec())
    }

    fn create_session(&mut self) -> Result<Vec<(u32, String)>, String> {
        let mut body = Writer::default();
        self.request_header(&mut body, 1, 10_000);
        body.string(Some("urn:eggdb:opcua-client")).string(Some("urn:eggdb"));
        body.u8(0x02).string(Some("EggDB"));
        body.u32(1).string(None).string(None).i32(-1);
        body.string(None).string(Some(&self.url.clone())).string(Some("EggDB ingest"));
        body.byte_string(Some(&[0u8; 32])).byte_string(None).f64(SESSION_TIMEOUT_MS).u32(MOST_MESSAGE as u32);
        let response = self.call(CREATE_SESSION_REQUEST, &body.bytes, CREATE_SESSION_RESPONSE)?;
        let mut reader = Reader::new(&response);
        let result = reader.response_header()?;
        if result != 0 {
            return Err(format!("the server refused the session: 0x{result:08x}"));
        }
        reader.node_id()?;
        self.authentication = reader.node_id()?;
        reader.f64()?;
        reader.byte_string()?;
        reader.byte_string()?;
        let mut policies = Vec::new();
        for _ in 0..reader.array_length()? {
            reader.string()?;
            reader.string()?;
            reader.string()?;
            reader.localized_text()?;
            reader.u32()?;
            reader.string()?;
            reader.string()?;
            for _ in 0..reader.array_length()? {
                reader.string()?;
            }
            reader.byte_string()?;
            let mode = reader.u32()?;
            reader.string()?;
            for _ in 0..reader.array_length()? {
                let policy_id = reader.string()?.unwrap_or_default();
                let token_type = reader.u32()?;
                reader.string()?;
                reader.string()?;
                reader.string()?;
                if mode == SECURITY_MODE_NONE {
                    policies.push((token_type, policy_id));
                }
            }
            reader.string()?;
            reader.u8()?;
        }
        Ok(policies)
    }

    fn activate_session(&mut self, login: Option<&(String, String)>, policies: &[(u32, String)]) -> Result<(), String> {
        let wanted = if login.is_some() { TOKEN_USER_NAME } else { TOKEN_ANONYMOUS };
        let policy = policies
            .iter()
            .find(|(token_type, _)| *token_type == wanted)
            .map(|(_, id)| id.clone())
            .unwrap_or_else(|| if login.is_some() { "username" } else { "anonymous" }.to_owned());
        let mut token = Writer::default();
        token.string(Some(&policy));
        let token_type = match login {
            Some((user, password)) => {
                token.string(Some(user)).byte_string(Some(password.as_bytes())).string(None);
                USER_NAME_IDENTITY_TOKEN
            }
            None => ANONYMOUS_IDENTITY_TOKEN,
        };
        let mut body = Writer::default();
        self.request_header(&mut body, 2, 10_000);
        body.string(None).byte_string(None).i32(-1).i32(-1).extension_object(token_type, &token.bytes).string(None).byte_string(None);
        let response = self.call(ACTIVATE_SESSION_REQUEST, &body.bytes, ACTIVATE_SESSION_RESPONSE)?;
        let result = Reader::new(&response).response_header()?;
        if result != 0 {
            return Err(format!("the server refused to activate the session: 0x{result:08x}"));
        }
        Ok(())
    }

    /// Creates a subscription publishing every `interval_ms` and a monitored item for the Value
    /// of each node, its handle the node's index; returns the status of each item.
    pub fn subscribe(&mut self, nodes: &[NodeId], interval_ms: f64) -> Result<Vec<u32>, String> {
        let mut body = Writer::default();
        self.request_header(&mut body, 3, 10_000);
        body.f64(interval_ms).u32(600).u32(20).u32(0).boolean(true).u8(0);
        let response = self.call(CREATE_SUBSCRIPTION_REQUEST, &body.bytes, CREATE_SUBSCRIPTION_RESPONSE)?;
        let mut reader = Reader::new(&response);
        let result = reader.response_header()?;
        if result != 0 {
            return Err(format!("the server refused the subscription: 0x{result:08x}"));
        }
        let subscription = reader.u32()?;
        self.subscription = Some(subscription);
        let mut body = Writer::default();
        self.request_header(&mut body, 4, 10_000);
        body.u32(subscription).u32(2).i32(i32::try_from(nodes.len()).unwrap_or(i32::MAX));
        for (handle, node) in nodes.iter().enumerate() {
            body.node_id(node).u32(13).string(None).u16(0).string(None);
            body.u32(2).u32(handle as u32).f64(-1.0).no_extension_object().u32(10).boolean(true);
        }
        let response = self.call(CREATE_MONITORED_ITEMS_REQUEST, &body.bytes, CREATE_MONITORED_ITEMS_RESPONSE)?;
        let mut reader = Reader::new(&response);
        let result = reader.response_header()?;
        if result != 0 {
            return Err(format!("the server refused the monitored items: 0x{result:08x}"));
        }
        let mut statuses = Vec::new();
        for _ in 0..reader.array_length()? {
            statuses.push(reader.u32()?);
            reader.u32()?;
            reader.f64()?;
            reader.u32()?;
            reader.extension_object()?;
        }
        Ok(statuses)
    }

    /// Sends one `Publish` request, acknowledging what earlier ones delivered, and returns the
    /// value changes of its answer (none for a keep-alive).
    pub fn publish(&mut self) -> Result<Vec<Change>, String> {
        let mut body = Writer::default();
        self.request_header(&mut body, 5, 50_000);
        let acknowledgements = std::mem::take(&mut self.acknowledgements);
        body.i32(i32::try_from(acknowledgements.len()).unwrap_or(0));
        for (subscription, sequence) in &acknowledgements {
            body.u32(*subscription).u32(*sequence);
        }
        let response = self.call(PUBLISH_REQUEST, &body.bytes, PUBLISH_RESPONSE)?;
        let mut reader = Reader::new(&response);
        let result = reader.response_header()?;
        if result != 0 {
            return Err(format!("the server refused to publish: 0x{result:08x}"));
        }
        let subscription = reader.u32()?;
        for _ in 0..reader.array_length()? {
            reader.u32()?;
        }
        reader.u8()?;
        let sequence = reader.u32()?;
        reader.date_time()?;
        let mut changes = Vec::new();
        let notifications = reader.array_length()?;
        for _ in 0..notifications {
            let (type_id, notification) = reader.extension_object()?;
            match type_of(&type_id) {
                DATA_CHANGE_NOTIFICATION => {
                    let mut inner = Reader::new(notification);
                    for _ in 0..inner.array_length()? {
                        let handle = inner.u32()?;
                        let (data_value, used) = eggdb_sql::opcua_data_value_prefix(&inner.bytes[inner.at..])?;
                        inner.take(used)?;
                        changes.push(Change { handle, data_value });
                    }
                }
                STATUS_CHANGE_NOTIFICATION => {
                    let status = Reader::new(notification).u32()?;
                    if status & 0x8000_0000 != 0 {
                        return Err(format!("the subscription ended with 0x{status:08x}"));
                    }
                }
                _ => {}
            }
        }
        if notifications > 0 {
            self.acknowledgements.push((subscription, sequence));
        }
        Ok(changes)
    }

    /// Closes the session and the secure channel; errors are ignored, as the connection ends.
    pub fn close(mut self) {
        let mut body = Writer::default();
        self.request_header(&mut body, 6, 2_000);
        body.boolean(true);
        let (sequence, request_id) = self.next_ids();
        let mut message = Writer::default();
        message.u32(self.channel_id).u32(self.token_id).u32(sequence).u32(request_id).node_id(&NodeId::Numeric(0, CLOSE_SESSION_REQUEST));
        message.bytes.extend_from_slice(&body.bytes);
        let _ = self.stream.write_all(&header(b"MSG", b'F', &message.bytes));
        let (sequence, request_id) = self.next_ids();
        let mut close = Writer::default();
        close.u32(self.channel_id).u32(self.token_id).u32(sequence).u32(request_id).node_id(&NodeId::Numeric(0, CLOSE_SECURE_CHANNEL_REQUEST));
        self.request_header(&mut close, 7, 2_000);
        let _ = self.stream.write_all(&header(b"CLO", b'F', &close.bytes));
    }
}
