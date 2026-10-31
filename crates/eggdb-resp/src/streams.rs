use crate::commands::{Context, SYNTAX, WRONG_TYPE, integer, load, reading, save, writing};
use crate::protocol::Reply;
use crate::value::{Data, Entry, Stream, StreamId};

const NAMES: &[&str] = &["xadd", "xlen", "xrange", "xrevrange", "xdel", "xtrim", "xread"];

pub(crate) fn is_known(name: &str) -> bool {
    NAMES.contains(&name)
}

fn stream_of(entry: Option<&Entry>) -> Result<Stream, String> {
    match entry.map(|entry| &entry.data) {
        None => Ok(Stream::default()),
        Some(Data::Stream(stream)) => Ok(stream.clone()),
        Some(_) => Err(WRONG_TYPE.to_owned()),
    }
}

fn id_text(id: StreamId) -> Vec<u8> {
    format!("{}-{}", id.0, id.1).into_bytes()
}

fn parse_id(bytes: &[u8], missing_sequence: u64) -> Result<StreamId, String> {
    let invalid = || "ERR Invalid stream ID specified as stream command argument".to_owned();
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    match text.split_once('-') {
        Some((millis, sequence)) => Ok((
            millis.parse().map_err(|_| invalid())?,
            sequence.parse().map_err(|_| invalid())?,
        )),
        None => Ok((text.parse().map_err(|_| invalid())?, missing_sequence)),
    }
}

fn bound(bytes: &[u8], low: bool) -> Result<(StreamId, bool), String> {
    match bytes {
        b"-" => Ok(((0, 0), false)),
        b"+" => Ok(((u64::MAX, u64::MAX), false)),
        _ => {
            let (exclusive, rest) = match bytes.strip_prefix(b"(") {
                Some(rest) => (true, rest),
                None => (false, bytes),
            };
            Ok((parse_id(rest, if low { 0 } else { u64::MAX })?, exclusive))
        }
    }
}

fn entry_reply(id: StreamId, pairs: &[(Vec<u8>, Vec<u8>)]) -> Reply {
    let mut fields = Vec::with_capacity(pairs.len() * 2);
    for (field, value) in pairs {
        fields.push(Reply::bulk(field.clone()));
        fields.push(Reply::bulk(value.clone()));
    }
    Reply::Array(Some(vec![Reply::bulk(id_text(id)), Reply::Array(Some(fields))]))
}

fn trim(stream: &mut Stream, keep: usize) -> usize {
    let mut removed = 0;
    while stream.entries.len() > keep {
        if let Some(first) = stream.entries.keys().next().copied() {
            stream.entries.remove(&first);
            removed += 1;
        }
    }
    removed
}

pub(crate) fn execute(context: &Context<'_>, db: u32, name: &str, args: &[Vec<u8>]) -> Option<Reply> {
    let now = context.now_ms;
    Some(match (name, args.len()) {
        ("xadd", count) if count >= 4 => xadd(context, db, args),
        ("xlen", 1) => reading(context, db, |keys| {
            Ok(Reply::Integer(stream_of(load(keys, &args[0], now)?.as_ref())?.entries.len() as i64))
        }),
        ("xrange" | "xrevrange", count) if count == 3 || count == 5 => {
            let (first, second) = if name == "xrange" { (&args[1], &args[2]) } else { (&args[2], &args[1]) };
            let (Ok((low, low_open)), Ok((high, high_open))) = (bound(first, true), bound(second, false)) else {
                return Some(Reply::Error("ERR Invalid stream ID specified as stream command argument".to_owned()));
            };
            let limit = if count == 5 {
                if !args[3].eq_ignore_ascii_case(b"count") {
                    return Some(Reply::Error(SYNTAX.to_owned()));
                }
                match integer(&args[4]) {
                    Ok(limit) => limit.max(0) as usize,
                    Err(error) => return Some(Reply::Error(error)),
                }
            } else {
                usize::MAX
            };
            reading(context, db, |keys| {
                let stream = stream_of(load(keys, &args[0], now)?.as_ref())?;
                let selected = stream.entries.iter().filter(|(id, _)| {
                    (if low_open { **id > low } else { **id >= low }) && (if high_open { **id < high } else { **id <= high })
                });
                let replies: Vec<Reply> = if name == "xrange" {
                    selected.take(limit).map(|(id, pairs)| entry_reply(*id, pairs)).collect()
                } else {
                    selected.rev().take(limit).map(|(id, pairs)| entry_reply(*id, pairs)).collect()
                };
                Ok(Reply::Array(Some(replies)))
            })
        }
        ("xdel", count) if count >= 2 => {
            let mut ids = Vec::with_capacity(args.len() - 1);
            for id in &args[1..] {
                match parse_id(id, 0) {
                    Ok(id) => ids.push(id),
                    Err(error) => return Some(Reply::Error(error)),
                }
            }
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let mut stream = stream_of(entry.as_ref())?;
                let removed = ids.iter().filter(|id| stream.entries.remove(id).is_some()).count();
                if entry.is_some() {
                    save(keys, &args[0], &Entry { data: Data::Stream(stream), expires_at_ms: entry.and_then(|entry| entry.expires_at_ms) })?;
                }
                Ok(Reply::Integer(removed as i64))
            })
        }
        ("xtrim", 3 | 4) => {
            if !args[1].eq_ignore_ascii_case(b"maxlen") {
                return Some(Reply::Error(SYNTAX.to_owned()));
            }
            let amount = args.last().map(|value| integer(value));
            let Some(Ok(keep)) = amount else {
                return Some(Reply::Error(crate::commands::NOT_INTEGER.to_owned()));
            };
            writing(context, db, |keys| {
                let entry = load(keys, &args[0], now)?;
                let mut stream = stream_of(entry.as_ref())?;
                let removed = trim(&mut stream, keep.max(0) as usize);
                if entry.is_some() {
                    save(keys, &args[0], &Entry { data: Data::Stream(stream), expires_at_ms: entry.and_then(|entry| entry.expires_at_ms) })?;
                }
                Ok(Reply::Integer(removed as i64))
            })
        }
        ("xread", count) if count >= 3 => xread(context, db, args),
        _ => return None,
    })
}

