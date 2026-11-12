//! LDAP simple binds (RFC 4511, section 4.2) that check a user's password against a directory:
//! the server connects (in clear, through StartTLS or as LDAPS), then either binds as
//! `prefix + user + suffix`, or binds as a search account, finds the user's entry under a base
//! DN by one attribute and binds as that entry, and unbinds.

use crate::ber;
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const SUCCESS: u8 = 0;
const INVALID_CREDENTIALS: u8 = 49;

/// Why a bind could not be made, apart from a wrong password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LdapError {
    /// The directory could not be reached or stopped answering.
    Unreachable(String),
    /// The directory's answer is not an LDAP bind response.
    Protocol,
    /// The directory refused for another reason than the credentials, with its result code.
    Refused(u8),
}

impl fmt::Display for LdapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(why) => write!(f, "the LDAP server cannot be reached: {why}"),
            Self::Protocol => f.write_str("the LDAP server's answer is not a bind response"),
            Self::Refused(code) => {
                write!(f, "the LDAP server refused the bind (result code {code})")
            }
        }
    }
}

impl std::error::Error for LdapError {}

/// How the connection to the directory is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LdapSecurity {
    /// No TLS; allowed only for a directory on the same machine or a trusted network.
    #[default]
    Plain,
    /// LDAP's StartTLS extended operation (RFC 4511, section 4.14), then TLS on the same connection.
    StartTls,
    /// TLS from the first byte (LDAPS, usually port 636).
    Ldaps,
}

/// The search half of search-and-bind: where the user's entry is looked for and as whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LdapSearch {
    /// The base DN searched, with its whole subtree.
    pub base_dn: String,
    /// The attribute that holds the user name, such as `uid` or `sAMAccountName`.
    pub attribute: String,
    /// The DN the server binds as to search; anonymous when `None`.
    pub bind_dn: Option<String>,
    /// That DN's password.
    pub bind_password: Option<String>,
}

/// Where and how to bind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LdapConfig {
    /// The directory's `host:port`.
    pub server: String,
    /// Written before the escaped user name, such as `uid=`.
    pub prefix: String,
    /// Written after it, such as `,ou=people,dc=example,dc=org`.
    pub suffix: String,
    /// How long to wait for the directory.
    pub timeout: Duration,
    /// How the connection is protected.
    pub security: LdapSecurity,
    /// The certificate authorities, in PEM, that the directory's certificate must chain to when
    /// TLS is used.
    pub trusted_pem: Option<String>,
    /// Search-and-bind instead of a DN built from the user name, when set.
    pub search: Option<LdapSearch>,
}

/// The user name escaped as an attribute value of a distinguished name (RFC 4514, section 2.4).
pub fn escape_dn_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let last = value.chars().count().saturating_sub(1);
    for (index, character) in value.chars().enumerate() {
        match character {
            '"' | '+' | ',' | ';' | '<' | '>' | '\\' | '=' => {
                out.push('\\');
                out.push(character);
            }
            '#' | ' ' if index == 0 => {
                out.push('\\');
                out.push(character);
            }
            ' ' if index == last => out.push_str("\\ "),
            '\0' => out.push_str("\\00"),
            other => out.push(other),
        }
    }
    out
}

fn bind_request(message_id: u32, name: &str, password: &str) -> Vec<u8> {
    let mut bind = ber::integer(3);
    bind.extend(ber::element(ber::OCTET_STRING, name.as_bytes()));
    bind.extend(ber::element(
        ber::SIMPLE_AUTHENTICATION,
        password.as_bytes(),
    ));
    let mut message = ber::integer(message_id);
    message.extend(ber::element(ber::BIND_REQUEST, &bind));
    ber::element(ber::SEQUENCE, &message)
}

fn unbind_request(message_id: u32) -> Vec<u8> {
    let mut message = ber::integer(message_id);
    message.extend(ber::element(ber::UNBIND_REQUEST, &[]));
    ber::element(ber::SEQUENCE, &message)
}

