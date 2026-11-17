//! Where a database key comes from (`--key-source`, D1196): a file, an environment variable, a
//! program's output (an HSM's or any vendor's tool), HashiCorp Vault's key-value store or its
//! Transit engine, or AWS KMS. Every source gives the same 32 bytes `--key-file` gives.

use eggdb_auth::sigv4::{SignedRequest, authorization, format_amz_date};
use eggdb_http::{host_header, parse_http_url, request};
use std::time::Duration;

const KEY_BYTES: usize = 32;
const TIMEOUT: Duration = Duration::from_secs(15);
const MOST_RESPONSE_BYTES: usize = 64 * 1024;
const MOST_OUTPUT_BYTES: usize = 4096;
const VAULT_TOKEN: &str = "VAULT_TOKEN";
const VAULT_NAMESPACE: &str = "VAULT_NAMESPACE";
const KMS_ENDPOINT: &str = "AWS_ENDPOINT_URL_KMS";
const KMS_TARGET: &str = "TrentService.Decrypt";
const KMS_CONTENT_TYPE: &str = "application/x-amz-json-1.1";

/// The key `spec` names: `file:<path>`, `env:<NAME>`, `command:<command line>`,
/// `vault:<URL of a KV secret>#<field>`, `vault-transit:<URL of a decrypt endpoint>@<file
/// holding the ciphertext>` or `aws-kms:<region>@<file holding the base64 ciphertext blob>`.
pub(crate) fn read_key(spec: &str) -> Result<[u8; KEY_BYTES], String> {
    let (kind, rest) = spec.split_once(':').ok_or_else(|| {
        format!("--key-source '{spec}' needs a kind: file:, env:, command:, vault:, vault-transit: or aws-kms:")
    })?;
    match kind {
        "file" => super::args::read_key_file(std::ffi::OsStr::new(rest)).map(|key| key.0),
        "env" => {
            let text = std::env::var(rest)
                .map_err(|_| format!("--key-source env:{rest}: the variable is not set"))?;
            key_from_text(&text).ok_or_else(|| shape_error(spec))
        }
        "command" => from_command(rest).and_then(|text| key_from_text(&text).ok_or_else(|| shape_error(spec))),
        "vault" => from_vault(rest),
        "vault-transit" => from_vault_transit(rest),
        "aws-kms" => from_kms(rest),
        _ => Err(format!(
            "--key-source kind '{kind}' is not known: use file:, env:, command:, vault:, vault-transit: or aws-kms:"
        )),
    }
}

fn shape_error(spec: &str) -> String {
    let kind = spec.split_once(':').map_or(spec, |(kind, _)| kind);
    format!("--key-source {kind}: must give 64 hexadecimal digits or the base64 of 32 bytes")
}

/// 32 bytes from `text`: 64 hexadecimal digits, or the standard base64 of 32 bytes.
pub(crate) fn key_from_text(text: &str) -> Option<[u8; KEY_BYTES]> {
    let text = text.trim();
    let mut key = [0u8; KEY_BYTES];
    if text.len() == 2 * KEY_BYTES && text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        for (index, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(text.get(2 * index..2 * index + 2)?, 16).ok()?;
        }
        return Some(key);
    }
    let decoded = eggdb_crypto::base64_decode(text).ok()?;
    if decoded.len() != KEY_BYTES {
        return None;
    }
    key.copy_from_slice(&decoded);
    Some(key)
}

fn from_command(line: &str) -> Result<String, String> {
    let output = if cfg!(windows) {
        std::process::Command::new("cmd").args(["/C", line]).output()
    } else {
        std::process::Command::new("sh").args(["-c", line]).output()
    }
    .map_err(|error| format!("--key-source command: cannot run it: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "--key-source command: it ended with {}",
            output.status
        ));
    }
    if output.stdout.len() > MOST_OUTPUT_BYTES {
        return Err("--key-source command: it printed more than a key".to_owned());
    }
    String::from_utf8(output.stdout).map_err(|_| "--key-source command: it printed no text".to_owned())
}

