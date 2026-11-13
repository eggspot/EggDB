//! The WebDAV (RFC 4918), CalDAV (RFC 4791) and CardDAV (RFC 6352) server (D895): `OPTIONS`,
//! `GET`, `HEAD`, `PUT`, `DELETE`, `MKCOL` (with RFC 5689's extended form), `MKCALENDAR`,
//! `COPY`, `MOVE`, `PROPFIND`, `PROPPATCH`, `LOCK`, `UNLOCK` and `REPORT`
//! (`calendar-query` with `time-range` filters over recurring events, `calendar-multiget`,
//! `addressbook-query`, `addressbook-multiget`, `sync-collection`), with the user's principal
//! and calendar and address book homes.

use crate::ical::{self, TimeRange};
use crate::store::{self, Kind, Resource, normalized, parent};
use crate::xml::{self, CALDAV, CARDDAV, DAV, escaped};
use eggdb_http::{Request, Response};
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};

const CALENDARSERVER: &str = "http://calendarserver.org/ns/";
const MAX_LISTED: usize = 10_000;
const ALLOWED: &str = "OPTIONS, GET, HEAD, PUT, DELETE, MKCOL, MKCALENDAR, COPY, MOVE, PROPFIND, PROPPATCH, LOCK, UNLOCK, REPORT, ACL, POST";
const EGGDB: &str = "https://eggdb.dev/ns/dav";
/// The largest request body the DAV endpoint accepts.
pub const MAX_REQUEST_BYTES: usize = 64 << 20;

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// `seconds` as an HTTP date: `Thu, 12 Nov 2026 10:00:00 GMT`.
fn http_date(seconds: u64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{}, {day:02} {} {year} {:02}:{:02}:{:02} GMT",
        DAYS[days.rem_euclid(7) as usize],
        MONTHS[(month - 1) as usize],
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn reply(status: u16, content_type: &str, body: Vec<u8>) -> Response {
    Response {
        status,
        content_type: content_type.to_owned(),
        body,
        headers: Vec::new(),
    }
}

fn safe_name(text: &str) -> String {
    let cleaned: String = text.chars().map(|character| if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') { character } else { '_' }).take(120).collect();
    if cleaned.is_empty() || cleaned.starts_with('.') { format!("x{cleaned}") } else { cleaned }
}

fn empty(status: u16) -> Response {
    reply(status, "text/plain; charset=utf-8", Vec::new())
}

fn precondition(status: u16, namespace_prefix: &str, condition: &str) -> Response {
    reply(
        status,
        "application/xml; charset=utf-8",
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:error xmlns:D=\"DAV:\" xmlns:C=\"{CALDAV}\" xmlns:A=\"{CARDDAV}\"><{namespace_prefix}:{condition}/></D:error>"
        )
        .into_bytes(),
    )
}

fn prefix_of(namespace: &str) -> Option<&'static str> {
    Some(match namespace {
        DAV => "D",
        CALDAV => "C",
        CARDDAV => "A",
        CALENDARSERVER => "CS",
        _ => return None,
    })
}

fn tag(namespace: &str, name: &str, inner: Option<&str>) -> String {
    let (open, close) = match prefix_of(namespace) {
        Some(prefix) => (format!("{prefix}:{name}"), format!("{prefix}:{name}")),
        None => (format!("X:{name} xmlns:X=\"{}\"", escaped(namespace)), format!("X:{name}")),
    };
    match inner {
        None => format!("<{open}/>"),
        Some(inner) => format!("<{open}>{inner}</{close}>"),
    }
}

