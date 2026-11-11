use crate::commands::{
    MAX_ITEM_BYTES, Mode, Shared, Stored, arithmetic, check_key, delete, flush_all, get_each,
    storage, touch,
};
use eggdb_platform::connection_limits::MemoryBudget;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_LINE_BYTES: usize = 2048;
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(200);
pub(crate) const VERSION: &str = "1.6.0 EggDB";

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
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
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                other => return other,
            }
        }
    }
}

fn line(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let read = reader
        .by_ref()
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)?;
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

const END: &[u8] = b"END\r\n";
const VALUE_BLOCK_FRAMING_BYTES: usize = 64;

fn value_block_bound(key: &[u8], data: &[u8]) -> usize {
    key.len() + data.len() + VALUE_BLOCK_FRAMING_BYTES
}

fn server_error(message: String) -> Vec<u8> {
    if message.starts_with("CLIENT_ERROR") || message.starts_with("SERVER_ERROR") {
        format!("{message}\r\n").into_bytes()
    } else {
        format!("SERVER_ERROR {message}\r\n").into_bytes()
    }
}

const OUT_OF_MEMORY_STORING: &[u8] = b"SERVER_ERROR out of memory storing object\r\n";
const OUT_OF_MEMORY: &[u8] = b"SERVER_ERROR out of memory\r\n";

