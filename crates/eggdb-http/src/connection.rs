use crate::content_coding::decode_request_body;
use crate::reader::{RequestReader, RequestSource, Version};
use crate::streamed::{BodyWriter, Reply, StreamedResponse};
use crate::{HttpError, Limits, Request, Response, reason};
use std::io::{self, Read, Write};
use crate::socket::Socket;
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const READ_POLL: Duration = Duration::from_millis(200);
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(5);
const SLOWEST_BODY_BYTES_PER_SECOND: u64 = 64 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const SLOWEST_RESPONSE_BYTES_PER_SECOND: u64 = 64 * 1024;
const LINGER_TIMEOUT: Duration = Duration::from_secs(1);
const LINGER_BYTES: usize = 1 << 20;
const COALESCED_BODY_BYTES: usize = 64 * 1024;

fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout)
        .or_else(|| now.checked_add(Duration::from_secs(24 * 60 * 60)))
        .unwrap_or(now)
}

struct PacedWriter<W: Write> {
    out: W,
    grace: Duration,
    slowest_bytes_per_second: u64,
    blocked: Duration,
    sent: u64,
}

impl<W: Write> PacedWriter<W> {
    fn new(out: W) -> Self {
        Self::with_pace(out, WRITE_TIMEOUT, SLOWEST_RESPONSE_BYTES_PER_SECOND)
    }

    fn with_pace(out: W, grace: Duration, slowest_bytes_per_second: u64) -> Self {
        Self {
            out,
            grace,
            slowest_bytes_per_second,
            blocked: Duration::ZERO,
            sent: 0,
        }
    }

    fn allowed_blocking(&self) -> Duration {
        let paced_millis = self
            .sent
            .saturating_mul(1000)
            .checked_div(self.slowest_bytes_per_second)
            .unwrap_or(u64::MAX);
        self.grace
            .saturating_add(Duration::from_millis(paced_millis))
    }
}

impl<W: Write> Write for PacedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.blocked >= self.allowed_blocking() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let started = Instant::now();
        let written = self.out.write(bytes);
        self.blocked = self.blocked.saturating_add(started.elapsed());
        let count = written?;
        self.sent = self.sent.saturating_add(count as u64);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.blocked >= self.allowed_blocking() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let started = Instant::now();
        let flushed = self.out.flush();
        self.blocked = self.blocked.saturating_add(started.elapsed());
        flushed
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Head,
    Body,
}

struct ConnectionStream<'s> {
    stream: Socket,
    stop: &'s (dyn Fn() -> bool + Sync),
    deadline: Instant,
    phase: Phase,
    body_started: Instant,
    body_bytes: u64,
}

impl ConnectionStream<'_> {
    fn wait_for_request(&mut self, timeout: Duration) {
        self.phase = Phase::Waiting;
        self.deadline = deadline_after(timeout);
    }

    fn body_read(&mut self, count: usize) {
        self.body_bytes = self.body_bytes.saturating_add(count as u64);
        let allowed_millis = self
            .body_bytes
            .saturating_mul(1000)
            .checked_div(SLOWEST_BODY_BYTES_PER_SECOND)
            .unwrap_or(u64::MAX);
        let whole_body = BODY_READ_TIMEOUT.saturating_add(Duration::from_millis(allowed_millis));
        let whole_body_deadline = self
            .body_started
            .checked_add(whole_body)
            .unwrap_or_else(|| deadline_after(whole_body));
        self.deadline = deadline_after(BODY_READ_TIMEOUT).min(whole_body_deadline);
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
                        self.body_read(count);
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
        self.body_started = Instant::now();
        self.body_bytes = 0;
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
    out: &mut impl Write,
    streamed: StreamedResponse<'_>,
    head_only: bool,
    version: Version,
    keep: bool,
) -> bool {
    let length = streamed.content_length;
    let chunked = version == Version::Http11 && length.is_none();
    let connection = ConnectionHeader::of(version, keep && (chunked || length.is_some()));
    let framed = !bodyless(streamed.status);
    let mut head = status_head(streamed.status, &streamed.content_type, connection);
    if let Some(length) = length {
        head.push_str(&format!("Content-Length: {length}\r\n"));
    } else if chunked && framed {
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
    match (written, length) {
        (Ok(sent), Some(length)) => sent == length && connection.keeps(),
        (Ok(_), None) => connection.keeps(),
        (Err(_), _) => false,
    }
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
        HttpError::InvalidHost => {
            Response::text(400, "an HTTP/1.1 request needs exactly one Host header")
        }
        HttpError::UnsupportedContentCoding => Response::text(
            415,
            "the request's content coding is not supported; send it with gzip, deflate or identity",
        )
        .with_header("Accept-Encoding", "gzip, deflate"),
        HttpError::CorruptContentCoding => Response::text(
            400,
            "the request body does not decode as its Content-Encoding says",
        ),
        HttpError::OverMemoryBudget => (limits.over_memory_budget)(),
        HttpError::Closed => return None,
    })
}

