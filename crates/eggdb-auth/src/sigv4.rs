//! AWS Signature Version 4 (`AWS4-HMAC-SHA256`), as AWS documents it, for the AWS-compatible
//! endpoints: a signature in the `Authorization` header or in a presigned URL's query, checked
//! over the canonical request with the URI encoded once (as S3 does), the request's date kept
//! within 15 minutes of the server's clock, and the payload's SHA-256 checked when the client
//! states it.

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const MAX_SKEW_SECONDS: i64 = 15 * 60;
const MAX_PRESIGNED_SECONDS: i64 = 7 * 24 * 60 * 60;
const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// The parts of an HTTP request a signature covers.
#[derive(Debug, Clone, Copy)]
pub struct SignedRequest<'r> {
    /// The method, such as `PUT`.
    pub method: &'r str,
    /// The path as it was sent, percent-encoded.
    pub path: &'r str,
    /// The query as it was sent, without the `?`.
    pub query: Option<&'r str>,
    /// The header fields, names in lower case.
    pub headers: &'r [(String, String)],
    /// The body.
    pub body: &'r [u8],
}

/// Why a signature was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigV4Error {
    /// The request carries no signature.
    Missing,
    /// The signature's fields are malformed or one it needs is absent.
    Malformed,
    /// The access key is not the server's.
    UnknownAccessKey,
    /// The request's date is more than 15 minutes from the server's clock.
    TimeSkewed,
    /// A presigned URL has expired, or asks to live longer than seven days.
    Expired,
    /// The payload does not match the SHA-256 the request states.
    PayloadMismatch,
    /// The payload is sent in a form this endpoint does not read, such as `aws-chunked`.
    UnsupportedPayload,
    /// The signature is not the one the secret makes.
    SignatureMismatch,
}

impl std::fmt::Display for SigV4Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SigV4Error::Missing => "the request is not signed",
            SigV4Error::Malformed => "the request's signature is malformed",
            SigV4Error::UnknownAccessKey => "the access key is not known",
            SigV4Error::TimeSkewed => "the request's date is more than 15 minutes from the server's clock",
            SigV4Error::Expired => "the presigned URL has expired",
            SigV4Error::PayloadMismatch => "the payload does not match its stated SHA-256",
            SigV4Error::UnsupportedPayload => "streaming (aws-chunked) payloads are not supported",
            SigV4Error::SignatureMismatch => "the request signature does not match",
        })
    }
}

impl std::error::Error for SigV4Error {}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac(key: &[u8], message: &str) -> [u8; 32] {
    eggdb_crypto::hmac_sha256(key, message.as_bytes())
}

fn header<'r>(request: &SignedRequest<'r>, name: &str) -> Option<&'r str> {
    request.headers.iter().find(|(field, _)| field == name).map(|(_, value)| value.as_str())
}

fn percent_decode(text: &str) -> Result<Vec<u8>, SigV4Error> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let pair = text.get(at + 1..at + 3).ok_or(SigV4Error::Malformed)?;
            out.push(u8::from_str_radix(pair, 16).map_err(|_| SigV4Error::Malformed)?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    Ok(out)
}

