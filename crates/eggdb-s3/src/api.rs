use crate::store::{ObjectInfo, Store, StoreError};
use crate::xml::{elements, escape, text};
use eggdb_auth::sigv4::{self, SigV4Error, SignedRequest, uri_encode, utc_time};
use eggdb_http::{Request, Response};
use eggdb_keyspace::Keyspace;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The largest request body the endpoint accepts: one `PutObject` or one multipart part.
pub const MAX_REQUEST_BYTES: usize = 64 << 20;
const MAX_KEYS: usize = 1000;
const MAX_PART_NUMBER: u32 = 10_000;
const XMLNS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
const DEFAULT_CONTENT_TYPE: &str = "binary/octet-stream";
const LIST_RESUME_MARK: char = '\u{10FFFF}';

/// The S3-compatible endpoint over one keyspace's buckets.
pub struct Endpoint<'k> {
    store: Store<'k>,
    access_key: String,
    secret: String,
    uploads: AtomicU64,
}

impl std::fmt::Debug for Endpoint<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("access_key", &self.access_key).finish_non_exhaustive()
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn etag(info: &ObjectInfo) -> String {
    if info.parts > 0 { format!("\"{}-{}\"", hex(&info.md5), info.parts) } else { format!("\"{}\"", hex(&info.md5)) }
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn iso_time(ms: u64) -> String {
    let time = utc_time(i64::try_from(ms / 1000).unwrap_or(i64::MAX));
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", time.year, time.month, time.day, time.hour, time.minute, time.second, ms % 1000)
}

fn http_time(ms: u64) -> String {
    let time = utc_time(i64::try_from(ms / 1000).unwrap_or(i64::MAX));
    let month = MONTHS[usize::try_from(time.month - 1).unwrap_or(0) % 12];
    let weekday = WEEKDAYS[usize::try_from(time.weekday).unwrap_or(0) % 7];
    format!("{weekday}, {:02} {month} {:04} {:02}:{:02}:{:02} GMT", time.day, time.year, time.hour, time.minute, time.second)
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            out.push(u8::from_str_radix(text.get(at + 1..at + 3)?, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn xml(status: u16, body: String) -> Response {
    Response { status, content_type: "application/xml".to_owned(), body: format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{body}").into_bytes(), headers: Vec::new() }
}

fn empty(status: u16) -> Response {
    Response { status, content_type: "application/xml".to_owned(), body: Vec::new(), headers: Vec::new() }
}

fn failure(status: u16, code: &str, message: &str, resource: &str) -> Response {
    xml(
        status,
        format!("<Error><Code>{code}</Code><Message>{}</Message><Resource>{}</Resource><RequestId>eggdb</RequestId></Error>", escape(message), escape(resource)),
    )
}

fn store_failure(error: &StoreError, resource: &str) -> Response {
    match error {
        StoreError::NoSuchBucket => failure(404, "NoSuchBucket", "The specified bucket does not exist", resource),
        StoreError::NoSuchKey => failure(404, "NoSuchKey", "The specified key does not exist.", resource),
        StoreError::NoSuchUpload => failure(404, "NoSuchUpload", "The specified multipart upload does not exist.", resource),
        StoreError::BucketNotEmpty => failure(409, "BucketNotEmpty", "The bucket you tried to delete is not empty", resource),
        StoreError::InvalidPart(number) => failure(400, "InvalidPart", &format!("Part {number} could not be found or its ETag does not match."), resource),
        StoreError::InvalidPartOrder => failure(400, "InvalidPartOrder", "The list of parts was not in ascending order.", resource),
        StoreError::Storage(message) => failure(500, "InternalError", message, resource),
    }
}

fn valid_bucket(name: &str) -> bool {
    (3..=63).contains(&name.len())
        && name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'-')
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name.as_bytes()[name.len() - 1].is_ascii_alphanumeric()
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 1024 && !key.contains('\0')
}

struct Call<'r> {
    request: &'r Request,
    bucket: String,
    key: String,
    query: Vec<(String, String)>,
    resource: String,
}

impl Call<'_> {
    fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(field, _)| field == name).map(|(_, value)| value.as_str())
    }

    fn has(&self, name: &str) -> bool {
        self.query.iter().any(|(field, _)| field == name)
    }

    fn metadata(&self) -> Vec<(String, String)> {
        self.request.headers.iter().filter_map(|(name, value)| Some((name.strip_prefix("x-amz-meta-")?.to_owned(), value.clone()))).collect()
    }

    fn content_type(&self) -> String {
        self.request.header("content-type").unwrap_or(DEFAULT_CONTENT_TYPE).to_owned()
    }

    fn content_md5_matches(&self) -> bool {
        match self.request.header("content-md5") {
            Some(stated) => eggdb_crypto::base64_decode(stated).is_ok_and(|digest| digest == eggdb_crypto::md5_checksum(&self.request.body)),
            None => true,
        }
    }
}