fn discard(reader: &mut impl Read, bytes: usize) -> io::Result<()> {
    let skipped = io::copy(&mut reader.take(bytes as u64), &mut io::sink())?;
    if skipped < bytes as u64 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

/// Runs one connection's commands from `reader` and writes the answers to `writer`, holding
/// large values and replies against `budget`; returns when the client quits or the input ends.
pub fn serve_stream(
    shared: &Shared<'_>,
    budget: &MemoryBudget,
    reader: impl Read,
    writer: &mut impl Write,
) -> io::Result<()> {
    let mut reader = BufReader::new(reader);
    if reader.fill_buf()?.first() == Some(&crate::binary::REQUEST_MAGIC) {
        return crate::binary::serve_binary(shared, budget, reader, writer);
    }
    let mut held = budget.empty_reservation();
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
        let words: Vec<&[u8]> = request
            .split(|byte| *byte == b' ')
            .filter(|word| !word.is_empty())
            .collect();
        let Some(command) = words.first() else {
            continue;
        };
        let now = now_ms();
        let noreply = words.last().is_some_and(|word| *word == b"noreply");
        let answer: Vec<u8> = match *command {
            b"set" | b"add" | b"replace" | b"append" | b"prepend" | b"cas" => {
                let is_cas = *command == b"cas";
                let expected_words = if is_cas { 6 } else { 5 };
                let (Some(flags), Some(exptime), Some(length)) = (
                    number::<u32>(words.get(2)),
                    number::<i64>(words.get(3)),
                    number::<usize>(words.get(4)),
                ) else {
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
                if !held.hold_if_large(length + 2) {
                    discard(&mut reader, length + 2)?;
                    if !noreply {
                        writer.write_all(OUT_OF_MEMORY_STORING)?;
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
                let counters = &shared.counters;
                counters.cmd_set.fetch_add(1, Ordering::Relaxed);
                let cas_command = matches!(mode, Mode::Cas(_));
                match storage(shared, mode, words[1], flags, exptime, data, now) {
                    Ok(Stored::Stored) => {
                        if cas_command {
                            counters.cas_hits.fetch_add(1, Ordering::Relaxed);
                        }
                        b"STORED\r\n".to_vec()
                    }
                    Ok(Stored::NotStored) => b"NOT_STORED\r\n".to_vec(),
                    Ok(Stored::Exists) => {
                        counters.cas_badval.fetch_add(1, Ordering::Relaxed);
                        b"EXISTS\r\n".to_vec()
                    }
                    Ok(Stored::NotFound) => {
                        counters.cas_misses.fetch_add(1, Ordering::Relaxed);
                        b"NOT_FOUND\r\n".to_vec()
                    }
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
                let keys: Vec<Vec<u8>> =
                    words[first_key..].iter().map(|key| key.to_vec()).collect();
                let mut out = Vec::new();
                let mut hits = 0u64;
                let read = get_each(shared, &keys, exptime, now, &mut |key, item| {
                    let room = out.len() + value_block_bound(key, &item.data) + END.len();
                    if !held.hold_if_large(room) {
                        return false;
                    }
                    hits += 1;
                    value_block(
                        &mut out,
                        key,
                        item.flags,
                        &item.data,
                        with_cas.then_some(item.cas),
                    );
                    true
                });
                let counters = &shared.counters;
                let asked = keys.len() as u64;
                counters.cmd_get.fetch_add(asked, Ordering::Relaxed);
                counters.get_hits.fetch_add(hits, Ordering::Relaxed);
                counters
                    .get_misses
                    .fetch_add(asked.saturating_sub(hits), Ordering::Relaxed);
                if touching {
                    counters.cmd_touch.fetch_add(asked, Ordering::Relaxed);
                    counters.touch_hits.fetch_add(hits, Ordering::Relaxed);
                    counters
                        .touch_misses
                        .fetch_add(asked.saturating_sub(hits), Ordering::Relaxed);
                }
                match read {
                    Ok(()) => {
                        out.extend_from_slice(END);
                        out
                    }
                    Err(error) => server_error(error),
                }
            }
            b"delete" if words.len() >= 2 => match delete(shared, words[1], now) {
                Ok(true) => {
                    shared.counters.delete_hits.fetch_add(1, Ordering::Relaxed);
                    b"DELETED\r\n".to_vec()
                }
                Ok(false) => {
                    shared.counters.delete_misses.fetch_add(1, Ordering::Relaxed);
                    b"NOT_FOUND\r\n".to_vec()
                }
                Err(error) => server_error(error),
            },
            b"incr" | b"decr" if words.len() >= 3 => match number::<u64>(words.get(2)) {
                None => b"CLIENT_ERROR invalid numeric delta argument\r\n".to_vec(),
                Some(delta) => {
                    let increment = *command == b"incr";
                    let outcome = arithmetic(shared, words[1], delta, increment, now);
                    let counters = &shared.counters;
                    let counter = match (increment, matches!(outcome, Ok(Some(_)))) {
                        (true, true) => &counters.incr_hits,
                        (true, false) => &counters.incr_misses,
                        (false, true) => &counters.decr_hits,
                        (false, false) => &counters.decr_misses,
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                    match outcome {
                        Ok(Some(value)) => format!("{value}\r\n").into_bytes(),
                        Ok(None) => b"NOT_FOUND\r\n".to_vec(),
                        Err(error) => server_error(error),
                    }
                }
            },
            b"touch" if words.len() >= 3 => match number::<i64>(words.get(2)) {
                None => b"CLIENT_ERROR invalid exptime argument\r\n".to_vec(),
                Some(exptime) => match touch(shared, words[1], exptime, now) {
                    Ok(true) => {
                        shared.counters.cmd_touch.fetch_add(1, Ordering::Relaxed);
                        shared.counters.touch_hits.fetch_add(1, Ordering::Relaxed);
                        b"TOUCHED\r\n".to_vec()
                    }
                    Ok(false) => {
                        shared.counters.cmd_touch.fetch_add(1, Ordering::Relaxed);
                        shared.counters.touch_misses.fetch_add(1, Ordering::Relaxed);
                        b"NOT_FOUND\r\n".to_vec()
                    }
                    Err(error) => server_error(error),
                },
            },
            b"flush_all" => match flush_all(shared).inspect(|()| {
                shared.counters.cmd_flush.fetch_add(1, Ordering::Relaxed);
            }) {
                Ok(()) => b"OK\r\n".to_vec(),
                Err(error) => server_error(error),
            },
            b"version" => format!("VERSION {VERSION}\r\n").into_bytes(),
            b"verbosity" => b"OK\r\n".to_vec(),
            b"stats" => match words.get(1).copied() {
                None => shared.stats(VERSION),
                Some(b"reset") => {
                    shared.counters.reset();
                    b"RESET\r\n".to_vec()
                }
                Some(b"settings") => format!(
                    "STAT maxbytes {}\r\nSTAT item_size_max {MAX_ITEM_BYTES}\r\nSTAT evictions {}\r\nSTAT cas_enabled yes\r\nSTAT binding_protocol auto-negotiate\r\nEND\r\n",
                    shared.lru.limit_bytes(),
                    if shared.lru.is_on() { "on" } else { "off" }
                )
                .into_bytes(),
                Some(b"items" | b"slabs" | b"sizes" | b"conns") => b"END\r\n".to_vec(),
                Some(_) => b"ERROR\r\n".to_vec(),
            },
            b"quit" => return Ok(()),
            b"mn" => b"MN\r\n".to_vec(),
            b"mg" if words.len() >= 2 => {
                let reply = crate::meta::meta_get(shared, words[1], &words[2..], now);
                if reply.quiet { Vec::new() } else { reply.bytes }
            }
            b"ms" if words.len() >= 3 => {
                let Some(length) =
                    number::<usize>(words.get(2)).filter(|length| *length <= MAX_ITEM_BYTES)
                else {
                    writer.write_all(b"CLIENT_ERROR bad data chunk\r\n")?;
                    return Ok(());
                };
                if !held.hold_if_large(length + 2) {
                    discard(&mut reader, length + 2)?;
                    writer.write_all(OUT_OF_MEMORY_STORING)?;
                    continue;
                }
                let mut data = vec![0u8; length + 2];
                reader.read_exact(&mut data)?;
                if data[length..] != *b"\r\n" {
                    writer.write_all(b"CLIENT_ERROR bad data chunk\r\n")?;
                    continue;
                }
                data.truncate(length);
                let reply = crate::meta::meta_set(shared, words[1], &words[3..], data, now);
                if reply.quiet { Vec::new() } else { reply.bytes }
            }
            b"md" if words.len() >= 2 => {
                let reply = crate::meta::meta_delete(shared, words[1], &words[2..], now);
                if reply.quiet { Vec::new() } else { reply.bytes }
            }
            b"ma" if words.len() >= 2 => {
                let reply = crate::meta::meta_arithmetic(shared, words[1], &words[2..], now);
                if reply.quiet { Vec::new() } else { reply.bytes }
            }
            b"me" if words.len() >= 2 => {
                crate::meta::meta_debug(shared, words[1], &words[2..], now).bytes
            }
            _ => b"ERROR\r\n".to_vec(),
        };
        let answer = if held.hold_if_large(answer.len()) {
            answer
        } else {
            OUT_OF_MEMORY.to_vec()
        };
        if !noreply {
            writer.write_all(&answer)?;
        }
        held.hold_if_large(0);
    }
}

/// How the memcached server runs.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The most connections served at once.
    pub max_connections: usize,
    /// The memory every connection's values and replies hold together, shared with the
    /// server's other listeners.
    pub memory_budget: std::sync::Arc<MemoryBudget>,
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
    let budget: &MemoryBudget = &config.memory_budget;
    std::thread::scope(|scope| {
        while !stop() {
            let stream = match eggdb_platform::accept_tcp(listener) {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err()
                || stream.set_read_timeout(Some(READ_POLL)).is_err()
            {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                let mut stream = stream;
                let _ = stream.write_all(b"SERVER_ERROR too many open connections\r\n");
                continue;
            }
            let active = &active;
            let counters = &shared.counters;
            counters.total_connections.fetch_add(1, Ordering::Relaxed);
            counters.curr_connections.fetch_add(1, Ordering::Relaxed);
            let spawned = std::thread::Builder::new()
                .name("eggdb-memcached".to_owned())
                .spawn_scoped(scope, move || {
                    if let Ok(mut writer) = stream.try_clone() {
                        let _ =
                            serve_stream(shared, budget, StopAware { stream, stop }, &mut writer);
                    }
                    counters.curr_connections.fetch_sub(1, Ordering::Relaxed);
                    active.fetch_sub(1, Ordering::AcqRel);
                });
            if spawned.is_err() {
                counters.curr_connections.fetch_sub(1, Ordering::Relaxed);
            }
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(())
}