fn bind_result(response: &[u8], message_id: u32) -> Result<u8, LdapError> {
    let (envelope, _) = ber::read(response).ok_or(LdapError::Protocol)?;
    if envelope.tag != ber::SEQUENCE {
        return Err(LdapError::Protocol);
    }
    let (id, rest) = ber::read(envelope.content).ok_or(LdapError::Protocol)?;
    if id.tag != ber::INTEGER || ber::integer(message_id)[2..] != *id.content {
        return Err(LdapError::Protocol);
    }
    let (operation, _) = ber::read(rest).ok_or(LdapError::Protocol)?;
    if operation.tag != ber::BIND_RESPONSE {
        return Err(LdapError::Protocol);
    }
    let (code, _) = ber::read(operation.content).ok_or(LdapError::Protocol)?;
    match (code.tag, code.content) {
        (ber::ENUMERATED, [code]) => Ok(*code),
        (ber::ENUMERATED, [0, code]) => Ok(*code),
        _ => Err(LdapError::Refused(u8::MAX)),
    }
}

fn read_response(stream: &mut impl Read) -> Result<Vec<u8>, LdapError> {
    let unreachable = |error: std::io::Error| LdapError::Unreachable(error.to_string());
    let mut response = vec![0u8; 2];
    stream.read_exact(&mut response).map_err(unreachable)?;
    if response[1] & 0x80 != 0 {
        let count = usize::from(response[1] & 0x7f);
        if count == 0 || count > 4 {
            return Err(LdapError::Protocol);
        }
        let mut digits = vec![0u8; count];
        stream.read_exact(&mut digits).map_err(unreachable)?;
        response.extend_from_slice(&digits);
    }
    let total = ber::header_length(&response).ok_or(LdapError::Protocol)?;
    if total > MAX_RESPONSE_BYTES {
        return Err(LdapError::Protocol);
    }
    let header = response.len();
    response.resize(total, 0);
    stream
        .read_exact(&mut response[header..])
        .map_err(unreachable)?;
    Ok(response)
}

fn send(stream: &mut impl Write, bytes: &[u8]) -> Result<(), LdapError> {
    stream
        .write_all(bytes)
        .map_err(|error| LdapError::Unreachable(error.to_string()))
}

fn bind(stream: &mut (impl Read + Write), id: u32, name: &str, password: &str) -> Result<u8, LdapError> {
    send(stream, &bind_request(id, name, password))?;
    let response = read_response(stream)?;
    bind_result(&response, id)
}

/// Binds over `stream` as `name` with `password`: `Ok(true)` when the directory accepts them,
/// `Ok(false)` for invalid credentials.
pub fn simple_bind(
    stream: &mut (impl Read + Write),
    name: &str,
    password: &str,
) -> Result<bool, LdapError> {
    let outcome = match bind(stream, 1, name, password)? {
        SUCCESS => Ok(true),
        INVALID_CREDENTIALS => Ok(false),
        other => Err(LdapError::Refused(other)),
    };
    let _ = stream.write_all(&unbind_request(2));
    outcome
}

fn search_request(message_id: u32, search: &LdapSearch, user: &str) -> Vec<u8> {
    let mut request = ber::element(ber::OCTET_STRING, search.base_dn.as_bytes());
    request.extend(ber::element(ber::ENUMERATED, &[2]));
    request.extend(ber::element(ber::ENUMERATED, &[0]));
    request.extend(ber::integer(2));
    request.extend(ber::integer(0));
    request.extend(ber::element(ber::BOOLEAN, &[0]));
    let mut assertion = ber::element(ber::OCTET_STRING, search.attribute.as_bytes());
    assertion.extend(ber::element(ber::OCTET_STRING, user.as_bytes()));
    request.extend(ber::element(ber::EQUALITY_MATCH, &assertion));
    request.extend(ber::element(
        ber::SEQUENCE,
        &ber::element(ber::OCTET_STRING, b"1.1"),
    ));
    let mut message = ber::integer(message_id);
    message.extend(ber::element(ber::SEARCH_REQUEST, &request));
    ber::element(ber::SEQUENCE, &message)
}

