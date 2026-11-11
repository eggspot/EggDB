use crate::commands::{
    Arithmetic, MAX_ITEM_BYTES, Mode, Shared, Stored, arithmetic_with, check_key, delete_if, get,
    storage, touch,
};
use crate::item::Item;
use std::sync::atomic::Ordering;

struct Flags<'w> {
    words: &'w [&'w [u8]],
}

impl<'w> Flags<'w> {
    fn has(&self, flag: u8) -> bool {
        self.words.iter().any(|word| word.first() == Some(&flag))
    }

    fn token(&self, flag: u8) -> Option<&'w [u8]> {
        self.words
            .iter()
            .find(|word| word.first() == Some(&flag))
            .map(|word| &word[1..])
    }

    fn number<T: std::str::FromStr>(&self, flag: u8) -> Option<T> {
        std::str::from_utf8(self.token(flag)?).ok()?.parse().ok()
    }
}

pub(crate) struct MetaReply {
    pub(crate) bytes: Vec<u8>,
    pub(crate) quiet: bool,
}

fn key_of(raw: &[u8], flags: &Flags<'_>) -> Result<Vec<u8>, Vec<u8>> {
    let key = if flags.has(b'b') {
        eggdb_crypto::base64_decode(raw)
            .map_err(|_| b"CLIENT_ERROR bad data chunk\r\n".to_vec())?
    } else {
        raw.to_vec()
    };
    if !flags.has(b'b') {
        check_key(&key).map_err(|error| format!("{error}\r\n").into_bytes())?;
    } else if key.is_empty() || key.len() > 250 {
        return Err(b"CLIENT_ERROR bad command line format\r\n".to_vec());
    }
    Ok(key)
}

fn echoed(raw_key: &[u8], flags: &Flags<'_>, out: &mut Vec<u8>) {
    if let Some(opaque) = flags.token(b'O') {
        out.extend_from_slice(b" O");
        out.extend_from_slice(opaque);
    }
    if flags.has(b'k') {
        out.extend_from_slice(b" k");
        out.extend_from_slice(raw_key);
        if flags.has(b'b') {
            out.extend_from_slice(b" b");
        }
    }
}

fn status(code: &[u8], raw_key: &[u8], flags: &Flags<'_>, quiet: bool) -> MetaReply {
    let mut bytes = code.to_vec();
    echoed(raw_key, flags, &mut bytes);
    bytes.extend_from_slice(b"\r\n");
    MetaReply { bytes, quiet }
}

fn server_error(error: String) -> MetaReply {
    let line = if error.starts_with("SERVER_ERROR") || error.starts_with("CLIENT_ERROR") {
        format!("{error}\r\n")
    } else {
        format!("SERVER_ERROR {error}\r\n")
    };
    MetaReply {
        bytes: line.into_bytes(),
        quiet: false,
    }
}

fn ttl_of(item: &Item, now: u64) -> i64 {
    item.expires_at_ms
        .map_or(-1, |at| (at.saturating_sub(now) / 1000) as i64)
}

fn item_flags(item: &Item, flags: &Flags<'_>, now: u64, out: &mut Vec<u8>) {
    if flags.has(b'f') {
        out.extend_from_slice(format!(" f{}", item.flags).as_bytes());
    }
    if flags.has(b'c') {
        out.extend_from_slice(format!(" c{}", item.cas).as_bytes());
    }
    if flags.has(b't') {
        out.extend_from_slice(format!(" t{}", ttl_of(item, now)).as_bytes());
    }
    if flags.has(b's') {
        out.extend_from_slice(format!(" s{}", item.data.len()).as_bytes());
    }
    if flags.has(b'h') {
        out.extend_from_slice(b" h0");
    }
    if flags.has(b'l') {
        out.extend_from_slice(b" l0");
    }
}

