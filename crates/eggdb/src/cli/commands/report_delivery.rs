use eggdb_tls::verify::{ServerName, TrustStore};
use eggdb_tls::{ClientConfig, ServerVerification, TlsStream};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const TRUST_FILE: &str = "EGGDB_REPORT_CA_FILE";
const SMTP_SERVER: &str = "EGGDB_SMTP_SERVER";
const SMTP_FROM: &str = "EGGDB_SMTP_FROM";
const SMTP_USER: &str = "EGGDB_SMTP_USER";
const SMTP_PASSWORD_FILE: &str = "EGGDB_SMTP_PASSWORD_FILE";
const SYSTEM_TRUST_FILES: [&str; 4] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/cert.pem",
    "/usr/local/etc/openssl/cert.pem",
];
const MOST_REPLY_BYTES: usize = 64 * 1024;
const MOST_TRUST_FILE_BYTES: u64 = 16 << 20;

/// An `https://` report target: host, port (443 unless given) and path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SecureTarget {
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) path: String,
}

pub(super) fn parse_https_url(url: &str) -> Option<SecureTarget> {
    let rest = url.strip_prefix("https://")?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']')?;
        let rest = &authority[end + 1..];
        let port = match rest.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None if rest.is_empty() => 443,
            None => return None,
        };
        (&authority[..=end], port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (authority, 443),
        }
    };
    Some(SecureTarget { host: host.to_owned(), port, path: path.to_owned() })
}

fn trust_store_from(variable: &str) -> Option<TrustStore> {
    let candidates: Vec<String> = match std::env::var(variable) {
        Ok(path) => vec![path],
        Err(_) => SYSTEM_TRUST_FILES.iter().map(|path| (*path).to_owned()).collect(),
    };
    for path in candidates {
        let Ok(file) = std::fs::File::open(path) else {
            continue;
        };
        let mut text = String::new();
        if file.take(MOST_TRUST_FILE_BYTES).read_to_string(&mut text).is_err() {
            continue;
        }
        if let Ok(store) = TrustStore::from_pem(&text) {
            return Some(store);
        }
    }
    None
}

pub(super) fn connect(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let address = (host.trim_start_matches('[').trim_end_matches(']'), port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the host has no address"))?;
    let stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

pub(super) fn secured(stream: TcpStream, host: &str) -> io::Result<TlsStream<TcpStream>> {
    secured_trusting(stream, host, TRUST_FILE)
}

fn secured_trusting(stream: TcpStream, host: &str, variable: &str) -> io::Result<TlsStream<TcpStream>> {
    let trust = trust_store_from(variable).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("no trusted certificates: set {variable}"))
    })?;
    let server_name = ServerName::parse(host.trim_start_matches('[').trim_end_matches(']'))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the host is not a valid server name"))?;
    let config = ClientConfig::new(ServerVerification::Full { trust, server_name }, host);
    TlsStream::connect(stream, &config).map_err(|error| io::Error::other(format!("TLS: {error:?}")))
}

/// Posts `body` to an `https://` target, checking its certificate against EGGDB_REPORT_CA_FILE
/// or the system's trusted roots; returns the status code.
pub(super) fn post_https(target: &SecureTarget, content_type: &str, body: &[u8], timeout: Duration) -> io::Result<u16> {
    post_https_trusting(target, content_type, body, timeout, TRUST_FILE)
}

/// [`post_https`] checking the certificate against the PEM file the environment variable
/// `variable` names, or the system's trusted roots when it is unset.
pub(super) fn post_https_trusting(
    target: &SecureTarget,
    content_type: &str,
    body: &[u8],
    timeout: Duration,
    variable: &str,
) -> io::Result<u16> {
    let mut stream = secured_trusting(connect(&target.host, target.port, timeout)?, &target.host, variable)?;
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nUser-Agent: eggdb\r\n\r\n",
        target.path,
        target.host,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    let line = read_line(&mut stream)?;
    let mut words = line.split_whitespace();
    match (words.next(), words.next().and_then(|code| code.parse::<u16>().ok())) {
        (Some(version), Some(code)) if version.starts_with("HTTP/1.") => Ok(code),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "the response has no HTTP status line")),
    }
}

pub(super) fn read_line(stream: &mut dyn Read) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < MOST_REPLY_BYTES {
        if stream.read(&mut byte)? == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&line).trim_end_matches('\r').to_owned())
}

trait Channel: Read + Write {}
impl<T: Read + Write> Channel for T {}

