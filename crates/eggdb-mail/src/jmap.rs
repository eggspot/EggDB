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
use eggdb_http::{Request, Response};
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};

const ACCOUNT: &str = "a";
const MAX_CALLS: usize = 64;
const MAX_OBJECTS: usize = 1000;
const PREVIEW_CHARS: usize = 256;
const CORE: &str = "urn:ietf:params:jmap:core";
const MAIL: &str = "urn:ietf:params:jmap:mail";
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
            "{{\"maxSizeUpload\":0,\"maxConcurrentUpload\":1,\"maxSizeRequest\":{MAX_REQUEST_BYTES},\"maxConcurrentRequests\":4,\"maxCallsInRequest\":{MAX_CALLS},\"maxObjectsInGet\":{MAX_OBJECTS},\"maxObjectsInSet\":{MAX_OBJECTS},\"collationAlgorithms\":[\"i;ascii-casemap\"]}}"
        );
        let mail = "{\"maxMailboxesPerEmail\":1,\"maxMailboxDepth\":null,\"maxSizeMailboxName\":1024,\"maxSizeAttachmentsPerEmail\":0,\"emailQuerySortOptions\":[\"receivedAt\",\"size\"],\"mayCreateTopLevelMailbox\":true}";
        json_reply(
            200,
            format!(
                "{{\"capabilities\":{{{core_name}:{core},{mail_name}:{{}}}},\"accounts\":{{\"{ACCOUNT}\":{{\"name\":{user},\"isPersonal\":true,\"isReadOnly\":false,\"accountCapabilities\":{{{mail_name}:{mail}}}}}}},\"primaryAccounts\":{{{mail_name}:\"{ACCOUNT}\"}},\"username\":{user},\"apiUrl\":\"/jmap/api\",\"downloadUrl\":\"/jmap/download/{{accountId}}/{{blobId}}/{{name}}?accept={{type}}\",\"uploadUrl\":\"/jmap/upload/{{accountId}}/\",\"eventSourceUrl\":\"/jmap/eventsource\",\"state\":{}}}",
                string(&state),
                core_name = string(CORE),
                mail_name = string(MAIL),
                user = string(&self.config.user),
            ),
        )
    }

    fn download(&self, blob: &str) -> Response {
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
            if !matches!(capability.as_str(), Some(CORE | MAIL)) {
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
            let (response_name, response) = match self.resolve_references(arguments, &responses) {
                Ok(arguments) => self.method(name, &arguments),
                Err(()) => method_error("invalidResultReference"),
            };
            responses.push((response_name, response, call_id.to_owned()));
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
            "Thread/get" => self.thread_get(arguments),
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
}