fn xadd(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let mut at = 1;
    let mut make_stream = true;
    let mut max_length = None;
    loop {
        match args.get(at).map(|word| String::from_utf8_lossy(word).to_ascii_lowercase()) {
            Some(word) if word == "nomkstream" => {
                make_stream = false;
                at += 1;
            }
            Some(word) if word == "maxlen" => {
                at += 1;
                if matches!(args.get(at).map(Vec::as_slice), Some(b"=" | b"~")) {
                    at += 1;
                }
                match args.get(at).map(|value| integer(value)) {
                    Some(Ok(length)) if length >= 0 => max_length = Some(length as usize),
                    _ => return Reply::Error(SYNTAX.to_owned()),
                }
                at += 1;
            }
            _ => break,
        }
    }
    let Some(id_argument) = args.get(at) else {
        return Reply::Error(SYNTAX.to_owned());
    };
    let pairs = &args[at + 1..];
    if pairs.is_empty() || pairs.len() % 2 != 0 {
        return Reply::Error("ERR wrong number of arguments for 'xadd' command".to_owned());
    }
    let explicit = if id_argument.as_slice() == b"*" {
        None
    } else {
        match parse_id(id_argument, 0) {
            Ok(id) => Some(id),
            Err(error) => return Reply::Error(error),
        }
    };
    writing(context, db, |keys| {
        let entry = load(keys, &args[0], now)?;
        if entry.is_none() && !make_stream {
            return Ok(Reply::null());
        }
        let mut stream = stream_of(entry.as_ref())?;
        let id = match explicit {
            Some((0, 0)) => return Err("ERR The ID specified in XADD must be greater than 0-0".to_owned()),
            Some(id) if id <= stream.last => {
                return Err("ERR The ID specified in XADD is equal or smaller than the target stream top item".to_owned());
            }
            Some(id) => id,
            None => {
                let millis = now.max(stream.last.0);
                if millis == stream.last.0 {
                    let sequence = stream.last.1.checked_add(1).ok_or_else(|| "ERR The stream has exhausted the last possible ID".to_owned())?;
                    (millis, sequence)
                } else {
                    (millis, 0)
                }
            }
        };
        let fields = pairs.chunks(2).map(|pair| (pair[0].clone(), pair[1].clone())).collect();
        stream.entries.insert(id, fields);
        stream.last = id;
        if let Some(keep) = max_length {
            trim(&mut stream, keep);
        }
        save(keys, &args[0], &Entry { data: Data::Stream(stream), expires_at_ms: entry.and_then(|entry| entry.expires_at_ms) })?;
        Ok(Reply::bulk(id_text(id)))
    })
}

fn xread(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let mut at = 0;
    let mut limit = usize::MAX;
    while let Some(word) = args.get(at).map(|word| String::from_utf8_lossy(word).to_ascii_lowercase()) {
        match word.as_str() {
            "count" => {
                match args.get(at + 1).map(|value| integer(value)) {
                    Some(Ok(count)) => limit = count.max(0) as usize,
                    _ => return Reply::Error(SYNTAX.to_owned()),
                }
                at += 2;
            }
            "block" => return Reply::Error("ERR XREAD BLOCK is not supported yet; read without BLOCK".to_owned()),
            "streams" => {
                at += 1;
                break;
            }
            _ => return Reply::Error(SYNTAX.to_owned()),
        }
    }
    let rest = &args[at.min(args.len())..];
    if rest.is_empty() || rest.len() % 2 != 0 {
        return Reply::Error("ERR Unbalanced 'xread' list of streams: for each stream key an ID or '$' must be specified.".to_owned());
    }
    let (names, ids) = rest.split_at(rest.len() / 2);
    reading(context, db, |keys| {
        let mut streams = Vec::new();
        for (key, id) in names.iter().zip(ids) {
            let stream = stream_of(load(keys, key, now)?.as_ref())?;
            let after = if id.as_slice() == b"$" { stream.last } else { parse_id(id, 0)? };
            let entries: Vec<Reply> = stream
                .entries
                .range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
                .take(limit)
                .map(|(id, pairs)| entry_reply(*id, pairs))
                .collect();
            if !entries.is_empty() {
                streams.push((Reply::bulk(key.clone()), Reply::Array(Some(entries))));
            }
        }
        Ok(if streams.is_empty() {
            Reply::Array(None)
        } else {
            Reply::PairArray(streams)
        })
    })
}