fn object_headers(info: &ObjectInfo) -> Vec<(String, String)> {
    let mut headers = vec![
        ("ETag".to_owned(), etag(info)),
        ("Last-Modified".to_owned(), http_time(info.modified_ms)),
        ("Accept-Ranges".to_owned(), "bytes".to_owned()),
    ];
    headers.extend(info.metadata.iter().map(|(name, value)| (format!("x-amz-meta-{name}"), value.clone())));
    headers
}

fn parse_range(header: &str, size: u64) -> Option<(u64, u64)> {
    let spec = header.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let (start, end) = if first.is_empty() {
        let suffix: u64 = last.parse().ok()?;
        (size.saturating_sub(suffix), size)
    } else {
        let start: u64 = first.parse().ok()?;
        let end = if last.is_empty() { size } else { last.parse::<u64>().ok()?.saturating_add(1).min(size) };
        (start, end)
    };
    (start < end && start < size).then_some((start, end))
}

impl<'k> Endpoint<'k> {
    /// An endpoint over `keyspace` whose requests must be signed with `access_key` and `secret`.
    pub fn new(keyspace: &'k (dyn Keyspace + Sync), access_key: &str, secret: &str) -> Self {
        Self { store: Store::new(keyspace), access_key: access_key.to_owned(), secret: secret.to_owned(), uploads: AtomicU64::new(1) }
    }

    /// Answers one HTTP request.
    pub fn answer(&self, request: &Request) -> Response {
        let signed = SignedRequest { method: &request.method, path: &request.path, query: request.query.as_deref(), headers: &request.headers, body: &request.body };
        let now = i64::try_from(now_ms() / 1000).unwrap_or(i64::MAX);
        if let Err(error) = sigv4::verify(&signed, &self.access_key, &self.secret, now) {
            let (status, code) = match error {
                SigV4Error::Missing | SigV4Error::Expired => (403, "AccessDenied"),
                SigV4Error::UnknownAccessKey => (403, "InvalidAccessKeyId"),
                SigV4Error::TimeSkewed => (403, "RequestTimeTooSkewed"),
                SigV4Error::PayloadMismatch => (400, "XAmzContentSHA256Mismatch"),
                SigV4Error::UnsupportedPayload => (501, "NotImplemented"),
                SigV4Error::Malformed => (400, "AuthorizationHeaderMalformed"),
                SigV4Error::SignatureMismatch => (403, "SignatureDoesNotMatch"),
            };
            return failure(status, code, &error.to_string(), &request.path);
        }
        let Some(path) = percent_decode(&request.path) else {
            return failure(400, "InvalidURI", "Couldn't parse the specified URI.", &request.path);
        };
        let trimmed = path.trim_start_matches('/');
        let (bucket, key) = trimmed.split_once('/').unwrap_or((trimmed, ""));
        let query = request
            .query
            .as_deref()
            .unwrap_or_default()
            .split('&')
            .filter(|pair| !pair.is_empty())
            .filter_map(|pair| {
                let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                Some((percent_decode(name)?, percent_decode(&value.replace('+', "%20"))?))
            })
            .filter(|(name, _)| !name.starts_with("X-Amz-") && name != "x-id")
            .collect();
        let call = Call { request, bucket: bucket.to_owned(), key: key.to_owned(), query, resource: path.clone() };
        if call.bucket.is_empty() {
            return match request.method.as_str() {
                "GET" => self.list_buckets(),
                _ => failure(405, "MethodNotAllowed", "The specified method is not allowed against this resource.", &call.resource),
            };
        }
        if !valid_bucket(&call.bucket) {
            return failure(400, "InvalidBucketName", "The specified bucket is not valid.", &call.resource);
        }
        if call.key.is_empty() {
            return self.bucket_call(&call);
        }
        if !valid_key(&call.key) {
            return failure(400, "KeyTooLongError", "Your key is too long or holds a NUL character.", &call.resource);
        }
        self.object_call(&call)
    }