/// `bytes` percent-encoded as SigV4 asks: unreserved characters kept, `/` kept when `keep_slash`.
pub fn uri_encode(bytes: &[u8], keep_slash: bool) -> String {
    bytes
        .iter()
        .map(|&byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => char::from(byte).to_string(),
            b'/' if keep_slash => "/".to_owned(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn query_pairs(query: Option<&str>) -> Result<Vec<(String, String)>, SigV4Error> {
    query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let name = String::from_utf8(percent_decode(name)?).map_err(|_| SigV4Error::Malformed)?;
            let value = String::from_utf8(percent_decode(value)?).map_err(|_| SigV4Error::Malformed)?;
            Ok((name, value))
        })
        .collect()
}

fn canonical_query(pairs: &[(String, String)], skip: Option<&str>) -> String {
    let mut encoded: Vec<(String, String)> = pairs
        .iter()
        .filter(|(name, _)| Some(name.as_str()) != skip)
        .map(|(name, value)| (uri_encode(name.as_bytes(), false), uri_encode(value.as_bytes(), false)))
        .collect();
    encoded.sort();
    encoded.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join("&")
}

fn canonical_request(request: &SignedRequest<'_>, query: &str, signed_headers: &str, payload_hash: &str) -> Result<String, SigV4Error> {
    let path = if request.path.is_empty() { "/" } else { request.path };
    let mut headers = String::new();
    for name in signed_headers.split(';') {
        let values: Vec<&str> = request.headers.iter().filter(|(field, _)| field == name).map(|(_, value)| value.as_str()).collect();
        if values.is_empty() {
            return Err(SigV4Error::Malformed);
        }
        let joined: Vec<String> = values.iter().map(|value| value.split_whitespace().collect::<Vec<_>>().join(" ")).collect();
        headers.push_str(&format!("{name}:{}\n", joined.join(",")));
    }
    Ok(format!("{}\n{}\n{query}\n{headers}\n{signed_headers}\n{payload_hash}", request.method, uri_encode(&percent_decode(path)?, true)))
}

fn signature(secret: &str, date: &str, region: &str, service: &str, amz_date: &str, scope: &str, canonical: &str) -> String {
    let string_to_sign = format!("{ALGORITHM}\n{amz_date}\n{scope}\n{}", hex(&eggdb_crypto::sha256(canonical.as_bytes())));
    let key = hmac(&hmac(&hmac(&hmac(format!("AWS4{secret}").as_bytes(), date), region), service), "aws4_request");
    hex(&hmac(&key, &string_to_sign))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The Unix time of an ISO 8601 basic timestamp, `YYYYMMDDTHHMMSSZ`.
pub fn parse_amz_date(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 16 || bytes[8] != b'T' || bytes[15] != b'Z' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(4..6)?, number(6..8)?);
    let (hour, minute, second) = (number(9..11)?, number(11..13)?, number(13..15)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

struct Scope<'s> {
    access_key: &'s str,
    date: &'s str,
    region: &'s str,
    service: &'s str,
    text: &'s str,
}

fn scope(credential: &str) -> Result<Scope<'_>, SigV4Error> {
    let (access_key, text) = credential.split_once('/').ok_or(SigV4Error::Malformed)?;
    let parts: Vec<&str> = text.split('/').collect();
    let [date, region, service, "aws4_request"] = parts.as_slice() else {
        return Err(SigV4Error::Malformed);
    };
    Ok(Scope { access_key, date, region, service, text })
}

fn payload_hash(request: &SignedRequest<'_>) -> Result<String, SigV4Error> {
    match header(request, "x-amz-content-sha256") {
        Some(UNSIGNED_PAYLOAD) => Ok(UNSIGNED_PAYLOAD.to_owned()),
        Some(stated) if stated.starts_with("STREAMING-") => Err(SigV4Error::UnsupportedPayload),
        Some(stated) => {
            let actual = hex(&eggdb_crypto::sha256(request.body));
            if !stated.eq_ignore_ascii_case(&actual) {
                return Err(SigV4Error::PayloadMismatch);
            }
            Ok(actual)
        }
        None => Ok(hex(&eggdb_crypto::sha256(request.body))),
    }
}

fn check_signature(expected: &str, given: &str) -> Result<(), SigV4Error> {
    if eggdb_crypto::constant_time_eq(expected.as_bytes(), given.as_bytes()) { Ok(()) } else { Err(SigV4Error::SignatureMismatch) }
}

/// Checks `request`'s signature, in its `Authorization` header or its query, against
/// `access_key` and `secret`, at the Unix time `now`.
pub fn verify(request: &SignedRequest<'_>, access_key: &str, secret: &str, now: i64) -> Result<(), SigV4Error> {
    let pairs = query_pairs(request.query)?;
    if let Some(authorization) = header(request, "authorization") {
        let fields = authorization.strip_prefix(ALGORITHM).and_then(|rest| rest.strip_prefix(' ')).ok_or(SigV4Error::Malformed)?;
        let mut credential = None;
        let mut signed_headers = None;
        let mut given = None;
        for part in fields.split(',') {
            let part = part.trim();
            if let Some(value) = part.strip_prefix("Credential=") {
                credential = Some(value);
            } else if let Some(value) = part.strip_prefix("SignedHeaders=") {
                signed_headers = Some(value);
            } else if let Some(value) = part.strip_prefix("Signature=") {
                given = Some(value);
            }
        }
        let (Some(credential), Some(signed_headers), Some(given)) = (credential, signed_headers, given) else {
            return Err(SigV4Error::Malformed);
        };
        let scope = scope(credential)?;
        if !eggdb_crypto::constant_time_eq(scope.access_key.as_bytes(), access_key.as_bytes()) {
            return Err(SigV4Error::UnknownAccessKey);
        }
        let amz_date = header(request, "x-amz-date").ok_or(SigV4Error::Malformed)?;
        let signed_at = parse_amz_date(amz_date).ok_or(SigV4Error::Malformed)?;
        if !amz_date.starts_with(scope.date) || scope.date.len() != 8 {
            return Err(SigV4Error::Malformed);
        }
        if (now - signed_at).abs() > MAX_SKEW_SECONDS {
            return Err(SigV4Error::TimeSkewed);
        }
        if !signed_headers.split(';').any(|name| name == "host") {
            return Err(SigV4Error::Malformed);
        }
        let canonical = canonical_request(request, &canonical_query(&pairs, None), signed_headers, &payload_hash(request)?)?;
        let expected = signature(secret, scope.date, scope.region, scope.service, amz_date, scope.text, &canonical);
        return check_signature(&expected, given);
    }
    let value = |name: &str| pairs.iter().find(|(field, _)| field == name).map(|(_, value)| value.as_str());
    let Some(algorithm) = value("X-Amz-Algorithm") else {
        return Err(SigV4Error::Missing);
    };
    if algorithm != ALGORITHM {
        return Err(SigV4Error::Malformed);
    }
    let (Some(credential), Some(amz_date), Some(expires), Some(signed_headers), Some(given)) =
        (value("X-Amz-Credential"), value("X-Amz-Date"), value("X-Amz-Expires"), value("X-Amz-SignedHeaders"), value("X-Amz-Signature"))
    else {
        return Err(SigV4Error::Malformed);
    };
    let scope = scope(credential)?;
    if !eggdb_crypto::constant_time_eq(scope.access_key.as_bytes(), access_key.as_bytes()) {
        return Err(SigV4Error::UnknownAccessKey);
    }
    let signed_at = parse_amz_date(amz_date).ok_or(SigV4Error::Malformed)?;
    let expires: i64 = expires.parse().map_err(|_| SigV4Error::Malformed)?;
    if !amz_date.starts_with(scope.date) || scope.date.len() != 8 || !signed_headers.split(';').any(|name| name == "host") {
        return Err(SigV4Error::Malformed);
    }
    if !(1..=MAX_PRESIGNED_SECONDS).contains(&expires) || now > signed_at + expires || now + MAX_SKEW_SECONDS < signed_at {
        return Err(SigV4Error::Expired);
    }
    let canonical = canonical_request(request, &canonical_query(&pairs, Some("X-Amz-Signature")), signed_headers, UNSIGNED_PAYLOAD)?;
    let expected = signature(secret, scope.date, scope.region, scope.service, amz_date, scope.text, &canonical);
    check_signature(&expected, given)
}

/// The `Authorization` header value that signs `request` with the headers `signed_headers`
/// (lower case, `host` and `x-amz-date` among them), for `region` and `service`.
pub fn authorization(request: &SignedRequest<'_>, signed_headers: &[&str], access_key: &str, secret: &str, region: &str, service: &str) -> Result<String, SigV4Error> {
    let amz_date = header(request, "x-amz-date").ok_or(SigV4Error::Malformed)?;
    let date = amz_date.get(..8).ok_or(SigV4Error::Malformed)?;
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let mut names: Vec<&str> = signed_headers.to_vec();
    names.sort_unstable();
    let signed = names.join(";");
    let canonical = canonical_request(request, &canonical_query(&query_pairs(request.query)?, None), &signed, &payload_hash(request)?)?;
    let signature = signature(secret, date, region, service, amz_date, &scope, &canonical);
    Ok(format!("{ALGORITHM} Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={signature}"))
}

/// What a presigned URL allows: one `method` on `path` at `host`, for `region` and `service`,
/// from `amz_date` for `expires_seconds`.
#[derive(Debug, Clone, Copy)]
pub struct Presign<'p> {
    /// The method, such as `GET`.
    pub method: &'p str,
    /// The path, percent-encoded as it will be sent.
    pub path: &'p str,
    /// The `Host` header the request will carry.
    pub host: &'p str,
    /// The region in the signature's scope.
    pub region: &'p str,
    /// The service in the signature's scope, such as `s3`.
    pub service: &'p str,
    /// When the URL starts to be valid, as `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'p str,
    /// How long it stays valid, at most seven days.
    pub expires_seconds: u32,
}