/// `mg <key> <flags>*`.
pub(crate) fn meta_get(shared: &Shared<'_>, raw_key: &[u8], words: &[&[u8]], now: u64) -> MetaReply {
    let flags = Flags { words };
    let key = match key_of(raw_key, &flags) {
        Ok(key) => key,
        Err(bytes) => return MetaReply { bytes, quiet: false },
    };
    let quiet = flags.has(b'q');
    shared.counters.cmd_get.fetch_add(1, Ordering::Relaxed);
    let found = match flags.number::<i64>(b'T') {
        Some(exptime) => {
            shared.counters.cmd_touch.fetch_add(1, Ordering::Relaxed);
            match touch(shared, &key, exptime, now) {
                Ok(true) => get(shared, &[key.clone()], None, now),
                Ok(false) => Ok(Vec::new()),
                Err(error) => Err(error),
            }
        }
        None => get(shared, &[key.clone()], None, now),
    };
    let item = match found {
        Ok(found) => found.into_iter().next().map(|(_, item)| item),
        Err(error) => return server_error(error),
    };
    let Some(item) = item else {
        shared.counters.get_misses.fetch_add(1, Ordering::Relaxed);
        return status(b"EN", raw_key, &flags, quiet);
    };
    shared.counters.get_hits.fetch_add(1, Ordering::Relaxed);
    let mut bytes = if flags.has(b'v') {
        format!("VA {}", item.data.len()).into_bytes()
    } else {
        b"HD".to_vec()
    };
    item_flags(&item, &flags, now, &mut bytes);
    echoed(raw_key, &flags, &mut bytes);
    bytes.extend_from_slice(b"\r\n");
    if flags.has(b'v') {
        bytes.extend_from_slice(&item.data);
        bytes.extend_from_slice(b"\r\n");
    }
    MetaReply {
        bytes,
        quiet: false,
    }
}

/// `ms <key> <datalen> <flags>*` with the data block already read.
pub(crate) fn meta_set(
    shared: &Shared<'_>,
    raw_key: &[u8],
    words: &[&[u8]],
    data: Vec<u8>,
    now: u64,
) -> MetaReply {
    let flags = Flags { words };
    let key = match key_of(raw_key, &flags) {
        Ok(key) => key,
        Err(bytes) => return MetaReply { bytes, quiet: false },
    };
    let quiet = flags.has(b'q');
    if data.len() > MAX_ITEM_BYTES {
        return server_error("SERVER_ERROR object too large for cache".to_owned());
    }
    let client_flags = flags.number::<u32>(b'F').unwrap_or(0);
    let exptime = flags.number::<i64>(b'T').unwrap_or(0);
    let compare = flags.number::<u64>(b'C');
    let mode = match (flags.token(b'M').and_then(|token| token.first()), compare) {
        (Some(b'E' | b'e'), _) => Mode::Add,
        (Some(b'A' | b'a'), _) => Mode::Append,
        (Some(b'P' | b'p'), _) => Mode::Prepend,
        (Some(b'R' | b'r'), _) => Mode::Replace,
        (None | Some(b'S' | b's'), Some(expected)) => Mode::Cas(expected),
        (None | Some(b'S' | b's'), None) => Mode::Set,
        (Some(_), _) => {
            return MetaReply {
                bytes: b"CLIENT_ERROR invalid mode for ms\r\n".to_vec(),
                quiet: false,
            };
        }
    };
    shared.counters.cmd_set.fetch_add(1, Ordering::Relaxed);
    match storage(shared, mode, &key, client_flags, exptime, data, now) {
        Ok(Stored::Stored) => {
            let mut reply = status(b"HD", raw_key, &flags, quiet);
            if flags.has(b'c')
                && let Ok(found) = get(shared, &[key], None, now)
                && let Some((_, item)) = found.into_iter().next()
            {
                let mut bytes = format!("HD c{}", item.cas).into_bytes();
                echoed(raw_key, &flags, &mut bytes);
                bytes.extend_from_slice(b"\r\n");
                reply.bytes = bytes;
            }
            if matches!(mode, Mode::Cas(_)) {
                shared.counters.cas_hits.fetch_add(1, Ordering::Relaxed);
            }
            reply
        }
        Ok(Stored::NotStored) => status(b"NS", raw_key, &flags, false),
        Ok(Stored::Exists) => {
            shared.counters.cas_badval.fetch_add(1, Ordering::Relaxed);
            status(b"EX", raw_key, &flags, false)
        }
        Ok(Stored::NotFound) => {
            shared.counters.cas_misses.fetch_add(1, Ordering::Relaxed);
            status(b"NF", raw_key, &flags, false)
        }
        Err(error) => server_error(error),
    }
}

