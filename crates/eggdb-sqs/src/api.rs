use crate::md5::md5_hex;
use eggdb_http::{Request, Response};
use eggdb_keyspace::Keyspace;
use eggdb_queues::{QueueError, Queues};
use eggdb_sql::json::{Json, Node, parse_document};
use std::time::{SystemTime, UNIX_EPOCH};

const ACCOUNT: &str = "000000000000";
const DEFAULT_VISIBILITY_SECONDS: u64 = 30;
const MAX_VISIBILITY_SECONDS: u64 = 12 * 60 * 60;
const MAX_DELAY_SECONDS: u64 = 15 * 60;
const MAX_RECEIVE: usize = 10;

/// The SQS-compatible endpoint over one keyspace's queues.
pub struct Endpoint<'k> {
    queues: Queues<'k>,
    base_url: String,
    access_key: String,
    secret: String,
}

impl std::fmt::Debug for Endpoint<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("base_url", &self.base_url).finish_non_exhaustive()
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

fn quoted(text: &str) -> String {
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

fn json(status: u16, body: String) -> Response {
    Response { status, content_type: "application/x-amz-json-1.0".to_owned(), body: body.into_bytes(), headers: Vec::new() }
}

fn failure(status: u16, code: &str, message: &str) -> Response {
    json(status, format!("{{\"__type\":{},\"message\":{}}}", quoted(&format!("com.amazonaws.sqs#{code}")), quoted(message)))
        .with_header("x-amzn-query-error", &format!("AWS.SimpleQueueService.{code};Sender"))
}

fn member<'n>(body: &'n Node, name: &str) -> Option<&'n Json> {
    match &body.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, node)| &node.value),
        _ => None,
    }
}

fn text<'n>(body: &'n Node, name: &str) -> Option<&'n str> {
    match member(body, name)? {
        Json::String(text) => Some(text),
        _ => None,
    }
}

fn number(body: &Node, name: &str) -> Option<u64> {
    match member(body, name)? {
        Json::Number(text) => text.parse().ok(),
        Json::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len()).step_by(2).map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok()).collect()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 80 && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

impl<'k> Endpoint<'k> {
    /// An endpoint whose queue URLs start with `base_url` (such as `http://127.0.0.1:9324`),
    /// signed with `access_key` and `secret`.
    pub fn new(keyspace: &'k (dyn Keyspace + Sync), base_url: &str, access_key: &str, secret: &str) -> Self {
        Self {
            queues: Queues::new(keyspace),
            base_url: base_url.trim_end_matches('/').to_owned(),
            access_key: access_key.to_owned(),
            secret: secret.to_owned(),
        }
    }

    fn queue_url(&self, name: &str) -> String {
        format!("{}/{ACCOUNT}/{name}", self.base_url)
    }

    fn queue_of(&self, body: &Node) -> Result<String, Response> {
        let url = text(body, "QueueUrl").ok_or_else(|| failure(400, "MissingParameter", "The request must contain the parameter QueueUrl."))?;
        let name = url.rsplit('/').next().unwrap_or_default();
        if !valid_name(name) {
            return Err(failure(400, "QueueDoesNotExist", "The specified queue does not exist."));
        }
        Ok(name.to_owned())
    }

    fn missing(error: &QueueError) -> Response {
        match error {
            QueueError::NoQueue(_) => failure(400, "QueueDoesNotExist", "The specified queue does not exist."),
            QueueError::TooLarge => failure(400, "InvalidParameterValue", "The message is too long."),
            QueueError::BadName(name) => failure(400, "InvalidParameterValue", &format!("The queue name {name} is not valid.")),
            other => failure(500, "InternalFailure", &other.to_string()),
        }
    }

