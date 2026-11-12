use super::report_delivery::{connect, parse_https_url, read_line, secured};
use std::io::{self, Read, Write};
use std::time::Duration;

const VARIABLE: &str = "EGGDB_MODEL_ENDPOINTS";
const ROLES_MARK: &str = "#roles=";
const MOST_ANSWER_BYTES: usize = 16 << 20;
const MOST_SHOWN_ERROR_BYTES: usize = 300;
const TIMEOUT: Duration = Duration::from_secs(120);
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

struct Entry {
    prefix: String,
    roles: Vec<String>,
}

fn entries(text: &str) -> Vec<Entry> {
    text.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| match entry.split_once(ROLES_MARK) {
            Some((prefix, roles)) => Entry {
                prefix: prefix.trim().to_owned(),
                roles: roles.split('+').map(str::trim).filter(|role| !role.is_empty()).map(str::to_owned).collect(),
            },
            None => Entry { prefix: entry.to_owned(), roles: Vec::new() },
        })
        .collect()
}

fn covers(prefix: &str, endpoint: &str) -> bool {
    endpoint == prefix
        || (endpoint.starts_with(prefix) && (prefix.ends_with('/') || endpoint[prefix.len()..].starts_with(['/', '?'])))
}

fn allowed(entries: &[Entry], endpoint: &str, role: Option<&str>) -> bool {
    entries.iter().any(|entry| {
        covers(&entry.prefix, endpoint) && role.is_none_or(|role| entry.roles.iter().any(|allowed| allowed == role))
    })
}

/// Lets `eggdb_remote_embed` and `eggdb_remote_complete` post to the endpoints the
/// comma-separated URL prefixes of `EGGDB_MODEL_ENDPOINTS` allow (`https://`, or plain `http://`
/// to this machine only), to superusers and to the roles an entry names after `#roles=`; with
/// none set, SQL calls no model.
pub(super) fn allow_from_environment() {
    let Ok(text) = std::env::var(VARIABLE) else {
        return;
    };
    let entries = entries(&text);
    if entries.is_empty() {
        return;
    }
    eggdb_sql::set_model_provider(Box::new(move |request| {
        if !allowed(&entries, request.endpoint, request.role) {
            return Err(match request.role {
                Some(_) => eggdb_sql::ForeignFailure::Denied(request.endpoint.to_owned()),
                None => eggdb_sql::ForeignFailure::Failed(format!("not among the endpoints {VARIABLE} allows")),
            });
        }
        post_json(request.endpoint, request.body, request.api_key).map_err(|error| eggdb_sql::ForeignFailure::Failed(error.to_string()))
    }));
}

trait Channel: Read + Write {}
impl<T: Read + Write> Channel for T {}

struct Target {
    host: String,
    port: u16,
    path: String,
    secure: bool,
}

fn target_of(url: &str) -> io::Result<Target> {
    if let Some(secure) = parse_https_url(url) {
        return Ok(Target { host: secure.host, port: secure.port, path: secure.path, secure: true });
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "an endpoint is an https:// URL, or http:// to this machine"))?;
    let (authority, path) = rest.find('/').map_or((rest, "/"), |at| (&rest[..at], &rest[at..]));
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || authority.starts_with('[') => (host, port.parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the port is not a number"))?),
        _ => (authority, 80),
    };
    if !LOOPBACK_HOSTS.contains(&host) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "plain http:// is only for models on this machine; use https://"));
    }
    Ok(Target { host: host.to_owned(), port, path: path.to_owned(), secure: false })
}

fn read_body(stream: &mut dyn Channel, length: Option<usize>, chunked: bool) -> io::Result<Vec<u8>> {
    let too_large = || io::Error::new(io::ErrorKind::InvalidData, "the answer is larger than 16 MiB");
    let mut body = Vec::new();
    if chunked {
        loop {
            let line = read_line(stream)?;
            let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a chunk size is not hexadecimal"))?;
            if size == 0 {
                while !read_line(stream)?.is_empty() {}
                return Ok(body);
            }
            if body.len() + size > MOST_ANSWER_BYTES {
                return Err(too_large());
            }
            let start = body.len();
            body.resize(start + size, 0);
            stream.read_exact(&mut body[start..])?;
            read_line(stream)?;
        }
    }
    match length {
        Some(length) if length > MOST_ANSWER_BYTES => Err(too_large()),
        Some(length) => {
            body.resize(length, 0);
            stream.read_exact(&mut body)?;
            Ok(body)
        }
        None => {
            stream.take(MOST_ANSWER_BYTES as u64 + 1).read_to_end(&mut body)?;
            if body.len() > MOST_ANSWER_BYTES {
                return Err(too_large());
            }
            Ok(body)
        }
    }
}

fn post_json(endpoint: &str, body: &str, api_key: Option<&str>) -> io::Result<String> {
    if api_key.is_some_and(|key| key.contains(['\r', '\n'])) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "the API key holds a line break"));
    }
    let target = target_of(endpoint)?;
    let tcp = connect(&target.host, target.port, TIMEOUT)?;
    let mut stream: Box<dyn Channel> = if target.secure { Box::new(secured(tcp, &target.host)?) } else { Box::new(tcp) };
    let authorization = api_key.map(|key| format!("Authorization: Bearer {key}\r\n")).unwrap_or_default();
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\n{authorization}Connection: close\r\nUser-Agent: eggdb\r\n\r\n",
        target.path,
        target.host,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()?;
    let status_line = read_line(&mut *stream)?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the answer has no HTTP status line"))?;
    let mut length = None;
    let mut chunked = false;
    loop {
        let line = read_line(&mut *stream)?;
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().ok();
            } else if name.trim().eq_ignore_ascii_case("transfer-encoding") && value.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }
    let answer = String::from_utf8_lossy(&read_body(&mut *stream, length, chunked)?).into_owned();
    if !(200..300).contains(&status) {
        let shown: String = answer.chars().take(MOST_SHOWN_ERROR_BYTES).collect();
        return Err(io::Error::other(format!("the endpoint answered {status}: {shown}")));
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::{allowed, entries, target_of};

    #[test]
    fn endpoints_are_allowed_by_prefix_and_role() {
        let allowed_entries = entries("https://api.models.example/v1#roles=analyst, http://127.0.0.1:11434/");
        assert!(allowed(&allowed_entries, "https://api.models.example/v1/embeddings", None));
        assert!(allowed(&allowed_entries, "https://api.models.example/v1/embeddings", Some("analyst")));
        assert!(!allowed(&allowed_entries, "https://api.models.example/v1/embeddings", Some("intern")));
        assert!(!allowed(&allowed_entries, "https://api.models.example/v10/x", None));
        assert!(!allowed(&allowed_entries, "https://api.models.example.evil/v1/x", None));
        assert!(allowed(&allowed_entries, "http://127.0.0.1:11434/v1/embeddings", None));
        assert!(target_of("http://10.0.0.5/v1/embeddings").is_err());
        assert_eq!(target_of("http://localhost:11434/api/embed").unwrap().port, 11434);
    }
}
