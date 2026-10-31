use crate::log::{self, error};
use crate::wire::{Parsed, Reader, Writer};
use eggdb_keyspace::Keyspace;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const MAX_REQUEST_BYTES: usize = 64 << 20;
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(200);
const BROKER_ID: i32 = 0;
const DEFAULT_PARTITIONS: i32 = 1;
const MAX_FETCH_BYTES: usize = 16 << 20;

const PRODUCE: i16 = 0;
const FETCH: i16 = 1;
const LIST_OFFSETS: i16 = 2;
const METADATA: i16 = 3;
const API_VERSIONS: i16 = 18;
const UNSUPPORTED_VERSION: i16 = 35;

const SUPPORTED: [(i16, i16, i16); 5] = [(PRODUCE, 3, 3), (FETCH, 4, 4), (LIST_OFFSETS, 1, 1), (METADATA, 0, 1), (API_VERSIONS, 0, 2)];

/// How the Kafka-compatible server runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// The host and port clients reach this broker at, as Metadata reports it.
    pub advertised: SocketAddr,
    /// The most connections served at once.
    pub max_connections: usize,
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

fn api_versions(version: i16, body: &mut Writer) {
    body.i16(if (0..=2).contains(&version) { error::NONE } else { UNSUPPORTED_VERSION });
    body.count(SUPPORTED.len());
    for (key, min, max) in SUPPORTED {
        body.i16(key).i16(min).i16(max);
    }
    if version >= 1 && version <= 2 {
        body.i32(0);
    }
}

fn metadata(keyspace: &dyn Keyspace, config: &ServerConfig, version: i16, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
    let wanted = request.array(|reader| Ok(reader.string()?.unwrap_or_default()))?;
    let all = log::topics(keyspace)?;
    let listed: Vec<(String, Option<i32>)> = if wanted.is_empty() {
        all.into_iter().map(|(name, partitions)| (name, Some(partitions))).collect()
    } else {
        wanted
            .into_iter()
            .map(|name| {
                let partitions = log::partitions(keyspace, &name, Some(DEFAULT_PARTITIONS))?;
                Ok((name, partitions))
            })
            .collect::<Parsed<_>>()?
    };
    body.count(1).i32(BROKER_ID).string(Some(&config.advertised.ip().to_string())).i32(i32::from(config.advertised.port()));
    if version >= 1 {
        body.string(None);
        body.i32(BROKER_ID);
    }
    body.count(listed.len());
    for (name, partitions) in listed {
        let code = match partitions {
            Some(_) => error::NONE,
            None if !log::valid_topic(&name) => error::INVALID_TOPIC,
            None => error::UNKNOWN_TOPIC_OR_PARTITION,
        };
        body.i16(code).string(Some(&name));
        if version >= 1 {
            body.i8(0);
        }
        let count = partitions.unwrap_or(0).max(0);
        body.count(count as usize);
        for partition in 0..count {
            body.i16(error::NONE).i32(partition).i32(BROKER_ID).count(1).i32(BROKER_ID).count(1).i32(BROKER_ID);
        }
    }
    Ok(())
}

fn partition_exists(keyspace: &dyn Keyspace, topic: &str, partition: i32) -> Parsed<bool> {
    Ok(log::partitions(keyspace, topic, None)?.is_some_and(|count| (0..count).contains(&partition)))
}