    /// Answers one HTTP request.
    pub fn answer(&self, request: &Request) -> Response {
        if !crate::sigv4::verify(request, &self.access_key, &self.secret) {
            return failure(403, "InvalidClientTokenId", "The security token included in the request is invalid.");
        }
        let Some(action) = request.header("x-amz-target").and_then(|target| target.strip_prefix("AmazonSQS.")) else {
            return failure(400, "InvalidAction", "Only the AWS JSON 1.0 protocol (X-Amz-Target) is supported.");
        };
        let text_body = String::from_utf8_lossy(&request.body).into_owned();
        let body = if text_body.trim().is_empty() {
            Node { value: Json::Object(Vec::new()), start: 0, end: 0 }
        } else {
            match parse_document(&text_body) {
                Ok(node) => node,
                Err(_) => return failure(400, "SerializationException", "The request body is not valid JSON."),
            }
        };
        let outcome = match action {
            "CreateQueue" => self.create_queue(&body),
            "GetQueueUrl" => self.get_queue_url(&body),
            "ListQueues" => self.list_queues(&body),
            "DeleteQueue" => self.queue_of(&body).and_then(|name| self.queues.delete(&name).map(|_| json(200, "{}".to_owned())).map_err(|error| Self::missing(&error))),
            "PurgeQueue" => self.queue_of(&body).and_then(|name| self.queues.purge(&name).map(|_| json(200, "{}".to_owned())).map_err(|error| Self::missing(&error))),
            "SendMessage" => self.send_message(&body),
            "ReceiveMessage" => self.receive_message(&body),
            "DeleteMessage" => self.delete_message(&body),
            "ChangeMessageVisibility" => self.change_visibility(&body),
            "GetQueueAttributes" => self.queue_attributes(&body),
            other => Err(failure(400, "InvalidAction", &format!("The action {other} is not valid for this endpoint."))),
        };
        outcome.unwrap_or_else(|response| response)
    }

    fn create_queue(&self, body: &Node) -> Result<Response, Response> {
        let name = text(body, "QueueName").ok_or_else(|| failure(400, "MissingParameter", "The request must contain the parameter QueueName."))?;
        if !valid_name(name) || name.ends_with(".fifo") {
            return Err(failure(400, "InvalidParameterValue", "Queue names have 1 to 80 letters, digits, hyphens and underscores; FIFO queues are not supported yet."));
        }
        self.queues.declare(name).map_err(|error| Self::missing(&error))?;
        Ok(json(200, format!("{{\"QueueUrl\":{}}}", quoted(&self.queue_url(name)))))
    }

    fn get_queue_url(&self, body: &Node) -> Result<Response, Response> {
        let name = text(body, "QueueName").unwrap_or_default();
        if !self.queues.exists(name).map_err(|error| Self::missing(&error))? {
            return Err(failure(400, "QueueDoesNotExist", "The specified queue does not exist."));
        }
        Ok(json(200, format!("{{\"QueueUrl\":{}}}", quoted(&self.queue_url(name)))))
    }