/// The string value of the first `"name": "..."` member in a JSON text, unescaped; enough for
/// the flat answers Vault and KMS give.
pub(crate) fn json_string(text: &str, name: &str) -> Option<String> {
    let quoted = format!("\"{name}\"");
    let mut from = 0;
    while let Some(found) = text.get(from..)?.find(&quoted) {
        let after = from + found + quoted.len();
        let rest = text.get(after..)?.trim_start();
        if let Some(value) = rest.strip_prefix(':') {
            let value = value.trim_start().strip_prefix('"')?;
            let mut out = String::new();
            let mut characters = value.chars();
            while let Some(character) = characters.next() {
                match character {
                    '"' => return Some(out),
                    '\\' => match characters.next()? {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        '/' => out.push('/'),
                        '\\' => out.push('\\'),
                        '"' => out.push('"'),
                        _ => return None,
                    },
                    character => out.push(character),
                }
            }
            return None;
        }
        from = after;
    }
    None
}

fn vault_headers() -> Result<Vec<(String, String)>, String> {
    let token = std::env::var(VAULT_TOKEN)
        .map_err(|_| format!("--key-source vault: set {VAULT_TOKEN} to a token that may read the key"))?;
    let mut headers = vec![("X-Vault-Token".to_owned(), token)];
    if let Ok(namespace) = std::env::var(VAULT_NAMESPACE) {
        headers.push(("X-Vault-Namespace".to_owned(), namespace));
    }
    Ok(headers)
}

fn from_vault(rest: &str) -> Result<[u8; KEY_BYTES], String> {
    let (url, field) = rest
        .rsplit_once('#')
        .ok_or("--key-source vault: give the secret's URL and its field, as vault:https://vault:8200/v1/secret/data/eggdb#key")?;
    let target = parse_http_url(url).map_err(|error| format!("--key-source vault: {error}"))?;
    let response = request(
        &target,
        "GET",
        &target.path,
        &vault_headers()?,
        &[],
        TIMEOUT,
        MOST_RESPONSE_BYTES,
    )
    .map_err(|error| format!("--key-source vault: cannot reach {}: {error}", host_header(&target)))?;
    if response.status != 200 {
        return Err(format!("--key-source vault: the server answered HTTP {}", response.status));
    }
    let text = String::from_utf8_lossy(&response.body);
    let value = json_string(&text, field)
        .ok_or_else(|| format!("--key-source vault: the secret has no text field '{field}'"))?;
    key_from_text(&value).ok_or_else(|| shape_error("vault:"))
}

fn ciphertext_file(path: &str, kind: &str) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|text| text.trim().to_owned())
        .map_err(|error| format!("--key-source {kind}: cannot read {path}: {error}"))
}

