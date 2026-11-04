use crate::reader::{RequestReader, RequestSource, Version};
use crate::streamed::{BodyWriter, Reply, StreamedResponse};
use crate::{HttpError, Limits, Request, Response, reason};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const READ_POLL: Duration = Duration::from_millis(200);
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const LINGER_TIMEOUT: Duration = Duration::from_secs(1);
const LINGER_BYTES: usize = 1 << 20;
const COALESCED_BODY_BYTES: usize = 64 * 1024;

fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout)
        .or_else(|| now.checked_add(Duration::from_secs(24 * 60 * 60)))
        .unwrap_or(now)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Head,
    Body,
}

struct ConnectionStream<'s> {
    stream: TcpStream,
    stop: &'s (dyn Fn() -> bool + Sync),
    deadline: Instant,
    phase: Phase,
}

impl ConnectionStream<'_> {
    fn wait_for_request(&mut self, timeout: Duration) {
        self.phase = Phase::Waiting;
        self.deadline = deadline_after(timeout);
    }
}

impl Read for ConnectionStream<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if Instant::now() >= self.deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.stream.read(buffer) {
                Ok(count) => {
                    if self.phase == Phase::Body {
                        self.deadline = deadline_after(BODY_READ_TIMEOUT);
                    }
                    return Ok(count);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    if (self.stop)() {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl RequestSource for ConnectionStream<'_> {
    fn head_started(&mut self) {
        self.phase = Phase::Head;
        self.deadline = deadline_after(HEAD_TIMEOUT);
    }

    fn head_ended(&mut self) {
        self.phase = Phase::Body;
        self.deadline = deadline_after(BODY_READ_TIMEOUT);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionHeader {
    Close,
    Persistent,
    KeepAlive,
}

impl ConnectionHeader {
    fn of(version: Version, keep: bool) -> Self {
        match (keep, version) {
            (false, _) => Self::Close,
            (true, Version::Http11) => Self::Persistent,
            (true, Version::Http10) => Self::KeepAlive,
        }
    }

    fn keeps(self) -> bool {
        self != Self::Close
    }
}

fn bodyless(status: u16) -> bool {
    status == 204 || status == 304 || (100..200).contains(&status)
}

fn server_owned(name: &str) -> bool {
    name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("transfer-encoding")
        || name.eq_ignore_ascii_case("keep-alive")
}

fn status_head(status: u16, content_type: &str, connection: ConnectionHeader) -> String {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\n",
        reason(status),
    );
    match connection {
        ConnectionHeader::Close => head.push_str("Connection: close\r\n"),
        ConnectionHeader::KeepAlive => head.push_str("Connection: keep-alive\r\n"),
        ConnectionHeader::Persistent => {}
    }
    head
}

fn declared_length(response: &Response) -> Option<&str> {
    response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.as_str())
}

fn whole_response_frames_itself(response: &Response, head_only: bool) -> bool {
    head_only
        || bodyless(response.status)
        || declared_length(response)
            .is_none_or(|declared| declared.parse::<usize>().ok() == Some(response.body.len()))
}

pub(crate) fn write_whole(
    out: &mut impl Write,
    response: &Response,
    head_only: bool,
    connection: ConnectionHeader,
) -> io::Result<()> {
    let mut head = status_head(response.status, &response.content_type, connection);
    let no_body = bodyless(response.status);
    if declared_length(response).is_none() && !no_body {
        head.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    }
    for (name, value) in response
        .headers
        .iter()
        .filter(|(name, _)| !server_owned(name))
        .filter(|(name, _)| response.status != 204 || !name.eq_ignore_ascii_case("content-length"))
    {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    let body: &[u8] = if head_only || no_body {
        &[]
    } else {
        &response.body
    };
    if body.len() <= COALESCED_BODY_BYTES {
        bytes.extend_from_slice(body);
        out.write_all(&bytes)?;
    } else {
        out.write_all(&bytes)?;
        out.write_all(body)?;
    }
    out.flush()
}

fn write_streamed(
    out: &mut TcpStream,
    streamed: StreamedResponse<'_>,
    head_only: bool,
    version: Version,
    keep: bool,
) -> bool {
    let chunked = version == Version::Http11;
    let connection = ConnectionHeader::of(version, keep && chunked);
    let framed = !bodyless(streamed.status);
    let mut head = status_head(streamed.status, &streamed.content_type, connection);
    if chunked && framed {
        head.push_str("Transfer-Encoding: chunked\r\n");
    }
    for (name, value) in streamed
        .headers
        .iter()
        .filter(|(name, _)| !server_owned(name) && !name.eq_ignore_ascii_case("content-length"))
    {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    if out.write_all(head.as_bytes()).is_err() {
        return false;
    }
    if head_only || !framed {
        return out.flush().is_ok() && connection.keeps();
    }
    let mut writer = BodyWriter::new(out, chunked);
    let written = (streamed.body)(&mut writer).and_then(|()| writer.finish());
    written.is_ok() && connection.keeps()
}

fn refusal(error: &HttpError, limits: &Limits<'_>) -> Option<Response> {
    Some(match error {
        HttpError::TooLarge => Response::text(431, "the request head is too large"),
        HttpError::BodyTooLarge => Response::text(413, "the request body is too large"),
        HttpError::Malformed => Response::text(400, "not an HTTP/1.1 request"),
        HttpError::UnsupportedTransferCoding => Response::text(
            501,
            "the request's transfer coding is not supported; send it with Content-Length or chunked",
        ),
        HttpError::OverMemoryBudget => (limits.over_memory_budget)(),
        HttpError::Closed => return None,
    })
}

fn linger(source: &mut ConnectionStream<'_>, writer: &TcpStream) {
    if writer.shutdown(Shutdown::Write).is_err() {
        return;
    }
    source.wait_for_request(LINGER_TIMEOUT);
    let mut sink = [0u8; 8192];
    let mut drained = 0;
    while drained < LINGER_BYTES {
        match source.read(&mut sink) {
            Ok(0) | Err(_) => return,
            Ok(count) => drained += count,
        }
    }
}

pub(crate) fn serve_connection<'h>(
    stream: TcpStream,
    handler: &(dyn Fn(&Request) -> Reply<'h> + Sync),
    limits: &Limits<'_>,
    stop: &(dyn Fn() -> bool + Sync),
    active: &AtomicUsize,
) {
    if stream.set_read_timeout(Some(READ_POLL)).is_err()
        || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
    {
        return;
    }
    let _ = stream.set_nodelay(true);
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = RequestReader::new(ConnectionStream {
        stream,
        stop,
        deadline: deadline_after(FIRST_REQUEST_TIMEOUT),
        phase: Phase::Waiting,
    });
    let mut held = limits.memory_budget.empty_reservation();
    let max_requests = limits.max_requests_per_connection.max(1);
    for served in 1..=max_requests {
        reader.source.wait_for_request(if served == 1 {
            FIRST_REQUEST_TIMEOUT
        } else {
            limits.keep_alive_idle_timeout
        });
        let read = {
            let mut send_continue = || {
                let _ = writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            };
            reader.next_request(limits.max_body_bytes, &mut held, &mut send_continue)
        };
        let (request, version) = match read {
            Ok(read) => read,
            Err(error) => {
                if let Some(response) = refusal(&error, limits) {
                    held.resize(0);
                    let _ = write_whole(&mut writer, &response, false, ConnectionHeader::Close);
                    linger(&mut reader.source, &writer);
                }
                return;
            }
        };
        let keep = version.wants_persistence(&request)
            && served < max_requests
            && active.load(Ordering::Acquire) < crate::KEEP_ALIVE_BELOW_CONNECTIONS
            && !stop();
        let head_only = request.method == "HEAD";
        let reply = handler(&request);
        drop(request);
        let kept = match reply {
            Reply::Whole(response) => {
                let response = if held.hold_if_large(response.body.len()) {
                    response
                } else {
                    drop(response);
                    held.resize(0);
                    (limits.over_memory_budget)()
                };
                let keep = keep && whole_response_frames_itself(&response, head_only);
                let connection = ConnectionHeader::of(version, keep);
                write_whole(&mut writer, &response, head_only, connection).is_ok()
                    && connection.keeps()
            }
            Reply::Streamed(streamed) => {
                held.resize(0);
                write_streamed(&mut writer, streamed, head_only, version, keep)
            }
        };
        held.resize(0);
        if !kept {
            linger(&mut reader.source, &writer);
            return;
        }
    }
}