fn operation_of(response: &[u8], message_id: u32) -> Result<(u8, Vec<u8>), LdapError> {
    let (envelope, _) = ber::read(response).ok_or(LdapError::Protocol)?;
    if envelope.tag != ber::SEQUENCE {
        return Err(LdapError::Protocol);
    }
    let (id, rest) = ber::read(envelope.content).ok_or(LdapError::Protocol)?;
    if id.tag != ber::INTEGER || ber::integer(message_id)[2..] != *id.content {
        return Err(LdapError::Protocol);
    }
    let (operation, _) = ber::read(rest).ok_or(LdapError::Protocol)?;
    Ok((operation.tag, operation.content.to_vec()))
}

fn result_code(content: &[u8]) -> Result<u8, LdapError> {
    let (code, _) = ber::read(content).ok_or(LdapError::Protocol)?;
    match (code.tag, code.content) {
        (ber::ENUMERATED, [code]) => Ok(*code),
        (ber::ENUMERATED, [0, code]) => Ok(*code),
        _ => Err(LdapError::Protocol),
    }
}

const MOST_SEARCH_MESSAGES: usize = 16;

fn find_user(
    stream: &mut (impl Read + Write),
    search: &LdapSearch,
    user: &str,
) -> Result<Option<String>, LdapError> {
    let account = search.bind_dn.as_deref().unwrap_or_default();
    let secret = search.bind_password.as_deref().unwrap_or_default();
    match bind(stream, 1, account, secret)? {
        SUCCESS => {}
        other => return Err(LdapError::Refused(other)),
    }
    send(stream, &search_request(2, search, user))?;
    let mut found = Vec::new();
    for _ in 0..MOST_SEARCH_MESSAGES {
        let response = read_response(stream)?;
        let (tag, content) = operation_of(&response, 2)?;
        match tag {
            ber::SEARCH_RESULT_ENTRY => {
                let (name, _) = ber::read(&content).ok_or(LdapError::Protocol)?;
                if name.tag != ber::OCTET_STRING {
                    return Err(LdapError::Protocol);
                }
                found.push(
                    String::from_utf8(name.content.to_vec()).map_err(|_| LdapError::Protocol)?,
                );
            }
            ber::SEARCH_RESULT_REFERENCE => {}
            ber::SEARCH_RESULT_DONE => {
                return match result_code(&content)? {
                    SUCCESS | SIZE_LIMIT_EXCEEDED => Ok(match found.as_slice() {
                        [only] => Some(only.clone()),
                        _ => None,
                    }),
                    other => Err(LdapError::Refused(other)),
                };
            }
            _ => return Err(LdapError::Protocol),
        }
    }
    Err(LdapError::Protocol)
}

const SIZE_LIMIT_EXCEEDED: u8 = 4;
const START_TLS_OID: &[u8] = b"1.3.6.1.4.1.1466.20037";

fn start_tls(stream: &mut (impl Read + Write)) -> Result<(), LdapError> {
    let mut message = ber::integer(1);
    message.extend(ber::element(
        ber::EXTENDED_REQUEST,
        &ber::element(ber::EXTENDED_REQUEST_NAME, START_TLS_OID),
    ));
    send(stream, &ber::element(ber::SEQUENCE, &message))?;
    let response = read_response(stream)?;
    let (tag, content) = operation_of(&response, 1)?;
    if tag != ber::EXTENDED_RESPONSE {
        return Err(LdapError::Protocol);
    }
    match result_code(&content)? {
        SUCCESS => Ok(()),
        other => Err(LdapError::Refused(other)),
    }
}

impl LdapConfig {
    fn check(&self, stream: &mut (impl Read + Write), user: &str, password: &str) -> Result<bool, LdapError> {
        let Some(search) = &self.search else {
            let name = format!("{}{}{}", self.prefix, escape_dn_value(user), self.suffix);
            return simple_bind(stream, &name, password);
        };
        let Some(entry) = find_user(stream, search, user)? else {
            let _ = stream.write_all(&unbind_request(3));
            return Ok(false);
        };
        let outcome = match bind(stream, 3, &entry, password)? {
            SUCCESS => Ok(true),
            INVALID_CREDENTIALS => Ok(false),
            other => Err(LdapError::Refused(other)),
        };
        let _ = stream.write_all(&unbind_request(4));
        outcome
    }

