//! A small client of the SQS-compatible AWS JSON 1.0 protocol of the comparison's own, over one
//! kept-alive HTTP/1.1 connection, each request signed with Signature Version 4 through EggDB's
//! own signer: `CreateQueue`, `SendMessage`, `ReceiveMessage` and `DeleteMessage`, against
//! EggDB's `--sqs-listen` and ElasticMQ alike. Batch actions wait for D58's legal check (D216),
//! so both sides are measured one message per request.

use crate::measure::{Measured, timed};
use eggdb_auth::sigv4::{SignedRequest, authorization, format_amz_date};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Client {
    host: String,
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    access_key: String,
    secret: String,
}

fn quoted(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn strings_named(json: &str, name: &str) -> Vec<String> {
    let pattern = format!("\"{name}\":\"");
    let mut found = Vec::new();
    let mut rest = json;
    while let Some(at) = rest.find(&pattern) {
        rest = &rest[at + pattern.len()..];
        let mut value = String::new();
        let mut characters = rest.char_indices();
        let mut end = rest.len();
        while let Some((index, character)) = characters.next() {
            match character {
                '"' => {
                    end = index + 1;
                    break;
                }
                '\\' => match characters.next() {
                    Some((_, '/')) => value.push('/'),
                    Some((_, 'n')) => value.push('\n'),
                    Some((_, escaped)) => value.push(escaped),
                    None => break,
                },
                other => value.push(other),
            }
        }
        found.push(value);
        rest = rest.get(end..).unwrap_or_default();
    }
    found
}

impl Client {
    fn open(address: &str, access_key: &str, secret: &str) -> Result<Self, String> {
        let stream = TcpStream::connect(address).map_err(|error| format!("cannot reach {address}: {error}"))?;
        stream.set_nodelay(true).map_err(|error| error.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(30))).map_err(|error| error.to_string())?;
        let writer = stream.try_clone().map_err(|error| error.to_string())?;
        Ok(Self {
            host: address.to_owned(),
            reader: BufReader::new(stream),
            writer,
            access_key: access_key.to_owned(),
            secret: secret.to_owned(),
        })
    }

    fn call(&mut self, action: &str, body: &str) -> Result<String, String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs() as i64);
        let headers = vec![
            ("host".to_owned(), self.host.clone()),
            ("x-amz-date".to_owned(), format_amz_date(now)),
            ("x-amz-target".to_owned(), format!("AmazonSQS.{action}")),
            ("content-type".to_owned(), "application/x-amz-json-1.0".to_owned()),
        ];
        let signed = SignedRequest { method: "POST", path: "/", query: None, headers: &headers, body: body.as_bytes() };
        let signature = authorization(&signed, &["content-type", "host", "x-amz-date", "x-amz-target"], &self.access_key, &self.secret, "us-east-1", "sqs")
            .map_err(|error| error.to_string())?;
        let mut request = String::from("POST / HTTP/1.1\r\n");
        for (name, value) in &headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str(&format!("authorization: {signature}\r\ncontent-length: {}\r\n\r\n{body}", body.len()));
        self.writer.write_all(request.as_bytes()).map_err(|error| error.to_string())?;
        let (status, answer) = self.response()?;
        if status != 200 {
            return Err(format!("{action} answered {status}: {answer}"));
        }
        Ok(answer)
    }

    fn line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        if self.reader.read_line(&mut line).map_err(|error| error.to_string())? == 0 {
            return Err("the server closed the connection".to_owned());
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }

    fn response(&mut self) -> Result<(u16, String), String> {
        let status_line = self.line()?;
        let status = status_line.split(' ').nth(1).and_then(|code| code.parse().ok()).ok_or_else(|| format!("a malformed status line: {status_line}"))?;
        let mut length = None;
        let mut chunked = false;
        loop {
            let line = self.line()?;
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                let value = value.trim();
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.parse::<usize>().ok();
                } else if name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked") {
                    chunked = true;
                }
            }
        }
        let mut body = Vec::new();
        if chunked {
            loop {
                let size = usize::from_str_radix(self.line()?.split(';').next().unwrap_or_default().trim(), 16).map_err(|_| "a malformed chunk size")?;
                let mut chunk = vec![0u8; size + 2];
                self.reader.read_exact(&mut chunk).map_err(|error| error.to_string())?;
                if size == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..size]);
            }
        } else {
            body = vec![0u8; length.unwrap_or(0)];
            self.reader.read_exact(&mut body).map_err(|error| error.to_string())?;
        }
        Ok((status, String::from_utf8_lossy(&body).into_owned()))
    }
}

/// Creates the queue `queue`, sends it `count` 100-byte messages one request each, then
/// receives them ten at a time and deletes each; returns the sending and the receiving side's
/// measurements.
pub(crate) fn send_and_receive(address: &str, access_key: &str, secret: &str, queue: &str, count: u64) -> Result<(Measured, Measured), String> {
    let mut client = Client::open(address, access_key, secret)?;
    let created = client.call("CreateQueue", &format!("{{\"QueueName\":{}}}", quoted(queue)))?;
    let url = strings_named(&created, "QueueUrl").into_iter().next().ok_or_else(|| format!("CreateQueue answered no QueueUrl: {created}"))?;
    let body = "s".repeat(100);
    let send = format!("{{\"QueueUrl\":{},\"MessageBody\":{}}}", quoted(&url), quoted(&body));
    let sent = timed(|latencies| {
        for _ in 0..count {
            let started = Instant::now();
            client.call("SendMessage", &send)?;
            latencies.push(started.elapsed().as_micros() as u64);
        }
        Ok(count)
    })?;
    let receive = format!("{{\"QueueUrl\":{},\"MaxNumberOfMessages\":10,\"VisibilityTimeout\":60}}", quoted(&url));
    let received = timed(|latencies| {
        let mut received = 0u64;
        let mut empty = 0;
        while received < count && empty < 3 {
            let started = Instant::now();
            let answer = client.call("ReceiveMessage", &receive)?;
            let handles = strings_named(&answer, "ReceiptHandle");
            if handles.is_empty() {
                empty += 1;
                continue;
            }
            empty = 0;
            for handle in handles {
                client.call("DeleteMessage", &format!("{{\"QueueUrl\":{},\"ReceiptHandle\":{}}}", quoted(&url), quoted(&handle)))?;
                received += 1;
            }
            latencies.push(started.elapsed().as_micros() as u64);
        }
        Ok(received)
    })?;
    Ok((sent, received))
}