    fn list_buckets(&self) -> Response {
        match self.store.buckets() {
            Ok(buckets) => {
                let listed: String = buckets
                    .iter()
                    .map(|(name, created)| format!("<Bucket><Name>{}</Name><CreationDate>{}</CreationDate></Bucket>", escape(name), iso_time(*created)))
                    .collect();
                xml(
                    200,
                    format!("<ListAllMyBucketsResult xmlns=\"{XMLNS}\"><Owner><ID>eggdb</ID><DisplayName>{}</DisplayName></Owner><Buckets>{listed}</Buckets></ListAllMyBucketsResult>", escape(&self.access_key)),
                )
            }
            Err(error) => store_failure(&error, "/"),
        }
    }

    fn bucket_call(&self, call: &Call<'_>) -> Response {
        let outcome = match call.request.method.as_str() {
            "PUT" if call.query.is_empty() => self.store.create_bucket(&call.bucket, now_ms()).map(|_| {
                let mut response = empty(200);
                response.headers.push(("Location".to_owned(), format!("/{}", call.bucket)));
                response
            }),
            "HEAD" => self.store.bucket_exists(&call.bucket).map(|exists| if exists { empty(200) } else { empty(404) }),
            "DELETE" if call.query.is_empty() => self.store.delete_bucket(&call.bucket).map(|()| empty(204)),
            "GET" if call.has("location") => self
                .store
                .bucket_exists(&call.bucket)
                .and_then(|exists| if exists { Ok(xml(200, format!("<LocationConstraint xmlns=\"{XMLNS}\"></LocationConstraint>"))) } else { Err(StoreError::NoSuchBucket) }),
            "GET" if call.query.iter().all(|(name, _)| matches!(name.as_str(), "list-type" | "prefix" | "delimiter" | "max-keys" | "continuation-token" | "start-after" | "marker" | "encoding-type" | "fetch-owner")) => {
                return self.list_objects(call);
            }
            "POST" if call.has("delete") => return self.delete_many(call),
            _ => return failure(501, "NotImplemented", "This bucket operation is not supported yet.", &call.resource),
        };
        outcome.unwrap_or_else(|error| store_failure(&error, &call.resource))
    }