    fn tls_config(&self) -> Result<eggdb_tls::ClientConfig, LdapError> {
        let host = self
            .server
            .rsplit_once(':')
            .map_or(self.server.as_str(), |(host, _)| host)
            .trim_matches(['[', ']']);
        let pem = self.trusted_pem.as_deref().ok_or_else(|| {
            LdapError::Unreachable("TLS to the directory needs its certificate authority (ca=)".to_owned())
        })?;
        let trust = eggdb_tls::verify::TrustStore::from_pem(pem)
            .map_err(|error| LdapError::Unreachable(error.to_string()))?;
        let server_name = eggdb_tls::verify::ServerName::parse(host)
            .map_err(|error| LdapError::Unreachable(error.to_string()))?;
        Ok(eggdb_tls::ClientConfig::new(
            eggdb_tls::ServerVerification::Full { trust, server_name },
            host,
        ))
    }

    /// Checks `user`'s `password` with a bind. An empty password is refused without
    /// asking the directory, because LDAP treats a bind without one as anonymous (RFC 4513,
    /// section 5.1.2).
    pub fn authenticate(&self, user: &str, password: &str) -> Result<bool, LdapError> {
        if password.is_empty() || user.is_empty() {
            return Ok(false);
        }
        let address = self
            .server
            .to_socket_addrs()
            .map_err(|error| LdapError::Unreachable(error.to_string()))?
            .next()
            .ok_or_else(|| LdapError::Unreachable("the name has no address".to_owned()))?;
        let mut stream = TcpStream::connect_timeout(&address, self.timeout)
            .map_err(|error| LdapError::Unreachable(error.to_string()))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .and_then(|()| stream.set_write_timeout(Some(self.timeout)))
            .map_err(|error| LdapError::Unreachable(error.to_string()))?;
        match self.security {
            LdapSecurity::Plain => self.check(&mut stream, user, password),
            LdapSecurity::Ldaps | LdapSecurity::StartTls => {
                let config = self.tls_config()?;
                if self.security == LdapSecurity::StartTls {
                    start_tls(&mut stream)?;
                }
                let mut secured = eggdb_tls::TlsStream::connect(stream, &config)
                    .map_err(|error| LdapError::Unreachable(error.to_string()))?;
                self.check(&mut secured, user, password)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Directory {
        answer: Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }

    impl Read for Directory {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.answer.read(buffer)
        }
    }

    impl Write for Directory {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.sent.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn answering(code: u8) -> Directory {
        let mut result = ber::element(ber::ENUMERATED, &[code]);
        result.extend(ber::element(ber::OCTET_STRING, b""));
        result.extend(ber::element(ber::OCTET_STRING, b""));
        let mut message = ber::integer(1);
        message.extend(ber::element(ber::BIND_RESPONSE, &result));
        Directory {
            answer: Cursor::new(ber::element(ber::SEQUENCE, &message)),
            sent: Vec::new(),
        }
    }

    #[test]
    fn a_bind_request_is_rfc_4511_ber_and_the_answer_decides() {
        let mut directory = answering(SUCCESS);
        assert_eq!(simple_bind(&mut directory, "uid=ada,dc=x", "pw"), Ok(true));
        let expected = [
            0x30, 0x1a, 0x02, 0x01, 0x01, 0x60, 0x15, 0x02, 0x01, 0x03, 0x04, 0x0c, b'u', b'i',
            b'd', b'=', b'a', b'd', b'a', b',', b'd', b'c', b'=', b'x', 0x80, 0x02, b'p', b'w',
        ];
        assert_eq!(&directory.sent[..expected.len()], &expected);
        assert_eq!(
            &directory.sent[expected.len()..],
            &[0x30, 0x05, 0x02, 0x01, 0x02, 0x42, 0x00]
        );
        assert_eq!(
            simple_bind(&mut answering(INVALID_CREDENTIALS), "n", "p"),
            Ok(false)
        );
        assert_eq!(
            simple_bind(&mut answering(53), "n", "p"),
            Err(LdapError::Refused(53))
        );
    }

    #[test]
    fn user_names_cannot_change_the_distinguished_name() {
        assert_eq!(escape_dn_value("ada"), "ada");
        assert_eq!(escape_dn_value("a,dc=evil"), "a\\,dc\\=evil");
        assert_eq!(escape_dn_value(" #x "), "\\ #x\\ ");
        assert_eq!(escape_dn_value("#a\0"), "\\#a\\00");
    }

    #[test]
    fn hostile_answers_are_errors() {
        for answer in [
            vec![],
            vec![0x30],
            vec![0x30, 0x84, 0xff, 0xff, 0xff, 0xff],
            vec![0x04, 0x00],
        ] {
            let mut directory = Directory {
                answer: Cursor::new(answer),
                sent: Vec::new(),
            };
            assert!(simple_bind(&mut directory, "n", "p").is_err());
        }
    }

    #[test]
    fn an_empty_password_is_never_sent() {
        let config = LdapConfig {
            server: "127.0.0.1:1".to_owned(),
            prefix: "uid=".to_owned(),
            suffix: String::new(),
            timeout: Duration::from_millis(10),
            security: LdapSecurity::Plain,
            trusted_pem: None,
            search: None,
        };
        assert_eq!(config.authenticate("ada", ""), Ok(false));
    }

    fn message(id: u32, operation: u8, content: &[u8]) -> Vec<u8> {
        let mut message = ber::integer(id);
        message.extend(ber::element(operation, content));
        ber::element(ber::SEQUENCE, &message)
    }

    fn result(code: u8) -> Vec<u8> {
        let mut result = ber::element(ber::ENUMERATED, &[code]);
        result.extend(ber::element(ber::OCTET_STRING, b""));
        result.extend(ber::element(ber::OCTET_STRING, b""));
        result
    }

    #[test]
    fn search_and_bind_finds_the_entry_then_binds_as_it() {
        let mut answer = message(1, ber::BIND_RESPONSE, &result(SUCCESS));
        let mut entry = ber::element(ber::OCTET_STRING, b"uid=ada,ou=people,dc=x");
        entry.extend(ber::element(ber::SEQUENCE, &[]));
        answer.extend(message(2, ber::SEARCH_RESULT_ENTRY, &entry));
        answer.extend(message(2, ber::SEARCH_RESULT_DONE, &result(SUCCESS)));
        answer.extend(message(3, ber::BIND_RESPONSE, &result(SUCCESS)));
        let mut directory = Directory {
            answer: Cursor::new(answer),
            sent: Vec::new(),
        };
        let config = LdapConfig {
            server: "127.0.0.1:1".to_owned(),
            prefix: String::new(),
            suffix: String::new(),
            timeout: Duration::from_millis(10),
            security: LdapSecurity::Plain,
            trusted_pem: None,
            search: Some(LdapSearch {
                base_dn: "dc=x".to_owned(),
                attribute: "uid".to_owned(),
                bind_dn: Some("cn=reader,dc=x".to_owned()),
                bind_password: Some("secret".to_owned()),
            }),
        };
        assert_eq!(config.check(&mut directory, "ada", "pw"), Ok(true));
        let sent = String::from_utf8_lossy(&directory.sent);
        assert!(sent.contains("cn=reader,dc=x") && sent.contains("uid=ada,ou=people,dc=x"));
        let mut empty = message(1, ber::BIND_RESPONSE, &result(SUCCESS));
        empty.extend(message(2, ber::SEARCH_RESULT_DONE, &result(SUCCESS)));
        let mut nobody = Directory {
            answer: Cursor::new(empty),
            sent: Vec::new(),
        };
        assert_eq!(config.check(&mut nobody, "eve", "pw"), Ok(false));
    }
}

