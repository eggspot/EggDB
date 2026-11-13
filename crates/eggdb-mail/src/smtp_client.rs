//! The SMTP client JMAP submissions are relayed with (RFC 5321): `EHLO`, `MAIL FROM`, one
//! `RCPT TO` per recipient, `DATA` with dot-stuffing, and `QUIT`.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

const REPLY_WAIT: Duration = Duration::from_secs(60);
const MOST_REPLY_LINES: usize = 100;

/// What the relay answered for each recipient: its reply, or why the message was not sent.
pub(crate) type Delivery = Vec<(String, Result<String, String>)>;

struct Session {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Session {
    fn reply(&mut self) -> Result<(u16, String), String> {
        let mut text = String::new();
        for _ in 0..MOST_REPLY_LINES {
            let mut line = String::new();
            if self.reader.read_line(&mut line).map_err(|error| error.to_string())? == 0 {
                return Err("the relay closed the connection".to_owned());
            }
            let line = line.trim_end_matches(['\r', '\n']);
            let code: u16 = line.get(..3).and_then(|code| code.parse().ok()).ok_or_else(|| format!("the relay answered {line:?}"))?;
            text.push_str(line);
            if line.as_bytes().get(3) != Some(&b'-') {
                return Ok((code, text));
            }
            text.push(' ');
        }
        Err("the relay's reply has too many lines".to_owned())
    }

    fn command(&mut self, line: &str) -> Result<(u16, String), String> {
        self.writer.write_all(format!("{line}\r\n").as_bytes()).map_err(|error| error.to_string())?;
        self.reply()
    }
}

fn stuffed(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 16);
    let mut line_start = true;
    for (at, &byte) in message.iter().enumerate() {
        if line_start && byte == b'.' {
            out.push(b'.');
        }
        if byte == b'\n' && (at == 0 || message[at - 1] != b'\r') {
            out.push(b'\r');
        }
        out.push(byte);
        line_start = byte == b'\n';
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

/// Sends `message` from `from` to `recipients` through the SMTP server at `relay`, greeting it
/// as `helo`; returns each recipient's outcome.
pub(crate) fn relay(relay: &str, helo: &str, from: &str, recipients: &[String], message: &[u8]) -> Result<Delivery, String> {
    let stream = TcpStream::connect(relay).map_err(|error| format!("cannot reach the relay {relay}: {error}"))?;
    stream.set_read_timeout(Some(REPLY_WAIT)).map_err(|error| error.to_string())?;
    let writer = stream.try_clone().map_err(|error| error.to_string())?;
    let mut session = Session { reader: BufReader::new(stream), writer };
    let (code, text) = session.reply()?;
    if code != 220 {
        return Err(format!("the relay greeted with {text}"));
    }
    let (code, text) = session.command(&format!("EHLO {helo}"))?;
    if code != 250 {
        let (code, text) = session.command(&format!("HELO {helo}")).map_err(|_| text.clone())?;
        if code != 250 {
            return Err(format!("the relay refused the greeting: {text}"));
        }
    }
    let (code, text) = session.command(&format!("MAIL FROM:<{from}>"))?;
    if code != 250 {
        let _ = session.command("QUIT");
        return Err(format!("the relay refused the sender: {text}"));
    }
    let mut outcomes = Vec::new();
    for recipient in recipients {
        let (code, text) = session.command(&format!("RCPT TO:<{recipient}>"))?;
        outcomes.push((recipient.clone(), if (250..=252).contains(&code) { Ok(text) } else { Err(text) }));
    }
    if outcomes.iter().all(|(_, outcome)| outcome.is_err()) {
        let _ = session.command("QUIT");
        return Ok(outcomes);
    }
    let (code, text) = session.command("DATA")?;
    if code != 354 {
        let _ = session.command("QUIT");
        return Err(format!("the relay refused the message: {text}"));
    }
    session.writer.write_all(&stuffed(message)).map_err(|error| error.to_string())?;
    let (code, text) = session.reply()?;
    let _ = session.command("QUIT");
    for (_, outcome) in &mut outcomes {
        if outcome.is_ok() {
            *outcome = if code == 250 { Ok(text.clone()) } else { Err(text.clone()) };
        }
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::stuffed;

    #[test]
    fn lines_starting_with_a_dot_are_stuffed_and_the_message_ends_with_a_lone_dot() {
        assert_eq!(stuffed(b"Subject: x\n\n.hidden\r\nend"), b"Subject: x\r\n\r\n..hidden\r\nend\r\n.\r\n".to_vec());
    }
}