    fn list_objects(&self, call: &Call<'_>) -> Response {
        let version2 = call.param("list-type") == Some("2");
        let prefix = call.param("prefix").unwrap_or_default();
        let delimiter = call.param("delimiter").unwrap_or_default();
        let url_encoded = call.param("encoding-type") == Some("url");
        let max_keys = call.param("max-keys").and_then(|text| text.parse::<usize>().ok()).unwrap_or(MAX_KEYS).min(MAX_KEYS);
        let token = call.param("continuation-token").and_then(|token| eggdb_crypto::base64_decode(token).ok()).and_then(|bytes| String::from_utf8(bytes).ok());
        let after = if version2 { token.clone().unwrap_or_else(|| call.param("start-after").unwrap_or_default().to_owned()) } else { call.param("marker").unwrap_or_default().to_owned() };
        let entries = match self.store.list(&call.bucket, prefix, &after) {
            Ok(entries) => entries,
            Err(error) => return store_failure(&error, &call.resource),
        };
        let show = |text: &str| if url_encoded { uri_encode(text.as_bytes(), true) } else { escape(text) };
        let mut contents = String::new();
        let mut common: Vec<String> = Vec::new();
        let mut count = 0;
        let mut truncated = false;
        let mut last = String::new();
        for (key, info) in &entries {
            if count >= max_keys {
                truncated = true;
                break;
            }
            let grouped = (!delimiter.is_empty()).then(|| key[prefix.len()..].find(delimiter).map(|at| key[..prefix.len() + at + delimiter.len()].to_owned())).flatten();
            match grouped {
                Some(group) => {
                    if common.last() == Some(&group) {
                        continue;
                    }
                    last = format!("{group}{LIST_RESUME_MARK}");
                    common.push(group);
                }
                None => {
                    contents.push_str(&format!(
                        "<Contents><Key>{}</Key><LastModified>{}</LastModified><ETag>{}</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
                        show(key),
                        iso_time(info.modified_ms),
                        escape(&etag(info)),
                        info.size
                    ));
                    last.clone_from(key);
                }
            }
            count += 1;
        }
        let prefixes: String = common.iter().map(|group| format!("<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>", show(group))).collect();
        let mut body = format!(
            "<ListBucketResult xmlns=\"{XMLNS}\"><Name>{}</Name><Prefix>{}</Prefix><MaxKeys>{max_keys}</MaxKeys><IsTruncated>{truncated}</IsTruncated>",
            escape(&call.bucket),
            show(prefix)
        );
        if !delimiter.is_empty() {
            body.push_str(&format!("<Delimiter>{}</Delimiter>", show(delimiter)));
        }
        if url_encoded {
            body.push_str("<EncodingType>url</EncodingType>");
        }
        if version2 {
            body.push_str(&format!("<KeyCount>{count}</KeyCount>"));
            if let Some(token) = call.param("continuation-token") {
                body.push_str(&format!("<ContinuationToken>{}</ContinuationToken>", escape(token)));
            }
            if let Some(start) = call.param("start-after") {
                body.push_str(&format!("<StartAfter>{}</StartAfter>", show(start)));
            }
            if truncated {
                body.push_str(&format!("<NextContinuationToken>{}</NextContinuationToken>", eggdb_crypto::base64_encode(last.as_bytes())));
            }
        } else {
            body.push_str(&format!("<Marker>{}</Marker>", show(&after)));
            if truncated {
                body.push_str(&format!("<NextMarker>{}</NextMarker>", show(&last)));
            }
        }
        body.push_str(&contents);
        body.push_str(&prefixes);
        body.push_str("</ListBucketResult>");
        xml(200, body)
    }

    fn delete_many(&self, call: &Call<'_>) -> Response {
        if !call.content_md5_matches() {
            return failure(400, "BadDigest", "The Content-MD5 you specified did not match what we received.", &call.resource);
        }
        let Ok(document) = std::str::from_utf8(&call.request.body) else {
            return failure(400, "MalformedXML", "The XML you provided was not well-formed.", &call.resource);
        };
        let parsed = elements(document, "Object").and_then(|objects| objects.iter().map(|object| text(object, "Key")).collect::<Result<Vec<_>, _>>());
        let Ok(keys) = parsed else {
            return failure(400, "MalformedXML", "The XML you provided was not well-formed.", &call.resource);
        };
        let quiet = text(document, "Quiet").ok().flatten().is_some_and(|quiet| quiet == "true");
        let mut result = format!("<DeleteResult xmlns=\"{XMLNS}\">");
        for key in keys.into_iter().flatten() {
            match self.store.delete_object(&call.bucket, &key) {
                Ok(()) if quiet => {}
                Ok(()) => result.push_str(&format!("<Deleted><Key>{}</Key></Deleted>", escape(&key))),
                Err(StoreError::NoSuchBucket) => return store_failure(&StoreError::NoSuchBucket, &call.resource),
                Err(error) => result.push_str(&format!("<Error><Key>{}</Key><Code>InternalError</Code><Message>{}</Message></Error>", escape(&key), escape(&format!("{error:?}")))),
            }
        }
        result.push_str("</DeleteResult>");
        xml(200, result)
    }

