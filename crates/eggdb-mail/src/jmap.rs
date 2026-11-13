//! JMAP over the email store (D894): the session resource and API of RFC 8620 with the mail
//! methods of RFC 8621 that read and organise mail: `Core/echo`, `Mailbox/get`,
//! `Mailbox/query`, `Mailbox/set`, `Email/query`, `Email/get`, `Email/set` and
//! `Thread/get`, result references between calls, and blob downloads of whole messages and of
//! their MIME parts, with each email's body structure, text and HTML bodies and attachments.

use crate::json::{Json, parse, string};
use crate::message::{addresses, decoded_words, headers, parse_rfc5322_date, utc_date};
use crate::mime::{self, Part};
use crate::store::{self, INBOX, Mailbox, MailError, StoredMessage};
use crate::ServerConfig;
use eggdb_http::{BodyWriter, Reply, Request, Response, StreamedResponse};
use std::io::Write as _;
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};

const ACCOUNT: &str = "a";
const MAX_CALLS: usize = 64;
const MAX_OBJECTS: usize = 1000;
const PREVIEW_CHARS: usize = 256;
const UPLOAD_KEPT_SECONDS: u64 = 24 * 3600;
const CORE: &str = "urn:ietf:params:jmap:core";
const MAIL: &str = "urn:ietf:params:jmap:mail";
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
const IDENTITY_ID: &str = "i1";
const STREAM_LIFETIME: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const STATE_POLL: std::time::Duration = std::time::Duration::from_secs(1);
/// The largest JMAP request body.
pub const MAX_REQUEST_BYTES: usize = 10 << 20;

fn hex(text: &str) -> String {
    text.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<String> {
    if text.len() % 2 != 0 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..text.len()).step_by(2).map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok()).collect();
    String::from_utf8(bytes?).ok()
}

fn mailbox_id(name: &str) -> String {
    format!("m{}", hex(name))
}

fn mailbox_of_id(id: &str) -> Option<String> {
    unhex(id.strip_prefix('m')?)
}

fn email_id(mailbox: &str, uid: u32) -> String {
    format!("e{}_{uid}", hex(mailbox))
}

fn email_of_id(id: &str) -> Option<(String, u32)> {
    let (mailbox, uid) = id.strip_prefix('e')?.split_once('_')?;
    Some((unhex(mailbox)?, uid.parse().ok()?))
}

fn keyword_of(flag: &str) -> Option<String> {
    Some(match flag.to_ascii_lowercase().as_str() {
        "\\seen" => "$seen".to_owned(),
        "\\flagged" => "$flagged".to_owned(),
        "\\answered" => "$answered".to_owned(),
        "\\draft" => "$draft".to_owned(),
        "\\deleted" | "\\recent" => return None,
        other => other.to_owned(),
    })
}

fn flag_of(keyword: &str) -> String {
    match keyword.to_ascii_lowercase().as_str() {
        "$seen" => "\\Seen".to_owned(),
        "$flagged" => "\\Flagged".to_owned(),
        "$answered" => "\\Answered".to_owned(),
        "$draft" => "\\Draft".to_owned(),
        other => other.to_owned(),
    }
}

fn role_of(name: &str) -> Json {
    let lowered = name.to_ascii_lowercase();
    let role = match lowered.as_str() {
        "inbox" => "inbox",
        "sent" | "sent items" | "sent messages" => "sent",
        "drafts" => "drafts",
        "trash" | "deleted items" => "trash",
        "junk" | "spam" => "junk",
        "archive" => "archive",
        _ => return Json::Null,
    };
    Json::String(role.to_owned())
}

fn method_error(kind: &str) -> (String, Json) {
    ("error".to_owned(), Json::Object(vec![("type".to_owned(), Json::String(kind.to_owned()))]))
}

fn text_reply(status: u16, body: &str) -> Response {
    Response {
        status,
        content_type: "text/plain; charset=utf-8".to_owned(),
        body: body.as_bytes().to_vec(),
        headers: Vec::new(),
    }
}

fn json_reply(status: u16, body: String) -> Response {
    Response {
        status,
        content_type: "application/json".to_owned(),
        body: body.into_bytes(),
        headers: Vec::new(),
    }
}

fn problem(status: u16, kind: &str, detail: &str) -> Response {
    let mut response = json_reply(
        status,
        format!("{{\"type\":{},\"status\":{status},\"detail\":{}}}", string(kind), string(detail)),
    );
    response.content_type = "application/problem+json".to_owned();
    response
}

fn address_json(value: Option<String>) -> Json {
    match value {
        None => Json::Null,
        Some(text) => Json::Array(
            addresses(&text)
                .into_iter()
                .map(|address| {
                    Json::Object(vec![
                        ("name".to_owned(), address.name.map_or(Json::Null, Json::String)),
                        ("email".to_owned(), Json::String(address.email)),
                    ])
                })
                .collect(),
        ),
    }
}

fn message_ids(value: Option<String>) -> Json {
    match value {
        None => Json::Null,
        Some(text) => Json::Array(
            text.split_whitespace()
                .map(|id| Json::String(id.trim_start_matches('<').trim_end_matches('>').to_owned()))
                .collect(),
        ),
    }
}

fn body_text(raw: &[u8]) -> String {
    let root = mime::parse(raw);
    let parts = BodyParts::of(&root);
    parts.text.iter().map(|(_, part)| mime::text_of(raw, part).0).collect::<Vec<_>>().join("\n")
}

struct BodyParts<'p> {
    text: Vec<(String, &'p Part)>,
    html: Vec<(String, &'p Part)>,
    attachments: Vec<(String, &'p Part)>,
}

impl<'p> BodyParts<'p> {
    fn of(root: &'p Part) -> Self {
        let mut plain = Vec::new();
        let mut html = Vec::new();
        let mut attachments = Vec::new();
        for (number, part) in mime::leaves(root) {
            let shown = !part.is_attachment() && part.major == "text";
            match (shown, part.minor.as_str()) {
                (true, "plain") => plain.push((number, part)),
                (true, "html") => html.push((number, part)),
                _ => attachments.push((number, part)),
            }
        }
        let text = if plain.is_empty() { html.clone() } else { plain.clone() };
        let html = if html.is_empty() { plain } else { html };
        Self { text, html, attachments }
    }
}

fn part_json(email: &str, raw: &[u8], number: Option<&str>, part: &Part) -> Json {
    let mut fields = vec![
        ("partId".to_owned(), number.map_or(Json::Null, |number| Json::String(number.to_owned()))),
        (
            "blobId".to_owned(),
            number.map_or(Json::Null, |number| Json::String(format!("{email}p{number}"))),
        ),
        ("size".to_owned(), Json::Number(mime::decoded_body(raw, part).len() as f64)),
        ("type".to_owned(), Json::String(format!("{}/{}", part.major, part.minor))),
        (
            "charset".to_owned(),
            match (part.parameter("charset"), part.major.as_str()) {
                (Some(charset), _) => Json::String(charset.to_owned()),
                (None, "text") => Json::String("us-ascii".to_owned()),
                (None, _) => Json::Null,
            },
        ),
        ("disposition".to_owned(), part.disposition.as_ref().map_or(Json::Null, |(kind, _)| Json::String(kind.clone()))),
        ("name".to_owned(), part.file_name().map_or(Json::Null, |name| Json::String(decoded_words(name)))),
        (
            "cid".to_owned(),
            part.id.as_deref().map_or(Json::Null, |id| Json::String(id.trim().trim_start_matches('<').trim_end_matches('>').to_owned())),
        ),
    ];
    if part.is_multipart() {
        fields.push(("subParts".to_owned(), Json::Array(Vec::new())));
    }
    Json::Object(fields)
}

fn structure_json(email: &str, raw: &[u8], part: &Part, number: &str) -> Json {
    if !part.is_multipart() {
        return part_json(email, raw, Some(if number.is_empty() { "1" } else { number }), part);
    }
    let Json::Object(mut fields) = part_json(email, raw, None, part) else {
        return Json::Null;
    };
    let children = part
        .children
        .iter()
        .enumerate()
        .map(|(index, child)| {
            let child_number = if number.is_empty() { (index + 1).to_string() } else { format!("{number}.{}", index + 1) };
            structure_json(email, raw, child, &child_number)
        })
        .collect();
    if let Some((_, slot)) = fields.iter_mut().find(|(name, _)| name == "subParts") {
        *slot = Json::Array(children);
    }
    Json::Object(fields)
}

/// The JMAP endpoint over one account.
pub struct Endpoint<'k, K: Keyspace + ?Sized> {
    keyspace: &'k K,
    config: ServerConfig,
}

impl<K: Keyspace + ?Sized> std::fmt::Debug for Endpoint<'_, K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("user", &self.config.user).finish_non_exhaustive()
    }
}

struct Store {
    messages: Vec<(Mailbox, Vec<StoredMessage>)>,
}

impl Store {
    fn find(&self, id: &str) -> Option<(&Mailbox, &StoredMessage)> {
        let (mailbox, uid) = email_of_id(id)?;
        let (found, messages) = self.messages.iter().find(|(found, _)| found.name == mailbox)?;
        Some((found, messages.iter().find(|message| message.uid == uid)?))
    }
}

fn read_all(keys: &dyn KeysRead) -> Result<Vec<(Mailbox, Vec<StoredMessage>)>, MailError> {
    let mut all = Vec::new();
    for mailbox in store::mailboxes(keys)? {
        let messages = store::messages(keys, &mailbox.name)?;
        all.push((mailbox, messages));
    }
    Ok(all)
}

impl<'k, K: Keyspace + ?Sized> Endpoint<'k, K> {
    /// The endpoint over `keyspace`'s database 0, for clients signing in as `config.user`.
    pub fn new(keyspace: &'k K, config: ServerConfig) -> Self {
        Self { keyspace, config }
    }

    fn signed_in(&self, request: &Request) -> bool {
        let Some(encoded) = request.header("authorization").and_then(|header| header.strip_prefix("Basic ")) else {
            return false;
        };
        let Ok(decoded) = eggdb_crypto::base64_decode(encoded.trim().as_bytes()) else {
            return false;
        };
        let credentials = String::from_utf8_lossy(&decoded).into_owned();
        let Some((user, password)) = credentials.split_once(':') else {
            return false;
        };
        user == self.config.user && eggdb_crypto::sha256(password.as_bytes()) == self.config.password_sha256
    }

    fn snapshot(&self) -> Result<Store, String> {
        let mut found = None;
        self.keyspace.read(0, &mut |keys| {
            found = Some(read_all(keys).map_err(|error| error.0));
            Ok(())
        })?;
        Ok(Store {
            messages: found.unwrap_or_else(|| Ok(Vec::new()))?,
        })
    }