/// The query (without `?`) of a URL presigned with `access_key` and `secret`.
pub fn presigned_query(presign: &Presign<'_>, access_key: &str, secret: &str) -> String {
    let date = presign.amz_date.get(..8).unwrap_or_default();
    let scope = format!("{date}/{}/{}/aws4_request", presign.region, presign.service);
    let pairs = vec![
        ("X-Amz-Algorithm".to_owned(), ALGORITHM.to_owned()),
        ("X-Amz-Credential".to_owned(), format!("{access_key}/{scope}")),
        ("X-Amz-Date".to_owned(), presign.amz_date.to_owned()),
        ("X-Amz-Expires".to_owned(), presign.expires_seconds.to_string()),
        ("X-Amz-SignedHeaders".to_owned(), "host".to_owned()),
    ];
    let query = canonical_query(&pairs, None);
    let headers = [("host".to_owned(), presign.host.to_owned())];
    let request = SignedRequest { method: presign.method, path: presign.path, query: Some(&query), headers: &headers, body: &[] };
    let canonical = canonical_request(&request, &query, "host", UNSIGNED_PAYLOAD).unwrap_or_default();
    let signature = signature(secret, date, presign.region, presign.service, presign.amz_date, &scope, &canonical);
    format!("{query}&X-Amz-Signature={signature}")
}

