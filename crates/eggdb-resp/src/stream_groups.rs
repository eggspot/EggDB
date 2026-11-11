use crate::blocking::milliseconds_timeout;
use crate::commands::{Context, NOT_INTEGER, SYNTAX, WRONG_TYPE, bulk_reply_bytes, integer, load, reading, save, writing};
use crate::keyspace::{KeysRead, KeysWrite};
use crate::pops::OnWrongType;
use crate::protocol::Reply;
use crate::streams::{bound, entry_reply, entry_reply_bytes, id_text, parse_id};
use crate::value::{Consumer, ConsumerGroup, Data, Entry, PendingEntry, Stream, StreamId};

const NAMES: &[&str] = &["xgroup", "xreadgroup", "xack", "xpending", "xclaim", "xautoclaim", "xinfo"];
const AUTOCLAIM_DEFAULT_COUNT: usize = 100;
const AUTOCLAIM_SCANS_PER_CLAIM: usize = 10;
const KEY_REQUIRED: &str = "ERR The XGROUP subcommand requires the key to exist. Note that for CREATE you may want to use the MKSTREAM option to create an empty stream automatically.";
const BUSY_GROUP: &str = "BUSYGROUP Consumer Group name already exists";

pub(crate) fn is_known(name: &str) -> bool {
    NAMES.contains(&name)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn lower(bytes: &[u8]) -> String {
    text(bytes).to_ascii_lowercase()
}

fn no_group(key: &[u8], group: &[u8], command: &str) -> String {
    format!(
        "NOGROUP No such key '{}' or consumer group '{}' in {command}",
        text(key),
        text(group)
    )
}

fn count_of(bytes: &[u8]) -> Result<usize, String> {
    let value = integer(bytes)?;
    usize::try_from(value).map_err(|_| NOT_INTEGER.to_owned())
}

fn millis_of(bytes: &[u8]) -> Result<u64, String> {
    let value = integer(bytes)?;
    u64::try_from(value).map_err(|_| NOT_INTEGER.to_owned())
}

struct Loaded {
    stream: Stream,
    expires_at_ms: Option<u64>,
}

fn load_stream(keys: &dyn KeysRead, key: &[u8], now: u64) -> Result<Option<Loaded>, String> {
    match load(keys, key, now)? {
        None => Ok(None),
        Some(Entry {
            data: Data::Stream(stream),
            expires_at_ms,
        }) => Ok(Some(Loaded { stream, expires_at_ms })),
        Some(_) => Err(WRONG_TYPE.to_owned()),
    }
}

fn store_stream(keys: &mut dyn KeysWrite, key: &[u8], loaded: Loaded) -> Result<(), String> {
    save(
        keys,
        key,
        &Entry {
            data: Data::Stream(loaded.stream),
            expires_at_ms: loaded.expires_at_ms,
        },
    )
}

fn consumer_seen<'g>(group: &'g mut ConsumerGroup, name: &[u8], now: u64) -> &'g mut Consumer {
    let consumer = group.consumers.entry(name.to_vec()).or_insert(Consumer {
        seen_at_ms: now,
        active_at_ms: 0,
    });
    consumer.seen_at_ms = now;
    consumer
}

fn group_id(bytes: &[u8], stream: &Stream) -> Result<StreamId, String> {
    if bytes == b"$" {
        Ok(stream.last)
    } else {
        parse_id(bytes, 0)
    }
}

fn entries_read_option(args: &[Vec<u8>]) -> Result<(bool, Option<u64>), String> {
    let mut make_stream = false;
    let mut entries_read = None;
    let mut at = 0;
    while let Some(word) = args.get(at) {
        match lower(word).as_str() {
            "mkstream" => make_stream = true,
            "entriesread" => {
                at += 1;
                let value = args.get(at).ok_or(SYNTAX)?;
                entries_read = Some(millis_of(value)?);
            }
            _ => return Err(SYNTAX.to_owned()),
        }
        at += 1;
    }
    Ok((make_stream, entries_read))
}