    fn object_call(&self, call: &Call<'_>) -> Response {
        let method = call.request.method.as_str();
        let upload = call.param("uploadId");
        match (method, upload) {
            ("POST", None) if call.has("uploads") => self.start_upload(call),
            ("PUT", Some(upload)) => self.upload_part(call, upload),
            ("POST", Some(upload)) => self.finish_upload(call, upload),
            ("DELETE", Some(upload)) => self.store.abort(upload).map(|()| empty(204)).unwrap_or_else(|error| store_failure(&error, &call.resource)),
            ("PUT", None) if call.query.is_empty() => match call.request.header("x-amz-copy-source") {
                Some(source) => self.copy_object(call, source),
                None => self.put_object(call),
            },
            ("GET" | "HEAD", None) if call.query.iter().all(|(name, _)| name.starts_with("response-") || name == "partNumber" || name == "versionId") => self.get_object(call, method == "HEAD"),
            ("DELETE", None) if call.query.is_empty() => match self.store.delete_object(&call.bucket, &call.key) {
                Ok(()) | Err(StoreError::NoSuchKey) => empty(204),
                Err(error) => store_failure(&error, &call.resource),
            },
            _ => failure(501, "NotImplemented", "This object operation is not supported yet.", &call.resource),
        }
    }

    fn put_object(&self, call: &Call<'_>) -> Response {
        if !call.content_md5_matches() {
            return failure(400, "BadDigest", "The Content-MD5 you specified did not match what we received.", &call.resource);
        }
        match self.store.put_object(&call.bucket, &call.key, &call.request.body, &call.content_type(), call.metadata(), now_ms()) {
            Ok(info) => {
                let mut response = empty(200);
                response.headers.push(("ETag".to_owned(), etag(&info)));
                response
            }
            Err(error) => store_failure(&error, &call.resource),
        }
    }

    fn copy_object(&self, call: &Call<'_>, source: &str) -> Response {
        let source = source.split_once('?').map_or(source, |(path, _)| path);
        let Some(decoded) = percent_decode(source) else {
            return failure(400, "InvalidArgument", "Copy Source must mention the source bucket and key: sourcebucket/sourcekey.", &call.resource);
        };
        let Some((bucket, key)) = decoded.trim_start_matches('/').split_once('/') else {
            return failure(400, "InvalidArgument", "Copy Source must mention the source bucket and key: sourcebucket/sourcekey.", &call.resource);
        };
        let (info, body) = match self.store.read_object(bucket, key, None) {
            Ok(found) => found,
            Err(error) => return store_failure(&error, &decoded),
        };
        let replace = call.request.header("x-amz-metadata-directive") == Some("REPLACE");
        let (content_type, metadata) = if replace { (call.content_type(), call.metadata()) } else { (info.content_type, info.metadata) };
        match self.store.put_object(&call.bucket, &call.key, &body, &content_type, metadata, now_ms()) {
            Ok(copied) => xml(
                200,
                format!("<CopyObjectResult xmlns=\"{XMLNS}\"><LastModified>{}</LastModified><ETag>{}</ETag></CopyObjectResult>", iso_time(copied.modified_ms), escape(&etag(&copied))),
            ),
            Err(error) => store_failure(&error, &call.resource),
        }
    }

    fn get_object(&self, call: &Call<'_>, head_only: bool) -> Response {
        let info = match self.store.object(&call.bucket, &call.key) {
            Ok(info) => info,
            Err(error) => return if head_only { empty(store_failure(&error, "").status) } else { store_failure(&error, &call.resource) },
        };
        let range = match call.request.header("range") {
            Some(header) => match parse_range(header, info.size) {
                Some(range) => Some(range),
                None => return failure(416, "InvalidRange", "The requested range is not satisfiable", &call.resource),
            },
            None => None,
        };
        let content_type = call.param("response-content-type").map_or_else(|| info.content_type.clone(), str::to_owned);
        let mut headers = object_headers(&info);
        let status = if let Some((start, end)) = range {
            headers.push(("Content-Range".to_owned(), format!("bytes {start}-{}/{}", end - 1, info.size)));
            206
        } else {
            200
        };
        if head_only {
            let length = range.map_or(info.size, |(start, end)| end - start);
            headers.push(("Content-Length".to_owned(), length.to_string()));
            return Response { status, content_type, body: Vec::new(), headers };
        }
        match self.store.read_object(&call.bucket, &call.key, range) {
            Ok((_, body)) => Response { status, content_type, body, headers },
            Err(error) => store_failure(&error, &call.resource),
        }
    }

