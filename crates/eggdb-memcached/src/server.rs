use crate::commands::{Mode, Shared, Stored, arithmetic, check_key, delete, flush_all, get, storage, touch, MAX_ITEM_BYTES};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_LINE_BYTES: usize = 2048;
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(200);
const VERSION: &str = "1.6.0 EggDB";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

struct StopAware<'s> {
    stream: TcpStream,
    stop: &'s (dyn Fn() -> bool + Sync),
}

impl Read for StopAware<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if (self.stop)() {
                return Ok(0);
            }
            match self.stream.read(buffer) {
                Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                other => return other,
            }
        }
    }
}

fn line(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let read = reader.by_ref().take(MAX_LINE_BYTES as u64 + 1).read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > MAX_LINE_BYTES || !line.ends_with(b"\n") {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    line.pop();
    if line.ends_with(b"\r") {
        line.pop();
    }
    Ok(Some(line))
}

fn number<T: std::str::FromStr>(word: Option<&&[u8]>) -> Option<T> {
    std::str::from_utf8(word?).ok()?.parse().ok()
}

fn value_block(out: &mut Vec<u8>, key: &[u8], flags: u32, data: &[u8], cas: Option<u64>) {
    out.extend_from_slice(b"VALUE ");
    out.extend_from_slice(key);
    out.extend_from_slice(format!(" {flags} {}", data.len()).as_bytes());
    if let Some(cas) = cas {
        out.extend_from_slice(format!(" {cas}").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
}

fn server_error(message: String) -> Vec<u8> {
    if message.starts_with("CLIENT_ERROR") || message.starts_with("SERVER_ERROR") {
        format!("{message}\r\n").into_bytes()
    } else {
        format!("SERVER_ERROR {message}\r\n").into_bytes()
    }
}

/// Runs one connection's commands from `reader` and writes the answers to `writer`; returns
/// when the client quits or the input ends.
pub fn serve_stream(shared: &Shared<'_>, reader: impl Read, writer: &mut impl Write) -> io::Result<()> {
    let mut reader = BufReader::new(reader);
    loop {
        let Some(request) = (match line(&mut reader) {
            Ok(request) => request,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                writer.write_all(b"CLIENT_ERROR line too long\r\n")?;
                return Ok(());
            }
            Err(error) => return Err(error),
        }) else {
            return Ok(());
        };
        let words: Vec<&[u8]> = request.split(|byte| *byte == b' ').filter(|word| !word.is_empty()).collect();
        let Some(command) = words.first() else {
            continue;
        };
        let now = now_ms();
        let noreply = words.last().is_some_and(|word| *word == b"noreply");
        let answer: Vec<u8> = match *command {
            b"set" | b"add" | b"replace" | b"append" | b"prepend" | b"cas" => {
                let is_cas = *command == b"cas";
                let expected_words = if is_cas { 6 } else { 5 };
                let (Some(flags), Some(exptime), Some(length)) =
                    (number::<u32>(words.get(2)), number::<i64>(words.get(3)), number::<usize>(words.get(4)))
                else {
                    writer.write_all(b"CLIENT_ERROR bad command line format\r\n")?;
                    continue;
                };
                if words.len() < expected_words || length > MAX_ITEM_BYTES {
                    writer.write_all(b"CLIENT_ERROR bad data chunk\r\n")?;
                    if length > MAX_ITEM_BYTES {
                        return Ok(());
                    }
                    continue;
                }
                let mut data = vec![0u8; length + 2];
                reader.read_exact(&mut data)?;
                if !data.ends_with(b"\r\n") {
                    writer.write_all(b"CLIENT_ERROR bad data chunk\r\n")?;
                    continue;
                }
                data.truncate(length);
                if let Err(error) = check_key(words[1]) {
                    writer.write_all(server_error(error).as_slice())?;
                    continue;
                }
                let mode = match *command {
                    b"add" => Mode::Add,
                    b"replace" => Mode::Replace,
                    b"append" => Mode::Append,
                    b"prepend" => Mode::Prepend,
                    b"cas" => match number::<u64>(words.get(5)) {
                        Some(expected) => Mode::Cas(expected),
                        None => {
                            writer.write_all(b"CLIENT_ERROR bad command line format\r\n")?;
                            continue;
                        }
                    },
                    _ => Mode::Set,
                };
                match storage(shared, mode, words[1], flags, exptime, data, now) {
                    Ok(Stored::Stored) => b"STORED\r\n".to_vec(),
                    Ok(Stored::NotStored) => b"NOT_STORED\r\n".to_vec(),
                    Ok(Stored::Exists) => b"EXISTS\r\n".to_vec(),
                    Ok(Stored::NotFound) => b"NOT_FOUND\r\n".to_vec(),
                    Err(error) => server_error(error),
                }
            }
            b"get" | b"gets" | b"gat" | b"gats" => {
                let touching = matches!(*command, b"gat" | b"gats");
                let with_cas = matches!(*command, b"gets" | b"gats");
                let (exptime, first_key) = if touching {
                    (number::<i64>(words.get(1)), 2)
                } else {
                    (None, 1)
                };
                if words.len() <= first_key || (touching && exptime.is_none()) {
                    writer.write_all(b"ERROR\r\n")?;
                    continue;
                }
                let keys: Vec<Vec<u8>> = words[first_key..].iter().map(|key| key.to_vec()).collect();
                match get(shared, &keys, exptime, now) {
                    Ok(found) => {
                        let mut out = Vec::new();
                        for (key, item) in found {
                            value_block(&mut out, &key, item.flags, &item.data, with_cas.then_some(item.cas));
                        }
                        out.extend_from_slice(b"END\r\n");
                        out
                    }
                    Err(error) => server_error(error),
                }
            }
            b"delete" if words.len() >= 2 => match delete(shared, words[1], now) {
                Ok(true) => b"DELETED\r\n".to_vec(),
                Ok(false) => b"NOT_FOUND\r\n".to_vec(),
                Err(error) => server_error(error),
            },
            b"incr" | b"decr" if words.len() >= 3 => match number::<u64>(words.get(2)) {
                None => b"CLIENT_ERROR invalid numeric delta argument\r\n".to_vec(),
                Some(delta) => match arithmetic(shared, words[1], delta, *command == b"incr", now) {
                    Ok(Some(value)) => format!("{value}\r\n").into_bytes(),
                    Ok(None) => b"NOT_FOUND\r\n".to_vec(),
                    Err(error) => server_error(error),
                },
            },
            b"touch" if words.len() >= 3 => match number::<i64>(words.get(2)) {
                None => b"CLIENT_ERROR invalid exptime argument\r\n".to_vec(),
                Some(exptime) => match touch(shared, words[1], exptime, now) {
                    Ok(true) => b"TOUCHED\r\n".to_vec(),
                    Ok(false) => b"NOT_FOUND\r\n".to_vec(),
                    Err(error) => server_error(error),
                },
            },
            b"flush_all" => match flush_all(shared) {
                Ok(()) => b"OK\r\n".to_vec(),
                Err(error) => server_error(error),
            },
            b"version" => format!("VERSION {VERSION}\r\n").into_bytes(),
            b"verbosity" => b"OK\r\n".to_vec(),
            b"stats" => b"STAT pid 0\r\nSTAT version 1.6.0\r\nEND\r\n".to_vec(),
            b"quit" => return Ok(()),
            b"mn" => b"MN\r\n".to_vec(),
            b"mg" if words.len() >= 2 => {
                let flags = &words[2..];
                let wants = |flag: u8| flags.iter().any(|word| word.first() == Some(&flag));
                match get(shared, &[words[1].to_vec()], None, now) {
                    Ok(found) => match found.into_iter().next() {
                        None if wants(b'q') => Vec::new(),
                        None => b"EN\r\n".to_vec(),
                        Some((key, item)) => {
                            let mut meta = String::new();
                            if wants(b'f') {
                                meta.push_str(&format!(" f{}", item.flags));
                            }
                            if wants(b'c') {
                                meta.push_str(&format!(" c{}", item.cas));
                            }
                            if wants(b't') {
                                let ttl = item.expires_at_ms.map_or(-1, |at| (at.saturating_sub(now) / 1000) as i64);
                                meta.push_str(&format!(" t{ttl}"));
                            }
                            if wants(b's') {
                                meta.push_str(&format!(" s{}", item.data.len()));
                            }
                            let mut out = Vec::new();
                            if wants(b'v') {
                                out.extend_from_slice(format!("VA {}", item.data.len()).as_bytes());
                            } else {
                                out.extend_from_slice(b"HD");
                            }
                            out.extend_from_slice(meta.as_bytes());
                            if wants(b'k') {
                                out.extend_from_slice(b" k");
                                out.extend_from_slice(&key);
                            }
                            out.extend_from_slice(b"\r\n");
                            if wants(b'v') {
                                out.extend_from_slice(&item.data);
                                out.extend_from_slice(b"\r\n");
                            }
                            out
                        }
                    },
                    Err(error) => server_error(error),
                }
            }
            b"ms" if words.len() >= 3 => {
                let Some(length) = number::<usize>(words.get(2)).filter(|length| *length <= MAX_ITEM_BYTES) else {
                    writer.write_all(b"CLIENT_ERROR bad data chunk\r\n")?;
                    return Ok(());
                };
                let mut data = vec![0u8; length + 2];
                reader.read_exact(&mut data)?;
                data.truncate(length);
                let flag_value = |flag: u8| {
                    words[3..]
                        .iter()
                        .find(|word| word.first() == Some(&flag))
                        .and_then(|word| std::str::from_utf8(&word[1..]).ok()?.parse::<i64>().ok())
                };
                let flags = flag_value(b'F').unwrap_or(0) as u32;
                let exptime = flag_value(b'T').unwrap_or(0);
                match storage(shared, Mode::Set, words[1], flags, exptime, data, now) {
                    Ok(_) => b"HD\r\n".to_vec(),
                    Err(error) => server_error(error),
                }
            }
            b"md" if words.len() >= 2 => match delete(shared, words[1], now) {
                Ok(true) => b"HD\r\n".to_vec(),
                Ok(false) => b"NF\r\n".to_vec(),
                Err(error) => server_error(error),
            },
            _ => b"ERROR\r\n".to_vec(),
        };
        if !noreply {
            writer.write_all(&answer)?;
        }
    }
}

/// How the memcached server runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerConfig {
    /// The most connections served at once.
    pub max_connections: usize,
}

/// Serves memcached clients on `listener` from `shared`, a thread per connection, until
/// `stop` turns true.
pub fn serve(
    shared: &Shared<'_>,
    listener: &TcpListener,
    config: ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let active = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        while !stop() {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err() || stream.set_read_timeout(Some(READ_POLL)).is_err() {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                let mut stream = stream;
                let _ = stream.write_all(b"SERVER_ERROR too many open connections\r\n");
                continue;
            }
            let active = &active;
            let spawned = std::thread::Builder::new()
                .name("eggdb-memcached".to_owned())
                .spawn_scoped(scope, move || {
                    if let Ok(mut writer) = stream.try_clone() {
                        let _ = serve_stream(shared, StopAware { stream, stop }, &mut writer);
                    }
                    active.fetch_sub(1, Ordering::AcqRel);
                });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(())
}