fn xgroup(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let Some(subcommand) = args.first().map(|word| lower(word)) else {
        return Reply::Error("ERR wrong number of arguments for 'xgroup' command".to_owned());
    };
    match (subcommand.as_str(), args.len()) {
        ("help", _) => Reply::Array(Some(
            [
                "XGROUP <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
                "CREATE <key> <groupname> <id|$> [option]",
                "CREATECONSUMER <key> <groupname> <consumer>",
                "DELCONSUMER <key> <groupname> <consumername>",
                "DESTROY <key> <groupname>",
                "SETID <key> <groupname> <id|$> [ENTRIESREAD entries_read]",
            ]
            .into_iter()
            .map(|line| Reply::Status(line.to_owned()))
            .collect(),
        )),
        ("create", count) if count >= 4 => {
            let (make_stream, entries_read) = match entries_read_option(&args[4..]) {
                Ok(options) => options,
                Err(error) => return Reply::Error(error),
            };
            writing(context, db, |keys| {
                let mut loaded = match load_stream(keys, &args[1], now)? {
                    Some(loaded) => loaded,
                    None if make_stream => Loaded {
                        stream: Stream::default(),
                        expires_at_ms: None,
                    },
                    None => return Err(KEY_REQUIRED.to_owned()),
                };
                if loaded.stream.groups.contains_key(&args[2]) {
                    return Err(BUSY_GROUP.to_owned());
                }
                let last_delivered = group_id(&args[3], &loaded.stream)?;
                let entries_read = entries_read.or_else(|| (last_delivered == (0, 0)).then_some(0));
                loaded.stream.groups.insert(
                    args[2].clone(),
                    ConsumerGroup {
                        last_delivered,
                        entries_read,
                        ..ConsumerGroup::default()
                    },
                );
                store_stream(keys, &args[1], loaded)?;
                Ok(Reply::ok())
            })
        }
        ("setid", count) if count >= 4 => {
            let entries_read = match entries_read_option(&args[4..]) {
                Ok((false, entries_read)) => entries_read,
                Ok((true, _)) => return Reply::Error(SYNTAX.to_owned()),
                Err(error) => return Reply::Error(error),
            };
            writing(context, db, |keys| {
                let mut loaded = load_stream(keys, &args[1], now)?.ok_or(KEY_REQUIRED)?;
                let last_delivered = group_id(&args[3], &loaded.stream)?;
                let group = loaded
                    .stream
                    .groups
                    .get_mut(&args[2])
                    .ok_or_else(|| format!("NOGROUP No such consumer group '{}' for key name '{}'", text(&args[2]), text(&args[1])))?;
                group.last_delivered = last_delivered;
                group.entries_read = entries_read;
                store_stream(keys, &args[1], loaded)?;
                Ok(Reply::ok())
            })
        }
        ("destroy", 3) => writing(context, db, |keys| {
            let mut loaded = load_stream(keys, &args[1], now)?.ok_or(KEY_REQUIRED)?;
            let removed = loaded.stream.groups.remove(&args[2]).is_some();
            if removed {
                store_stream(keys, &args[1], loaded)?;
            }
            Ok(Reply::Integer(i64::from(removed)))
        }),
        ("createconsumer" | "delconsumer", 4) => writing(context, db, |keys| {
            let mut loaded = load_stream(keys, &args[1], now)?.ok_or(KEY_REQUIRED)?;
            let group = loaded
                .stream
                .groups
                .get_mut(&args[2])
                .ok_or_else(|| format!("NOGROUP No such consumer group '{}' for key name '{}'", text(&args[2]), text(&args[1])))?;
            let reply = if subcommand == "createconsumer" {
                let created = !group.consumers.contains_key(&args[3]);
                consumer_seen(group, &args[3], now);
                Reply::Integer(i64::from(created))
            } else {
                let before = group.pending.len();
                group.pending.retain(|_, pending| pending.consumer != args[3]);
                let dropped = before - group.pending.len();
                group.consumers.remove(&args[3]);
                Reply::Integer(dropped as i64)
            };
            store_stream(keys, &args[1], loaded)?;
            Ok(reply)
        }),
        _ => Reply::Error(format!("ERR unknown subcommand or wrong number of arguments for '{subcommand}'. Try XGROUP HELP.")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupStart {
    NewEntries,
    History(StreamId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupRead {
    group: Vec<u8>,
    consumer: Vec<u8>,
    count: usize,
    no_ack: bool,
    pub(crate) block_ms: Option<u64>,
    pub(crate) keys: Vec<Vec<u8>>,
    starts: Vec<GroupStart>,
}

pub(crate) fn parse_xreadgroup(args: &[Vec<u8>]) -> Result<GroupRead, String> {
    let (Some(word), Some(group), Some(consumer)) = (args.first(), args.get(1), args.get(2)) else {
        return Err(SYNTAX.to_owned());
    };
    if lower(word) != "group" {
        return Err(SYNTAX.to_owned());
    }
    let mut at = 3;
    let mut count = usize::MAX;
    let mut block_ms = None;
    let mut no_ack = false;
    let streams_at = loop {
        let word = args.get(at).ok_or(SYNTAX)?;
        match lower(word).as_str() {
            "count" => {
                let wanted = integer(args.get(at + 1).ok_or(SYNTAX)?)?;
                count = if wanted <= 0 { usize::MAX } else { usize::try_from(wanted).unwrap_or(usize::MAX) };
                at += 2;
            }
            "block" => {
                block_ms = Some(milliseconds_timeout(args.get(at + 1).ok_or(SYNTAX)?)?);
                at += 2;
            }
            "noack" => {
                no_ack = true;
                at += 1;
            }
            "streams" => break at + 1,
            _ => return Err(SYNTAX.to_owned()),
        }
    };
    let rest = args.get(streams_at..).unwrap_or_default();
    if rest.is_empty() || !rest.len().is_multiple_of(2) {
        return Err("ERR Unbalanced 'xreadgroup' list of streams: for each stream key an ID or '>' must be specified.".to_owned());
    }
    let (keys, ids) = rest.split_at(rest.len() / 2);
    let mut starts = Vec::with_capacity(ids.len());
    for id in ids {
        starts.push(if id.as_slice() == b">" {
            GroupStart::NewEntries
        } else {
            GroupStart::History(parse_id(id, 0)?)
        });
    }
    Ok(GroupRead {
        group: group.clone(),
        consumer: consumer.clone(),
        count,
        no_ack,
        block_ms,
        keys: keys.to_vec(),
        starts,
    })
}

pub(crate) fn read_group(
    keys: &mut dyn KeysWrite,
    read: &GroupRead,
    context: &Context<'_>,
    on_wrong_type: OnWrongType,
) -> Result<Option<Reply>, String> {
    let now = context.now_ms;
    let mut streams = Vec::new();
    let mut reply_bytes = 0usize;
    let mut any_history = false;
    for (key, start) in read.keys.iter().zip(&read.starts) {
        let mut loaded = match load_stream(keys, key, now) {
            Ok(Some(loaded)) => loaded,
            Ok(None) => return Err(no_group(key, &read.group, "XREADGROUP with GROUP option")),
            Err(_) if on_wrong_type == OnWrongType::Wait => continue,
            Err(error) => return Err(error),
        };
        let Stream { entries, groups, .. } = &mut loaded.stream;
        let group = groups
            .get_mut(&read.group)
            .ok_or_else(|| no_group(key, &read.group, "XREADGROUP with GROUP option"))?;
        let mut replies = Vec::new();
        match *start {
            GroupStart::NewEntries => {
                let fresh: Vec<StreamId> = entries
                    .range((std::ops::Bound::Excluded(group.last_delivered), std::ops::Bound::Unbounded))
                    .take(read.count)
                    .map(|(id, _)| *id)
                    .collect();
                for id in &fresh {
                    let pairs = &entries[id];
                    reply_bytes = reply_bytes.saturating_add(entry_reply_bytes(*id, pairs));
                    context.check_reply_room(reply_bytes)?;
                    replies.push(entry_reply(*id, pairs));
                    group.last_delivered = *id;
                    group.entries_read = group.entries_read.map(|count| count.saturating_add(1));
                    if !read.no_ack {
                        group.pending.insert(
                            *id,
                            PendingEntry {
                                consumer: read.consumer.clone(),
                                delivered_at_ms: now,
                                deliveries: 1,
                            },
                        );
                    }
                }
                let consumer = consumer_seen(group, &read.consumer, now);
                if !fresh.is_empty() {
                    consumer.active_at_ms = now;
                }
            }
            GroupStart::History(after) => {
                any_history = true;
                consumer_seen(group, &read.consumer, now);
                let held: Vec<StreamId> = group
                    .pending
                    .range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
                    .filter(|(_, pending)| pending.consumer == read.consumer)
                    .take(read.count)
                    .map(|(id, _)| *id)
                    .collect();
                for id in &held {
                    match entries.get(id) {
                        Some(pairs) => {
                            reply_bytes = reply_bytes.saturating_add(entry_reply_bytes(*id, pairs));
                            context.check_reply_room(reply_bytes)?;
                            replies.push(entry_reply(*id, pairs));
                        }
                        None => replies.push(Reply::Array(Some(vec![Reply::bulk(id_text(*id)), Reply::Array(None)]))),
                    }
                }
            }
        }
        store_stream(keys, key, loaded)?;
        if !replies.is_empty() || matches!(start, GroupStart::History(_)) {
            reply_bytes = reply_bytes.saturating_add(bulk_reply_bytes(key));
            streams.push((Reply::bulk(key.clone()), Reply::Array(Some(replies))));
        }
    }
    Ok((!streams.is_empty() || any_history).then_some(Reply::PairArray(streams)))
}

fn xreadgroup(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let read = match parse_xreadgroup(args) {
        Ok(read) => read,
        Err(error) => return Reply::Error(error),
    };
    writing(context, db, |keys| {
        Ok(read_group(keys, &read, context, OnWrongType::Refuse)?.unwrap_or(Reply::Array(None)))
    })
}

fn xack(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let mut ids = Vec::with_capacity(args.len() - 2);
    for id in &args[2..] {
        match parse_id(id, 0) {
            Ok(id) => ids.push(id),
            Err(error) => return Reply::Error(error),
        }
    }
    writing(context, db, |keys| {
        let Some(mut loaded) = load_stream(keys, &args[0], context.now_ms)? else {
            return Ok(Reply::Integer(0));
        };
        let Some(group) = loaded.stream.groups.get_mut(&args[1]) else {
            return Ok(Reply::Integer(0));
        };
        let acknowledged = ids.iter().filter(|id| group.pending.remove(id).is_some()).count();
        if acknowledged > 0 {
            store_stream(keys, &args[0], loaded)?;
        }
        Ok(Reply::Integer(acknowledged as i64))
    })
}

fn xpending(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let mut at = 2;
    let mut min_idle = 0u64;
    if args.get(at).is_some_and(|word| lower(word) == "idle") {
        match args.get(at + 1).map(|value| millis_of(value)) {
            Some(Ok(idle)) => min_idle = idle,
            Some(Err(error)) => return Reply::Error(error),
            None => return Reply::Error(SYNTAX.to_owned()),
        }
        at += 2;
    }
    let extended = match &args[at.min(args.len())..] {
        [] if at == 2 => None,
        [start, end, count] => Some((start, end, count, None)),
        [start, end, count, consumer] => Some((start, end, count, Some(consumer))),
        _ => return Reply::Error(SYNTAX.to_owned()),
    };
    let extended = match extended {
        None => None,
        Some((start, end, count, consumer)) => {
            let (Ok(low), Ok(high)) = (bound(start, true), bound(end, false)) else {
                return Reply::Error("ERR Invalid stream ID specified as stream command argument".to_owned());
            };
            let count = match count_of(count) {
                Ok(count) => count,
                Err(error) => return Reply::Error(error),
            };
            Some((low, high, count, consumer.cloned()))
        }
    };
    reading(context, db, |keys| {
        let loaded = load_stream(keys, &args[0], now)?.ok_or_else(|| no_group(&args[0], &args[1], "XPENDING"))?;
        let group = loaded
            .stream
            .groups
            .get(&args[1])
            .ok_or_else(|| no_group(&args[0], &args[1], "XPENDING"))?;
        let Some(((low, low_open), (high, high_open), count, consumer)) = extended else {
            if group.pending.is_empty() {
                return Ok(Reply::Array(Some(vec![Reply::Integer(0), Reply::null(), Reply::null(), Reply::Array(None)])));
            }
            let mut per_consumer: Vec<(Vec<u8>, usize)> = Vec::new();
            for pending in group.pending.values() {
                match per_consumer.iter_mut().find(|(name, _)| *name == pending.consumer) {
                    Some((_, held)) => *held += 1,
                    None => per_consumer.push((pending.consumer.clone(), 1)),
                }
            }
            per_consumer.sort();
            let first = group.pending.keys().next().copied().unwrap_or_default();
            let last = group.pending.keys().next_back().copied().unwrap_or_default();
            return Ok(Reply::Array(Some(vec![
                Reply::Integer(group.pending.len() as i64),
                Reply::bulk(id_text(first)),
                Reply::bulk(id_text(last)),
                Reply::Array(Some(
                    per_consumer
                        .into_iter()
                        .map(|(name, held)| Reply::Array(Some(vec![Reply::bulk(name), Reply::bulk(held.to_string().into_bytes())])))
                        .collect(),
                )),
            ])));
        };
        let rows = group
            .pending
            .iter()
            .filter(|(id, _)| {
                (if low_open { **id > low } else { **id >= low }) && (if high_open { **id < high } else { **id <= high })
            })
            .filter(|(_, pending)| consumer.as_ref().is_none_or(|wanted| pending.consumer == *wanted))
            .filter(|(_, pending)| now.saturating_sub(pending.delivered_at_ms) >= min_idle)
            .take(count)
            .map(|(id, pending)| {
                Reply::Array(Some(vec![
                    Reply::bulk(id_text(*id)),
                    Reply::bulk(pending.consumer.clone()),
                    Reply::Integer(now.saturating_sub(pending.delivered_at_ms) as i64),
                    Reply::Integer(pending.deliveries as i64),
                ]))
            })
            .collect();
        Ok(Reply::Array(Some(rows)))
    })
}

#[derive(Default)]
struct ClaimOptions {
    idle_ms: Option<u64>,
    time_ms: Option<u64>,
    retry_count: Option<u64>,
    force: bool,
    just_id: bool,
    last_id: Option<StreamId>,
}

fn claim_one(
    group: &mut ConsumerGroup,
    entries: &std::collections::BTreeMap<StreamId, crate::value::Fields>,
    id: StreamId,
    consumer: &[u8],
    now: u64,
    options: &ClaimOptions,
) -> bool {
    if !entries.contains_key(&id) {
        group.pending.remove(&id);
        return false;
    }
    let delivered_at_ms = options
        .time_ms
        .or_else(|| options.idle_ms.map(|idle| now.saturating_sub(idle)))
        .unwrap_or(now);
    let pending = group.pending.entry(id).or_insert(PendingEntry {
        consumer: consumer.to_vec(),
        delivered_at_ms,
        deliveries: 0,
    });
    pending.consumer = consumer.to_vec();
    pending.delivered_at_ms = delivered_at_ms;
    if let Some(retry_count) = options.retry_count {
        pending.deliveries = retry_count;
    } else if !options.just_id {
        pending.deliveries = pending.deliveries.saturating_add(1);
    }
    true
}

fn xclaim(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let min_idle = match millis_of(&args[3]) {
        Ok(idle) => idle,
        Err(error) => return Reply::Error(error),
    };
    let mut ids = Vec::new();
    let mut options = ClaimOptions::default();
    let mut at = 4;
    while let Some(word) = args.get(at) {
        if let Ok(id) = parse_id(word, 0) {
            if ids.len() == at - 4 {
                ids.push(id);
                at += 1;
                continue;
            }
        }
        let value = args.get(at + 1);
        let outcome: Result<usize, String> = match lower(word).as_str() {
            "idle" => value.ok_or_else(|| SYNTAX.to_owned()).and_then(|value| millis_of(value)).map(|idle| {
                options.idle_ms = Some(idle);
                2
            }),
            "time" => value.ok_or_else(|| SYNTAX.to_owned()).and_then(|value| millis_of(value)).map(|time| {
                options.time_ms = Some(time);
                2
            }),
            "retrycount" => value.ok_or_else(|| SYNTAX.to_owned()).and_then(|value| millis_of(value)).map(|count| {
                options.retry_count = Some(count);
                2
            }),
            "lastid" => value.ok_or_else(|| SYNTAX.to_owned()).and_then(|value| parse_id(value, 0)).map(|id| {
                options.last_id = Some(id);
                2
            }),
            "force" => {
                options.force = true;
                Ok(1)
            }
            "justid" => {
                options.just_id = true;
                Ok(1)
            }
            _ => Err("ERR Unrecognized XCLAIM option".to_owned()),
        };
        match outcome {
            Ok(step) => at += step,
            Err(error) => return Reply::Error(error),
        }
    }
    if ids.is_empty() {
        return Reply::Error(SYNTAX.to_owned());
    }
    writing(context, db, |keys| {
        let mut loaded = load_stream(keys, &args[0], now)?.ok_or_else(|| no_group(&args[0], &args[1], "XCLAIM"))?;
        let Stream { entries, groups, .. } = &mut loaded.stream;
        let group = groups.get_mut(&args[1]).ok_or_else(|| no_group(&args[0], &args[1], "XCLAIM"))?;
        if let Some(last_id) = options.last_id {
            if last_id > group.last_delivered {
                group.last_delivered = last_id;
            }
        }
        let mut claimed = Vec::new();
        for id in &ids {
            let eligible = match group.pending.get(id) {
                Some(pending) => now.saturating_sub(pending.delivered_at_ms) >= min_idle,
                None => options.force && entries.contains_key(id),
            };
            if eligible && claim_one(group, entries, *id, &args[2], now, &options) {
                claimed.push(*id);
            }
        }
        let consumer = consumer_seen(group, &args[2], now);
        if !claimed.is_empty() {
            consumer.active_at_ms = now;
        }
        let replies = claimed
            .iter()
            .map(|id| {
                if options.just_id {
                    Reply::bulk(id_text(*id))
                } else {
                    entry_reply(*id, &entries[id])
                }
            })
            .collect();
        store_stream(keys, &args[0], loaded)?;
        Ok(Reply::Array(Some(replies)))
    })
}

fn xautoclaim(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let min_idle = match millis_of(&args[3]) {
        Ok(idle) => idle,
        Err(error) => return Reply::Error(error),
    };
    let start = match bound(&args[4], true) {
        Ok((id, _)) => id,
        Err(error) => return Reply::Error(error),
    };
    let mut count = AUTOCLAIM_DEFAULT_COUNT;
    let mut just_id = false;
    let mut at = 5;
    while let Some(word) = args.get(at) {
        match lower(word).as_str() {
            "count" => match args.get(at + 1).map(|value| count_of(value)) {
                Some(Ok(wanted)) if wanted > 0 => {
                    count = wanted;
                    at += 2;
                }
                Some(Err(error)) => return Reply::Error(error),
                _ => return Reply::Error("ERR COUNT must be > 0".to_owned()),
            },
            "justid" => {
                just_id = true;
                at += 1;
            }
            _ => return Reply::Error(SYNTAX.to_owned()),
        }
    }
    let options = ClaimOptions {
        just_id,
        ..ClaimOptions::default()
    };
    writing(context, db, |keys| {
        let mut loaded = load_stream(keys, &args[0], now)?.ok_or_else(|| no_group(&args[0], &args[1], "XAUTOCLAIM"))?;
        let Stream { entries, groups, .. } = &mut loaded.stream;
        let group = groups.get_mut(&args[1]).ok_or_else(|| no_group(&args[0], &args[1], "XAUTOCLAIM"))?;
        let scan: Vec<StreamId> = group
            .pending
            .range(start..)
            .take(count.saturating_mul(AUTOCLAIM_SCANS_PER_CLAIM))
            .map(|(id, _)| *id)
            .collect();
        let mut claimed = Vec::new();
        let mut deleted = Vec::new();
        let mut next = None;
        for id in &scan {
            if claimed.len() >= count {
                next = Some(*id);
                break;
            }
            let idle_enough = group
                .pending
                .get(id)
                .is_some_and(|pending| now.saturating_sub(pending.delivered_at_ms) >= min_idle);
            if !idle_enough {
                continue;
            }
            if claim_one(group, entries, *id, &args[2], now, &options) {
                claimed.push(*id);
            } else {
                deleted.push(*id);
            }
        }
        let next = next
            .or_else(|| {
                scan.last().and_then(|last| {
                    group
                        .pending
                        .range((std::ops::Bound::Excluded(*last), std::ops::Bound::Unbounded))
                        .next()
                        .map(|(id, _)| *id)
                })
            })
            .unwrap_or((0, 0));
        let consumer = consumer_seen(group, &args[2], now);
        if !claimed.is_empty() {
            consumer.active_at_ms = now;
        }
        let claimed_replies = claimed
            .iter()
            .map(|id| if just_id { Reply::bulk(id_text(*id)) } else { entry_reply(*id, &entries[id]) })
            .collect();
        let deleted_replies = deleted.iter().map(|id| Reply::bulk(id_text(*id))).collect();
        store_stream(keys, &args[0], loaded)?;
        Ok(Reply::Array(Some(vec![
            Reply::bulk(id_text(next)),
            Reply::Array(Some(claimed_replies)),
            Reply::Array(Some(deleted_replies)),
        ])))
    })
}

fn entry_or_null(stream: &Stream, last: bool) -> Reply {
    let found = if last { stream.entries.iter().next_back() } else { stream.entries.iter().next() };
    found.map_or_else(Reply::null, |(id, pairs)| entry_reply(*id, pairs))
}

fn lag(stream: &Stream, group: &ConsumerGroup) -> Reply {
    if group.entries_read.is_none() {
        return Reply::null();
    }
    Reply::Integer(
        stream
            .entries
            .range((std::ops::Bound::Excluded(group.last_delivered), std::ops::Bound::Unbounded))
            .count() as i64,
    )
}

fn field(name: &str, value: Reply) -> (Reply, Reply) {
    (Reply::bulk(name.as_bytes().to_vec()), value)
}

fn xinfo(context: &Context<'_>, db: u32, args: &[Vec<u8>]) -> Reply {
    let now = context.now_ms;
    let subcommand = lower(&args[0]);
    if subcommand == "help" {
        return Reply::Array(Some(
            [
                "XINFO <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
                "CONSUMERS <key> <groupname>",
                "GROUPS <key>",
                "STREAM <key> [FULL [COUNT <count>]]",
            ]
            .into_iter()
            .map(|line| Reply::Status(line.to_owned()))
            .collect(),
        ));
    }
    let Some(key) = args.get(1) else {
        return Reply::Error(format!("ERR wrong number of arguments for 'xinfo|{subcommand}' command"));
    };
    reading(context, db, |keys| {
        let loaded = load_stream(keys, key, now)?.ok_or_else(|| "ERR no such key".to_owned())?;
        let stream = &loaded.stream;
        match (subcommand.as_str(), args.len()) {
            ("stream", _) => Ok(Reply::Map(vec![
                field("length", Reply::Integer(stream.entries.len() as i64)),
                field("radix-tree-keys", Reply::Integer(i64::from(!stream.entries.is_empty()))),
                field("radix-tree-nodes", Reply::Integer(1)),
                field("last-generated-id", Reply::bulk(id_text(stream.last))),
                field("max-deleted-entry-id", Reply::bulk(id_text((0, 0)))),
                field("entries-added", Reply::Integer(stream.entries.len() as i64)),
                field(
                    "recorded-first-entry-id",
                    Reply::bulk(id_text(stream.entries.keys().next().copied().unwrap_or_default())),
                ),
                field("groups", Reply::Integer(stream.groups.len() as i64)),
                field("first-entry", entry_or_null(stream, false)),
                field("last-entry", entry_or_null(stream, true)),
            ])),
            ("groups", 2) => Ok(Reply::Array(Some(
                stream
                    .groups
                    .iter()
                    .map(|(name, group)| {
                        Reply::Map(vec![
                            field("name", Reply::bulk(name.clone())),
                            field("consumers", Reply::Integer(group.consumers.len() as i64)),
                            field("pending", Reply::Integer(group.pending.len() as i64)),
                            field("last-delivered-id", Reply::bulk(id_text(group.last_delivered))),
                            field(
                                "entries-read",
                                group.entries_read.map_or_else(Reply::null, |count| Reply::Integer(count as i64)),
                            ),
                            field("lag", lag(stream, group)),
                        ])
                    })
                    .collect(),
            ))),
            ("consumers", 3) => {
                let group = stream
                    .groups
                    .get(&args[2])
                    .ok_or_else(|| format!("NOGROUP No such consumer group '{}' for key name '{}'", text(&args[2]), text(key)))?;
                Ok(Reply::Array(Some(
                    group
                        .consumers
                        .iter()
                        .map(|(name, consumer)| {
                            let pending = group.pending.values().filter(|pending| pending.consumer == *name).count();
                            let inactive = if consumer.active_at_ms == 0 {
                                -1
                            } else {
                                now.saturating_sub(consumer.active_at_ms) as i64
                            };
                            Reply::Map(vec![
                                field("name", Reply::bulk(name.clone())),
                                field("pending", Reply::Integer(pending as i64)),
                                field("idle", Reply::Integer(now.saturating_sub(consumer.seen_at_ms) as i64)),
                                field("inactive", Reply::Integer(inactive)),
                            ])
                        })
                        .collect(),
                )))
            }
            _ => Err(format!("ERR unknown subcommand or wrong number of arguments for '{subcommand}'. Try XINFO HELP.")),
        }
    })
}

pub(crate) fn execute(context: &Context<'_>, db: u32, name: &str, args: &[Vec<u8>]) -> Option<Reply> {
    Some(match (name, args.len()) {
        ("xgroup", count) if count >= 1 => xgroup(context, db, args),
        ("xreadgroup", count) if count >= 6 => xreadgroup(context, db, args),
        ("xack", count) if count >= 3 => xack(context, db, args),
        ("xpending", count) if count >= 2 => xpending(context, db, args),
        ("xclaim", count) if count >= 5 => xclaim(context, db, args),
        ("xautoclaim", count) if count >= 5 => xautoclaim(context, db, args),
        ("xinfo", count) if count >= 1 => xinfo(context, db, args),
        _ => return None,
    })
}