    fn start_upload(&self, call: &Call<'_>) -> Response {
        let sequence = self.uploads.fetch_add(1, Ordering::Relaxed);
        let seed = format!("{}\0{}\0{}\0{sequence}", call.bucket, call.key, now_ms());
        let upload = hex(&eggdb_crypto::sha256(seed.as_bytes())[..16]);
        match self.store.create_upload(&upload, &call.bucket, &call.key, &call.content_type(), call.metadata(), now_ms()) {
            Ok(()) => xml(
                200,
                format!(
                    "<InitiateMultipartUploadResult xmlns=\"{XMLNS}\"><Bucket>{}</Bucket><Key>{}</Key><UploadId>{upload}</UploadId></InitiateMultipartUploadResult>",
                    escape(&call.bucket),
                    escape(&call.key)
                ),
            ),
            Err(error) => store_failure(&error, &call.resource),
        }
    }

    fn upload_part(&self, call: &Call<'_>, upload: &str) -> Response {
        let Some(number) = call.param("partNumber").and_then(|number| number.parse::<u32>().ok()).filter(|number| (1..=MAX_PART_NUMBER).contains(number)) else {
            return failure(400, "InvalidArgument", "Part number must be an integer between 1 and 10000, inclusive", &call.resource);
        };
        if !call.content_md5_matches() {
            return failure(400, "BadDigest", "The Content-MD5 you specified did not match what we received.", &call.resource);
        }
        match self.store.put_part(upload, &call.bucket, &call.key, number, &call.request.body) {
            Ok(md5) => {
                let mut response = empty(200);
                response.headers.push(("ETag".to_owned(), format!("\"{}\"", hex(&md5))));
                response
            }
            Err(error) => store_failure(&error, &call.resource),
        }
    }

    fn finish_upload(&self, call: &Call<'_>, upload: &str) -> Response {
        let malformed = || failure(400, "MalformedXML", "The XML you provided was not well-formed.", &call.resource);
        let Ok(document) = std::str::from_utf8(&call.request.body) else {
            return malformed();
        };
        let Ok(listed) = elements(document, "Part") else {
            return malformed();
        };
        let mut parts = Vec::with_capacity(listed.len());
        for part in listed {
            let number = text(part, "PartNumber").ok().flatten().and_then(|number| number.trim().parse::<u32>().ok());
            let tag = text(part, "ETag").ok().flatten().map(|tag| tag.trim().trim_matches('"').to_owned());
            let digest = tag.as_deref().and_then(|tag| {
                if tag.len() != 32 {
                    return None;
                }
                let mut digest = [0u8; 16];
                for (at, byte) in digest.iter_mut().enumerate() {
                    *byte = u8::from_str_radix(tag.get(at * 2..at * 2 + 2)?, 16).ok()?;
                }
                Some(digest)
            });
            match (number, digest) {
                (Some(number), Some(digest)) => parts.push((number, digest)),
                (Some(number), None) => return store_failure(&StoreError::InvalidPart(number), &call.resource),
                _ => return malformed(),
            }
        }
        match self.store.complete(upload, &call.bucket, &call.key, &parts, now_ms()) {
            Ok(info) => xml(
                200,
                format!(
                    "<CompleteMultipartUploadResult xmlns=\"{XMLNS}\"><Location>/{}</Location><Bucket>{}</Bucket><Key>{}</Key><ETag>{}</ETag></CompleteMultipartUploadResult>",
                    escape(call.resource.trim_start_matches('/')),
                    escape(&call.bucket),
                    escape(&call.key),
                    escape(&etag(&info))
                ),
            ),
            Err(error) => store_failure(&error, &call.resource),
        }
    }
}
