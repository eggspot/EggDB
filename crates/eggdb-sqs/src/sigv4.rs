use eggdb_http::Request;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac(key: &[u8], message: &str) -> [u8; 32] {
    eggdb_crypto::hmac_sha256(key, message.as_bytes())
}

fn uri_encode(text: &str, keep_slash: bool) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => char::from(byte).to_string(),
            b'/' if keep_slash => "/".to_owned(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn canonical_query(query: Option<&str>) -> String {
    let mut pairs: Vec<(String, String)> = query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (name.to_owned(), value.to_owned())
        })
        .collect();
    pairs.sort();
    pairs.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join("&")
}

/// Whether `request` carries a valid AWS Signature Version 4 for `access_key` and `secret`.
pub(crate) fn verify(request: &Request, access_key: &str, secret: &str) -> bool {
    let Some(authorization) = request.header("authorization") else {
        return false;
    };
    let Some(rest) = authorization.strip_prefix("AWS4-HMAC-SHA256 ") else {
        return false;
    };
    let mut credential = None;
    let mut signed_headers = None;
    let mut signature = None;
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("Credential=") {
            credential = Some(value);
        } else if let Some(value) = part.strip_prefix("SignedHeaders=") {
            signed_headers = Some(value);
        } else if let Some(value) = part.strip_prefix("Signature=") {
            signature = Some(value);
        }
    }
    let (Some(credential), Some(signed_headers), Some(signature)) = (credential, signed_headers, signature) else {
        return false;
    };
    let mut scope_parts = credential.splitn(2, '/');
    let (Some(key), Some(scope)) = (scope_parts.next(), scope_parts.next()) else {
        return false;
    };
    let pieces: Vec<&str> = scope.split('/').collect();
    let [date, region, service, terminator] = pieces.as_slice() else {
        return false;
    };
    if key != access_key || *terminator != "aws4_request" {
        return false;
    }
    let Some(amz_date) = request.header("x-amz-date") else {
        return false;
    };
    let mut canonical_headers = String::new();
    for name in signed_headers.split(';') {
        let Some(value) = request.header(name) else {
            return false;
        };
        canonical_headers.push_str(&format!("{}:{}\n", name.to_ascii_lowercase(), value.split_whitespace().collect::<Vec<_>>().join(" ")));
    }
    let payload_hash = hex(&eggdb_crypto::sha256(&request.body));
    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        request.method,
        uri_encode(if request.path.is_empty() { "/" } else { &request.path }, true),
        canonical_query(request.query.as_deref())
    );
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex(&eggdb_crypto::sha256(canonical.as_bytes())));
    let signing_key = hmac(&hmac(&hmac(&hmac(format!("AWS4{secret}").as_bytes(), date), region), service), "aws4_request");
    let expected = hex(&hmac(&signing_key, &string_to_sign));
    eggdb_crypto::constant_time_eq(expected.as_bytes(), signature.as_bytes())
}

#[cfg(test)]
pub(crate) fn sign(request: &mut Request, access_key: &str, secret: &str, amz_date: &str, region: &str) {
    request.headers.push(("x-amz-date".to_owned(), amz_date.to_owned()));
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/sqs/aws4_request");
    let signed_headers = "host;x-amz-date";
    let mut canonical_headers = String::new();
    for name in signed_headers.split(';') {
        canonical_headers.push_str(&format!("{name}:{}\n", request.header(name).unwrap_or_default()));
    }
    let canonical = format!("{}\n/\n\n{canonical_headers}\n{signed_headers}\n{}", request.method, hex(&eggdb_crypto::sha256(&request.body)));
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex(&eggdb_crypto::sha256(canonical.as_bytes())));
    let signing_key = hmac(&hmac(&hmac(&hmac(format!("AWS4{secret}").as_bytes(), date), region), "sqs"), "aws4_request");
    let signature = hex(&hmac(&signing_key, &string_to_sign));
    request.headers.push((
        "authorization".to_owned(),
        format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"),
    ));
}
