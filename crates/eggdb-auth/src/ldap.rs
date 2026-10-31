//! LDAP simple binds (RFC 4511, section 4.2) that check a user's password against a directory:
//! the server connects, binds as `prefix + user + suffix` with the password, and unbinds.

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
            Self::Refused(code) => write!(f, "the LDAP server refused the bind (result code {code})"),
        }
    }
}

impl std::error::Error for LdapError {}

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
    bind.extend(ber::element(ber::SIMPLE_AUTHENTICATION, password.as_bytes()));
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
    let mut response = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(total) = ber::header_length(&response)
            && response.len() >= total
        {
            response.truncate(total);
            return Ok(response);
        }
        if response.len() > MAX_RESPONSE_BYTES {
            return Err(LdapError::Protocol);
        }
        let count = stream
            .read(&mut chunk)
            .map_err(|error| LdapError::Unreachable(error.to_string()))?;
        if count == 0 {
            return Err(LdapError::Unreachable("the connection closed".to_owned()));
        }
        response.extend_from_slice(&chunk[..count]);
    }
}

/// Binds over `stream` as `name` with `password`: `Ok(true)` when the directory accepts them,
/// `Ok(false)` for invalid credentials.
pub fn simple_bind(
    stream: &mut (impl Read + Write),
    name: &str,
    password: &str,
) -> Result<bool, LdapError> {
    stream
        .write_all(&bind_request(1, name, password))
        .map_err(|error| LdapError::Unreachable(error.to_string()))?;
    let response = read_response(stream)?;
    let outcome = match bind_result(&response, 1)? {
        SUCCESS => Ok(true),
        INVALID_CREDENTIALS => Ok(false),
        other => Err(LdapError::Refused(other)),
    };
    let _ = stream.write_all(&unbind_request(2));
    outcome
}

impl LdapConfig {
    /// Checks `user`'s `password` with a simple bind. An empty password is refused without
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
        let name = format!("{}{}{}", self.prefix, escape_dn_value(user), self.suffix);
        simple_bind(&mut stream, &name, password)
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
        assert_eq!(&directory.sent[expected.len()..], &[0x30, 0x05, 0x02, 0x01, 0x02, 0x42, 0x00]);
        assert_eq!(simple_bind(&mut answering(INVALID_CREDENTIALS), "n", "p"), Ok(false));
        assert_eq!(simple_bind(&mut answering(53), "n", "p"), Err(LdapError::Refused(53)));
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
        for answer in [vec![], vec![0x30], vec![0x30, 0x84, 0xff, 0xff, 0xff, 0xff], vec![0x04, 0x00]] {
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
        };
        assert_eq!(config.authenticate("ada", ""), Ok(false));
    }
}
