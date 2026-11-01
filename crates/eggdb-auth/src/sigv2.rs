//! S3's query-string authentication with Signature Version 2 (`AWSAccessKeyId`, `Expires` and
//! `Signature` in the URL), as Amazon S3's documentation describes it and as SDKs such as boto3
//! still presign URLs by default: HMAC-SHA1 over the method, `Content-MD5`, `Content-Type`, the
//! expiry, the `x-amz-` headers and the resource with its sub-resources.

use crate::sigv4::{MAX_PRESIGNED_SECONDS, SigV4Error, SignedRequest, query_pairs};

const SUBRESOURCES: [&str; 26] = [
    "acl",
    "cors",
    "delete",
    "lifecycle",
    "location",
    "logging",
    "notification",
    "partNumber",
    "policy",
    "requestPayment",
    "response-cache-control",
    "response-content-disposition",
    "response-content-encoding",
    "response-content-language",
    "response-content-type",
    "response-expires",
    "restore",
    "tagging",
    "torrent",
    "uploadId",
    "uploads",
    "versionId",
    "versioning",
    "versions",
    "website",
    "select",
];

/// Whether `request` carries a Signature Version 2 query signature for [`verify_query`].
pub fn is_query_signed(request: &SignedRequest<'_>) -> bool {
    query_pairs(request.query).is_ok_and(|pairs| {
        ["AWSAccessKeyId", "Signature", "Expires"]
            .iter()
            .all(|name| pairs.iter().any(|(field, _)| field == name))
    })
}

/// Checks a Signature Version 2 query signature against `access_key` and `secret` at `now`
/// (seconds since 1970): the key must match, the URL must not have expired nor expire more
/// than seven days from now, and the signature must match exactly.
pub fn verify_query(
    request: &SignedRequest<'_>,
    access_key: &str,
    secret: &str,
    now: i64,
) -> Result<(), SigV4Error> {
    let pairs = query_pairs(request.query)?;
    let value = |name: &str| {
        pairs
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    };
    let (Some(key), Some(given), Some(expires)) = (
        value("AWSAccessKeyId"),
        value("Signature"),
        value("Expires"),
    ) else {
        return Err(SigV4Error::Missing);
    };
    if !eggdb_crypto::constant_time_eq(key.as_bytes(), access_key.as_bytes()) {
        return Err(SigV4Error::UnknownAccessKey);
    }
    let expires: i64 = expires.parse().map_err(|_| SigV4Error::Malformed)?;
    if now > expires || expires - now > MAX_PRESIGNED_SECONDS {
        return Err(SigV4Error::Expired);
    }
    let string_to_sign = string_to_sign(request, &pairs, expires);
    let expected = eggdb_crypto::base64_encode(&eggdb_crypto::hmac::<eggdb_crypto::Sha1>(
        secret.as_bytes(),
        string_to_sign.as_bytes(),
    ));
    if eggdb_crypto::constant_time_eq(expected.as_bytes(), given.as_bytes()) {
        Ok(())
    } else {
        Err(SigV4Error::SignatureMismatch)
    }
}

fn header<'r>(request: &'r SignedRequest<'_>, name: &str) -> &'r str {
    request
        .headers
        .iter()
        .find(|(field, _)| field == name)
        .map_or("", |(_, value)| value.as_str())
}

fn string_to_sign(request: &SignedRequest<'_>, pairs: &[(String, String)], expires: i64) -> String {
    let mut amz_headers: Vec<(&str, &str)> = request
        .headers
        .iter()
        .filter(|(name, _)| name.starts_with("x-amz-"))
        .map(|(name, value)| (name.as_str(), value.trim()))
        .collect();
    amz_headers.sort();
    let mut text = format!(
        "{}\n{}\n{}\n{expires}\n",
        request.method,
        header(request, "content-md5"),
        header(request, "content-type")
    );
    for (name, value) in amz_headers {
        text.push_str(name);
        text.push(':');
        text.push_str(value);
        text.push('\n');
    }
    text.push_str(request.path);
    let mut subresources: Vec<&(String, String)> = pairs
        .iter()
        .filter(|(name, _)| SUBRESOURCES.contains(&name.as_str()))
        .collect();
    subresources.sort();
    for (index, (name, value)) in subresources.into_iter().enumerate() {
        text.push(if index == 0 { '?' } else { '&' });
        text.push_str(name);
        if !value.is_empty() {
            text.push('=');
            text.push_str(value);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'r>(query: &'r str, headers: &'r [(String, String)]) -> SignedRequest<'r> {
        SignedRequest {
            method: "GET",
            path: "/johnsmith/photos/puppy.jpg",
            query: Some(query),
            headers,
            body: &[],
        }
    }

    #[test]
    fn the_documented_query_string_example_verifies() {
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let query = "AWSAccessKeyId=AKIAIOSFODNN7EXAMPLE&Expires=1175139620&Signature=NpgCjnDzrM%2BWFzoENXmpNDUsSn8%3D";
        let headers = Vec::new();
        let signed = request(query, &headers);
        assert!(is_query_signed(&signed));
        assert_eq!(
            verify_query(&signed, "AKIAIOSFODNN7EXAMPLE", secret, 1_175_139_000),
            Ok(())
        );
        assert_eq!(
            verify_query(&signed, "AKIAIOSFODNN7EXAMPLE", secret, 1_175_139_621),
            Err(SigV4Error::Expired)
        );
        assert_eq!(
            verify_query(&signed, "AKIAIOSFODNN7EXAMPLE", "wrong", 1_175_139_000),
            Err(SigV4Error::SignatureMismatch)
        );
        assert_eq!(
            verify_query(&signed, "someone else", secret, 1_175_139_000),
            Err(SigV4Error::UnknownAccessKey)
        );
        let far = "AWSAccessKeyId=AKIAIOSFODNN7EXAMPLE&Expires=1185139620&Signature=x";
        assert_eq!(
            verify_query(
                &request(far, &headers),
                "AKIAIOSFODNN7EXAMPLE",
                secret,
                1_175_139_000
            ),
            Err(SigV4Error::Expired)
        );
    }
}