fn from_vault_transit(rest: &str) -> Result<[u8; KEY_BYTES], String> {
    let (url, path) = rest.rsplit_once('@').ok_or(
        "--key-source vault-transit: give the decrypt URL and the file holding the ciphertext, as vault-transit:https://vault:8200/v1/transit/decrypt/eggdb@/etc/eggdb/key.wrapped",
    )?;
    let ciphertext = ciphertext_file(path, "vault-transit")?;
    if ciphertext.contains(['"', '\\']) {
        return Err("--key-source vault-transit: the ciphertext file holds quotes".to_owned());
    }
    let target = parse_http_url(url).map_err(|error| format!("--key-source vault-transit: {error}"))?;
    let mut headers = vault_headers()?;
    headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
    let body = format!("{{\"ciphertext\":\"{ciphertext}\"}}");
    let response = request(
        &target,
        "POST",
        &target.path,
        &headers,
        body.as_bytes(),
        TIMEOUT,
        MOST_RESPONSE_BYTES,
    )
    .map_err(|error| format!("--key-source vault-transit: cannot reach {}: {error}", host_header(&target)))?;
    if response.status != 200 {
        return Err(format!("--key-source vault-transit: the server answered HTTP {}", response.status));
    }
    let text = String::from_utf8_lossy(&response.body);
    let plaintext = json_string(&text, "plaintext")
        .ok_or("--key-source vault-transit: the answer has no plaintext")?;
    key_from_text(&plaintext).ok_or_else(|| shape_error("vault-transit:"))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_kms(rest: &str) -> Result<[u8; KEY_BYTES], String> {
    let (region, path) = rest.split_once('@').ok_or(
        "--key-source aws-kms: give the region and the file holding the base64 ciphertext blob, as aws-kms:eu-west-1@/etc/eggdb/key.blob",
    )?;
    if region.is_empty() || !region.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') {
        return Err(format!("--key-source aws-kms: '{region}' is not a region"));
    }
    let blob = ciphertext_file(path, "aws-kms")?;
    if eggdb_crypto::base64_decode(&blob).is_err() {
        return Err(format!("--key-source aws-kms: {path} does not hold base64"));
    }
    let access_key = std::env::var("AWS_ACCESS_KEY_ID")
        .map_err(|_| "--key-source aws-kms: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY".to_owned())?;
    let secret = std::env::var("AWS_SECRET_ACCESS_KEY")
        .map_err(|_| "--key-source aws-kms: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY".to_owned())?;
    let url = std::env::var(KMS_ENDPOINT).unwrap_or_else(|_| format!("https://kms.{region}.amazonaws.com/"));
    let target = parse_http_url(&url).map_err(|error| format!("--key-source aws-kms: {error}"))?;
    let body = format!("{{\"CiphertextBlob\":\"{blob}\"}}");
    let mut headers = vec![
        ("host".to_owned(), host_header(&target)),
        ("content-type".to_owned(), KMS_CONTENT_TYPE.to_owned()),
        ("x-amz-date".to_owned(), format_amz_date(now())),
        ("x-amz-target".to_owned(), KMS_TARGET.to_owned()),
        (
            "x-amz-content-sha256".to_owned(),
            hex(&eggdb_crypto::sha256(body.as_bytes())),
        ),
    ];
    let mut signed_names = vec!["host", "content-type", "x-amz-date", "x-amz-target", "x-amz-content-sha256"];
    let token = std::env::var("AWS_SESSION_TOKEN").ok();
    if let Some(token) = &token {
        headers.push(("x-amz-security-token".to_owned(), token.clone()));
        signed_names.push("x-amz-security-token");
    }
    let signed = SignedRequest {
        method: "POST",
        path: &target.path,
        query: None,
        headers: &headers,
        body: body.as_bytes(),
    };
    let value = authorization(&signed, &signed_names, &access_key, &secret, region, "kms")
        .map_err(|error| format!("--key-source aws-kms: cannot sign the request: {error}"))?;
    headers.push(("authorization".to_owned(), value));
    let response = request(
        &target,
        "POST",
        &target.path,
        &headers,
        body.as_bytes(),
        TIMEOUT,
        MOST_RESPONSE_BYTES,
    )
    .map_err(|error| format!("--key-source aws-kms: cannot reach {}: {error}", host_header(&target)))?;
    if response.status != 200 {
        let text = String::from_utf8_lossy(&response.body);
        let kind = json_string(&text, "__type").unwrap_or_default();
        return Err(format!("--key-source aws-kms: KMS answered HTTP {} {kind}", response.status));
    }
    let text = String::from_utf8_lossy(&response.body);
    let plaintext = json_string(&text, "Plaintext").ok_or("--key-source aws-kms: the answer has no Plaintext")?;
    key_from_text(&plaintext).ok_or_else(|| shape_error("aws-kms:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_read_from_hex_or_base64_and_nothing_else() {
        let hex_key = "00".repeat(31) + "ff";
        assert_eq!(key_from_text(&hex_key).map(|key| key[31]), Some(0xff));
        let base64_key = eggdb_crypto::base64_encode(&[7u8; 32]);
        assert_eq!(key_from_text(&base64_key), Some([7u8; 32]));
        assert_eq!(key_from_text("abcd"), None);
        assert_eq!(key_from_text(&eggdb_crypto::base64_encode(&[1u8; 16])), None);
    }

    #[test]
    fn json_strings_are_found_by_name_and_unescaped() {
        let answer = r#"{"data": {"data": {"key": "a\/b\"c"}, "metadata": {}}}"#;
        assert_eq!(json_string(answer, "key").as_deref(), Some("a/b\"c"));
        assert_eq!(json_string(r#"{"Plaintext":"AAAA","KeyId":"k"}"#, "Plaintext").as_deref(), Some("AAAA"));
        assert_eq!(json_string(r#"{"other":"x"}"#, "key"), None);
    }

    #[test]
    fn an_environment_variable_and_a_program_give_a_key() {
        assert!(read_key("nowhere:x").is_err());
        assert!(read_key("env:EGGDB_TEST_KEY_SOURCE_UNSET").is_err());
        if cfg!(unix) {
            let key = read_key(&format!("command:printf {}", "ab".repeat(32))).unwrap();
            assert_eq!(key, [0xab; 32]);
        }
    }
}