/// A moment in UTC, split into its calendar fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcTime {
    /// The year, such as 2026.
    pub year: i64,
    /// The month, 1 to 12.
    pub month: i64,
    /// The day of the month, 1 to 31.
    pub day: i64,
    /// The hour, 0 to 23.
    pub hour: i64,
    /// The minute, 0 to 59.
    pub minute: i64,
    /// The second, 0 to 59.
    pub second: i64,
    /// The day of the week, 0 for Sunday to 6 for Saturday.
    pub weekday: i64,
}

/// The calendar fields of the Unix time `unix`, in the proleptic Gregorian calendar.
pub fn utc_time(unix: i64) -> UtcTime {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    UtcTime {
        year: year_of_era + era * 400 + i64::from(month <= 2),
        month,
        day,
        hour: seconds / 3600,
        minute: seconds / 60 % 60,
        second: seconds % 60,
        weekday: (days + 4).rem_euclid(7),
    }
}

/// The Unix time `unix` as an ISO 8601 basic timestamp, `YYYYMMDDTHHMMSSZ`.
pub fn format_amz_date(unix: i64) -> String {
    let time = utc_time(unix);
    format!("{:04}{:02}{:02}T{:02}{:02}{:02}Z", time.year, time.month, time.day, time.hour, time.minute, time.second)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_440_938_160;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect()
    }

    #[test]
    fn the_aws_get_vanilla_example_signs_as_documented() {
        let fields = headers(&[("host", "example.amazonaws.com"), ("x-amz-date", "20150830T123600Z")]);
        let request = SignedRequest { method: "GET", path: "/", query: None, headers: &fields, body: b"" };
        let authorization = authorization(&request, &["host", "x-amz-date"], "AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "us-east-1", "service").unwrap();
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        let mut signed = fields.clone();
        signed.push(("authorization".to_owned(), authorization));
        let request = SignedRequest { headers: &signed, ..request };
        assert_eq!(verify(&request, "AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", NOW), Ok(()));
        assert_eq!(verify(&request, "AKIDEXAMPLE", "other", NOW), Err(SigV4Error::SignatureMismatch));
        assert_eq!(verify(&request, "SOMEONE", "x", NOW), Err(SigV4Error::UnknownAccessKey));
        assert_eq!(verify(&request, "AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", NOW + 3600), Err(SigV4Error::TimeSkewed));
    }

    #[test]
    fn payload_hashes_are_checked_and_presigned_urls_expire() {
        let fields = headers(&[("host", "h"), ("x-amz-date", "20150830T123600Z"), ("x-amz-content-sha256", "00")]);
        let request = SignedRequest { method: "PUT", path: "/b/k", query: None, headers: &fields, body: b"data" };
        assert_eq!(authorization(&request, &["host"], "a", "s", "r", "s3"), Err(SigV4Error::PayloadMismatch));
        let presign = Presign { method: "GET", path: "/bucket/a%20key", host: "h", region: "us-east-1", service: "s3", amz_date: "20150830T123600Z", expires_seconds: 60 };
        let query = presigned_query(&presign, "a", "s");
        let fields = headers(&[("host", "h")]);
        let request = SignedRequest { method: "GET", path: "/bucket/a%20key", query: Some(&query), headers: &fields, body: b"" };
        assert_eq!(verify(&request, "a", "s", NOW), Ok(()));
        assert_eq!(verify(&request, "a", "s", NOW + 61), Err(SigV4Error::Expired));
        let tampered = SignedRequest { path: "/bucket/other", ..request };
        assert_eq!(verify(&tampered, "a", "s", NOW), Err(SigV4Error::SignatureMismatch));
        assert_eq!(verify(&SignedRequest { query: None, ..request }, "a", "s", NOW), Err(SigV4Error::Missing));
        assert_eq!(parse_amz_date("19700101T000000Z"), Some(0));
        assert_eq!(parse_amz_date("20150830T123600Z"), Some(NOW));
        assert_eq!(parse_amz_date("2015-08-30"), None);
        assert_eq!(format_amz_date(NOW), "20150830T123600Z");
        assert_eq!(format_amz_date(951_782_400), "20000229T000000Z");
        assert_eq!(utc_time(0).weekday, 4);
        assert_eq!(utc_time(NOW).weekday, 0);
    }
}
