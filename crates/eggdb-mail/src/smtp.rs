//! An SMTP receiver (RFC 5321) that delivers mail into the store's `INBOX` (D892): `EHLO`/
//! `HELO`, `MAIL FROM`, `RCPT TO`, `DATA` with dot-unstuffing, `RSET`, `NOOP`, `VRFY` and
//! `QUIT`, with the `SIZE`, `8BITMIME` and `PIPELINING` extensions. It never relays: every
//! accepted message is stored here, with a `Received` field and the envelope's recipients in
//! `Delivered-To` fields.

use crate::message::rfc5322_date;
use crate::store::{INBOX, append};
use crate::{ServerConfig, now_seconds};
use eggdb_keyspace::Keyspace;
use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;

const MAX_COMMAND_BYTES: usize = 1000;
const MAX_RECIPIENTS: usize = 100;

#[derive(Default)]
struct Envelope {
    helo: Option<String>,
    sender: Option<String>,
    recipients: Vec<String>,
}

fn path_of(argument: &str, keyword: &str) -> Option<String> {
    let rest = argument.get(..keyword.len()).filter(|start| start.eq_ignore_ascii_case(keyword)).map(|_| &argument[keyword.len()..])?;
    let rest = rest.trim_start();
    let open = rest.find('<')?;
    let close = rest[open..].find('>')? + open;
    let path = &rest[open + 1..close];
    if path.contains(['\r', '\n', ' ']) || path.len() > 256 {
        return None;
    }
    Some(path.to_owned())
}

fn reply(stream: &mut impl Write, text: &str) -> io::Result<()> {
    stream.write_all(text.as_bytes())?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn timed_out(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted)
}

/// One line, kept across read time-outs (which only let `stop` be checked) so a slow client
/// loses nothing; `None` at the end of the stream or on `stop`.
fn read_line(reader: &mut impl BufRead, limit: usize, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        if stop() {
            return Ok(None);
        }
        let room = (limit + 2).saturating_sub(line.len()) as u64;
        match reader.by_ref().take(room).read_until(b'\n', &mut line) {
            Ok(0) => return if line.is_empty() { Ok(None) } else { Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the connection ended mid-line")) },
            Ok(_) if line.ends_with(b"\n") => return Ok(Some(line)),
            Ok(_) if line.len() > limit => return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long")),
            Ok(_) => {}
            Err(error) if timed_out(&error) => {}
            Err(error) => return Err(error),
        }
    }
}