    fn change(&self, body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), MailError>) -> Result<(), String> {
        self.keyspace.write(0, &mut |keys| body(keys).map_err(|error| error.0))
    }

    fn state(store: &Store) -> String {
        let total: u64 = store
            .messages
            .iter()
            .map(|(mailbox, messages)| u64::from(mailbox.uid_next) + messages.len() as u64 * 7 + messages.iter().map(|message| message.flags.len() as u64).sum::<u64>())
            .sum();
        format!("s{total}")
    }

    fn states(&self) -> crate::push::States {
        let mail = self.snapshot().map(|store| Self::state(&store)).unwrap_or_default();
        let submissions = self.stored_submissions().map(|stored| Self::submission_state(stored.len())).unwrap_or_default();
        vec![("Email", mail.clone()), ("Mailbox", mail.clone()), ("Thread", mail), ("EmailSubmission", submissions)]
    }

    fn fingerprint(states: &crate::push::States) -> String {
        states.iter().map(|(kind, state)| format!("{kind}={state};")).collect()
    }

    /// Answers one HTTP request, streaming the EventSource (`/jmap/eventsource`) and answering
    /// everything else whole as [`Endpoint::answer`] does.
    pub fn reply(&self, request: &Request) -> Reply<'_> {
        if request.path != "/jmap/eventsource" || !self.signed_in(request) {
            return Reply::Whole(self.answer(request));
        }
        let query = request.query.clone().unwrap_or_default();
        let parameter = |name: &str| query.split('&').find_map(|pair| pair.split_once('=').filter(|(key, _)| *key == name).map(|(_, value)| value.to_owned()));
        let types: Option<Vec<String>> = parameter("types").filter(|types| types != "*").map(|types| types.split(',').map(str::to_owned).collect());
        let close_after_state = parameter("closeafter").as_deref() == Some("state");
        let ping = parameter("ping").and_then(|seconds| seconds.parse::<u64>().ok()).unwrap_or(0);
        let ping = (ping > 0).then(|| std::time::Duration::from_secs(ping.clamp(5, 300)));
        let last_event = request.header("last-event-id").map(str::to_owned);
        let body = move |writer: &mut BodyWriter<'_>| -> std::io::Result<()> {
            let started = std::time::Instant::now();
            let mut last_ping = std::time::Instant::now();
            let mut sent = last_event;
            loop {
                let states = self.states();
                let fingerprint = Self::fingerprint(&states);
                if sent.as_deref() != Some(fingerprint.as_str()) {
                    let event = crate::push::state_change(ACCOUNT, &states, types.as_deref());
                    writer.write_all(format!("event: state\nid: {fingerprint}\ndata: {event}\n\n").as_bytes())?;
                    writer.flush()?;
                    let first = sent.is_none();
                    sent = Some(fingerprint);
                    if close_after_state && !first {
                        return Ok(());
                    }
                }
                if let Some(interval) = ping
                    && last_ping.elapsed() >= interval
                {
                    writer.write_all(format!("event: ping\ndata: {{\"@type\":\"Ping\",\"interval\":{}}}\n\n", interval.as_secs()).as_bytes())?;
                    writer.flush()?;
                    last_ping = std::time::Instant::now();
                }
                if started.elapsed() > STREAM_LIFETIME {
                    return Ok(());
                }
                std::thread::sleep(STATE_POLL);
            }
        };
        Reply::Streamed(StreamedResponse {
            status: 200,
            content_type: "text/event-stream".to_owned(),
            headers: vec![("Cache-Control".to_owned(), "no-cache".to_owned())],
            body: Box::new(body),
            held_bytes: 0,
            content_length: None,
        })
    }

    /// Sends every verified PushSubscription a `StateChange` when the account's state moves,
    /// checking once a second, until `stop` answers true.
    pub fn run_push(&self, stop: &(dyn Fn() -> bool + Sync)) {
        let mut sent = std::collections::BTreeMap::new();
        while !stop() {
            let states = self.states();
            crate::push::deliver(self.keyspace, ACCOUNT, &states, crate::now_seconds(), &mut sent);
            let waited = std::time::Instant::now();
            while !stop() && waited.elapsed() < STATE_POLL {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    fn push_get(&self, arguments: &Json) -> Json {
        let wanted = Self::wanted_ids(arguments);
        let mut list = Vec::new();
        for subscription in crate::push::subscriptions(self.keyspace) {
            if wanted.as_ref().is_some_and(|ids| !ids.contains(&subscription.id)) {
                continue;
            }
            let shown: Vec<(String, Json)> = match subscription.json {
                Json::Object(members) => members.into_iter().filter(|(key, _)| matches!(key.as_str(), "id" | "deviceClientId" | "expires" | "types")).collect(),
                _ => Vec::new(),
            };
            list.push(Json::Object(shown));
        }
        Json::Object(vec![("list".to_owned(), Json::Array(list)), ("notFound".to_owned(), Json::Array(Vec::new()))])
    }

    fn push_set(&self, arguments: &Json) -> Result<Json, String> {
        let failure = |kind: &str, description: &str| {
            Json::Object(vec![("type".to_owned(), Json::String(kind.to_owned())), ("description".to_owned(), Json::String(description.to_owned()))])
        };
        let existing = crate::push::subscriptions(self.keyspace);
        let now = crate::now_seconds();
        let mut created = Vec::new();
        let mut not_created = Vec::new();
        let mut count = existing.len();
        if let Some(Json::Object(creations)) = arguments.get("create") {
            for (creation_id, creation) in creations {
                let outcome = (|| -> Result<Json, Json> {
                    if count >= crate::push::MOST_SUBSCRIPTIONS {
                        return Err(failure("overQuota", "the account holds the most push subscriptions it may"));
                    }
                    if creation.get("keys").is_some_and(|keys| *keys != Json::Null) {
                        return Err(failure("invalidProperties", "encrypted push (keys) is not supported yet"));
                    }
                    let device = creation.get("deviceClientId").and_then(Json::as_str).ok_or_else(|| failure("invalidProperties", "deviceClientId is required"))?;
                    let url = creation.get("url").and_then(Json::as_str).ok_or_else(|| failure("invalidProperties", "url is required"))?;
                    eggdb_http::parse_http_url(url).map_err(|error| failure("invalidProperties", &error))?;
                    let requested = creation.get("expires").and_then(Json::as_str).and_then(parse_rfc3339).unwrap_or(u64::MAX);
                    let expires_at = requested.min(now + crate::push::MOST_SUBSCRIPTION_SECONDS);
                    let code = crate::push::verification_code().ok_or_else(|| failure("serverFail", "no random bytes for the verification code"))?;
                    let mut id = String::new();
                    let types = creation.get("types").cloned().unwrap_or(Json::Null);
                    self.change(&mut |keys| {
                        id = format!("P{}", store::next_number(keys, "push")?);
                        let record = Json::Object(vec![
                            ("id".to_owned(), Json::String(id.clone())),
                            ("deviceClientId".to_owned(), Json::String(device.to_owned())),
                            ("url".to_owned(), Json::String(url.to_owned())),
                            ("expires".to_owned(), Json::String(utc_date(expires_at))),
                            ("expiresAt".to_owned(), Json::Number(expires_at as f64)),
                            ("types".to_owned(), types.clone()),
                            ("verificationCode".to_owned(), Json::String(code.clone())),
                            ("verified".to_owned(), Json::Bool(false)),
                        ]);
                        store::put_push(keys, &id, &record.to_text())
                    })
                    .map_err(|_| failure("serverFail", "the subscription could not be kept"))?;
                    crate::push::post(url, &format!("{{\"@type\":\"PushVerification\",\"pushSubscriptionId\":{},\"verificationCode\":{}}}", string(&id), string(&code)));
                    count += 1;
                    Ok(Json::Object(vec![("id".to_owned(), Json::String(id)), ("expires".to_owned(), Json::String(utc_date(expires_at)))]))
                })();
                match outcome {
                    Ok(record) => created.push((creation_id.clone(), record)),
                    Err(error) => not_created.push((creation_id.clone(), error)),
                }
            }
        }
        let mut updated = Vec::new();
        let mut not_updated = Vec::new();
        if let Some(Json::Object(updates)) = arguments.get("update") {
            for (id, patch) in updates {
                let Some(found) = existing.iter().find(|subscription| subscription.id == *id) else {
                    not_updated.push((id.clone(), failure("notFound", "no such push subscription")));
                    continue;
                };
                let Json::Object(mut members) = found.json.clone() else { continue };
                let mut refused = None;
                if let Json::Object(changes) = patch {
                    for (key, value) in changes {
                        match key.as_str() {
                            "verificationCode" if value.as_str().is_some_and(|code| Some(code) == found.json.get("verificationCode").and_then(Json::as_str)) => {
                                members.retain(|(member, _)| member != "verified");
                                members.push(("verified".to_owned(), Json::Bool(true)));
                            }
                            "verificationCode" => refused = Some(failure("invalidProperties", "the verification code does not match")),
                            "types" => {
                                members.retain(|(member, _)| member != "types");
                                members.push(("types".to_owned(), value.clone()));
                            }
                            "expires" => {
                                let at = value.as_str().and_then(parse_rfc3339).unwrap_or(now).min(now + crate::push::MOST_SUBSCRIPTION_SECONDS);
                                members.retain(|(member, _)| member != "expires" && member != "expiresAt");
                                members.push(("expires".to_owned(), Json::String(utc_date(at))));
                                members.push(("expiresAt".to_owned(), Json::Number(at as f64)));
                            }
                            _ => refused = Some(failure("invalidProperties", "only verificationCode, types and expires may change")),
                        }
                    }
                }
                if let Some(error) = refused {
                    not_updated.push((id.clone(), error));
                    continue;
                }
                let record = Json::Object(members).to_text();
                match self.change(&mut |keys| store::put_push(keys, id, &record)) {
                    Ok(()) => updated.push((id.clone(), Json::Null)),
                    Err(_) => not_updated.push((id.clone(), failure("serverFail", "the subscription could not be kept"))),
                }
            }
        }
        let mut destroyed = Vec::new();
        let mut not_destroyed = Vec::new();
        for id in arguments.get("destroy").and_then(Json::as_array).unwrap_or_default().iter().filter_map(Json::as_str) {
            let mut removed = false;
            match self.change(&mut |keys| {
                removed = store::remove_push(keys, id)?;
                Ok(())
            }) {
                Ok(()) if removed => destroyed.push(Json::String(id.to_owned())),
                _ => not_destroyed.push((id.to_owned(), failure("notFound", "no such push subscription"))),
            }
        }
        let optional = |entries: Vec<(String, Json)>| if entries.is_empty() { Json::Null } else { Json::Object(entries) };
        Ok(Json::Object(vec![
            ("created".to_owned(), optional(created)),
            ("notCreated".to_owned(), optional(not_created)),
            ("updated".to_owned(), optional(updated)),
            ("notUpdated".to_owned(), optional(not_updated)),
            ("destroyed".to_owned(), Json::Array(destroyed)),
            ("notDestroyed".to_owned(), optional(not_destroyed)),
        ]))
    }

    /// Answers one HTTP request.
    pub fn answer(&self, request: &Request) -> Response {
        if !self.signed_in(request) {
            let mut response = text_reply(401, "sign in with HTTP Basic authentication");
            response.headers.push(("WWW-Authenticate".to_owned(), "Basic realm=\"eggdb\"".to_owned()));
            return response;
        }
        let path = request.path.as_str();
        if path == "/.well-known/jmap" || path == "/jmap/session" {
            return self.session();
        }
        if path == "/jmap/api" || path == "/jmap/api/" {
            if request.method != "POST" {
                return problem(405, "about:blank", "the API takes POST");
            }
            return self.api(&request.body);
        }
        if let Some(rest) = path.strip_prefix("/jmap/upload/") {
            if request.method != "POST" {
                return problem(405, "about:blank", "uploads take POST");
            }
            return match rest.trim_end_matches('/') {
                ACCOUNT => self.upload(request),
                _ => problem(404, "about:blank", "no such account"),
            };
        }
        if let Some(rest) = path.strip_prefix("/jmap/download/") {
            let parts: Vec<&str> = rest.split('/').collect();
            if let [account, blob, ..] = parts.as_slice()
                && *account == ACCOUNT
            {
                return self.download(blob);
            }
        }
        text_reply(404, "the JMAP session is at /.well-known/jmap")
    }

    fn session(&self) -> Response {
        let state = self.snapshot().map(|store| Self::state(&store)).unwrap_or_default();
        let core = format!(
            "{{\"maxSizeUpload\":{MAX_REQUEST_BYTES},\"maxConcurrentUpload\":1,\"maxSizeRequest\":{MAX_REQUEST_BYTES},\"maxConcurrentRequests\":4,\"maxCallsInRequest\":{MAX_CALLS},\"maxObjectsInGet\":{MAX_OBJECTS},\"maxObjectsInSet\":{MAX_OBJECTS},\"collationAlgorithms\":[\"i;ascii-casemap\"]}}"
        );
        let mail = "{\"maxMailboxesPerEmail\":1,\"maxMailboxDepth\":null,\"maxSizeMailboxName\":1024,\"maxSizeAttachmentsPerEmail\":0,\"emailQuerySortOptions\":[\"receivedAt\",\"size\"],\"mayCreateTopLevelMailbox\":true}";
        json_reply(
            200,
            format!(
                "{{\"capabilities\":{{{core_name}:{core},{mail_name}:{{}},{submission_name}:{{}}}},\"accounts\":{{\"{ACCOUNT}\":{{\"name\":{user},\"isPersonal\":true,\"isReadOnly\":false,\"accountCapabilities\":{{{mail_name}:{mail},{submission_name}:{{\"maxDelayedSend\":0,\"submissionExtensions\":{{}}}}}}}}}},\"primaryAccounts\":{{{mail_name}:\"{ACCOUNT}\",{submission_name}:\"{ACCOUNT}\"}},\"username\":{user},\"apiUrl\":\"/jmap/api\",\"downloadUrl\":\"/jmap/download/{{accountId}}/{{blobId}}/{{name}}?accept={{type}}\",\"uploadUrl\":\"/jmap/upload/{{accountId}}/\",\"eventSourceUrl\":\"/jmap/eventsource\",\"state\":{}}}",
                string(&state),
                core_name = string(CORE),
                mail_name = string(MAIL),
                submission_name = string(SUBMISSION),
                user = string(&self.config.user),
            ),
        )
    }

    fn upload(&self, request: &Request) -> Response {
        let digest = eggdb_crypto::sha256(&request.body);
        let id = format!("u{}", digest[..16].iter().map(|byte| format!("{byte:02x}")).collect::<String>());
        let content_type = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map_or_else(|| "application/octet-stream".to_owned(), |(_, value)| value.clone());
        let now = crate::now_seconds();
        let upload = store::Upload {
            content_type: content_type.clone(),
            uploaded_at: now,
            bytes: request.body.clone(),
        };
        let stored = self.change(&mut |keys| {
            store::remove_uploads_before(keys, now.saturating_sub(UPLOAD_KEPT_SECONDS))?;
            store::put_upload(keys, &id, &upload)
        });
        match stored {
            Ok(()) => json_reply(
                201,
                format!(
                    "{{\"accountId\":\"{ACCOUNT}\",\"blobId\":{},\"type\":{},\"size\":{}}}",
                    string(&id),
                    string(&content_type),
                    request.body.len()
                ),
            ),
            Err(_) => problem(500, "about:blank", "the upload could not be kept"),
        }
    }

    fn blob_bytes(&self, blob: &str) -> Option<Vec<u8>> {
        if blob.starts_with('u') {
            let mut found = None;
            self.keyspace
                .read(0, &mut |keys| {
                    found = store::upload(keys, blob).map_err(|error| error.0)?;
                    Ok(())
                })
                .ok()?;
            return found.map(|upload| upload.bytes);
        }
        let store = self.snapshot().ok()?;
        store.find(blob).map(|(_, message)| message.raw.clone())
    }

    fn email_import(&self, arguments: &Json) -> Result<Json, String> {
        let old_state = Self::state(&self.snapshot().map_err(|_| "serverFail".to_owned())?);
        let failure = |kind: &str| Json::Object(vec![("type".to_owned(), Json::String(kind.to_owned()))]);
        let mut created = Vec::new();
        let mut not_created = Vec::new();
        if let Some(Json::Object(emails)) = arguments.get("emails") {
            for (creation, email) in emails {
                let Some(raw) = email.get("blobId").and_then(Json::as_str).and_then(|blob| self.blob_bytes(blob)) else {
                    not_created.push((creation.clone(), failure("blobNotFound")));
                    continue;
                };
                let targets: Vec<String> = match email.get("mailboxIds") {
                    Some(Json::Object(ids)) => ids.iter().filter(|(_, value)| *value == Json::Bool(true)).filter_map(|(id, _)| mailbox_of_id(id)).collect(),
                    _ => Vec::new(),
                };
                let [target] = targets.as_slice() else {
                    not_created.push((creation.clone(), failure("invalidProperties")));
                    continue;
                };
                let flags: Vec<String> = match email.get("keywords") {
                    Some(Json::Object(keywords)) => keywords.iter().filter(|(_, value)| *value == Json::Bool(true)).map(|(keyword, _)| flag_of(keyword)).collect(),
                    _ => Vec::new(),
                };
                let received = email.get("receivedAt").and_then(Json::as_str).and_then(parse_rfc3339).unwrap_or_else(crate::now_seconds);
                let mut uid = 0;
                let outcome = self.change(&mut |keys| {
                    if store::canonical(target) != INBOX && store::mailbox(&*keys, target)?.is_none() {
                        return Err(MailError("invalidProperties".to_owned()));
                    }
                    uid = store::append(keys, target, &flags, received, &raw)?;
                    Ok(())
                });
                match outcome {
                    Ok(()) => {
                        let id = email_id(target, uid);
                        created.push((
                            creation.clone(),
                            Json::Object(vec![
                                ("id".to_owned(), Json::String(id.clone())),
                                ("blobId".to_owned(), Json::String(id.clone())),
                                ("threadId".to_owned(), Json::String(format!("t{}", &id[1..]))),
                                ("size".to_owned(), Json::Number(raw.len() as f64)),
                            ]),
                        ));
                    }
                    Err(kind) => not_created.push((creation.clone(), failure(if kind == "invalidProperties" { "invalidProperties" } else { "serverFail" }))),
                }
            }
        }
        let new_state = Self::state(&self.snapshot().map_err(|_| "serverFail".to_owned())?);
        Ok(Json::Object(vec![
            ("accountId".to_owned(), Json::String(ACCOUNT.to_owned())),
            ("oldState".to_owned(), Json::String(old_state)),
            ("newState".to_owned(), Json::String(new_state)),
            ("created".to_owned(), if created.is_empty() { Json::Null } else { Json::Object(created) }),
            ("notCreated".to_owned(), if not_created.is_empty() { Json::Null } else { Json::Object(not_created) }),
        ]))
    }

    fn download(&self, blob: &str) -> Response {
        if blob.starts_with('u') {
            let mut found = None;
            let read = self.keyspace.read(0, &mut |keys| {
                found = store::upload(keys, blob).map_err(|error| error.0)?;
                Ok(())
            });
            return match (read, found) {
                (Ok(()), Some(upload)) => Response {
                    status: 200,
                    content_type: upload.content_type,
                    body: upload.bytes,
                    headers: Vec::new(),
                },
                (Ok(()), None) => text_reply(404, "no such blob"),
                (Err(_), _) => text_reply(500, "the store could not be read"),
            };
        }
        let Ok(store) = self.snapshot() else {
            return text_reply(500, "the store could not be read");
        };
        let (email, part_number) = match blob.rsplit_once('p') {
            Some((email, number)) if email.contains('_') => (email, Some(number)),
            _ => (blob, None),
        };
        let Some((_, message)) = store.find(email) else {
            return text_reply(404, "no such blob");
        };
        let Some(number) = part_number else {
            return Response {
                status: 200,
                content_type: "message/rfc822".to_owned(),
                body: message.raw.clone(),
                headers: Vec::new(),
            };
        };
        let numbers: Option<Vec<usize>> = number.split('.').map(|piece| piece.parse().ok()).collect();
        let root = mime::parse(&message.raw);
        match numbers.as_deref().and_then(|numbers| mime::part_at(&root, numbers)) {
            Some(part) => Response {
                status: 200,
                content_type: format!("{}/{}", part.major, part.minor),
                body: mime::decoded_body(&message.raw, part),
                headers: Vec::new(),
            },
            None => text_reply(404, "no such blob"),
        }
    }

    fn api(&self, body: &[u8]) -> Response {
        let Some(request) = std::str::from_utf8(body).ok().and_then(parse) else {
            return problem(400, "urn:ietf:params:jmap:error:notJSON", "the request is not JSON");
        };
        let (Some(using), Some(calls)) = (request.get("using").and_then(Json::as_array), request.get("methodCalls").and_then(Json::as_array)) else {
            return problem(400, "urn:ietf:params:jmap:error:notRequest", "the request needs using and methodCalls");
        };
        for capability in using {
            if !matches!(capability.as_str(), Some(CORE | MAIL | SUBMISSION)) {
                return problem(400, "urn:ietf:params:jmap:error:unknownCapability", "a capability in using is not offered");
            }
        }
        if calls.len() > MAX_CALLS {
            return problem(400, "urn:ietf:params:jmap:error:limit", "too many method calls");
        }
        let mut responses: Vec<(String, Json, String)> = Vec::new();
        for call in calls {
            let Some([name, arguments, call_id]) = call.as_array().and_then(|parts| <&[Json; 3]>::try_from(parts).ok()) else {
                return problem(400, "urn:ietf:params:jmap:error:notRequest", "a method call is [name, arguments, id]");
            };
            let (Some(name), Some(call_id)) = (name.as_str(), call_id.as_str()) else {
                return problem(400, "urn:ietf:params:jmap:error:notRequest", "a method call is [name, arguments, id]");
            };
            let mut implicit = None;
            let (response_name, response) = match self.resolve_references(arguments, &responses) {
                Ok(arguments) if name == "EmailSubmission/set" => match self.submission_set(&arguments, &responses) {
                    Ok((response, follow_up)) => {
                        implicit = follow_up;
                        (name.to_owned(), response)
                    }
                    Err(kind) => method_error(&kind),
                },
                Ok(arguments) => self.method(name, &arguments),
                Err(()) => method_error("invalidResultReference"),
            };
            responses.push((response_name, response, call_id.to_owned()));
            if let Some(update) = implicit {
                let (implicit_name, implicit_response) = match self.email_set(&update) {
                    Ok(response) => ("Email/set".to_owned(), response),
                    Err(kind) => method_error(&kind),
                };
                responses.push((implicit_name, implicit_response, call_id.to_owned()));
            }
        }
        let state = self.snapshot().map(|store| Self::state(&store)).unwrap_or_default();
        let listed: Vec<String> = responses
            .iter()
            .map(|(name, arguments, id)| format!("[{},{},{}]", string(name), arguments.to_text(), string(id)))
            .collect();
        json_reply(200, format!("{{\"methodResponses\":[{}],\"sessionState\":{}}}", listed.join(","), string(&state)))
    }

    fn resolve_references(&self, arguments: &Json, earlier: &[(String, Json, String)]) -> Result<Json, ()> {
        let Json::Object(members) = arguments else {
            return Ok(arguments.clone());
        };
        let mut resolved = Vec::new();
        for (key, value) in members {
            let Some(target) = key.strip_prefix('#') else {
                resolved.push((key.clone(), value.clone()));
                continue;
            };
            let (Some(result_of), Some(name), Some(path)) = (
                value.get("resultOf").and_then(Json::as_str),
                value.get("name").and_then(Json::as_str),
                value.get("path").and_then(Json::as_str),
            ) else {
                return Err(());
            };
            let (_, found, _) = earlier.iter().find(|(response, _, id)| id == result_of && response == name).ok_or(())?;
            resolved.push((target.to_owned(), pointer(found, path).ok_or(())?));
        }
        Ok(Json::Object(resolved))
    }

    fn method(&self, name: &str, arguments: &Json) -> (String, Json) {
        if arguments.get("accountId").and_then(Json::as_str).is_some_and(|account| account != ACCOUNT) {
            return method_error("accountNotFound");
        }
        let outcome = match name {
            "Core/echo" => return (name.to_owned(), arguments.clone()),
            "Mailbox/get" => self.mailbox_get(arguments),
            "Mailbox/query" => self.mailbox_query(),
            "Mailbox/set" => self.mailbox_set(arguments),
            "Email/query" => self.email_query(arguments),
            "Email/get" => self.email_get(arguments),
            "Email/set" => self.email_set(arguments),
            "Email/import" => self.email_import(arguments),
            "Thread/get" => self.thread_get(arguments),
            "Identity/get" => Ok(self.identity_get(arguments)),
            "EmailSubmission/get" => self.submission_get(arguments),
            "EmailSubmission/query" => self.submission_query(),
            "PushSubscription/get" => Ok(self.push_get(arguments)),
            "PushSubscription/set" => self.push_set(arguments),
            _ => return method_error("unknownMethod"),
        };
        match outcome {
            Ok(response) => (name.to_owned(), response),
            Err(kind) => method_error(&kind),
        }
    }

    fn wanted_ids(arguments: &Json) -> Option<Vec<String>> {
        arguments.get("ids").and_then(Json::as_array).map(|ids| ids.iter().filter_map(Json::as_str).map(str::to_owned).collect())
    }

    fn mailbox_json(mailbox: &Mailbox, messages: &[StoredMessage]) -> Json {
        let unread = messages.iter().filter(|message| !message.flags.iter().any(|flag| flag.eq_ignore_ascii_case("\\Seen"))).count();
        let parent = mailbox.name.rsplit_once('/').map(|(parent, _)| parent.to_owned());
        let shown = mailbox.name.rsplit('/').next().unwrap_or(&mailbox.name).to_owned();
        let rights = ["mayReadItems", "mayAddItems", "mayRemoveItems", "maySetSeen", "maySetKeywords", "mayCreateChild", "mayRename", "mayDelete", "maySubmit"];
        Json::Object(vec![
            ("id".to_owned(), Json::String(mailbox_id(&mailbox.name))),
            ("name".to_owned(), Json::String(shown)),
            ("parentId".to_owned(), parent.map_or(Json::Null, |parent| Json::String(mailbox_id(&parent)))),
            ("role".to_owned(), role_of(&mailbox.name)),
            ("sortOrder".to_owned(), Json::Number(if mailbox.name == INBOX { 0.0 } else { 10.0 })),
            ("totalEmails".to_owned(), Json::Number(messages.len() as f64)),
            ("unreadEmails".to_owned(), Json::Number(unread as f64)),
            ("totalThreads".to_owned(), Json::Number(messages.len() as f64)),
            ("unreadThreads".to_owned(), Json::Number(unread as f64)),
            (
                "myRights".to_owned(),
                Json::Object(rights.iter().map(|right| ((*right).to_owned(), Json::Bool(*right != "maySubmit" && !(mailbox.name == INBOX && matches!(*right, "mayRename" | "mayDelete"))))).collect()),
            ),
            ("isSubscribed".to_owned(), Json::Bool(mailbox.subscribed || mailbox.name == INBOX)),
        ])
    }

    fn mailbox_get(&self, arguments: &Json) -> Result<Json, String> {
        let _ = self.change(&mut |keys| store::ensure_mailbox(keys, INBOX).map(drop));
        let store = self.snapshot().map_err(|_| "serverFail".to_owned())?;
        let wanted = Self::wanted_ids(arguments);
        let mut list = Vec::new();
        let mut not_found = Vec::new();
        match &wanted {
            None => list.extend(store.messages.iter().map(|(mailbox, messages)| Self::mailbox_json(mailbox, messages))),
            Some(ids) => {
                for id in ids {
                    match store.messages.iter().find(|(mailbox, _)| Some(mailbox.name.clone()) == mailbox_of_id(id)) {
                        Some((mailbox, messages)) => list.push(Self::mailbox_json(mailbox, messages)),
                        None => not_found.push(Json::String(id.clone())),
                    }
                }
            }
        }
        Ok(Json::Object(vec![
            ("accountId".to_owned(), Json::String(ACCOUNT.to_owned())),
            ("state".to_owned(), Json::String(Self::state(&store))),
            ("list".to_owned(), Json::Array(list)),
            ("notFound".to_owned(), Json::Array(not_found)),
        ]))
    }

    fn mailbox_query(&self) -> Result<Json, String> {
        let store = self.snapshot().map_err(|_| "serverFail".to_owned())?;
        let ids: Vec<Json> = store.messages.iter().map(|(mailbox, _)| Json::String(mailbox_id(&mailbox.name))).collect();
        Ok(Json::Object(vec![
            ("accountId".to_owned(), Json::String(ACCOUNT.to_owned())),
            ("queryState".to_owned(), Json::String(Self::state(&store))),
            ("canCalculateChanges".to_owned(), Json::Bool(false)),
            ("position".to_owned(), Json::Number(0.0)),
            ("total".to_owned(), Json::Number(ids.len() as f64)),
            ("ids".to_owned(), Json::Array(ids)),
        ]))
    }

    fn mailbox_set(&self, arguments: &Json) -> Result<Json, String> {
        let mut created = Vec::new();
        let mut not_created = Vec::new();
        let mut destroyed = Vec::new();
        let mut not_destroyed = Vec::new();
        if let Some(Json::Object(creations)) = arguments.get("create") {
            for (creation_id, value) in creations {
                let parent = value.get("parentId").and_then(Json::as_str).and_then(mailbox_of_id);
                let Some(name) = value.get("name").and_then(Json::as_str) else {
                    not_created.push((creation_id.clone(), Json::Object(vec![("type".to_owned(), Json::String("invalidProperties".to_owned()))])));
                    continue;
                };
                let full = match parent {
                    Some(parent) => format!("{parent}/{name}"),
                    None => name.to_owned(),
                };
                match self.change(&mut |keys| store::create_mailbox(keys, &full).map(drop)) {
                    Ok(()) => created.push((creation_id.clone(), Json::Object(vec![("id".to_owned(), Json::String(mailbox_id(&full)))]))),
                    Err(message) => not_created.push((
                        creation_id.clone(),
                        Json::Object(vec![
                            ("type".to_owned(), Json::String("invalidProperties".to_owned())),
                            ("description".to_owned(), Json::String(message)),
                        ]),
                    )),
                }
            }
        }
        if let Some(ids) = arguments.get("destroy").and_then(Json::as_array) {
            for id in ids.iter().filter_map(Json::as_str) {
                let outcome = mailbox_of_id(id).ok_or_else(|| "no such mailbox".to_owned()).and_then(|name| self.change(&mut |keys| store::delete_mailbox(keys, &name)));
                match outcome {
                    Ok(()) => destroyed.push(Json::String(id.to_owned())),
                    Err(_) => not_destroyed.push((id.to_owned(), Json::Object(vec![("type".to_owned(), Json::String("notFound".to_owned()))]))),
                }
            }
        }
        let store = self.snapshot().map_err(|_| "serverFail".to_owned())?;
        Ok(Json::Object(vec![
            ("accountId".to_owned(), Json::String(ACCOUNT.to_owned())),
            ("newState".to_owned(), Json::String(Self::state(&store))),
            ("created".to_owned(), Json::Object(created)),
            ("notCreated".to_owned(), Json::Object(not_created)),
            ("destroyed".to_owned(), Json::Array(destroyed)),
            ("notDestroyed".to_owned(), Json::Object(not_destroyed)),
        ]))
    }

    fn matches(filter: &Json, mailbox: &Mailbox, message: &StoredMessage, depth: usize) -> bool {
        if depth > 16 {
            return false;
        }
        if let Some(operator) = filter.get("operator").and_then(Json::as_str) {
            let conditions = filter.get("conditions").and_then(Json::as_array).unwrap_or_default();
            let mut results = conditions.iter().map(|condition| Self::matches(condition, mailbox, message, depth + 1));
            return match operator {
                "AND" => results.all(|matched| matched),
                "OR" => results.any(|matched| matched),
                _ => !results.any(|matched| matched),
            };
        }
        let field = |name: &str| crate::message::header(&message.raw, name).unwrap_or_default().to_lowercase();
        let contains = |haystack: String, key: &str| filter.get(key).and_then(Json::as_str).is_none_or(|needle| haystack.contains(&needle.to_lowercase()));
        let keyword = |flagged: bool, key: &str| {
            filter.get(key).and_then(Json::as_str).is_none_or(|wanted| {
                message.flags.iter().any(|flag| keyword_of(flag).as_deref() == Some(wanted.to_lowercase().as_str())) == flagged
            })
        };
        let in_mailbox = filter.get("inMailbox").and_then(Json::as_str).is_none_or(|id| mailbox_of_id(id).as_deref() == Some(mailbox.name.as_str()));
        let date = |key: &str| filter.get(key).and_then(Json::as_str).and_then(|text| parse_rfc3339(text));
        let before = date("before").is_none_or(|limit| message.internal_date < limit);
        let after = date("after").is_none_or(|limit| message.internal_date >= limit);
        let size = |key: &str| filter.get(key).and_then(Json::as_u64);
        let larger = size("minSize").is_none_or(|limit| message.raw.len() as u64 >= limit);
        let smaller = size("maxSize").is_none_or(|limit| (message.raw.len() as u64) < limit);
        in_mailbox
            && before
            && after
            && larger
            && smaller
            && contains(field("From"), "from")
            && contains(field("To"), "to")
            && contains(field("Cc"), "cc")
            && contains(field("Subject"), "subject")
            && contains(body_text(&message.raw).to_lowercase(), "body")
            && contains(String::from_utf8_lossy(&message.raw).to_lowercase(), "text")
            && keyword(true, "hasKeyword")
            && keyword(false, "notKeyword")
    }

    fn email_query(&self, arguments: &Json) -> Result<Json, String> {
        let store = self.snapshot().map_err(|_| "serverFail".to_owned())?;
        let filter = arguments.get("filter").cloned().unwrap_or(Json::Object(Vec::new()));
        let mut found: Vec<(&Mailbox, &StoredMessage)> = store
            .messages
            .iter()
            .flat_map(|(mailbox, messages)| messages.iter().map(move |message| (mailbox, message)))
            .filter(|(mailbox, message)| Self::matches(&filter, mailbox, message, 0))
            .collect();
        let sort = arguments.get("sort").and_then(Json::as_array).and_then(<[Json]>::first);
        let property = sort.and_then(|sort| sort.get("property")).and_then(Json::as_str).unwrap_or("receivedAt");
        let ascending = sort.and_then(|sort| sort.get("isAscending")).is_some_and(|value| *value == Json::Bool(true));
        found.sort_by(|(_, left), (_, right)| {
            let order = if property == "size" { left.raw.len().cmp(&right.raw.len()) } else { left.internal_date.cmp(&right.internal_date).then(left.uid.cmp(&right.uid)) };
            if ascending { order } else { order.reverse() }
        });
        let position = arguments.get("position").and_then(Json::as_u64).unwrap_or(0) as usize;
        let limit = arguments.get("limit").and_then(Json::as_u64).map_or(MAX_OBJECTS, |limit| (limit as usize).min(MAX_OBJECTS));
        let ids: Vec<Json> = found.iter().skip(position).take(limit).map(|(mailbox, message)| Json::String(email_id(&mailbox.name, message.uid))).collect();
        Ok(Json::Object(vec![
            ("accountId".to_owned(), Json::String(ACCOUNT.to_owned())),
            ("queryState".to_owned(), Json::String(Self::state(&store))),
            ("canCalculateChanges".to_owned(), Json::Bool(false)),
            ("position".to_owned(), Json::Number(position as f64)),
            ("total".to_owned(), Json::Number(found.len() as f64)),
            ("ids".to_owned(), Json::Array(ids)),
        ]))
    }

    fn email_json(mailbox: &Mailbox, message: &StoredMessage, properties: Option<&[Json]>, body_values: bool) -> Json {
        let id = email_id(&mailbox.name, message.uid);
        let fields = headers(&message.raw);
        let value = |name: &str| fields.iter().find(|(found, _)| found.eq_ignore_ascii_case(name)).map(|(_, value)| value.clone());
        let decoded = |name: &str| value(name).map(|text| decoded_words(&text));
        let root = mime::parse(&message.raw);
        let parts = BodyParts::of(&root);
        let text = body_text(&message.raw);
        let preview: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(PREVIEW_CHARS).collect();
        let listed = |list: &[(String, &Part)]| {
            Json::Array(list.iter().map(|(number, part)| part_json(&id, &message.raw, Some(number.as_str()), part)).collect())
        };
        let keywords: Vec<(String, Json)> = message.flags.iter().filter_map(|flag| keyword_of(flag)).map(|keyword| (keyword, Json::Bool(true))).collect();
        let mut all = vec![
            ("id".to_owned(), Json::String(id.clone())),
            ("blobId".to_owned(), Json::String(id.clone())),
            ("threadId".to_owned(), Json::String(format!("t{}", &id[1..]))),
            ("mailboxIds".to_owned(), Json::Object(vec![(mailbox_id(&mailbox.name), Json::Bool(true))])),
            ("keywords".to_owned(), Json::Object(keywords)),
            ("size".to_owned(), Json::Number(message.raw.len() as f64)),
            ("receivedAt".to_owned(), Json::String(utc_date(message.internal_date))),
            ("messageId".to_owned(), message_ids(value("Message-ID"))),
            ("inReplyTo".to_owned(), message_ids(value("In-Reply-To"))),
            ("references".to_owned(), message_ids(value("References"))),
            ("subject".to_owned(), decoded("Subject").map_or(Json::Null, Json::String)),
            ("from".to_owned(), address_json(decoded("From"))),
            ("sender".to_owned(), address_json(decoded("Sender"))),
            ("to".to_owned(), address_json(decoded("To"))),
            ("cc".to_owned(), address_json(decoded("Cc"))),
            ("bcc".to_owned(), address_json(decoded("Bcc"))),
            ("replyTo".to_owned(), address_json(decoded("Reply-To"))),
            ("sentAt".to_owned(), value("Date").and_then(|date| parse_rfc5322_date(&date)).map_or(Json::Null, |seconds| Json::String(utc_date(seconds)))),
            ("preview".to_owned(), Json::String(preview)),
            ("hasAttachment".to_owned(), Json::Bool(!parts.attachments.is_empty())),
            ("bodyStructure".to_owned(), structure_json(&id, &message.raw, &root, "")),
            ("textBody".to_owned(), listed(&parts.text)),
            ("htmlBody".to_owned(), listed(&parts.html)),
            ("attachments".to_owned(), listed(&parts.attachments)),
        ];
        if body_values {
            let mut values: Vec<(String, Json)> = Vec::new();
            for (number, part) in parts.text.iter().chain(&parts.html) {
                if values.iter().any(|(known, _)| known == number) {
                    continue;
                }
                let (value, unknown_charset) = mime::text_of(&message.raw, part);
                values.push((
                    number.clone(),
                    Json::Object(vec![
                        ("value".to_owned(), Json::String(value)),
                        ("isEncodingProblem".to_owned(), Json::Bool(unknown_charset)),
                        ("isTruncated".to_owned(), Json::Bool(false)),
                    ]),
                ));
            }
            all.push(("bodyValues".to_owned(), Json::Object(values)));
        }
        let wanted: Option<Vec<&str>> = properties.map(|names| names.iter().filter_map(Json::as_str).collect());
        Json::Object(
            all.into_iter()
                .filter(|(name, _)| name == "id" || name == "bodyValues" || wanted.as_ref().is_none_or(|wanted| wanted.contains(&name.as_str())))
                .collect(),
        )
    }

    fn email_get(&self, arguments: &Json) -> Result<Json, String> {
        let store = self.snapshot().map_err(|_| "serverFail".to_owned())?;
        let properties = arguments.get("properties").and_then(Json::as_array);
        let body_values = arguments.get("fetchTextBodyValues").or_else(|| arguments.get("fetchAllBodyValues")).is_some_and(|value| *value == Json::Bool(true));
        let ids = Self::wanted_ids(arguments).ok_or_else(|| "requestTooLarge".to_owned())?;
        if ids.len() > MAX_OBJECTS {
            return Err("requestTooLarge".to_owned());
        }
        let mut list = Vec::new();
        let mut not_found = Vec::new();
        for id in &ids {
            match store.find(id) {
                Some((mailbox, message)) => list.push(Self::email_json(mailbox, message, properties, body_values)),
                None => not_found.push(Json::String(id.clone())),
            }
        }
        Ok(Json::Object(vec![
            ("accountId".to_owned(), Json::String(ACCOUNT.to_owned())),
            ("state".to_owned(), Json::String(Self::state(&store))),
            ("list".to_owned(), Json::Array(list)),
            ("notFound".to_owned(), Json::Array(not_found)),
        ]))
    }

    fn email_set(&self, arguments: &Json) -> Result<Json, String> {
        let mut updated = Vec::new();
        let mut not_updated = Vec::new();
        let mut destroyed = Vec::new();
        let mut not_destroyed = Vec::new();
        let failure = |kind: &str| Json::Object(vec![("type".to_owned(), Json::String(kind.to_owned()))]);
        if let Some(Json::Object(updates)) = arguments.get("update") {
            for (id, patch) in updates {
                let Some((mailbox, uid)) = email_of_id(id) else {
                    not_updated.push((id.clone(), failure("notFound")));
                    continue;
                };
                let outcome = self.change(&mut |keys| {
                    let Some(message) = store::messages(&*keys, &mailbox)?.into_iter().find(|message| message.uid == uid) else {
                        return Err(MailError("notFound".to_owned()));
                    };
                    let mut flags = message.flags.clone();
                    if let Some(Json::Object(keywords)) = patch.get("keywords") {
                        flags.retain(|flag| keyword_of(flag).is_none());
                        flags.extend(keywords.iter().filter(|(_, value)| *value == Json::Bool(true)).map(|(keyword, _)| flag_of(keyword)));
                    }
                    if let Json::Object(members) = patch {
                        for (path, value) in members {
                            if let Some(keyword) = path.strip_prefix("keywords/") {
                                let flag = flag_of(keyword);
                                flags.retain(|found| !found.eq_ignore_ascii_case(&flag));
                                if *value == Json::Bool(true) {
                                    flags.push(flag);
                                }
                            }
                        }
                    }
                    store::set_flags(keys, &mailbox, uid, &flags)?;
                    let target = match patch.get("mailboxIds") {
                        Some(Json::Object(ids)) => ids.iter().find(|(_, value)| *value == Json::Bool(true)).and_then(|(id, _)| mailbox_of_id(id)),
                        _ => None,
                    };
                    if let Some(target) = target.filter(|target| *target != mailbox) {
                        if store::mailbox(&*keys, &target)?.is_none() {
                            return Err(MailError("invalidProperties".to_owned()));
                        }
                        store::append(keys, &target, &flags, message.internal_date, &message.raw)?;
                        store::remove(keys, &mailbox, uid)?;
                    }
                    Ok(())
                });
                match outcome {
                    Ok(()) => updated.push((id.clone(), Json::Null)),
                    Err(kind) => not_updated.push((id.clone(), failure(if kind == "invalidProperties" { "invalidProperties" } else { "notFound" }))),
                }
            }
        }
        if let Some(ids) = arguments.get("destroy").and_then(Json::as_array) {
            for id in ids.iter().filter_map(Json::as_str) {
                let removed = email_of_id(id).map(|(mailbox, uid)| {
                    let mut removed = false;
                    let outcome = self.change(&mut |keys| {
                        removed = store::remove(keys, &mailbox, uid)?;
                        Ok(())
                    });
                    outcome.is_ok() && removed
                });
                if removed == Some(true) {
                    destroyed.push(Json::String(id.to_owned()));
                } else {
                    not_destroyed.push((id.to_owned(), failure("notFound")));
                }
            }
        }
        if arguments.get("create").is_some_and(|creations| *creations != Json::Null && *creations != Json::Object(Vec::new())) {
            return Err("forbidden".to_owned());
        }
        let store = self.snapshot().map_err(|_| "serverFail".to_owned())?;
        Ok(Json::Object(vec![
            ("accountId".to_owned(), Json::String(ACCOUNT.to_owned())),
            ("newState".to_owned(), Json::String(Self::state(&store))),
            ("updated".to_owned(), Json::Object(updated)),
            ("notUpdated".to_owned(), Json::Object(not_updated)),
            ("destroyed".to_owned(), Json::Array(destroyed)),
            ("notDestroyed".to_owned(), Json::Object(not_destroyed)),
        ]))
    }

    fn address(&self) -> String {
        format!("{}@{}", self.config.user, self.config.domain)
    }

    fn identity_get(&self, arguments: &Json) -> Json {
        let identity = Json::Object(vec![
            ("id".to_owned(), Json::String(IDENTITY_ID.to_owned())),
            ("name".to_owned(), Json::String(self.config.user.clone())),
            ("email".to_owned(), Json::String(self.address())),
            ("replyTo".to_owned(), Json::Null),
            ("bcc".to_owned(), Json::Null),
            ("textSignature".to_owned(), Json::String(String::new())),
            ("htmlSignature".to_owned(), Json::String(String::new())),
            ("mayDelete".to_owned(), Json::Bool(false)),
        ]);
        let (list, not_found) = match Self::wanted_ids(arguments) {
            None => (vec![identity], Vec::new()),
            Some(ids) => {
                let found = ids.iter().any(|id| id == IDENTITY_ID);
                (
                    if found { vec![identity] } else { Vec::new() },
                    ids.into_iter().filter(|id| id != IDENTITY_ID).map(Json::String).collect(),
                )
            }
        };
        Json::Object(vec![
            ("accountId".to_owned(), Json::String(ACCOUNT.to_owned())),
            ("state".to_owned(), Json::String("i0".to_owned())),
            ("list".to_owned(), Json::Array(list)),
            ("notFound".to_owned(), Json::Array(not_found)),
        ])
    }

    fn stored_submissions(&self) -> Result<Vec<(String, Json)>, String> {
        let mut found = Vec::new();
        self.keyspace.read(0, &mut |keys| {
            found = store::submissions(keys).map_err(|error| error.0)?;
            Ok(())
        })?;
        Ok(found.into_iter().filter_map(|(id, text)| Some((id, parse(&text)?))).collect())
    }

    fn submission_state(count: usize) -> String {
        format!("u{count}")
    }

    fn submission_get(&self, arguments: &Json) -> Result<Json, String> {
        let stored = self.stored_submissions().map_err(|_| "serverFail".to_owned())?;
        let state = Self::submission_state(stored.len());
        let (list, not_found): (Vec<Json>, Vec<Json>) = match Self::wanted_ids(arguments) {
            None => (stored.into_iter().map(|(_, json)| json).collect(), Vec::new()),
            Some(ids) => {
                let mut list = Vec::new();
                let mut missing = Vec::new();
                for id in ids {
                    match stored.iter().find(|(known, _)| *known == id) {
                        Some((_, json)) => list.push(json.clone()),
                        None => missing.push(Json::String(id)),
                    }
                }
                (list, missing)
            }
        };
        Ok(Json::Object(vec![
            ("accountId".to_owned(), Json::String(ACCOUNT.to_owned())),
            ("state".to_owned(), Json::String(state)),
            ("list".to_owned(), Json::Array(list)),
            ("notFound".to_owned(), Json::Array(not_found)),
        ]))
    }

    fn submission_query(&self) -> Result<Json, String> {
        let stored = self.stored_submissions().map_err(|_| "serverFail".to_owned())?;
        Ok(Json::Object(vec![
            ("accountId".to_owned(), Json::String(ACCOUNT.to_owned())),
            ("queryState".to_owned(), Json::String(Self::submission_state(stored.len()))),
            ("canCalculateChanges".to_owned(), Json::Bool(false)),
            ("position".to_owned(), Json::Number(0.0)),
            ("total".to_owned(), Json::Number(stored.len() as f64)),
            ("ids".to_owned(), Json::Array(stored.into_iter().map(|(id, _)| Json::String(id)).collect())),
        ]))
    }

    fn is_local(&self, recipient: &str) -> bool {
        recipient
            .rsplit_once('@')
            .is_some_and(|(_, domain)| domain.eq_ignore_ascii_case(&self.config.domain) || domain.eq_ignore_ascii_case("localhost"))
    }

    fn submit(&self, creation: &Json, earlier: &[(String, Json, String)]) -> Result<(Json, String), Json> {
        let failure = |kind: &str, description: &str| {
            Json::Object(vec![
                ("type".to_owned(), Json::String(kind.to_owned())),
                ("description".to_owned(), Json::String(description.to_owned())),
            ])
        };
        let email_id = match creation.get("emailId").and_then(Json::as_str) {
            Some(reference) if reference.starts_with('#') => earlier
                .iter()
                .rev()
                .filter(|(name, _, _)| name == "Email/set" || name == "Email/import")
                .find_map(|(_, response, _)| response.get("created")?.get(&reference[1..])?.get("id")?.as_str().map(str::to_owned))
                .ok_or_else(|| failure("invalidProperties", "emailId names no Email created earlier in this request"))?,
            Some(id) => id.to_owned(),
            None => return Err(failure("invalidProperties", "emailId is required")),
        };
        if creation.get("identityId").and_then(Json::as_str) != Some(IDENTITY_ID) {
            return Err(failure("invalidProperties", "identityId must be the account's identity, i1"));
        }
        let store = self.snapshot().map_err(|_| failure("serverFail", "the store could not be read"))?;
        let (_, message) = store.find(&email_id).ok_or_else(|| failure("invalidProperties", "emailId names no Email"))?;
        let header_fields = headers(&message.raw);
        let mail_from = creation
            .get("envelope")
            .and_then(|envelope| envelope.get("mailFrom"))
            .and_then(|from| from.get("email"))
            .and_then(Json::as_str)
            .map_or_else(|| self.address(), str::to_owned);
        let recipients: Vec<String> = match creation.get("envelope").and_then(|envelope| envelope.get("rcptTo")).and_then(Json::as_array) {
            Some(listed) => listed.iter().filter_map(|to| to.get("email").and_then(Json::as_str).map(str::to_owned)).collect(),
            None => header_fields
                .iter()
                .filter(|(name, _)| ["to", "cc", "bcc"].contains(&name.to_ascii_lowercase().as_str()))
                .flat_map(|(_, value)| addresses(value))
                .map(|address| address.email)
                .collect(),
        };
        if recipients.is_empty() {
            return Err(failure("noRecipients", "the Email names no recipient and no envelope was given"));
        }
        if message.raw.len() > self.config.max_message_bytes {
            return Err(failure("tooLarge", "the Email is larger than the server sends"));
        }
        let (local, remote): (Vec<String>, Vec<String>) = recipients.iter().cloned().partition(|recipient| self.is_local(recipient));
        if !remote.is_empty() && self.config.relay.is_none() {
            return Err(failure("forbiddenToSend", "mail to other domains needs eggdb serve --smtp-relay"));
        }
        let sent = without_bcc(&message.raw);
        let mut statuses: Vec<(String, Json)> = Vec::new();
        let status = |reply: &str, delivered: &str| {
            Json::Object(vec![
                ("smtpReply".to_owned(), Json::String(reply.to_owned())),
                ("delivered".to_owned(), Json::String(delivered.to_owned())),
                ("displayed".to_owned(), Json::String("unknown".to_owned())),
            ])
        };
        if let (Some(relay), false) = (&self.config.relay, remote.is_empty()) {
            let outcomes = crate::smtp_client::relay(relay, &self.config.domain, &mail_from, &remote, &sent)
                .map_err(|error| failure("serverFail", &error))?;
            if outcomes.iter().all(|(_, outcome)| outcome.is_err()) {
                let rejected = outcomes.iter().map(|(recipient, _)| Json::String(recipient.clone())).collect();
                let mut error = failure("invalidRecipients", "the relay refused every recipient");
                if let Json::Object(members) = &mut error {
                    members.push(("invalidRecipients".to_owned(), Json::Array(rejected)));
                }
                return Err(error);
            }
            for (recipient, outcome) in outcomes {
                statuses.push(match outcome {
                    Ok(reply) => (recipient, status(&reply, "queued")),
                    Err(reply) => (recipient, status(&reply, "no")),
                });
            }
        }
        if !local.is_empty() {
            let mut delivered = format!("Delivered-To: {}\r\n", local.join(", ")).into_bytes();
            delivered.extend_from_slice(&sent);
            self.change(&mut |keys| store::append(keys, INBOX, &[], crate::now_seconds(), &delivered).map(drop))
                .map_err(|_| failure("serverFail", "the Email could not be delivered to the store"))?;
            for recipient in local {
                statuses.push((recipient, status("250 2.0.0 delivered to the store", "yes")));
            }
        }
        let mut number = 0;
        let send_at = utc_date(crate::now_seconds());
        let envelope = Json::Object(vec![
            ("mailFrom".to_owned(), Json::Object(vec![("email".to_owned(), Json::String(mail_from.clone())), ("parameters".to_owned(), Json::Null)])),
            (
                "rcptTo".to_owned(),
                Json::Array(recipients.iter().map(|to| Json::Object(vec![("email".to_owned(), Json::String(to.clone())), ("parameters".to_owned(), Json::Null)])).collect()),
            ),
        ]);
        let mut record = Json::Null;
        self.change(&mut |keys| {
            number = store::next_number(keys, "submission")?;
            let id = format!("S{number}");
            record = Json::Object(vec![
                ("id".to_owned(), Json::String(id.clone())),
                ("identityId".to_owned(), Json::String(IDENTITY_ID.to_owned())),
                ("emailId".to_owned(), Json::String(email_id.clone())),
                ("threadId".to_owned(), Json::String(format!("t{}", email_id.get(1..).unwrap_or_default()))),
                ("envelope".to_owned(), envelope.clone()),
                ("sendAt".to_owned(), Json::String(send_at.clone())),
                ("undoStatus".to_owned(), Json::String("final".to_owned())),
                ("deliveryStatus".to_owned(), Json::Object(statuses.clone())),
                ("dsnBlobIds".to_owned(), Json::Array(Vec::new())),
                ("mdnBlobIds".to_owned(), Json::Array(Vec::new())),
            ]);
            store::put_submission(keys, &id, &record.to_text())
        })
        .map_err(|_| failure("serverFail", "the submission could not be kept"))?;
        let created = Json::Object(vec![
            ("id".to_owned(), Json::String(format!("S{number}"))),
            ("sendAt".to_owned(), Json::String(send_at)),
            ("undoStatus".to_owned(), Json::String("final".to_owned())),
        ]);
        Ok((created, email_id))
    }

    fn submission_set(&self, arguments: &Json, earlier: &[(String, Json, String)]) -> Result<(Json, Option<Json>), String> {
        if arguments.get("accountId").and_then(Json::as_str).is_some_and(|account| account != ACCOUNT) {
            return Err("accountNotFound".to_owned());
        }
        let old_state = Self::submission_state(self.stored_submissions().map_err(|_| "serverFail".to_owned())?.len());
        let mut created = Vec::new();
        let mut not_created = Vec::new();
        let mut sent_emails: Vec<(String, String, String)> = Vec::new();
        if let Some(Json::Object(creations)) = arguments.get("create") {
            for (creation_id, creation) in creations {
                match self.submit(creation, earlier) {
                    Ok((record, email_id)) => {
                        let submission_id = record.get("id").and_then(Json::as_str).unwrap_or_default().to_owned();
                        sent_emails.push((creation_id.clone(), submission_id, email_id));
                        created.push((creation_id.clone(), record));
                    }
                    Err(error) => not_created.push((creation_id.clone(), error)),
                }
            }
        }
        let refusal = |kind: &str| Json::Object(vec![("type".to_owned(), Json::String(kind.to_owned()))]);
        let not_updated: Vec<(String, Json)> = match arguments.get("update") {
            Some(Json::Object(updates)) => updates.iter().map(|(id, _)| (id.clone(), refusal("cannotUnsend"))).collect(),
            _ => Vec::new(),
        };
        let not_destroyed: Vec<(String, Json)> = match arguments.get("destroy").and_then(Json::as_array) {
            Some(ids) => ids.iter().filter_map(Json::as_str).map(|id| (id.to_owned(), refusal("forbidden"))).collect(),
            None => Vec::new(),
        };
        let email_of = |key: &str| -> Option<String> {
            sent_emails
                .iter()
                .find(|(creation_id, submission_id, _)| key.strip_prefix('#') == Some(creation_id.as_str()) || key == submission_id)
                .map(|(_, _, email_id)| email_id.clone())
        };
        let mut updates = Vec::new();
        if let Some(Json::Object(patches)) = arguments.get("onSuccessUpdateEmail") {
            for (key, patch) in patches {
                if let Some(email_id) = email_of(key) {
                    updates.push((email_id, patch.clone()));
                }
            }
        }
        let destroys: Vec<Json> = arguments
            .get("onSuccessDestroyEmail")
            .and_then(Json::as_array)
            .map(|keys| keys.iter().filter_map(Json::as_str).filter_map(email_of).map(Json::String).collect())
            .unwrap_or_default();
        let follow_up = (!updates.is_empty() || !destroys.is_empty()).then(|| {
            let mut members = vec![("accountId".to_owned(), Json::String(ACCOUNT.to_owned()))];
            if !updates.is_empty() {
                members.push(("update".to_owned(), Json::Object(updates)));
            }
            if !destroys.is_empty() {
                members.push(("destroy".to_owned(), Json::Array(destroys)));
            }
            Json::Object(members)
        });
        let new_state = Self::submission_state(self.stored_submissions().map_err(|_| "serverFail".to_owned())?.len());
        let optional = |entries: Vec<(String, Json)>| if entries.is_empty() { Json::Null } else { Json::Object(entries) };
        Ok((
            Json::Object(vec![
                ("accountId".to_owned(), Json::String(ACCOUNT.to_owned())),
                ("oldState".to_owned(), Json::String(old_state)),
                ("newState".to_owned(), Json::String(new_state)),
                ("created".to_owned(), optional(created)),
                ("notCreated".to_owned(), optional(not_created)),
                ("notUpdated".to_owned(), optional(not_updated)),
                ("notDestroyed".to_owned(), optional(not_destroyed)),
            ]),
            follow_up,
        ))
    }

    fn thread_get(&self, arguments: &Json) -> Result<Json, String> {
        let store = self.snapshot().map_err(|_| "serverFail".to_owned())?;
        let ids = Self::wanted_ids(arguments).unwrap_or_default();
        let mut list = Vec::new();
        let mut not_found = Vec::new();
        for id in &ids {
            let email = id.strip_prefix('t').map(|rest| format!("e{rest}"));
            match email.filter(|email| store.find(email).is_some()) {
                Some(email) => list.push(Json::Object(vec![
                    ("id".to_owned(), Json::String(id.clone())),
                    ("emailIds".to_owned(), Json::Array(vec![Json::String(email)])),
                ])),
                None => not_found.push(Json::String(id.clone())),
            }
        }
        Ok(Json::Object(vec![
            ("accountId".to_owned(), Json::String(ACCOUNT.to_owned())),
            ("state".to_owned(), Json::String(Self::state(&store))),
            ("list".to_owned(), Json::Array(list)),
            ("notFound".to_owned(), Json::Array(not_found)),
        ]))
    }
}

/// The value a JMAP result reference's path (RFC 8620, section 3.7) names: JSON pointer steps,
/// where `*` over an array maps the rest of the path over its items and flattens arrays.
fn pointer(value: &Json, path: &str) -> Option<Json> {
    let steps: Vec<String> = path.split('/').skip(1).map(|step| step.replace("~1", "/").replace("~0", "~")).collect();
    walk(value, &steps)
}

fn walk(value: &Json, steps: &[String]) -> Option<Json> {
    let Some((first, rest)) = steps.split_first() else {
        return Some(value.clone());
    };
    match value {
        Json::Array(items) if first == "*" => {
            let mut out = Vec::new();
            for item in items {
                match walk(item, rest)? {
                    Json::Array(inner) => out.extend(inner),
                    other => out.push(other),
                }
            }
            Some(Json::Array(out))
        }
        Json::Array(items) => walk(items.get(first.parse::<usize>().ok()?)?, rest),
        Json::Object(_) => walk(value.get(first)?, rest),
        _ => None,
    }
}

fn without_bcc(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut skipping = false;
    let mut at = 0;
    while at < raw.len() {
        let end = raw[at..].iter().position(|byte| *byte == b'\n').map_or(raw.len(), |found| at + found + 1);
        let line = &raw[at..end];
        if line == b"\r\n" || line == b"\n" {
            out.extend_from_slice(&raw[at..]);
            return out;
        }
        let continuation = line.first().is_some_and(|byte| *byte == b' ' || *byte == b'\t');
        if !continuation {
            skipping = line.len() >= 4 && line[..4].eq_ignore_ascii_case(b"bcc:");
        }
        if !skipping {
            out.extend_from_slice(line);
        }
        at = end;
    }
    out
}

fn parse_rfc3339(text: &str) -> Option<u64> {
    let (date, time) = text.split_once('T')?;
    let mut parts = date.splitn(3, '-');
    let year = parts.next()?;
    let month: usize = parts.next()?.parse().ok()?;
    let day = parts.next()?;
    let names = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let clock = time.trim_end_matches('Z');
    let clock = clock.split(['+', '-']).next().unwrap_or(clock);
    parse_rfc5322_date(&format!("{day} {} {year} {} +0000", names.get(month.checked_sub(1)?)?, clock.split('.').next().unwrap_or(clock)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::MemoryKeyspace;

    fn call(endpoint: &Endpoint<'_, MemoryKeyspace>, method: &str, path: &str, body: &str) -> String {
        let response = endpoint.answer(&Request {
            method: method.to_owned(),
            path: path.to_owned(),
            query: None,
            headers: vec![("authorization".to_owned(), format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:secret")))],
            body: body.as_bytes().to_vec(),
        });
        assert_eq!(response.status, 200, "{}", String::from_utf8_lossy(&response.body));
        String::from_utf8_lossy(&response.body).into_owned()
    }

    #[test]
    fn jmap_queries_reads_and_updates_mail() {
        let keyspace = MemoryKeyspace::default();
        keyspace
            .write(0, &mut |keys| {
                store::append(keys, INBOX, &[], 100, b"From: Ann <ann@example.org>\r\nSubject: Lunch\r\n\r\nNoon?\r\n").map_err(|error| error.0)?;
                store::append(keys, INBOX, &["\\Seen".to_owned()], 200, b"Subject: Later\r\n\r\nTomorrow\r\n").map(drop).map_err(|error| error.0)
            })
            .unwrap();
        let config = ServerConfig {
            user: "eggdb".to_owned(),
            password_sha256: eggdb_crypto::sha256(b"secret"),
            max_connections: 4,
            max_message_bytes: 1 << 20,
            domain: "localhost".to_owned(),
            relay: None,
        };
        let endpoint = Endpoint::new(&keyspace, config);
        let session = call(&endpoint, "GET", "/.well-known/jmap", "");
        assert!(session.contains("\"apiUrl\":\"/jmap/api\""), "{session}");
        let answered = call(
            &endpoint,
            "POST",
            "/jmap/api",
            r##"{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[
                ["Email/query",{"filter":{"notKeyword":"$seen"},"sort":[{"property":"receivedAt","isAscending":false}]},"q"],
                ["Email/get",{"#ids":{"resultOf":"q","name":"Email/query","path":"/ids"},"properties":["subject","from","keywords"],"fetchTextBodyValues":true},"g"],
                ["Mailbox/get",{"ids":null},"m"]]}"##,
        );
        assert!(answered.contains("\"subject\":\"Lunch\""), "{answered}");
        assert!(answered.contains("\"email\":\"ann@example.org\""), "{answered}");
        assert!(answered.contains("\"value\":\"Noon?\\r\\n\""), "{answered}");
        assert!(answered.contains("\"unreadEmails\":1"), "{answered}");
        assert!(!answered.contains("Later"), "{answered}");
        let id = email_id(INBOX, 1);
        let changed = call(
            &endpoint,
            "POST",
            "/jmap/api",
            &format!(r#"{{"using":["urn:ietf:params:jmap:mail"],"methodCalls":[["Email/set",{{"update":{{"{id}":{{"keywords/$seen":true}}}}}},"s"],["Email/query",{{"filter":{{"hasKeyword":"$seen"}}}},"q"]]}}"#),
        );
        assert!(changed.contains(&format!("\"updated\":{{\"{id}\":null}}")), "{changed}");
        assert!(changed.contains("\"total\":2"), "{changed}");
        assert_eq!(pointer(&parse(r#"{"list":[{"id":"a"},{"id":"b"}]}"#).unwrap(), "/list/*/id"), parse(r#"["a","b"]"#));
    }
    #[test]
    fn emails_list_their_parts_and_parts_download() {
        let keyspace = MemoryKeyspace::default();
        keyspace
            .write(0, &mut |keys| {
                store::append(
                    keys,
                    INBOX,
                    &[],
                    100,
                    b"Subject: Menu\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nCaf=C3=A9 menu\r\n--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=menu.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0x\r\n--b--\r\n",
                )
                .map(drop)
                .map_err(|error| error.0)
            })
            .unwrap();
        let config = ServerConfig {
            user: "eggdb".to_owned(),
            password_sha256: eggdb_crypto::sha256(b"secret"),
            max_connections: 4,
            max_message_bytes: 1 << 20,
            domain: "localhost".to_owned(),
            relay: None,
        };
        let endpoint = Endpoint::new(&keyspace, config);
        let id = email_id(INBOX, 1);
        let answered = call(
            &endpoint,
            "POST",
            "/jmap/api",
            &format!(r#"{{"using":["urn:ietf:params:jmap:mail"],"methodCalls":[["Email/get",{{"ids":["{id}"],"properties":["hasAttachment","attachments","textBody","preview"],"fetchTextBodyValues":true}},"g"]]}}"#),
        );
        assert!(answered.contains("\"hasAttachment\":true"), "{answered}");
        assert!(answered.contains("\"name\":\"menu.pdf\""), "{answered}");
        assert!(answered.contains("\"preview\":\"Café menu\""), "{answered}");
        assert!(answered.contains("\"value\":\"Café menu\""), "{answered}");
        let pdf = call(&endpoint, "GET", &format!("/jmap/download/{ACCOUNT}/{id}p2/menu.pdf"), "");
        assert_eq!(pdf, "%PDF-1");
    }

    #[test]
    fn uploaded_messages_import_into_a_mailbox() {
        let keyspace = MemoryKeyspace::default();
        let config = ServerConfig {
            user: "eggdb".to_owned(),
            password_sha256: eggdb_crypto::sha256(b"secret"),
            max_connections: 4,
            max_message_bytes: 1 << 20,
            domain: "localhost".to_owned(),
            relay: None,
        };
        let endpoint = Endpoint::new(&keyspace, config);
        let upload = endpoint.answer(&Request {
            method: "POST".to_owned(),
            path: format!("/jmap/upload/{ACCOUNT}/"),
            query: None,
            headers: vec![
                ("authorization".to_owned(), format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:secret"))),
                ("content-type".to_owned(), "message/rfc822".to_owned()),
            ],
            body: b"Subject: Imported\r\n\r\nHello\r\n".to_vec(),
        });
        assert_eq!(upload.status, 201);
        let uploaded = parse(&String::from_utf8_lossy(&upload.body)).unwrap();
        let blob = uploaded.get("blobId").and_then(Json::as_str).unwrap().to_owned();
        assert_eq!(call(&endpoint, "GET", &format!("/jmap/download/{ACCOUNT}/{blob}/m.eml"), ""), "Subject: Imported\r\n\r\nHello\r\n");
        let inbox = mailbox_id(INBOX);
        let imported = call(
            &endpoint,
            "POST",
            "/jmap/api",
            &format!(r#"{{"using":["urn:ietf:params:jmap:mail"],"methodCalls":[["Email/import",{{"emails":{{"k1":{{"blobId":"{blob}","mailboxIds":{{"{inbox}":true}},"keywords":{{"$seen":true}}}},"k2":{{"blobId":"missing","mailboxIds":{{"{inbox}":true}}}}}}}},"i"],["Email/query",{{"filter":{{"hasKeyword":"$seen"}}}},"q"]]}}"#),
        );
        assert!(imported.contains("\"k1\":{\"id\":"), "{imported}");
        assert!(imported.contains("\"k2\":{\"type\":\"blobNotFound\"}"), "{imported}");
        assert!(imported.contains("\"total\":1"), "{imported}");
    }

    #[test]
    fn submissions_deliver_locally_update_the_email_and_refuse_other_domains_without_a_relay() {
        let keyspace = MemoryKeyspace::default();
        keyspace
            .write(0, &mut |keys| {
                store::append(keys, INBOX, &["\\Draft".to_owned()], 100, b"From: eggdb@localhost\r\nTo: eggdb@localhost\r\nBcc: hidden@localhost\r\nSubject: Note\r\n\r\nHi\r\n").map_err(|error| error.0)?;
                store::append(keys, INBOX, &[], 200, b"From: eggdb@localhost\r\nTo: ann@example.org\r\nSubject: Out\r\n\r\nHello\r\n").map(drop).map_err(|error| error.0)
            })
            .unwrap();
        let config = ServerConfig {
            user: "eggdb".to_owned(),
            password_sha256: eggdb_crypto::sha256(b"secret"),
            max_connections: 4,
            max_message_bytes: 1 << 20,
            domain: "localhost".to_owned(),
            relay: None,
        };
        let endpoint = Endpoint::new(&keyspace, config);
        let session = call(&endpoint, "GET", "/.well-known/jmap", "");
        assert!(session.contains("urn:ietf:params:jmap:submission"), "{session}");
        let (local, remote) = (email_id(INBOX, 1), email_id(INBOX, 2));
        let answered = call(
            &endpoint,
            "POST",
            "/jmap/api",
            &format!(
                r##"{{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail","urn:ietf:params:jmap:submission"],"methodCalls":[
                ["Identity/get",{{}},"i"],
                ["EmailSubmission/set",{{"create":{{"k1":{{"emailId":"{local}","identityId":"i1"}},"k2":{{"emailId":"{remote}","identityId":"i1"}}}},"onSuccessUpdateEmail":{{"#k1":{{"keywords/$draft":null}}}}}},"s"],
                ["EmailSubmission/get",{{"ids":null}},"g"]]}}"##
            ),
        );
        assert!(answered.contains("\"email\":\"eggdb@localhost\""), "{answered}");
        assert!(answered.contains("\"created\":{\"k1\":{\"id\":\"S1\""), "{answered}");
        assert!(answered.contains("\"k2\":{\"type\":\"forbiddenToSend\""), "{answered}");
        assert!(answered.contains(&format!("[\"Email/set\",{{\"accountId\":\"a\"")), "the implicit Email/set follows: {answered}");
        assert!(answered.contains("\"delivered\":\"yes\""), "{answered}");
        let mut inbox = Vec::new();
        keyspace
            .read(0, &mut |keys| {
                inbox = store::messages(keys, INBOX).map_err(|error| error.0)?;
                Ok(())
            })
            .unwrap();
        assert_eq!(inbox.len(), 3, "the local copy was delivered");
        let delivered = String::from_utf8_lossy(&inbox[2].raw).into_owned();
        assert!(delivered.starts_with("Delivered-To: eggdb@localhost, hidden@localhost\r\n"), "{delivered}");
        assert!(!delivered.contains("Bcc:"), "{delivered}");
        assert!(!inbox[0].flags.iter().any(|flag| flag == "\\Draft"), "{:?}", inbox[0].flags);
    }

    fn push_sink(posts: usize) -> (String, std::sync::mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/push", listener.local_addr().unwrap());
        let (sender, receiver) = std::sync::mpsc::channel();
        let sink = std::thread::spawn(move || {
            for _ in 0..posts {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                let mut writer = stream;
                writer.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n").unwrap();
                sender.send(String::from_utf8(body).unwrap()).unwrap();
            }
        });
        (url, receiver, sink)
    }

    #[test]
    fn push_subscriptions_are_verified_then_sent_each_state_change() {
        let keyspace = MemoryKeyspace::default();
        let config = ServerConfig {
            user: "eggdb".to_owned(),
            password_sha256: eggdb_crypto::sha256(b"secret"),
            max_connections: 4,
            max_message_bytes: 1 << 20,
            domain: "localhost".to_owned(),
            relay: None,
        };
        let endpoint = Endpoint::new(&keyspace, config);
        let (url, posts, sink) = push_sink(2);
        let created = call(
            &endpoint,
            "POST",
            "/jmap/api",
            &format!(r#"{{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["PushSubscription/set",{{"create":{{"p":{{"deviceClientId":"phone","url":"{url}","types":["Email"]}}}}}},"c"]]}}"#),
        );
        assert!(created.contains("\"id\":\"P1\""), "{created}");
        let verification = posts.recv().unwrap();
        assert!(verification.contains("\"@type\":\"PushVerification\""), "{verification}");
        let code = parse(&verification).unwrap().get("verificationCode").and_then(Json::as_str).unwrap().to_owned();
        let refused = call(&endpoint, "POST", "/jmap/api", r#"{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["PushSubscription/set",{"update":{"P1":{"verificationCode":"wrong"}}},"u"]]}"#);
        assert!(refused.contains("notUpdated"), "{refused}");
        call(&endpoint, "POST", "/jmap/api", &format!(r#"{{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["PushSubscription/set",{{"update":{{"P1":{{"verificationCode":"{code}"}}}}}},"u"]]}}"#));
        let listed = call(&endpoint, "POST", "/jmap/api", r#"{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["PushSubscription/get",{"ids":null},"g"]]}"#);
        assert!(listed.contains("\"deviceClientId\":\"phone\"") && !listed.contains("/push"), "the url is not shown: {listed}");
        let mut sent = std::collections::BTreeMap::new();
        crate::push::deliver(&keyspace, ACCOUNT, &endpoint.states(), crate::now_seconds(), &mut sent);
        keyspace
            .write(0, &mut |keys| store::append(keys, INBOX, &[], 1, b"Subject: new\r\n\r\nx\r\n").map(drop).map_err(|error| error.0))
            .unwrap();
        crate::push::deliver(&keyspace, ACCOUNT, &endpoint.states(), crate::now_seconds(), &mut sent);
        let change = posts.recv().unwrap();
        assert!(change.contains("\"@type\":\"StateChange\"") && change.contains("\"Email\"") && !change.contains("Mailbox"), "{change}");
        sink.join().unwrap();
        assert!(matches!(endpoint.reply(&Request {
            method: "GET".to_owned(),
            path: "/jmap/eventsource".to_owned(),
            query: Some("types=*&closeafter=state&ping=0".to_owned()),
            headers: vec![("authorization".to_owned(), format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:secret")))],
            body: Vec::new(),
        }), Reply::Streamed(StreamedResponse { status: 200, .. })));
    }
}