    fn list_queues(&self, body: &Node) -> Result<Response, Response> {
        let prefix = text(body, "QueueNamePrefix").unwrap_or_default();
        let urls: Vec<String> = self
            .queues
            .names()
            .map_err(|error| Self::missing(&error))?
            .into_iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| quoted(&self.queue_url(&name)))
            .collect();
        Ok(json(200, format!("{{\"QueueUrls\":[{}]}}", urls.join(","))))
    }

    fn send_message(&self, body: &Node) -> Result<Response, Response> {
        let name = self.queue_of(body)?;
        let message = text(body, "MessageBody").ok_or_else(|| failure(400, "MissingParameter", "The request must contain the parameter MessageBody."))?;
        let delay = number(body, "DelaySeconds").unwrap_or(0);
        if delay > MAX_DELAY_SECONDS {
            return Err(failure(400, "InvalidParameterValue", "DelaySeconds must be between 0 and 900."));
        }
        let id = self.queues.send(&name, message.as_bytes(), delay * 1000, now_ms()).map_err(|error| Self::missing(&error))?;
        Ok(json(200, format!("{{\"MessageId\":{},\"MD5OfMessageBody\":{}}}", quoted(&format!("{name}-{id}")), quoted(&md5_hex(message.as_bytes())))))
    }

    fn receive_message(&self, body: &Node) -> Result<Response, Response> {
        let name = self.queue_of(body)?;
        let max = usize::try_from(number(body, "MaxNumberOfMessages").unwrap_or(1)).unwrap_or(usize::MAX);
        if !(1..=MAX_RECEIVE).contains(&max) {
            return Err(failure(400, "InvalidParameterValue", "MaxNumberOfMessages must be between 1 and 10."));
        }
        let visibility = number(body, "VisibilityTimeout").unwrap_or(DEFAULT_VISIBILITY_SECONDS).min(MAX_VISIBILITY_SECONDS);
        let messages = self.queues.receive(&name, max, visibility * 1000, now_ms()).map_err(|error| Self::missing(&error))?;
        let rendered: Vec<String> = messages
            .iter()
            .map(|message| {
                let body = String::from_utf8_lossy(&message.body);
                format!(
                    "{{\"MessageId\":{},\"ReceiptHandle\":{},\"MD5OfBody\":{},\"Body\":{},\"Attributes\":{{\"ApproximateReceiveCount\":{},\"SentTimestamp\":{}}}}}",
                    quoted(&format!("{name}-{}", message.id)),
                    quoted(&hex(format!("{name}\0{}\0{}", message.id, message.receive_count).as_bytes())),
                    quoted(&md5_hex(&message.body)),
                    quoted(&body),
                    quoted(&message.receive_count.to_string()),
                    quoted(&message.sent_ms.to_string())
                )
            })
            .collect();
        Ok(json(200, format!("{{\"Messages\":[{}]}}", rendered.join(","))))
    }

    fn receipt(&self, body: &Node) -> Result<(String, u64), Response> {
        let invalid = || failure(400, "ReceiptHandleIsInvalid", "The input receipt handle is invalid.");
        let handle = text(body, "ReceiptHandle").ok_or_else(invalid)?;
        let decoded = unhex(handle).and_then(|bytes| String::from_utf8(bytes).ok()).ok_or_else(invalid)?;
        let mut parts = decoded.split('\0');
        let (Some(name), Some(id)) = (parts.next(), parts.next().and_then(|id| id.parse::<u64>().ok())) else {
            return Err(invalid());
        };
        if name != self.queue_of(body)? {
            return Err(invalid());
        }
        Ok((name.to_owned(), id))
    }

    fn delete_message(&self, body: &Node) -> Result<Response, Response> {
        let (name, id) = self.receipt(body)?;
        self.queues.ack(&name, id).map_err(|error| Self::missing(&error))?;
        Ok(json(200, "{}".to_owned()))
    }

    fn change_visibility(&self, body: &Node) -> Result<Response, Response> {
        let (name, id) = self.receipt(body)?;
        let seconds = number(body, "VisibilityTimeout").unwrap_or(0).min(MAX_VISIBILITY_SECONDS);
        if seconds == 0 {
            self.queues.release(&name, id, now_ms()).map_err(|error| Self::missing(&error))?;
        } else {
            return Err(failure(400, "UnsupportedOperation", "Only VisibilityTimeout 0 (return the message now) is supported yet."));
        }
        Ok(json(200, "{}".to_owned()))
    }

    fn queue_attributes(&self, body: &Node) -> Result<Response, Response> {
        let name = self.queue_of(body)?;
        let depth = self.queues.depth(&name, now_ms()).map_err(|error| Self::missing(&error))?;
        Ok(json(
            200,
            format!(
                "{{\"Attributes\":{{\"ApproximateNumberOfMessages\":{},\"ApproximateNumberOfMessagesNotVisible\":{},\"QueueArn\":{}}}}}",
                quoted(&depth.ready.to_string()),
                quoted(&depth.waiting.to_string()),
                quoted(&format!("arn:aws:sqs:eggdb:{ACCOUNT}:{name}"))
            ),
        ))
    }
}