fn reply(stream: &mut dyn Channel) -> io::Result<(u16, Vec<String>)> {
    let mut lines = Vec::new();
    loop {
        let line = read_line(stream)?;
        let code = line.get(..3).and_then(|code| code.parse::<u16>().ok()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, format!("the mail server answered '{line}'"))
        })?;
        let more = line.as_bytes().get(3) == Some(&b'-');
        lines.push(line.get(4..).unwrap_or_default().to_owned());
        if !more || lines.len() > 100 {
            return Ok((code, lines));
        }
    }
}

fn command(stream: &mut dyn Channel, line: &str, expected: u16) -> io::Result<Vec<String>> {
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\r\n")?;
    stream.flush()?;
    let (code, lines) = reply(stream)?;
    if code / 100 != expected / 100 {
        return Err(io::Error::other(format!("the mail server refused '{}': {code} {}", line.split(':').next().unwrap_or(line), lines.join(" "))));
    }
    Ok(lines)
}

fn header_safe(text: &str) -> String {
    text.chars().filter(|character| !character.is_control()).collect()
}

/// Sends a report by e-mail to `to` through the operator's mail server (EGGDB_SMTP_SERVER,
/// `host:port`), with STARTTLS when the server offers it and AUTH PLAIN over TLS when
/// EGGDB_SMTP_USER and EGGDB_SMTP_PASSWORD_FILE are set.
pub(super) fn send_mail(to: &str, subject: &str, text: &str, timeout: Duration) -> io::Result<()> {
    let server = std::env::var(SMTP_SERVER).map_err(|_| io::Error::new(io::ErrorKind::NotFound, "EGGDB_SMTP_SERVER is not set"))?;
    let (host, port) = server.rsplit_once(':').and_then(|(host, port)| Some((host.to_owned(), port.parse::<u16>().ok()?))).unwrap_or((server.clone(), 25));
    let from = std::env::var(SMTP_FROM).unwrap_or_else(|_| "eggdb@localhost".to_owned());
    if [to, from.as_str()].iter().any(|address| address.contains(['<', '>', '\r', '\n', ' ']) || !address.contains('@')) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "an e-mail address is not valid"));
    }
    let tcp = connect(&host, port, timeout)?;
    let mut stream: Box<dyn Channel> = Box::new(tcp.try_clone()?);
    let (greeting, _) = reply(&mut *stream)?;
    if greeting != 220 {
        return Err(io::Error::other(format!("the mail server greeted with {greeting}")));
    }
    let offered = command(&mut *stream, "EHLO eggdb", 250)?;
    let mut secure = false;
    if offered.iter().any(|line| line.eq_ignore_ascii_case("STARTTLS")) {
        command(&mut *stream, "STARTTLS", 220)?;
        drop(stream);
        stream = Box::new(secured(tcp, &host)?);
        secure = true;
        command(&mut *stream, "EHLO eggdb", 250)?;
    }
    if let (Ok(user), Ok(password_file)) = (std::env::var(SMTP_USER), std::env::var(SMTP_PASSWORD_FILE)) {
        if !secure {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "the mail server offers no STARTTLS, so the password is not sent"));
        }
        let password = std::fs::read_to_string(password_file)?;
        let token = eggdb_crypto::base64_encode(format!("\0{user}\0{}", password.trim_end()).as_bytes());
        command(&mut *stream, &format!("AUTH PLAIN {token}"), 235)?;
    }
    command(&mut *stream, &format!("MAIL FROM:<{from}>"), 250)?;
    command(&mut *stream, &format!("RCPT TO:<{to}>"), 250)?;
    command(&mut *stream, "DATA", 354)?;
    let mut message = format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {}\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n",
        header_safe(subject)
    );
    for line in text.lines() {
        if line.starts_with('.') {
            message.push('.');
        }
        message.push_str(line);
        message.push_str("\r\n");
    }
    message.push_str(".\r\n");
    stream.write_all(message.as_bytes())?;
    stream.flush()?;
    let (code, lines) = reply(&mut *stream)?;
    if code != 250 {
        return Err(io::Error::other(format!("the mail server refused the message: {code} {}", lines.join(" "))));
    }
    let _ = command(&mut *stream, "QUIT", 221);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_targets_parse_with_and_without_ports() {
        assert_eq!(
            parse_https_url("https://hooks.example.com/reports"),
            Some(SecureTarget { host: "hooks.example.com".to_owned(), port: 443, path: "/reports".to_owned() })
        );
        assert_eq!(parse_https_url("https://hooks.example.com:8443").map(|target| target.port), Some(8443));
        assert_eq!(parse_https_url("https://user@hooks.example.com/"), None);
        assert_eq!(parse_https_url("http://hooks.example.com/"), None);
    }
}