/// `md <key> <flags>*`.
pub(crate) fn meta_delete(shared: &Shared<'_>, raw_key: &[u8], words: &[&[u8]], now: u64) -> MetaReply {
    let flags = Flags { words };
    let key = match key_of(raw_key, &flags) {
        Ok(key) => key,
        Err(bytes) => return MetaReply { bytes, quiet: false },
    };
    let quiet = flags.has(b'q');
    match delete_if(shared, &key, flags.number::<u64>(b'C'), now) {
        Ok(Some(true)) => {
            shared.counters.delete_hits.fetch_add(1, Ordering::Relaxed);
            status(b"HD", raw_key, &flags, quiet)
        }
        Ok(Some(false)) => status(b"EX", raw_key, &flags, false),
        Ok(None) => {
            shared.counters.delete_misses.fetch_add(1, Ordering::Relaxed);
            status(b"NF", raw_key, &flags, false)
        }
        Err(error) => server_error(error),
    }
}

/// `ma <key> <flags>*`.
pub(crate) fn meta_arithmetic(
    shared: &Shared<'_>,
    raw_key: &[u8],
    words: &[&[u8]],
    now: u64,
) -> MetaReply {
    let flags = Flags { words };
    let key = match key_of(raw_key, &flags) {
        Ok(key) => key,
        Err(bytes) => return MetaReply { bytes, quiet: false },
    };
    let quiet = flags.has(b'q');
    let delta = match flags.token(b'D') {
        None => 1,
        Some(_) => match flags.number::<u64>(b'D') {
            Some(delta) => delta,
            None => {
                return MetaReply {
                    bytes: b"CLIENT_ERROR bad token in command line format\r\n".to_vec(),
                    quiet: false,
                };
            }
        },
    };
    let increment = !matches!(
        flags.token(b'M').and_then(|token| token.first()),
        Some(b'D' | b'd' | b'-')
    );
    let vivify = flags.number::<i64>(b'N');
    let initial = flags.number::<u64>(b'J').unwrap_or(0);
    let outcome = arithmetic_with(
        shared,
        &key,
        Arithmetic {
            delta,
            increment,
            vivify: vivify.map(|exptime| (exptime, initial)),
            compare: flags.number::<u64>(b'C'),
            touch: flags.number::<i64>(b'T'),
        },
        now,
    );
    let counter = match (increment, &outcome) {
        (true, Ok(Some(_))) => &shared.counters.incr_hits,
        (true, _) => &shared.counters.incr_misses,
        (false, Ok(Some(_))) => &shared.counters.decr_hits,
        (false, _) => &shared.counters.decr_misses,
    };
    counter.fetch_add(1, Ordering::Relaxed);
    match outcome {
        Ok(Some(Ok(item))) => {
            let value = String::from_utf8_lossy(&item.data).into_owned();
            let mut bytes = if flags.has(b'v') {
                format!("VA {}", value.len()).into_bytes()
            } else {
                b"HD".to_vec()
            };
            if flags.has(b't') {
                bytes.extend_from_slice(format!(" t{}", ttl_of(&item, now)).as_bytes());
            }
            if flags.has(b'c') {
                bytes.extend_from_slice(format!(" c{}", item.cas).as_bytes());
            }
            echoed(raw_key, &flags, &mut bytes);
            bytes.extend_from_slice(b"\r\n");
            if flags.has(b'v') {
                bytes.extend_from_slice(value.as_bytes());
                bytes.extend_from_slice(b"\r\n");
                return MetaReply { bytes, quiet: false };
            }
            MetaReply { bytes, quiet }
        }
        Ok(Some(Err(()))) => status(b"EX", raw_key, &flags, false),
        Ok(None) => status(b"NF", raw_key, &flags, false),
        Err(error) => server_error(error),
    }
}

/// `me <key>`: the item's metadata as memcached's debug line.
pub(crate) fn meta_debug(shared: &Shared<'_>, raw_key: &[u8], words: &[&[u8]], now: u64) -> MetaReply {
    let flags = Flags { words };
    let key = match key_of(raw_key, &flags) {
        Ok(key) => key,
        Err(bytes) => return MetaReply { bytes, quiet: false },
    };
    match get(shared, &[key], None, now) {
        Ok(found) => match found.into_iter().next() {
            None => MetaReply {
                bytes: b"EN\r\n".to_vec(),
                quiet: false,
            },
            Some((_, item)) => MetaReply {
                bytes: format!(
                    "ME {} exp={} la=0 cas={} fetch=no cls=1 size={}\r\n",
                    String::from_utf8_lossy(raw_key),
                    ttl_of(&item, now),
                    item.cas,
                    item.data.len()
                )
                .into_bytes(),
                quiet: false,
            },
        },
        Err(error) => server_error(error),
    }
}