fn read_data(reader: &mut impl BufRead, limit: usize, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<Option<Vec<u8>>> {
    let mut message = Vec::new();
    let mut too_large = false;
    loop {
        let Some(line) = read_line(reader, 64 * 1024, stop)? else {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the connection ended inside DATA"));
        };
        let content = line.strip_suffix(b"\r\n").or_else(|| line.strip_suffix(b"\n")).unwrap_or(&line);
        if content == b"." {
            return Ok(if too_large { None } else { Some(message) });
        }
        let unstuffed = content.strip_prefix(b".").unwrap_or(content);
        if too_large || message.len() + unstuffed.len() + 2 > limit {
            too_large = true;
            message.clear();
            continue;
        }
        message.extend_from_slice(unstuffed);
        message.extend_from_slice(b"\r\n");
    }
}

fn deliver(keyspace: &dyn Keyspace, envelope: &Envelope, peer: &str, message: &[u8]) -> Result<(), String> {
    let now = now_seconds();
    let mut stored = format!(
        "Received: from {} ([{peer}]) by eggdb with ESMTP; {}\r\n",
        envelope.helo.as_deref().unwrap_or("unknown"),
        rfc5322_date(now)
    )
    .into_bytes();
    for recipient in &envelope.recipients {
        stored.extend_from_slice(format!("Delivered-To: {recipient}\r\n").as_bytes());
    }
    if !envelope.sender.as_deref().unwrap_or_default().is_empty() {
        stored.extend_from_slice(format!("Return-Path: <{}>\r\n", envelope.sender.as_deref().unwrap_or_default()).as_bytes());
    }
    stored.extend_from_slice(message);
    keyspace.write(0, &mut |keys| append(keys, INBOX, &[], now, &stored).map(drop).map_err(|error| error.0))
}

/// Serves one SMTP connection until `QUIT`, the end of the stream or `stop`.
pub(crate) fn serve_connection(keyspace: &dyn Keyspace, config: &ServerConfig, stream: TcpStream, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    let peer = stream.peer_addr().map_or_else(|_| "unknown".to_owned(), |address| address.ip().to_string());
    let mut reader = BufReader::new(crate::transport::Transport::Plain(stream));
    reply(reader.get_mut(), "220 eggdb ESMTP ready")?;
    let mut envelope = Envelope::default();
    while !stop() {
        let line = match read_line(&mut reader, MAX_COMMAND_BYTES, stop) {
            Ok(Some(line)) => line,
            Ok(None) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                reply(reader.get_mut(), "500 5.5.2 Line too long")?;
                continue;
            }
            Err(error) => return Err(error),
        };
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        let (verb, argument) = text.split_once(' ').unwrap_or((text, ""));
        match verb.to_ascii_uppercase().as_str() {
            "EHLO" | "HELO" => {
                if argument.trim().is_empty() {
                    reply(reader.get_mut(), "501 5.5.4 Give your host name")?;
                    continue;
                }
                envelope = Envelope {
                    helo: Some(argument.trim().to_owned()),
                    ..Envelope::default()
                };
                if verb.eq_ignore_ascii_case("EHLO") {
                    let starttls = if config.tls.is_some() && !reader.get_ref().secured() { "250-STARTTLS\r\n" } else { "" };
                    let greeting = format!("250-eggdb greets {}\r\n250-SIZE {}\r\n250-8BITMIME\r\n{starttls}250 PIPELINING", argument.trim(), config.max_message_bytes);
                    reply(reader.get_mut(), &greeting)?;
                } else {
                    reply(reader.get_mut(), "250 eggdb")?;
                }
            }
            "MAIL" => {
                if envelope.helo.is_none() {
                    reply(reader.get_mut(), "503 5.5.1 Say EHLO first")?;
                } else if envelope.sender.is_some() {
                    reply(reader.get_mut(), "503 5.5.1 Sender already given")?;
                } else {
                    match path_of(argument, "FROM:") {
                        Some(sender) => {
                            let declared = argument
                                .split_whitespace()
                                .find_map(|parameter| parameter.strip_prefix("SIZE=").or_else(|| parameter.strip_prefix("size=")))
                                .and_then(|size| size.parse::<usize>().ok());
                            if declared.is_some_and(|size| size > config.max_message_bytes) {
                                reply(reader.get_mut(), "552 5.3.4 Message too large")?;
                            } else {
                                envelope.sender = Some(sender);
                                reply(reader.get_mut(), "250 2.1.0 OK")?;
                            }
                        }
                        None => reply(reader.get_mut(), "501 5.5.4 Syntax: MAIL FROM:<address>")?,
                    }
                }
            }
            "RCPT" => {
                if envelope.sender.is_none() {
                    reply(reader.get_mut(), "503 5.5.1 Need MAIL first")?;
                } else if envelope.recipients.len() >= MAX_RECIPIENTS {
                    reply(reader.get_mut(), "452 4.5.3 Too many recipients")?;
                } else {
                    match path_of(argument, "TO:").filter(|path| !path.is_empty()) {
                        Some(recipient) => {
                            envelope.recipients.push(recipient);
                            reply(reader.get_mut(), "250 2.1.5 OK")?;
                        }
                        None => reply(reader.get_mut(), "501 5.5.4 Syntax: RCPT TO:<address>")?,
                    }
                }
            }
            "DATA" => {
                if envelope.recipients.is_empty() {
                    reply(reader.get_mut(), "503 5.5.1 Need RCPT first")?;
                    continue;
                }
                reply(reader.get_mut(), "354 End data with <CR><LF>.<CR><LF>")?;
                match read_data(&mut reader, config.max_message_bytes, stop)? {
                    None => reply(reader.get_mut(), "552 5.3.4 Message too large")?,
                    Some(message) => match deliver(keyspace, &envelope, &peer, &message) {
                        Ok(()) => reply(reader.get_mut(), "250 2.0.0 Stored")?,
                        Err(_) => reply(reader.get_mut(), "451 4.3.0 The message could not be stored")?,
                    },
                }
                envelope = Envelope {
                    helo: envelope.helo.take(),
                    ..Envelope::default()
                };
            }
            "RSET" => {
                envelope = Envelope {
                    helo: envelope.helo.take(),
                    ..Envelope::default()
                };
                reply(reader.get_mut(), "250 2.0.0 OK")?;
            }
            "STARTTLS" => match &config.tls {
                Some(tls) if !reader.get_ref().secured() => {
                    reply(reader.get_mut(), "220 2.0.0 Ready to start TLS")?;
                    crate::transport::start_tls(&mut reader, tls)?;
                    envelope = Envelope::default();
                }
                Some(_) => reply(reader.get_mut(), "503 5.5.1 TLS is already active")?,
                None => reply(reader.get_mut(), "454 4.7.0 TLS not available")?,
            },
            "NOOP" => reply(reader.get_mut(), "250 2.0.0 OK")?,
            "VRFY" => reply(reader.get_mut(), "252 2.5.2 Cannot verify, but will accept")?,
            "QUIT" => {
                reply(reader.get_mut(), "221 2.0.0 Bye")?;
                return Ok(());
            }
            _ => reply(reader.get_mut(), "502 5.5.2 Command not recognized")?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_dot_stuffed_data_are_read() {
        assert_eq!(path_of("FROM:<ann@example.org> SIZE=10", "FROM:").as_deref(), Some("ann@example.org"));
        assert_eq!(path_of("from: <>", "FROM:").as_deref(), Some(""));
        assert_eq!(path_of("TO:bob", "TO:"), None);
        let mut input: &[u8] = b"Subject: hi\r\n\r\n..leading dot\r\n.\r\nNEXT\r\n";
        let never = || false;
        let data = read_data(&mut input, 1000, &never).unwrap().unwrap();
        assert_eq!(data, b"Subject: hi\r\n\r\n.leading dot\r\n");
        let mut big: &[u8] = b"0123456789\r\n0123456789\r\n.\r\n";
        assert_eq!(read_data(&mut big, 15, &never).unwrap(), None);
    }
}
