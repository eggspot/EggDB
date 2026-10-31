use crate::xml::{elements, unescape};
use eggdb_auth::sigv4::{SignedRequest, authorization, format_amz_date, uri_encode};
use eggdb_http::{HttpTarget, host_header, parse_http_url, request};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_LIST_BODY: usize = 16 << 20;

/// One bucket of an S3-compatible service reached over `http://`, every request signed with
/// AWS Signature Version 4.
#[derive(Clone)]
pub struct Bucket {
    target: HttpTarget,
    bucket: String,
    region: String,
    access_key: String,
    secret: String,
    max_object: usize,
}

impl std::fmt::Debug for Bucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bucket").field("endpoint", &host_header(&self.target)).field("bucket", &self.bucket).finish_non_exhaustive()
    }
}

/// Where an `s3://bucket/prefix` URL points, with the settings read from the environment.
#[derive(Debug, Clone)]
pub struct Location {
    /// The bucket.
    pub bucket: Bucket,
    /// The key prefix, without a trailing `/`, empty for the whole bucket.
    pub prefix: String,
}

impl Location {
    /// The full key of `name` under the prefix.
    pub fn key(&self, name: &str) -> String {
        if self.prefix.is_empty() { name.to_owned() } else { format!("{}/{name}", self.prefix) }
    }

    /// Reads `s3://bucket[/prefix]`, with the endpoint from `AWS_ENDPOINT_URL` (an `http://`
    /// URL), the keys from `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, and the region from
    /// `AWS_REGION` (`us-east-1` when unset), as `variable` returns them.
    pub fn parse(url: &str, variable: &dyn Fn(&str) -> Option<String>, max_object: usize) -> Result<Self, String> {
        let rest = url.strip_prefix("s3://").ok_or_else(|| format!("{url} is not an s3:// URL"))?;
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        if bucket.is_empty() || bucket.contains(['?', '#']) || prefix.contains(['?', '#']) {
            return Err(format!("{url}: give s3://bucket or s3://bucket/prefix"));
        }
        let endpoint = variable("AWS_ENDPOINT_URL").ok_or("set AWS_ENDPOINT_URL to the S3-compatible service's http:// address")?;
        let target = parse_http_url(&endpoint)?;
        let access_key = variable("AWS_ACCESS_KEY_ID").ok_or("set AWS_ACCESS_KEY_ID")?;
        let secret = variable("AWS_SECRET_ACCESS_KEY").ok_or("set AWS_SECRET_ACCESS_KEY")?;
        let region = variable("AWS_REGION").unwrap_or_else(|| "us-east-1".to_owned());
        Ok(Self {
            bucket: Bucket { target, bucket: bucket.to_owned(), region, access_key, secret, max_object },
            prefix: prefix.trim_end_matches('/').to_owned(),
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

impl Bucket {
    fn call(&self, method: &str, key: &str, query: &str, body: &[u8], max_body: usize) -> Result<eggdb_http::ClientResponse, String> {
        let path = format!("/{}/{}", uri_encode(self.bucket.as_bytes(), false), uri_encode(key.as_bytes(), true));
        let path = if key.is_empty() { format!("/{}", uri_encode(self.bucket.as_bytes(), false)) } else { path };
        let mut headers = vec![
            ("host".to_owned(), host_header(&self.target)),
            ("x-amz-content-sha256".to_owned(), hex(&eggdb_crypto::sha256(body))),
            ("x-amz-date".to_owned(), format_amz_date(now())),
        ];
        let query_option = (!query.is_empty()).then_some(query);
        let signed = SignedRequest { method, path: &path, query: query_option, headers: &headers, body };
        let value = authorization(&signed, &["host", "x-amz-content-sha256", "x-amz-date"], &self.access_key, &self.secret, &self.region, "s3").map_err(|error| error.to_string())?;
        headers.push(("authorization".to_owned(), value));
        let target = if query.is_empty() { path } else { format!("{path}?{query}") };
        request(&self.target, method, &target, &headers, body, TIMEOUT, max_body).map_err(|error| format!("cannot reach {}: {error}", host_header(&self.target)))
    }

    fn refused(response: &eggdb_http::ClientResponse, what: &str) -> String {
        let text = String::from_utf8_lossy(&response.body);
        let code = elements(&text, "Code").ok().and_then(|codes| codes.first().map(|code| (*code).to_owned())).unwrap_or_default();
        format!("{what} failed with HTTP {} {code}", response.status)
    }

    /// Stores `body` as `key`.
    pub fn put(&self, key: &str, body: &[u8]) -> Result<(), String> {
        let response = self.call("PUT", key, "", body, 1 << 20)?;
        if response.status / 100 == 2 { Ok(()) } else { Err(Self::refused(&response, &format!("storing {key}"))) }
    }

    /// The object `key`, or `None` when there is none.
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let response = self.call("GET", key, "", b"", self.max_object)?;
        match response.status {
            200 => Ok(Some(response.body)),
            404 => Ok(None),
            _ => Err(Self::refused(&response, &format!("reading {key}"))),
        }
    }

    /// Deletes `key`.
    pub fn delete(&self, key: &str) -> Result<(), String> {
        let response = self.call("DELETE", key, "", b"", 1 << 20)?;
        if response.status / 100 == 2 || response.status == 404 { Ok(()) } else { Err(Self::refused(&response, &format!("deleting {key}"))) }
    }

    /// Every key that starts with `prefix`, in order.
    pub fn list(&self, prefix: &str) -> Result<Vec<String>, String> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = format!("list-type=2&prefix={}", uri_encode(prefix.as_bytes(), false));
            if let Some(token) = &token {
                query = format!("continuation-token={}&{query}", uri_encode(token.as_bytes(), false));
            }
            let response = self.call("GET", "", &query, b"", MAX_LIST_BODY)?;
            if response.status != 200 {
                return Err(Self::refused(&response, &format!("listing {prefix}")));
            }
            let text = String::from_utf8_lossy(&response.body).into_owned();
            for contents in elements(&text, "Contents")? {
                if let Some(key) = elements(contents, "Key")?.first() {
                    keys.push(unescape(key)?);
                }
            }
            let truncated = elements(&text, "IsTruncated")?.first().is_some_and(|value| *value == "true");
            token = elements(&text, "NextContinuationToken")?.first().map(|value| unescape(value)).transpose()?;
            if !truncated || token.is_none() {
                return Ok(keys);
            }
        }
    }
}