fn href_of(resource: &Resource) -> String {
    let mut href: String = resource
        .path
        .split('/')
        .map(|segment| {
            segment
                .bytes()
                .map(|byte| {
                    if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'@' | b'+') {
                        char::from(byte).to_string()
                    } else {
                        format!("%{byte:02X}")
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/");
    if resource.kind.is_collection() && !href.ends_with('/') {
        href.push('/');
    }
    href
}

fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%'
            && let Some(byte) = text.get(at + 1..at + 3).and_then(|pair| u8::from_str_radix(pair, 16).ok())
        {
            out.push(byte);
            at += 3;
            continue;
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The path a request names, from its own target or a `Destination` header (an absolute URL
/// or a path).
fn path_of(target: &str) -> Option<String> {
    let target = match target.find("://") {
        Some(scheme) => {
            let after = &target[scheme + 3..];
            after.find('/').map_or("/", |at| &after[at..])
        }
        None => target,
    };
    let target = target.split(['?', '#']).next().unwrap_or(target);
    normalized(&percent_decoded(target))
}

/// The DAV endpoint over one user's files, calendars and address books.
pub struct Endpoint<'k, K: Keyspace + ?Sized> {
    keyspace: &'k K,
    user: String,
    password_sha256: [u8; 32],
    read_only: bool,
}

impl<K: Keyspace + ?Sized> std::fmt::Debug for Endpoint<'_, K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("user", &self.user).finish_non_exhaustive()
    }
}

struct Wanted {
    all: bool,
    names_only: bool,
    properties: Vec<(String, String)>,
}

impl<'k, K: Keyspace + ?Sized> Endpoint<'k, K> {
    /// The endpoint over `keyspace`'s database 0 for `user`, signing in with the password
    /// whose SHA-256 is `password_sha256`; with `read_only`, changes are refused.
    pub fn new(keyspace: &'k K, user: &str, password_sha256: [u8; 32], read_only: bool) -> Self {
        Self {
            keyspace,
            user: user.to_owned(),
            password_sha256,
            read_only,
        }
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
        user == self.user && eggdb_crypto::sha256(password.as_bytes()) == self.password_sha256
    }

    fn principal(&self) -> String {
        format!("/principals/{}", self.user)
    }

    fn calendar_home(&self) -> String {
        format!("/calendars/{}", self.user)
    }

    fn inbox(&self) -> String {
        format!("{}/inbox", self.calendar_home())
    }

    fn outbox(&self) -> String {
        format!("{}/outbox", self.calendar_home())
    }

    fn default_calendar(&self) -> String {
        format!("{}/calendar", self.calendar_home())
    }

    fn address(&self) -> String {
        format!("mailto:{}@localhost", self.user)
    }

    fn addressbook_home(&self) -> String {
        format!("/addressbooks/{}", self.user)
    }

    fn read<T>(&self, body: &mut dyn FnMut(&dyn KeysRead) -> Result<T, String>) -> Result<T, String> {
        let mut outcome = None;
        self.keyspace.read(0, &mut |keys| {
            outcome = Some(body(keys));
            Ok(())
        })?;
        outcome.unwrap_or_else(|| Err("the read did not run".to_owned()))
    }

    fn write<T>(&self, body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<T, String>) -> Result<T, String> {
        let mut outcome = None;
        self.keyspace.write(0, &mut |keys| {
            let result = body(keys);
            let failed = result.as_ref().err().cloned();
            outcome = Some(result);
            failed.map_or(Ok(()), Err)
        })?;
        outcome.unwrap_or_else(|| Err("the change did not run".to_owned()))
    }

    fn collection(path: &str, kind: Kind, display: Option<&str>) -> Resource {
        Resource {
            path: path.to_owned(),
            kind,
            content_type: String::new(),
            modified: 0,
            version: 0,
            properties: display
                .map(|name| vec![(DAV.to_owned(), "displayname".to_owned(), name.to_owned())])
                .unwrap_or_default(),
            body: Vec::new(),
        }
    }

    fn ensure_homes(&self) {
        let inbox = self.inbox();
        if self.read_only || self.read(&mut |keys| store::get(keys, &inbox)).is_ok_and(|found| found.is_some()) {
            return;
        }
        let homes = [
            ("/principals".to_owned(), Kind::Collection, None),
            (self.principal(), Kind::Collection, None),
            ("/calendars".to_owned(), Kind::Collection, None),
            (self.calendar_home(), Kind::Collection, None),
            (format!("{}/calendar", self.calendar_home()), Kind::Calendar, Some("Calendar")),
            (self.inbox(), Kind::Collection, Some("Inbox")),
            (self.outbox(), Kind::Collection, Some("Outbox")),
            ("/addressbooks".to_owned(), Kind::Collection, None),
            (self.addressbook_home(), Kind::Collection, None),
            (format!("{}/contacts", self.addressbook_home()), Kind::AddressBook, Some("Contacts")),
        ];
        let now = now_seconds();
        let _ = self.write(&mut |keys| {
            if store::get(&*keys, &self.inbox())?.is_some() {
                return Ok(());
            }
            for (path, kind, display) in &homes {
                if store::get(&*keys, path)?.is_none() {
                    store::put(keys, Self::collection(path, *kind, *display), now)?;
                }
            }
            Ok(())
        });
    }

    /// Answers one HTTP request.
    pub fn answer(&self, request: &Request) -> Response {
        if !self.signed_in(request) {
            let mut response = empty(401);
            response.headers.push(("WWW-Authenticate".to_owned(), "Basic realm=\"eggdb\"".to_owned()));
            return response;
        }
        let method = request.method.to_ascii_uppercase();
        if matches!(request.path.as_str(), "/.well-known/caldav" | "/.well-known/carddav") {
            let mut response = empty(301);
            response.headers.push(("Location".to_owned(), format!("{}/", self.principal())));
            return response;
        }
        let Some(path) = path_of(&request.path) else {
            return empty(400);
        };
        let changes = matches!(method.as_str(), "PUT" | "DELETE" | "MKCOL" | "MKCALENDAR" | "COPY" | "MOVE" | "PROPPATCH" | "LOCK" | "UNLOCK");
        if changes && self.read_only {
            return empty(403);
        }
        self.ensure_homes();
        let mut response = match method.as_str() {
            "OPTIONS" => empty(200),
            "GET" | "HEAD" => self.get(&path, method == "HEAD"),
            "PUT" => self.put(&path, request),
            "DELETE" => self.delete(&path, request),
            "MKCOL" => self.make_collection(&path, request, Kind::Collection),
            "MKCALENDAR" => self.make_collection(&path, request, Kind::Calendar),
            "COPY" | "MOVE" => self.copy(&path, request, method == "MOVE"),
            "PROPFIND" => self.propfind(&path, request),
            "PROPPATCH" => self.proppatch(&path, request),
            "LOCK" => self.lock(&path),
            "UNLOCK" => empty(204),
            "REPORT" => self.report(&path, request),
            "ACL" => self.acl_request(&path, request),
            "POST" if path == self.outbox() => self.free_busy(request),
            _ => empty(405),
        };
        response.headers.push(("DAV".to_owned(), "1, 2, 3, access-control, calendar-access, calendar-auto-schedule, addressbook".to_owned()));
        if method == "OPTIONS" || response.status == 405 {
            response.headers.push(("Allow".to_owned(), ALLOWED.to_owned()));
        }
        response
    }

    fn get(&self, path: &str, head: bool) -> Response {
        let found = self.read(&mut |keys| {
            let resource = store::get(keys, path)?;
            let listed = match &resource {
                Some(resource) if resource.kind.is_collection() => store::children(keys, path)?,
                _ => Vec::new(),
            };
            Ok((resource, listed))
        });
        let Ok((Some(resource), listed)) = found else {
            return empty(404);
        };
        let mut response = if resource.kind.is_collection() {
            let names: Vec<String> = listed.iter().map(href_of).collect();
            reply(200, "text/plain; charset=utf-8", names.join("\n").into_bytes())
        } else {
            reply(200, &resource.content_type, resource.body.clone())
        };
        response.headers.push(("ETag".to_owned(), resource.etag()));
        response.headers.push(("Last-Modified".to_owned(), http_date(resource.modified)));
        if head {
            response.body.clear();
        }
        response
    }

    fn put(&self, path: &str, request: &Request) -> Response {
        let Some(parent_path) = parent(path) else {
            return empty(405);
        };
        let content_type = request.header("content-type").unwrap_or("application/octet-stream").to_owned();
        let if_match = request.header("if-match").map(str::to_owned);
        let if_none_match = request.header("if-none-match").map(str::to_owned);
        let body = request.body.clone();
        let outcome = self.write(&mut |keys| {
            let Some(container) = store::get(&*keys, &parent_path)? else {
                return Ok(Err(empty(409)));
            };
            if !container.kind.is_collection() {
                return Ok(Err(empty(409)));
            }
            let existing = store::get(&*keys, path)?;
            if existing.as_ref().is_some_and(|resource| resource.kind.is_collection()) {
                return Ok(Err(empty(405)));
            }
            if if_none_match.as_deref() == Some("*") && existing.is_some() {
                return Ok(Err(empty(412)));
            }
            if let Some(wanted) = &if_match
                && existing.as_ref().is_none_or(|resource| wanted != "*" && !wanted.split(',').any(|tag| tag.trim() == resource.etag()))
            {
                return Ok(Err(empty(412)));
            }
            match container.kind {
                Kind::Calendar if !body.starts_with(b"BEGIN:VCALENDAR") => return Ok(Err(precondition(403, "C", "valid-calendar-data"))),
                Kind::AddressBook if !body.starts_with(b"BEGIN:VCARD") => return Ok(Err(precondition(403, "A", "valid-address-data"))),
                _ => {}
            }
            let created = existing.is_none();
            let properties = existing.map(|resource| resource.properties).unwrap_or_default();
            let stored = store::put(
                keys,
                Resource {
                    path: path.to_owned(),
                    kind: Kind::File,
                    content_type: content_type.clone(),
                    modified: 0,
                    version: 0,
                    properties,
                    body: body.clone(),
                },
                now_seconds(),
            )?;
            Ok(Ok((created, stored.etag())))
        });
        match outcome {
            Ok(Ok((created, etag))) => {
                self.scheduled_after_put(&parent_path, &body);
                let mut response = empty(if created { 201 } else { 204 });
                response.headers.push(("ETag".to_owned(), etag));
                response
            }
            Ok(Err(response)) => response,
            Err(_) => empty(500),
        }
    }

    fn delete(&self, path: &str, request: &Request) -> Response {
        if path == "/" {
            return empty(403);
        }
        let if_match = request.header("if-match").map(str::to_owned);
        let mut removed_body: Option<Vec<u8>> = None;
        let outcome = self.write(&mut |keys| {
            let Some(existing) = store::get(&*keys, path)? else {
                return Ok(404);
            };
            if let Some(wanted) = &if_match
                && wanted != "*"
                && !wanted.split(',').any(|tag| tag.trim() == existing.etag())
            {
                return Ok(412);
            }
            if existing.kind == Kind::File {
                removed_body = Some(existing.body.clone());
            }
            store::remove(keys, path)?;
            Ok(204)
        });
        if outcome == Ok(204)
            && let Some(body) = removed_body
            && parent(path).is_some_and(|parent_path| parent_path != self.inbox() && parent_path != self.outbox())
        {
            self.queue_outgoing(&body, "CANCEL");
        }
        empty(outcome.unwrap_or(500))
    }

    fn make_collection(&self, path: &str, request: &Request, default_kind: Kind) -> Response {
        let Some(parent_path) = parent(path) else {
            return empty(405);
        };
        let mut kind = default_kind;
        let mut display = None;
        if !request.body.is_empty() {
            let Some(document) = std::str::from_utf8(&request.body).ok().and_then(xml::parse) else {
                return empty(400);
            };
            let mut found = Vec::new();
            document.descendants(DAV, "resourcetype", &mut found);
            if let Some(types) = found.first() {
                if types.child(CALDAV, "calendar").is_some() {
                    kind = Kind::Calendar;
                } else if types.child(CARDDAV, "addressbook").is_some() {
                    kind = Kind::AddressBook;
                }
            }
            let mut names = Vec::new();
            document.descendants(DAV, "displayname", &mut names);
            display = names.first().map(|name| name.text.clone());
        }
        let outcome = self.write(&mut |keys| {
            if store::get(&*keys, path)?.is_some() {
                return Ok(405);
            }
            match store::get(&*keys, &parent_path)? {
                Some(container) if container.kind == Kind::Collection => {}
                _ => return Ok(409),
            }
            store::put(keys, Self::collection(path, kind, display.as_deref()), now_seconds())?;
            Ok(201)
        });
        empty(outcome.unwrap_or(500))
    }

    fn copy(&self, path: &str, request: &Request, moving: bool) -> Response {
        let Some(destination) = request.header("destination").and_then(path_of) else {
            return empty(400);
        };
        if destination == path || destination.starts_with(&format!("{path}/")) || path == "/" {
            return empty(403);
        }
        let overwrite = !request.header("overwrite").is_some_and(|value| value.trim().eq_ignore_ascii_case("F"));
        let Some(destination_parent) = parent(&destination) else {
            return empty(403);
        };
        let outcome = self.write(&mut |keys| {
            let resources = store::subtree(&*keys, path)?;
            if resources.is_empty() {
                return Ok(404);
            }
            if !store::get(&*keys, &destination_parent)?.is_some_and(|container| container.kind.is_collection()) {
                return Ok(409);
            }
            let existed = store::get(&*keys, &destination)?.is_some();
            if existed && !overwrite {
                return Ok(412);
            }
            if existed {
                store::remove(keys, &destination)?;
            }
            let now = now_seconds();
            for resource in resources {
                let moved = format!("{destination}{}", &resource.path[path.len()..]);
                store::put(keys, Resource { path: moved, ..resource }, now)?;
            }
            if moving {
                store::remove(keys, path)?;
            }
            Ok(if existed { 204 } else { 201 })
        });
        empty(outcome.unwrap_or(500))
    }

    fn wanted(body: &[u8]) -> Option<Wanted> {
        if body.iter().all(u8::is_ascii_whitespace) {
            return Some(Wanted {
                all: true,
                names_only: false,
                properties: Vec::new(),
            });
        }
        let document = xml::parse(std::str::from_utf8(body).ok()?)?;
        let names_only = document.child(DAV, "propname").is_some();
        let all = document.child(DAV, "allprop").is_some();
        let properties = document
            .child(DAV, "prop")
            .map(|prop| prop.children.iter().map(|child| (child.namespace.clone(), child.name.clone())).collect())
            .unwrap_or_default();
        Some(Wanted { all, names_only, properties })
    }

    fn live(&self, resource: &Resource, namespace: &str, name: &str) -> Option<String> {
        let href = |path: &str| format!("<D:href>{}/</D:href>", escaped(path));
        Some(match (namespace, name) {
            (DAV, "resourcetype") => match resource.kind {
                Kind::File => String::new(),
                Kind::Collection if resource.path == self.principal() => "<D:collection/><D:principal/>".to_owned(),
                Kind::Collection if resource.path == self.inbox() => "<D:collection/><C:schedule-inbox/>".to_owned(),
                Kind::Collection if resource.path == self.outbox() => "<D:collection/><C:schedule-outbox/>".to_owned(),
                Kind::Collection => "<D:collection/>".to_owned(),
                Kind::Calendar => "<D:collection/><C:calendar/>".to_owned(),
                Kind::AddressBook => "<D:collection/><A:addressbook/>".to_owned(),
            },
            (DAV, "displayname") => escaped(
                resource
                    .properties
                    .iter()
                    .find(|(found_namespace, found, _)| found_namespace == DAV && found == "displayname")
                    .map_or_else(|| resource.path.rsplit('/').next().unwrap_or_default(), |(_, _, value)| value.as_str()),
            ),
            (DAV, "getcontenttype") if resource.kind == Kind::File => escaped(&resource.content_type),
            (DAV, "getcontentlength") if resource.kind == Kind::File => resource.body.len().to_string(),
            (DAV, "getetag") => escaped(&resource.etag()),
            (DAV, "getlastmodified") => http_date(resource.modified),
            (DAV, "current-user-principal") | (DAV, "principal-URL") | (DAV, "owner") => href(&self.principal()),
            (DAV, "supportedlock") => "<D:lockentry><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockentry>".to_owned(),
            (DAV, "lockdiscovery") => String::new(),
            (DAV, "sync-token") if resource.kind.is_collection() => format!("data:,{}", resource.version),
            (CALDAV, "calendar-home-set") => href(&self.calendar_home()),
            (CARDDAV, "addressbook-home-set") => href(&self.addressbook_home()),
            (CALDAV, "calendar-user-address-set") => format!("<D:href>{}</D:href>", escaped(&self.address())),
            (CALDAV, "schedule-inbox-URL") => href(&self.inbox()),
            (CALDAV, "schedule-outbox-URL") => href(&self.outbox()),
            (CALDAV, "schedule-default-calendar-URL") => href(&self.default_calendar()),
            (CALDAV, "calendar-user-type") => "INDIVIDUAL".to_owned(),
            (DAV, "current-user-privilege-set") => crate::acl::current_user_privileges(self.read_only),
            (DAV, "acl") => crate::acl::acl(&self.principal()),
            (DAV, "supported-privilege-set") => crate::acl::supported_privileges(),
            (DAV, "acl-restrictions") => "<D:grant-only/><D:no-invert/>".to_owned(),
            (DAV, "inherited-acl-set") | (DAV, "group-membership") => String::new(),
            (DAV, "principal-collection-set") => href("/principals"),
            (CALDAV, "supported-calendar-component-set") if resource.kind == Kind::Calendar => {
                "<C:comp name=\"VEVENT\"/><C:comp name=\"VTODO\"/><C:comp name=\"VJOURNAL\"/>".to_owned()
            }
            (CALENDARSERVER, "getctag") if resource.kind.is_collection() => resource.version.to_string(),
            _ => return None,
        })
    }

    fn property(&self, resource: &Resource, namespace: &str, name: &str, data: Option<&str>) -> Option<String> {
        if let Some(data) = data {
            return Some(tag(namespace, name, Some(data)));
        }
        if let Some(value) = self.live(resource, namespace, name) {
            return Some(tag(namespace, name, if value.is_empty() { None } else { Some(&value) }));
        }
        resource
            .properties
            .iter()
            .find(|(found_namespace, found, _)| found_namespace == namespace && found == name)
            .map(|(_, _, value)| tag(namespace, name, Some(&escaped(value))))
    }

    fn response_xml(&self, resource: &Resource, wanted: &Wanted, data_for: &dyn Fn(&str, &str, &Resource) -> Option<String>) -> String {
        let mut found = Vec::new();
        let mut missing = Vec::new();
        let names: Vec<(String, String)> = if wanted.all || wanted.names_only {
            let mut names: Vec<(String, String)> = ["resourcetype", "displayname", "getcontenttype", "getcontentlength", "getetag", "getlastmodified"]
                .iter()
                .map(|name| (DAV.to_owned(), (*name).to_owned()))
                .collect();
            names.extend(resource.properties.iter().map(|(namespace, name, _)| (namespace.clone(), name.clone())));
            names
        } else {
            wanted.properties.clone()
        };
        for (namespace, name) in &names {
            if wanted.names_only {
                found.push(tag(namespace, name, None));
                continue;
            }
            let data = data_for(namespace, name, resource);
            match self.property(resource, namespace, name, data.as_deref()) {
                Some(xml) => found.push(xml),
                None if !(wanted.all) => missing.push(tag(namespace, name, None)),
                None => {}
            }
        }
        let mut out = format!("<D:response><D:href>{}</D:href>", escaped(&href_of(resource)));
        if !found.is_empty() || missing.is_empty() {
            out.push_str(&format!("<D:propstat><D:prop>{}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>", found.join("")));
        }
        if !missing.is_empty() {
            out.push_str(&format!("<D:propstat><D:prop>{}</D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>", missing.join("")));
        }
        out.push_str("</D:response>");
        out
    }

    fn multistatus(body: &str) -> Response {
        reply(
            207,
            "application/xml; charset=utf-8",
            format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"{CALDAV}\" xmlns:A=\"{CARDDAV}\" xmlns:CS=\"{CALENDARSERVER}\">{body}</D:multistatus>"
            )
            .into_bytes(),
        )
    }

    fn listed(&self, path: &str, depth: &str) -> Result<Vec<Resource>, String> {
        self.read(&mut |keys| {
            let Some(resource) = store::get(keys, path)? else {
                return Ok(Vec::new());
            };
            let mut found = vec![resource.clone()];
            if resource.kind.is_collection() {
                match depth {
                    "0" => {}
                    "1" => found.extend(store::children(keys, path)?),
                    _ => found.extend(store::subtree(keys, path)?.into_iter().skip(1)),
                }
            }
            found.truncate(MAX_LISTED);
            Ok(found)
        })
    }

    fn propfind(&self, path: &str, request: &Request) -> Response {
        let Some(wanted) = Self::wanted(&request.body) else {
            return empty(400);
        };
        let depth = request.header("depth").unwrap_or("infinity").trim().to_ascii_lowercase();
        let Ok(resources) = self.listed(path, &depth) else {
            return empty(500);
        };
        if resources.is_empty() {
            return empty(404);
        }
        let no_data = |_: &str, _: &str, _: &Resource| None;
        let body: String = resources.iter().map(|resource| self.response_xml(resource, &wanted, &no_data)).collect();
        Self::multistatus(&body)
    }

    fn proppatch(&self, path: &str, request: &Request) -> Response {
        let Some(document) = std::str::from_utf8(&request.body).ok().and_then(xml::parse) else {
            return empty(400);
        };
        if !document.is(DAV, "propertyupdate") {
            return empty(400);
        }
        let mut changes: Vec<(bool, String, String, String)> = Vec::new();
        for instruction in &document.children {
            let setting = instruction.is(DAV, "set");
            if !setting && !instruction.is(DAV, "remove") {
                continue;
            }
            if let Some(prop) = instruction.child(DAV, "prop") {
                for property in &prop.children {
                    changes.push((setting, property.namespace.clone(), property.name.clone(), property.text.clone()));
                }
            }
        }
        let protected = |namespace: &str, name: &str| namespace == DAV && name != "displayname";
        if changes.iter().any(|(_, namespace, name, _)| protected(namespace, name)) {
            let listed: String = changes
                .iter()
                .map(|(_, namespace, name, _)| {
                    let status = if protected(namespace, name) { "403 Forbidden" } else { "424 Failed Dependency" };
                    format!("<D:propstat><D:prop>{}</D:prop><D:status>HTTP/1.1 {status}</D:status></D:propstat>", tag(namespace, name, None))
                })
                .collect();
            return Self::multistatus(&format!("<D:response><D:href>{}</D:href>{listed}</D:response>", escaped(path)));
        }
        let outcome = self.write(&mut |keys| {
            let Some(mut resource) = store::get(&*keys, path)? else {
                return Ok(None);
            };
            for (setting, namespace, name, value) in &changes {
                resource.properties.retain(|(found_namespace, found, _)| !(found_namespace == namespace && found == name));
                if *setting {
                    resource.properties.push((namespace.clone(), name.clone(), value.clone()));
                }
            }
            let stored = store::put(keys, resource, now_seconds())?;
            Ok(Some(stored))
        });
        match outcome {
            Ok(Some(resource)) => {
                let listed: String = changes.iter().map(|(_, namespace, name, _)| tag(namespace, name, None)).collect();
                Self::multistatus(&format!(
                    "<D:response><D:href>{}</D:href><D:propstat><D:prop>{listed}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
                    escaped(&href_of(&resource))
                ))
            }
            Ok(None) => empty(404),
            Err(_) => empty(500),
        }
    }

    fn lock(&self, path: &str) -> Response {
        let token = format!("opaquelocktoken:{:016x}-{}", now_seconds(), path.len());
        let mut response = reply(
            200,
            "application/xml; charset=utf-8",
            format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:prop xmlns:D=\"DAV:\"><D:lockdiscovery><D:activelock><D:locktype><D:write/></D:locktype><D:lockscope><D:exclusive/></D:lockscope><D:depth>infinity</D:depth><D:timeout>Second-3600</D:timeout><D:locktoken><D:href>{token}</D:href></D:locktoken><D:lockroot><D:href>{}</D:href></D:lockroot></D:activelock></D:lockdiscovery></D:prop>",
                escaped(path)
            )
            .into_bytes(),
        );
        response.headers.push(("Lock-Token".to_owned(), format!("<{token}>")));
        response
    }

    fn scheduled_after_put(&self, parent_path: &str, body: &[u8]) {
        if self.read_only || !body.starts_with(b"BEGIN:VCALENDAR") {
            return;
        }
        let text = String::from_utf8_lossy(body).into_owned();
        if parent_path == self.inbox() {
            self.apply_incoming(&text);
        } else if parent_path != self.outbox() && self.read(&mut |keys| store::get(keys, parent_path)).ok().flatten().is_some_and(|parent| parent.kind == Kind::Calendar) {
            self.queue_outgoing(body, "REQUEST");
        }
    }

    fn apply_incoming(&self, message: &str) {
        let Some(uid) = scheduling::first(&scheduling::lines(message), "UID").map(str::to_owned) else {
            return;
        };
        let calendar = self.default_calendar();
        let _ = self.write(&mut |keys| {
            let existing = store::children(&*keys, &calendar)?
                .into_iter()
                .find(|resource| resource.kind == Kind::File && scheduling::first(&scheduling::lines(&String::from_utf8_lossy(&resource.body)), "UID") == Some(uid.as_str()));
            let existing_text = existing.as_ref().map(|resource| String::from_utf8_lossy(&resource.body).into_owned());
            let scheduling::Applied::Replace(text) = scheduling::apply(message, existing_text.as_deref()) else {
                return Ok(());
            };
            let target = existing.as_ref().map_or_else(|| format!("{calendar}/{}.ics", safe_name(&uid)), |resource| resource.path.clone());
            let properties = existing.map(|resource| resource.properties).unwrap_or_default();
            store::put(
                keys,
                Resource {
                    path: target,
                    kind: Kind::File,
                    content_type: "text/calendar; charset=utf-8".to_owned(),
                    modified: 0,
                    version: 0,
                    properties,
                    body: text.into_bytes(),
                },
                now_seconds(),
            )?;
            Ok(())
        });
    }

    fn queue_outgoing(&self, body: &[u8], method: &str) {
        let text = String::from_utf8_lossy(body).into_owned();
        let Some((message, recipients)) = scheduling::outgoing(&text, &self.user, method) else {
            return;
        };
        let uid = scheduling::first(&scheduling::lines(&text), "UID").unwrap_or("event").to_owned();
        let outbox = self.outbox();
        let now = now_seconds();
        let _ = self.write(&mut |keys| {
            let queued = store::children(&*keys, &outbox)?.len();
            store::put(
                keys,
                Resource {
                    path: format!("{outbox}/{}-{now}-{queued}.ics", safe_name(&uid)),
                    kind: Kind::File,
                    content_type: "text/calendar; charset=utf-8".to_owned(),
                    modified: 0,
                    version: 0,
                    properties: vec![(EGGDB.to_owned(), "recipients".to_owned(), recipients.join(" "))],
                    body: message.clone().into_bytes(),
                },
                now,
            )?;
            Ok(())
        });
    }

    fn free_busy(&self, request: &Request) -> Response {
        let Some(question) = std::str::from_utf8(&request.body).ok().and_then(scheduling::free_busy_request) else {
            return precondition(400, "C", "valid-scheduling-message");
        };
        let home = self.calendar_home();
        let (inbox, outbox) = (self.inbox(), self.outbox());
        let objects = match self.read(&mut |keys| {
            Ok(store::subtree(keys, &home)?
                .into_iter()
                .filter(|resource| resource.kind == Kind::File && !resource.path.starts_with(&format!("{inbox}/")) && !resource.path.starts_with(&format!("{outbox}/")))
                .filter(|resource| resource.body.starts_with(b"BEGIN:VCALENDAR"))
                .map(|resource| String::from_utf8_lossy(&resource.body).into_owned())
                .collect::<Vec<_>>())
        }) {
            Ok(objects) => objects,
            Err(_) => return empty(500),
        };
        let periods = scheduling::busy(&objects, TimeRange { start: Some(question.range.0), end: Some(question.range.1) });
        let now = i64::try_from(now_seconds()).unwrap_or(i64::MAX);
        let mut responses = String::new();
        for attendee in &question.attendees {
            let (status, data) = if scheduling::is_user(attendee, &self.user) {
                ("2.0;Success", format!("<C:calendar-data>{}</C:calendar-data>", escaped(&scheduling::free_busy_reply(&question, attendee, &periods, now))))
            } else {
                ("3.7;Invalid calendar user", String::new())
            };
            responses.push_str(&format!(
                "<C:response><C:recipient><D:href>{}</D:href></C:recipient><C:request-status>{status}</C:request-status>{data}</C:response>",
                escaped(attendee)
            ));
        }
        reply(
            200,
            "application/xml; charset=utf-8",
            format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<C:schedule-response xmlns:D=\"DAV:\" xmlns:C=\"{CALDAV}\">{responses}</C:schedule-response>").into_bytes(),
        )
    }

    fn acl_request(&self, path: &str, request: &Request) -> Response {
        if self.read(&mut |keys| store::get(keys, path)).ok().flatten().is_none() {
            return empty(404);
        }
        match crate::acl::refusal(&String::from_utf8_lossy(&request.body), &self.principal()) {
            None => empty(200),
            Some(condition) => precondition(403, "D", condition),
        }
    }

    fn principal_report(&self, document: &xml::Element) -> Response {
        if document.is(DAV, "principal-search-property-set") {
            return reply(
                200,
                "application/xml; charset=utf-8",
                format!(
                    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:principal-search-property-set xmlns:D=\"DAV:\" xmlns:C=\"{CALDAV}\"><D:principal-search-property><D:prop><D:displayname/></D:prop><D:description xml:lang=\"en\">Name</D:description></D:principal-search-property><D:principal-search-property><D:prop><C:calendar-user-address-set/></D:prop><D:description xml:lang=\"en\">Address</D:description></D:principal-search-property></D:principal-search-property-set>"
                )
                .into_bytes(),
            );
        }
        if !crate::acl::search_matches(document, &self.user, &self.address()) {
            return Self::multistatus("");
        }
        let principal = self.principal();
        let Ok(Some(resource)) = self.read(&mut |keys| store::get(keys, &principal)) else {
            return Self::multistatus("");
        };
        let wanted = Wanted {
            all: false,
            names_only: false,
            properties: document
                .child(DAV, "prop")
                .map(|prop| prop.children.iter().map(|child| (child.namespace.clone(), child.name.clone())).collect())
                .unwrap_or_default(),
        };
        Self::multistatus(&self.response_xml(&resource, &wanted, &|_, _, _| None))
    }

    fn report(&self, path: &str, request: &Request) -> Response {
        let Some(document) = std::str::from_utf8(&request.body).ok().and_then(xml::parse) else {
            return empty(400);
        };
        if document.is(DAV, "principal-property-search") || document.is(DAV, "principal-search-property-set") {
            return self.principal_report(&document);
        }
        let properties: Vec<(String, String)> = document
            .child(DAV, "prop")
            .map(|prop| prop.children.iter().map(|child| (child.namespace.clone(), child.name.clone())).collect())
            .unwrap_or_default();
        let wanted = Wanted {
            all: false,
            names_only: false,
            properties,
        };
        let expand = document
            .child(DAV, "prop")
            .and_then(|prop| prop.child(CALDAV, "calendar-data"))
            .and_then(|data| data.child(CALDAV, "expand"))
            .map(|expand| {
                let bound = |name: &str| expand.attribute(name).and_then(ical::parse_moment).map(|moment| moment.seconds);
                TimeRange {
                    start: bound("start"),
                    end: bound("end"),
                }
            });
        let data = |namespace: &str, name: &str, resource: &Resource| {
            let wanted_data = (namespace == CALDAV && name == "calendar-data") || (namespace == CARDDAV && name == "address-data");
            if !wanted_data || resource.kind != Kind::File {
                return None;
            }
            let text = String::from_utf8_lossy(&resource.body);
            Some(match expand {
                Some(range) if name == "calendar-data" => escaped(&ical::expanded(&text, range)),
                _ => escaped(&text),
            })
        };
        let multiget = document.is(CALDAV, "calendar-multiget") || document.is(CARDDAV, "addressbook-multiget");
        let resources: Vec<Resource> = if multiget {
            let mut hrefs = Vec::new();
            document.descendants(DAV, "href", &mut hrefs);
            let paths: Vec<String> = hrefs.iter().filter_map(|href| path_of(href.text.trim())).collect();
            match self.read(&mut |keys| {
                let mut found = Vec::new();
                for wanted in &paths {
                    found.push((wanted.clone(), store::get(keys, wanted)?));
                }
                Ok(found)
            }) {
                Ok(found) => {
                    let mut missing = String::new();
                    let mut present = Vec::new();
                    for (wanted_path, resource) in found {
                        match resource {
                            Some(resource) => present.push(resource),
                            None => missing.push_str(&format!(
                                "<D:response><D:href>{}</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response>",
                                escaped(&wanted_path)
                            )),
                        }
                    }
                    let body: String = present.iter().map(|resource| self.response_xml(resource, &wanted, &data)).collect();
                    return Self::multistatus(&format!("{body}{missing}"));
                }
                Err(_) => return empty(500),
            }
        } else if document.is(CALDAV, "calendar-query") || document.is(CARDDAV, "addressbook-query") || document.is(DAV, "sync-collection") {
            match self.listed(path, "1") {
                Ok(listed) => listed.into_iter().skip(1).filter(|resource| resource.kind == Kind::File).collect(),
                Err(_) => return empty(500),
            }
        } else {
            return precondition(403, "D", "supported-report");
        };
        let mut filters = Vec::new();
        document.descendants(CALDAV, "comp-filter", &mut filters);
        let components: Vec<String> = filters
            .iter()
            .filter_map(|filter| filter.attribute("name"))
            .filter(|name| !name.eq_ignore_ascii_case("VCALENDAR"))
            .map(str::to_ascii_uppercase)
            .collect();
        let ranged: Vec<(String, TimeRange)> = filters
            .iter()
            .filter_map(|filter| {
                let range = filter.child(CALDAV, "time-range")?;
                let bound = |name: &str| range.attribute(name).and_then(ical::parse_moment).map(|moment| moment.seconds);
                Some((
                    filter.attribute("name")?.to_ascii_uppercase(),
                    TimeRange {
                        start: bound("start"),
                        end: bound("end"),
                    },
                ))
            })
            .collect();
        let mut text_matches = Vec::new();
        document.descendants(CARDDAV, "text-match", &mut text_matches);
        let kept: Vec<&Resource> = resources
            .iter()
            .filter(|resource| {
                let body = String::from_utf8_lossy(&resource.body);
                let components_hold = components.iter().all(|name| body.contains(&format!("BEGIN:{name}")));
                let lowered = body.to_lowercase();
                let texts_hold = text_matches.iter().all(|filter| lowered.contains(&filter.text.trim().to_lowercase()));
                let ranges_hold = ranged.is_empty() || {
                    let found = ical::components(&body);
                    ranged.iter().all(|(name, range)| {
                        found.iter().any(|component| &component.name == name && ical::overlaps(component, *range))
                    })
                };
                components_hold && texts_hold && ranges_hold
            })
            .collect();
        let mut body: String = kept.iter().map(|resource| self.response_xml(resource, &wanted, &data)).collect();
        if document.is(DAV, "sync-collection")
            && let Ok(Some(collection)) = self.read(&mut |keys| store::get(keys, path))
        {
            body.push_str(&format!("<D:sync-token>data:,{}</D:sync-token>", collection.version));
        }
        Self::multistatus(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::MemoryKeyspace;

    fn call(endpoint: &Endpoint<'_, MemoryKeyspace>, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Response {
        let mut all = vec![("authorization".to_owned(), format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:secret")))];
        all.extend(headers.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())));
        endpoint.answer(&Request {
            method: method.to_owned(),
            path: path.to_owned(),
            query: None,
            headers: all,
            body: body.as_bytes().to_vec(),
        })
    }

    #[test]
    fn webdav_files_and_collections() {
        let keyspace = MemoryKeyspace::default();
        let endpoint = Endpoint::new(&keyspace, "eggdb", eggdb_crypto::sha256(b"secret"), false);
        assert_eq!(call(&endpoint, "MKCOL", "/docs", &[], "").status, 201);
        assert_eq!(call(&endpoint, "MKCOL", "/docs", &[], "").status, 405);
        assert_eq!(call(&endpoint, "MKCOL", "/missing/x", &[], "").status, 409);
        let put = call(&endpoint, "PUT", "/docs/a.txt", &[("content-type", "text/plain")], "hello");
        assert_eq!(put.status, 201);
        assert_eq!(call(&endpoint, "PUT", "/docs/a.txt", &[("if-none-match", "*")], "again").status, 412);
        assert_eq!(call(&endpoint, "GET", "/docs/a.txt", &[], "").body, b"hello");
        let listed = call(&endpoint, "PROPFIND", "/docs/", &[("depth", "1")], "<?xml version=\"1.0\"?><D:propfind xmlns:D=\"DAV:\"><D:prop><D:getcontentlength/><D:resourcetype/><D:nothing/></D:prop></D:propfind>");
        assert_eq!(listed.status, 207);
        let text = String::from_utf8_lossy(&listed.body).into_owned();
        assert!(text.contains("<D:href>/docs/a.txt</D:href>"), "{text}");
        assert!(text.contains("<D:getcontentlength>5</D:getcontentlength>"), "{text}");
        assert!(text.contains("HTTP/1.1 404 Not Found"), "{text}");
        assert_eq!(call(&endpoint, "MOVE", "/docs/a.txt", &[("destination", "http://localhost/docs/b.txt")], "").status, 201);
        assert_eq!(call(&endpoint, "GET", "/docs/a.txt", &[], "").status, 404);
        let patched = call(&endpoint, "PROPPATCH", "/docs/b.txt", &[], "<D:propertyupdate xmlns:D=\"DAV:\"><D:set><D:prop><D:displayname>Notes</D:displayname></D:prop></D:set></D:propertyupdate>");
        assert_eq!(patched.status, 207);
        assert_eq!(call(&endpoint, "DELETE", "/docs", &[], "").status, 204);
        assert_eq!(call(&endpoint, "GET", "/docs/b.txt", &[], "").status, 404);
    }

    #[test]
    fn caldav_and_carddav_collections_answer_reports() {
        let keyspace = MemoryKeyspace::default();
        let endpoint = Endpoint::new(&keyspace, "eggdb", eggdb_crypto::sha256(b"secret"), false);
        let principal = call(&endpoint, "PROPFIND", "/principals/eggdb/", &[("depth", "0")], "<D:propfind xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><C:calendar-home-set/><D:current-user-principal/></D:prop></D:propfind>");
        let text = String::from_utf8_lossy(&principal.body).into_owned();
        assert!(text.contains("<C:calendar-home-set><D:href>/calendars/eggdb/</D:href></C:calendar-home-set>"), "{text}");
        let event = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:1\r\nSUMMARY:Lunch\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(call(&endpoint, "PUT", "/calendars/eggdb/calendar/1.ics", &[("content-type", "text/calendar")], event).status, 201);
        assert_eq!(call(&endpoint, "PUT", "/calendars/eggdb/calendar/bad.ics", &[], "not a calendar").status, 403);
        let queried = call(
            &endpoint,
            "REPORT",
            "/calendars/eggdb/calendar/",
            &[("depth", "1")],
            "<C:calendar-query xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><D:getetag/><C:calendar-data/></D:prop><C:filter><C:comp-filter name=\"VCALENDAR\"/></C:filter></C:calendar-query>",
        );
        let text = String::from_utf8_lossy(&queried.body).into_owned();
        assert!(text.contains("SUMMARY:Lunch"), "{text}");
        assert_eq!(call(&endpoint, "MKCALENDAR", "/calendars/eggdb/work", &[], "").status, 201);
        let card = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Ann Example\r\nEND:VCARD\r\n";
        assert_eq!(call(&endpoint, "PUT", "/addressbooks/eggdb/contacts/ann.vcf", &[("content-type", "text/vcard")], card).status, 201);
        let found = call(
            &endpoint,
            "REPORT",
            "/addressbooks/eggdb/contacts/",
            &[],
            "<A:addressbook-multiget xmlns:D=\"DAV:\" xmlns:A=\"urn:ietf:params:xml:ns:carddav\"><D:prop><A:address-data/></D:prop><D:href>/addressbooks/eggdb/contacts/ann.vcf</D:href><D:href>/addressbooks/eggdb/contacts/gone.vcf</D:href></A:addressbook-multiget>",
        );
        let text = String::from_utf8_lossy(&found.body).into_owned();
        assert!(text.contains("FN:Ann Example"), "{text}");
        assert!(text.contains("404 Not Found"), "{text}");
    }

    #[test]
    fn calendar_queries_keep_the_events_in_their_time_range() {
        let keyspace = MemoryKeyspace::default();
        let endpoint = Endpoint::new(&keyspace, "eggdb", eggdb_crypto::sha256(b"secret"), false);
        let standup = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:standup\r\nSUMMARY:Standup\r\nDTSTART:20260105T090000Z\r\nDURATION:PT15M\r\nRRULE:FREQ=DAILY;COUNT=5\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let review = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:review\r\nSUMMARY:Review\r\nDTSTART:20260201T090000Z\r\nDTEND:20260201T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(call(&endpoint, "PUT", "/calendars/eggdb/calendar/standup.ics", &[], standup).status, 201);
        assert_eq!(call(&endpoint, "PUT", "/calendars/eggdb/calendar/review.ics", &[], review).status, 201);
        let query = |start: &str, end: &str| {
            let body = format!(
                "<C:calendar-query xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><C:calendar-data/></D:prop><C:filter><C:comp-filter name=\"VCALENDAR\"><C:comp-filter name=\"VEVENT\"><C:time-range start=\"{start}\" end=\"{end}\"/></C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"
            );
            String::from_utf8_lossy(&call(&endpoint, "REPORT", "/calendars/eggdb/calendar/", &[("depth", "1")], &body).body).into_owned()
        };
        let week = query("20260107T000000Z", "20260108T000000Z");
        assert!(week.contains("Standup") && !week.contains("Review"), "{week}");
        let later = query("20260110T000000Z", "20260202T000000Z");
        assert!(!later.contains("Standup") && later.contains("Review"), "{later}");
    }

    #[test]
    fn scheduling_answers_free_busy_applies_replies_and_queues_invitations() {
        let keyspace = MemoryKeyspace::default();
        let endpoint = Endpoint::new(&keyspace, "eggdb", eggdb_crypto::sha256(b"secret"), false);
        let principal = call(&endpoint, "PROPFIND", "/principals/eggdb/", &[("depth", "0")], "<D:propfind xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><C:schedule-inbox-URL/><C:schedule-outbox-URL/><D:current-user-privilege-set/><D:acl/></D:prop></D:propfind>");
        let text = String::from_utf8_lossy(&principal.body).into_owned();
        assert!(text.contains("/calendars/eggdb/inbox/") && text.contains("/calendars/eggdb/outbox/"), "{text}");
        assert!(text.contains("<D:write-content/>") && text.contains("<D:all/>"), "{text}");
        let meeting = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:m1\r\nDTSTAMP:20261101T000000Z\r\nDTSTART:20261113T090000Z\r\nDTEND:20261113T100000Z\r\nORGANIZER:mailto:eggdb@localhost\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:ann@example.org\r\nSUMMARY:Plan\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(call(&endpoint, "PUT", "/calendars/eggdb/calendar/m1.ics", &[], meeting).status, 201);
        let queued = call(&endpoint, "PROPFIND", "/calendars/eggdb/outbox/", &[("depth", "1")], "");
        let queued = String::from_utf8_lossy(&queued.body).into_owned();
        assert!(queued.contains("/calendars/eggdb/outbox/m1-"), "an invitation waits in the outbox: {queued}");
        let reply = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:m1\r\nDTSTAMP:20261102T000000Z\r\nORGANIZER:mailto:eggdb@localhost\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:ann@example.org\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(call(&endpoint, "PUT", "/calendars/eggdb/inbox/r1.ics", &[], reply).status, 201);
        let updated = String::from_utf8_lossy(&call(&endpoint, "GET", "/calendars/eggdb/calendar/m1.ics", &[], "").body).into_owned();
        assert!(updated.contains("ATTENDEE;PARTSTAT=ACCEPTED:mailto:ann@example.org"), "{updated}");
        let question = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VFREEBUSY\r\nUID:fb1\r\nDTSTAMP:20261101T000000Z\r\nDTSTART:20261113T000000Z\r\nDTEND:20261114T000000Z\r\nORGANIZER:mailto:ann@example.org\r\nATTENDEE:mailto:eggdb@localhost\r\nATTENDEE:mailto:bob@example.org\r\nEND:VFREEBUSY\r\nEND:VCALENDAR\r\n";
        let answer = call(&endpoint, "POST", "/calendars/eggdb/outbox/", &[("content-type", "text/calendar")], question);
        let answer = String::from_utf8_lossy(&answer.body).into_owned();
        assert!(answer.contains("FREEBUSY;FBTYPE=BUSY:20261113T090000Z/20261113T100000Z"), "{answer}");
        assert!(answer.contains("3.7;Invalid calendar user"), "{answer}");
        assert_eq!(call(&endpoint, "ACL", "/calendars/eggdb/calendar/", &[], "<D:acl xmlns:D=\"DAV:\"><D:ace><D:principal><D:href>/principals/stranger/</D:href></D:principal><D:grant><D:privilege><D:read/></D:privilege></D:grant></D:ace></D:acl>").status, 403);
        assert_eq!(call(&endpoint, "ACL", "/calendars/eggdb/calendar/", &[], "<D:acl xmlns:D=\"DAV:\"><D:ace><D:principal><D:self/></D:principal><D:grant><D:privilege><D:all/></D:privilege></D:grant></D:ace></D:acl>").status, 200);
        let found = call(&endpoint, "REPORT", "/principals/", &[], "<D:principal-property-search xmlns:D=\"DAV:\"><D:property-search><D:prop><D:displayname/></D:prop><D:match>egg</D:match></D:property-search><D:prop><D:displayname/></D:prop></D:principal-property-search>");
        assert!(String::from_utf8_lossy(&found.body).contains("/principals/eggdb"), "{}", String::from_utf8_lossy(&found.body));
    }
}