fn linger(source: &mut ConnectionStream<'_>, writer: &Socket) {
    if writer.shutdown_write().is_err() {
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
    client: std::net::IpAddr,
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
    let stream = match limits.tls {
        Some(config) => match Socket::secure(stream, config, READ_POLL) {
            Ok(secured) => secured,
            Err(_) => return,
        },
        None => Socket::Plain(stream),
    };
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = RequestReader::new(ConnectionStream {
        stream,
        stop,
        deadline: deadline_after(FIRST_REQUEST_TIMEOUT),
        phase: Phase::Waiting,
        body_started: Instant::now(),
        body_bytes: 0,
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
            reader
                .next_request(limits.max_body_bytes, &mut held, &mut send_continue)
                .and_then(|(mut request, version)| {
                    if (limits.decode_content_encoding)(&request) {
                        decode_request_body(
                            &mut request,
                            limits.max_body_bytes,
                            &mut held,
                            limits.memory_budget,
                        )?;
                    }
                    Ok((request, version))
                })
        };
        let (request, version) = match read {
            Ok(read) => read,
            Err(error) => {
                if let Some(response) = refusal(&error, limits) {
                    held.resize(0);
                    let _ = write_whole(
                        &mut PacedWriter::new(&mut writer),
                        &response,
                        false,
                        ConnectionHeader::Close,
                    );
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
        let reply = match limits.rate_limit.map(|limiter| limiter.admit(client)) {
            Some(Err(wait)) => Reply::Whole(crate::too_many_requests_response(wait)),
            _ => handler(&request),
        };
        drop(request);
        let mut paced = PacedWriter::new(&mut writer);
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
                write_whole(&mut paced, &response, head_only, connection).is_ok()
                    && connection.keeps()
            }
            Reply::Streamed(streamed) => {
                let body_bytes = if head_only { 0 } else { streamed.held_bytes };
                if held.hold_if_large(body_bytes) {
                    write_streamed(&mut paced, streamed, head_only, version, keep)
                } else {
                    drop(streamed);
                    held.resize(0);
                    let response = (limits.over_memory_budget)();
                    let keep = keep && whole_response_frames_itself(&response, head_only);
                    let connection = ConnectionHeader::of(version, keep);
                    write_whole(&mut paced, &response, head_only, connection).is_ok()
                        && connection.keeps()
                }
            }
        };
        held.resize(0);
        if !kept {
            linger(&mut reader.source, &writer);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PacedWriter;
    use std::io::{self, Write};
    use std::time::{Duration, Instant};

    struct SlowReader {
        delay: Duration,
    }

    impl Write for SlowReader {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            std::thread::sleep(self.delay);
            Ok(bytes.len().min(1))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_client_reading_slower_than_the_pace_is_cut_off_however_steady_it_is() {
        let slow = SlowReader {
            delay: Duration::from_millis(5),
        };
        let mut paced = PacedWriter::with_pace(slow, Duration::from_millis(50), 1000);
        let started = Instant::now();
        let error = paced
            .write_all(&[0u8; 1_000_000])
            .expect_err("the response deadline passes");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(paced.sent < 1000, "{}", paced.sent);
    }

    #[test]
    fn a_client_reading_at_the_pace_gets_the_whole_response() {
        let mut paced = PacedWriter::with_pace(Vec::new(), Duration::from_millis(50), 100);
        paced
            .write_all(&[7u8; 100_000])
            .expect("an eager reader is never cut off");
        paced.flush().expect("flushed");
        assert_eq!(paced.out.len(), 100_000);
    }
}