fn produce(keyspace: &dyn Keyspace, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<bool> {
    let _transactional_id = request.string()?;
    let acks = request.i16()?;
    let _timeout = request.i32()?;
    let topics = request.array(|reader| {
        let name = reader.string()?.unwrap_or_default();
        let partitions = reader.array(|reader| Ok((reader.i32()?, reader.bytes()?.map(<[u8]>::to_vec).unwrap_or_default())))?;
        Ok((name, partitions))
    })?;
    body.count(topics.len());
    for (name, partitions) in topics {
        if log::partitions(keyspace, &name, Some(DEFAULT_PARTITIONS))?.is_none() {
            body.string(Some(&name)).count(partitions.len());
            for (partition, _) in partitions {
                body.i32(partition).i16(error::INVALID_TOPIC).i64(-1).i64(-1);
            }
            continue;
        }
        body.string(Some(&name)).count(partitions.len());
        for (partition, records) in partitions {
            let outcome = if partition_exists(keyspace, &name, partition)? {
                log::append(keyspace, &name, partition, &records)?
            } else {
                Err(error::UNKNOWN_TOPIC_OR_PARTITION)
            };
            match outcome {
                Ok(base) => body.i32(partition).i16(error::NONE).i64(base).i64(-1),
                Err(code) => body.i32(partition).i16(code).i64(-1).i64(-1),
            };
        }
    }
    body.i32(0);
    Ok(acks != 0)
}

fn fetch(keyspace: &dyn Keyspace, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
    let _replica = request.i32()?;
    let _max_wait = request.i32()?;
    let _min_bytes = request.i32()?;
    let max_bytes = usize::try_from(request.i32()?).unwrap_or(0).clamp(1, MAX_FETCH_BYTES);
    let _isolation = request.i8()?;
    let topics = request.array(|reader| {
        let name = reader.string()?.unwrap_or_default();
        let partitions = reader.array(|reader| Ok((reader.i32()?, reader.i64()?, reader.i32()?)))?;
        Ok((name, partitions))
    })?;
    body.i32(0).count(topics.len());
    for (name, partitions) in topics {
        body.string(Some(&name)).count(partitions.len());
        for (partition, offset, partition_max) in partitions {
            if !partition_exists(keyspace, &name, partition)? {
                body.i32(partition).i16(error::UNKNOWN_TOPIC_OR_PARTITION).i64(-1).i64(-1).i32(-1).bytes(&[]);
                continue;
            }
            let (earliest, high) = log::offsets(keyspace, &name, partition)?;
            if offset < earliest || offset > high {
                body.i32(partition).i16(error::OFFSET_OUT_OF_RANGE).i64(high).i64(high).i32(-1).bytes(&[]);
                continue;
            }
            let limit = usize::try_from(partition_max).unwrap_or(0).clamp(1, max_bytes);
            let (records, high) = log::fetch(keyspace, &name, partition, offset, limit)?;
            body.i32(partition).i16(error::NONE).i64(high).i64(high).i32(-1).bytes(&records);
        }
    }
    Ok(())
}

fn list_offsets(keyspace: &dyn Keyspace, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
    let _replica = request.i32()?;
    let topics = request.array(|reader| {
        let name = reader.string()?.unwrap_or_default();
        let partitions = reader.array(|reader| Ok((reader.i32()?, reader.i64()?)))?;
        Ok((name, partitions))
    })?;
    body.count(topics.len());
    for (name, partitions) in topics {
        body.string(Some(&name)).count(partitions.len());
        for (partition, timestamp) in partitions {
            if !partition_exists(keyspace, &name, partition)? {
                body.i32(partition).i16(error::UNKNOWN_TOPIC_OR_PARTITION).i64(-1).i64(-1);
                continue;
            }
            let (earliest, high) = log::offsets(keyspace, &name, partition)?;
            let offset = if timestamp == -2 { earliest } else { high };
            body.i32(partition).i16(error::NONE).i64(-1).i64(offset);
        }
    }
    Ok(())
}

/// Answers one request frame (without its size prefix); returns the response frame, or `None`
/// when the request asks for no answer (produce with `acks = 0`) or cannot be read.
pub(crate) fn answer(keyspace: &dyn Keyspace, config: &ServerConfig, frame: &[u8]) -> Option<Vec<u8>> {
    let mut request = Reader::new(frame);
    let key = request.i16().ok()?;
    let version = request.i16().ok()?;
    let correlation = request.i32().ok()?;
    let _client = request.string().ok()?;
    let mut body = Writer::default();
    body.i32(correlation);
    let supported = SUPPORTED.iter().any(|(known, min, max)| *known == key && (*min..=*max).contains(&version));
    let respond = if key == API_VERSIONS {
        api_versions(version, &mut body);
        true
    } else if !supported {
        return None;
    } else {
        let outcome = match key {
            METADATA => metadata(keyspace, config, version, &mut request, &mut body).map(|()| true),
            PRODUCE => produce(keyspace, &mut request, &mut body),
            FETCH => fetch(keyspace, &mut request, &mut body).map(|()| true),
            _ => list_offsets(keyspace, &mut request, &mut body).map(|()| true),
        };
        outcome.ok()?
    };
    if !respond {
        return Some(Vec::new());
    }
    let mut framed = (body.out.len() as i32).to_be_bytes().to_vec();
    framed.extend_from_slice(&body.out);
    Some(framed)
}

fn serve_stream(keyspace: &dyn Keyspace, config: &ServerConfig, mut reader: impl Read, writer: &mut impl Write) -> io::Result<()> {
    loop {
        let mut size = [0u8; 4];
        match reader.read_exact(&mut size) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        }
        let Some(length) = usize::try_from(i32::from_be_bytes(size)).ok().filter(|length| *length <= MAX_REQUEST_BYTES) else {
            return Ok(());
        };
        let mut frame = vec![0u8; length];
        reader.read_exact(&mut frame)?;
        match answer(keyspace, config, &frame) {
            Some(response) => writer.write_all(&response)?,
            None => return Ok(()),
        }
    }
}

/// Serves Kafka clients on `listener` from `keyspace` until `stop` turns true.
pub fn serve(keyspace: &(dyn Keyspace + Sync), listener: &TcpListener, config: &ServerConfig, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
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
                continue;
            }
            let active = &active;
            let spawned = std::thread::Builder::new().name("eggdb-kafka".to_owned()).spawn_scoped(scope, move || {
                if let Ok(mut writer) = stream.try_clone() {
                    let _ = serve_stream(keyspace, config, StopAware { stream, stop }, &mut writer);
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
