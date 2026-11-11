use crate::groups::Coordinator;
use crate::log::{self, error};
use crate::wire::{Parsed, Reader, Writer};
use eggdb_keyspace::Keyspace;
use eggdb_platform::connection_limits::MemoryBudget;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
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
const SASL_HANDSHAKE: i16 = 17;
const API_VERSIONS: i16 = 18;
const SASL_AUTHENTICATE: i16 = 36;
const UNSUPPORTED_SASL_MECHANISM: i16 = 33;
const INVALID_CONFIG: i16 = 40;
const ILLEGAL_SASL_STATE: i16 = 34;
const SASL_AUTHENTICATION_FAILED: i16 = 58;
const CREATE_TOPICS: i16 = 19;
const DELETE_TOPICS: i16 = 20;
const UNSUPPORTED_VERSION: i16 = 35;
const OFFSET_COMMIT: i16 = 8;
const OFFSET_FETCH: i16 = 9;
const FIND_COORDINATOR: i16 = 10;
const JOIN_GROUP: i16 = 11;
const HEARTBEAT: i16 = 12;
const LEAVE_GROUP: i16 = 13;
const SYNC_GROUP: i16 = 14;
const DESCRIBE_GROUPS: i16 = 15;
const LIST_GROUPS: i16 = 16;
const DELETE_GROUPS: i16 = 42;
const INIT_PRODUCER_ID: i16 = 22;
const INVALID_REQUEST: i16 = 42;
const UNKNOWN_SERVER_ERROR: i16 = -1;

const SUPPORTED: [(i16, i16, i16); 20] = [
    (INIT_PRODUCER_ID, 0, 1),
    (DESCRIBE_GROUPS, 0, 1),
    (LIST_GROUPS, 0, 2),
    (DELETE_GROUPS, 0, 1),
    (OFFSET_COMMIT, 2, 7),
    (OFFSET_FETCH, 1, 3),
    (FIND_COORDINATOR, 0, 2),
    (JOIN_GROUP, 0, 5),
    (HEARTBEAT, 0, 3),
    (LEAVE_GROUP, 0, 3),
    (SYNC_GROUP, 0, 3),
    (SASL_HANDSHAKE, 1, 1),
    (SASL_AUTHENTICATE, 0, 1),
    (PRODUCE, 3, 3),
    (FETCH, 4, 4),
    (LIST_OFFSETS, 1, 1),
    (METADATA, 0, 1),
    (API_VERSIONS, 0, 2),
    (CREATE_TOPICS, 0, 1),
    (DELETE_TOPICS, 0, 1),
];

struct CreatedTopic {
    name: String,
    partitions: i32,
    replication: i16,
    assigned: bool,
    retention: Option<log::Retention>,
}

fn create_topics(
    keyspace: &dyn Keyspace,
    version: i16,
    request: &mut Reader<'_>,
    body: &mut Writer,
) -> Parsed<()> {
    let wanted = request.array(|reader| {
        let name = reader.string()?.unwrap_or_default();
        let partitions = reader.i32()?;
        let replication = reader.i16()?;
        let assignments = reader.array(|assignment| {
            let _partition = assignment.i32()?;
            assignment.array(|broker| broker.i32())
        })?;
        let configs = reader.array(|config| Ok((config.string()?, config.string()?)))?;
        let mut retention = Some(log::Retention::UNLIMITED);
        for (key, value) in configs {
            let parsed = value.as_deref().and_then(|value| value.parse::<i64>().ok());
            match (key.as_deref(), parsed) {
                (Some("retention.ms"), Some(ms)) if ms >= -1 => {
                    if let Some(kept) = retention.as_mut() {
                        kept.ms = ms;
                    }
                }
                (Some("retention.bytes"), Some(bytes)) if bytes >= -1 => {
                    if let Some(kept) = retention.as_mut() {
                        kept.bytes = bytes;
                    }
                }
                (Some("retention.ms" | "retention.bytes"), _) => retention = None,
                _ => {}
            }
        }
        Ok(CreatedTopic {
            name,
            partitions,
            replication,
            assigned: !assignments.is_empty(),
            retention,
        })
    })?;
    let _timeout = request.i32()?;
    let validate_only = version >= 1 && request.i8()? != 0;
    body.count(wanted.len());
    for topic in wanted {
        let (code, message) = if topic.assigned {
            (
                log::error::INVALID_PARTITIONS,
                "replica assignments are not supported: EggDB is one broker",
            )
        } else if topic.replication != -1 && topic.replication != 1 {
            (
                log::error::INVALID_REPLICATION_FACTOR,
                "the replication factor must be 1: EggDB is one broker",
            )
        } else if topic.retention.is_none() {
            (
                INVALID_CONFIG,
                "retention.ms and retention.bytes take a whole number, -1 for no limit",
            )
        } else {
            let retention = topic.retention.unwrap_or(log::Retention::UNLIMITED);
            let code = log::create_topic(
                keyspace,
                &topic.name,
                topic.partitions,
                retention,
                validate_only,
            )?;
            let message = match code {
                log::error::TOPIC_ALREADY_EXISTS => "the topic already exists",
                log::error::INVALID_TOPIC => "the topic name is not valid",
                log::error::INVALID_PARTITIONS => "the partition count must be 1 to 1024",
                _ => "",
            };
            (code, message)
        };
        body.string(Some(&topic.name)).i16(code);
        if version >= 1 {
            body.string((code != log::error::NONE).then_some(message));
        }
    }
    Ok(())
}

fn delete_topics(
    keyspace: &dyn Keyspace,
    version: i16,
    request: &mut Reader<'_>,
    body: &mut Writer,
) -> Parsed<()> {
    let names = request.array(|reader| Ok(reader.string()?.unwrap_or_default()))?;
    let _timeout = request.i32()?;
    if version >= 1 {
        body.i32(0);
    }
    body.count(names.len());
    for name in names {
        let code = log::delete_topic(keyspace, &name)?;
        body.string(Some(&name)).i16(code);
    }
    Ok(())
}

/// How the Kafka-compatible server runs.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The host and port clients reach this broker at, as Metadata reports it.
    pub advertised: SocketAddr,
    /// The most connections served at once.
    pub max_connections: usize,
    /// The memory every connection's requests and responses hold together, shared with the
    /// server's other listeners (D284, D307).
    pub memory_budget: Arc<MemoryBudget>,
    /// The user and the SHA-256 of the password clients authenticate with over SASL/PLAIN;
    /// `None` serves clients without authentication.
    pub credentials: Option<(String, [u8; 32])>,
}

/// Where one connection stands in its SASL exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Login {
    Waiting,
    Handshaken,
    Done,
}

fn sasl_handshake(request: &mut Reader<'_>, login: &mut Login, body: &mut Writer) -> Parsed<()> {
    let mechanism = request.string()?.unwrap_or_default();
    let code = if mechanism == "PLAIN" {
        if *login == Login::Waiting {
            *login = Login::Handshaken;
        }
        error::NONE
    } else {
        UNSUPPORTED_SASL_MECHANISM
    };
    body.i16(code).count(1).string(Some("PLAIN"));
    Ok(())
}

fn sasl_authenticate(
    config: &ServerConfig,
    version: i16,
    request: &mut Reader<'_>,
    login: &mut Login,
    body: &mut Writer,
) -> Parsed<()> {
    let token = request.bytes()?.unwrap_or_default();
    let (code, message) = if *login != Login::Handshaken {
        (ILLEGAL_SASL_STATE, "SaslHandshake with PLAIN comes first")
    } else {
        let mut parts = token.split(|byte| *byte == 0);
        let (_identity, user, password) = (parts.next(), parts.next(), parts.next());
        let accepted = match (&config.credentials, user, password, parts.next()) {
            (Some((expected, password_sha256)), Some(user), Some(password), None) => {
                eggdb_crypto::constant_time_eq(user, expected.as_bytes())
                    && eggdb_crypto::constant_time_eq(
                        &eggdb_crypto::sha256(password),
                        password_sha256,
                    )
            }
            _ => false,
        };
        if accepted {
            *login = Login::Done;
            (error::NONE, "")
        } else {
            (
                SASL_AUTHENTICATION_FAILED,
                "Authentication failed: Invalid username or password",
            )
        }
    };
    body.i16(code)
        .string((code != error::NONE).then_some(message))
        .bytes(&[]);
    if version >= 1 {
        body.i64(0);
    }
    Ok(())
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

fn api_versions(version: i16, body: &mut Writer) {
    body.i16(if (0..=2).contains(&version) {
        error::NONE
    } else {
        UNSUPPORTED_VERSION
    });
    body.count(SUPPORTED.len());
    for (key, min, max) in SUPPORTED {
        body.i16(key).i16(min).i16(max);
    }
    if (1..=2).contains(&version) {
        body.i32(0);
    }
}

fn metadata(
    keyspace: &dyn Keyspace,
    config: &ServerConfig,
    version: i16,
    request: &mut Reader<'_>,
    body: &mut Writer,
) -> Parsed<()> {
    let wanted = request.array(|reader| Ok(reader.string()?.unwrap_or_default()))?;
    let all = log::topics(keyspace)?;
    let listed: Vec<(String, Option<i32>)> = if wanted.is_empty() {
        all.into_iter()
            .map(|(name, partitions)| (name, Some(partitions)))
            .collect()
    } else {
        wanted
            .into_iter()
            .map(|name| {
                let partitions = log::partitions(keyspace, &name, Some(DEFAULT_PARTITIONS))?;
                Ok((name, partitions))
            })
            .collect::<Parsed<_>>()?
    };
    body.count(1)
        .i32(BROKER_ID)
        .string(Some(&config.advertised.ip().to_string()))
        .i32(i32::from(config.advertised.port()));
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
            body.i16(error::NONE)
                .i32(partition)
                .i32(BROKER_ID)
                .count(1)
                .i32(BROKER_ID)
                .count(1)
                .i32(BROKER_ID);
        }
    }
    Ok(())
}

fn partition_exists(keyspace: &dyn Keyspace, topic: &str, partition: i32) -> Parsed<bool> {
    Ok(
        log::partitions(keyspace, topic, None)?
            .is_some_and(|count| (0..count).contains(&partition)),
    )
}

fn produce(keyspace: &dyn Keyspace, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<bool> {
    let _transactional_id = request.string()?;
    let acks = request.i16()?;
    let _timeout = request.i32()?;
    let topics = request.array(|reader| {
        let name = reader.string()?.unwrap_or_default();
        let partitions = reader.array(|reader| {
            Ok((
                reader.i32()?,
                reader.bytes()?.map(<[u8]>::to_vec).unwrap_or_default(),
            ))
        })?;
        Ok((name, partitions))
    })?;
    body.count(topics.len());
    for (name, partitions) in topics {
        if log::partitions(keyspace, &name, Some(DEFAULT_PARTITIONS))?.is_none() {
            body.string(Some(&name)).count(partitions.len());
            for (partition, _) in partitions {
                body.i32(partition)
                    .i16(error::INVALID_TOPIC)
                    .i64(-1)
                    .i64(-1);
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

fn init_producer_id(keyspace: &dyn Keyspace, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
    let transactional_id = request.string()?;
    let _transaction_timeout = request.i32()?;
    body.i32(0);
    if transactional_id.is_some() {
        body.i16(INVALID_REQUEST).i64(-1).i16(-1);
        return Ok(());
    }
    match log::init_producer(keyspace) {
        Ok(id) => body.i16(error::NONE).i64(id).i16(0),
        Err(_) => body.i16(UNKNOWN_SERVER_ERROR).i64(-1).i16(-1),
    };
    Ok(())
}

fn fetch(keyspace: &dyn Keyspace, request: &mut Reader<'_>, body: &mut Writer) -> Parsed<()> {
    let _replica = request.i32()?;
    let _max_wait = request.i32()?;
    let _min_bytes = request.i32()?;
    let max_bytes = usize::try_from(request.i32()?)
        .unwrap_or(0)
        .clamp(1, MAX_FETCH_BYTES);
    let _isolation = request.i8()?;
    let topics = request.array(|reader| {
        let name = reader.string()?.unwrap_or_default();
        let partitions =
            reader.array(|reader| Ok((reader.i32()?, reader.i64()?, reader.i32()?)))?;
        Ok((name, partitions))
    })?;
    body.i32(0).count(topics.len());
    let mut bytes_left = max_bytes;
    for (name, partitions) in topics {
        body.string(Some(&name)).count(partitions.len());
        for (partition, offset, partition_max) in partitions {
            if !partition_exists(keyspace, &name, partition)? {
                body.i32(partition)
                    .i16(error::UNKNOWN_TOPIC_OR_PARTITION)
                    .i64(-1)
                    .i64(-1)
                    .i32(-1)
                    .bytes(&[]);
                continue;
            }
            let (earliest, high) = log::offsets(keyspace, &name, partition)?;
            if offset < earliest || offset > high {
                body.i32(partition)
                    .i16(error::OFFSET_OUT_OF_RANGE)
                    .i64(high)
                    .i64(high)
                    .i32(-1)
                    .bytes(&[]);
                continue;
            }
            let limit = usize::try_from(partition_max)
                .unwrap_or(0)
                .clamp(1, max_bytes)
                .min(bytes_left);
            let (records, high) = if limit == 0 {
                (Vec::new(), high)
            } else {
                log::fetch(keyspace, &name, partition, offset, limit)?
            };
            bytes_left = bytes_left.saturating_sub(records.len());
            body.i32(partition)
                .i16(error::NONE)
                .i64(high)
                .i64(high)
                .i32(-1)
                .bytes(&records);
        }
    }
    Ok(())
}

fn list_offsets(
    keyspace: &dyn Keyspace,
    request: &mut Reader<'_>,
    body: &mut Writer,
) -> Parsed<()> {
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
                body.i32(partition)
                    .i16(error::UNKNOWN_TOPIC_OR_PARTITION)
                    .i64(-1)
                    .i64(-1);
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
pub(crate) fn answer(
    keyspace: &dyn Keyspace,
    config: &ServerConfig,
    frame: &[u8],
) -> Option<Vec<u8>> {
    let mut login = Login::Done;
    answer_for(keyspace, config, &Coordinator::default(), frame, &mut login)
}

pub(crate) fn answer_for(
    keyspace: &dyn Keyspace,
    config: &ServerConfig,
    coordinator: &Coordinator,
    frame: &[u8],
    login: &mut Login,
) -> Option<Vec<u8>> {
    let mut request = Reader::new(frame);
    let key = request.i16().ok()?;
    let version = request.i16().ok()?;
    let correlation = request.i32().ok()?;
    let _client = request.string().ok()?;
    let mut body = Writer::default();
    body.i32(correlation);
    let supported = SUPPORTED
        .iter()
        .any(|(known, min, max)| *known == key && (*min..=*max).contains(&version));
    let respond = if key == API_VERSIONS {
        api_versions(version, &mut body);
        true
    } else if !supported {
        return None;
    } else if key == SASL_HANDSHAKE {
        sasl_handshake(&mut request, login, &mut body).ok()?;
        true
    } else if key == SASL_AUTHENTICATE {
        sasl_authenticate(config, version, &mut request, login, &mut body).ok()?;
        true
    } else if *login != Login::Done {
        return None;
    } else {
        let outcome = match key {
            METADATA => metadata(keyspace, config, version, &mut request, &mut body).map(|()| true),
            PRODUCE => produce(keyspace, &mut request, &mut body),
            FETCH => fetch(keyspace, &mut request, &mut body).map(|()| true),
            CREATE_TOPICS => {
                create_topics(keyspace, version, &mut request, &mut body).map(|()| true)
            }
            DELETE_TOPICS => {
                delete_topics(keyspace, version, &mut request, &mut body).map(|()| true)
            }
            FIND_COORDINATOR => Coordinator::find_coordinator(
                version,
                &mut request,
                &config.advertised.ip().to_string(),
                i32::from(config.advertised.port()),
                &mut body,
            )
            .map(|()| true),
            JOIN_GROUP => coordinator.join_group(version, &mut request, &mut body).map(|()| true),
            SYNC_GROUP => coordinator.sync_group(version, &mut request, &mut body).map(|()| true),
            HEARTBEAT => coordinator.heartbeat(version, &mut request, &mut body).map(|()| true),
            LEAVE_GROUP => coordinator.leave_group(version, &mut request, &mut body).map(|()| true),
            INIT_PRODUCER_ID => init_producer_id(keyspace, &mut request, &mut body).map(|()| true),
            DESCRIBE_GROUPS => coordinator
                .describe_groups(version, &mut request, &mut body)
                .map(|()| true),
            LIST_GROUPS => coordinator.list_groups(version, &mut body).map(|()| true),
            DELETE_GROUPS => coordinator
                .delete_groups(keyspace, version, &mut request, &mut body)
                .map(|()| true),
            OFFSET_COMMIT => coordinator
                .offset_commit(keyspace, version, &mut request, &mut body)
                .map(|()| true),
            OFFSET_FETCH => Coordinator::offset_fetch(keyspace, version, &mut request, &mut body).map(|()| true),
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

pub(crate) fn serve_stream(
    keyspace: &dyn Keyspace,
    config: &ServerConfig,
    coordinator: &Coordinator,
    mut reader: impl Read,
    writer: &mut impl Write,
) -> io::Result<()> {
    let mut held = config.memory_budget.empty_reservation();
    let mut login = if config.credentials.is_some() {
        Login::Waiting
    } else {
        Login::Done
    };
    loop {
        let mut size = [0u8; 4];
        match reader.read_exact(&mut size) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        }
        let Some(length) = usize::try_from(i32::from_be_bytes(size))
            .ok()
            .filter(|length| *length <= MAX_REQUEST_BYTES)
        else {
            return Ok(());
        };
        if !held.hold_if_large(length) {
            return Ok(());
        }
        let mut frame = vec![0u8; length];
        reader.read_exact(&mut frame)?;
        let answered = answer_for(keyspace, config, coordinator, &frame, &mut login);
        drop(frame);
        let Some(response) = answered else {
            return Ok(());
        };
        if !held.hold_if_large(response.len()) {
            return Ok(());
        }
        writer.write_all(&response)?;
        drop(response);
        held.resize(0);
    }
}

/// Serves Kafka clients on `listener` from `keyspace` until `stop` turns true.
pub fn serve(
    keyspace: &(dyn Keyspace + Sync),
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let active = AtomicUsize::new(0);
    let coordinator = Coordinator::default();
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
                continue;
            }
            let (active, coordinator) = (&active, &coordinator);
            let spawned = std::thread::Builder::new()
                .name("eggdb-kafka".to_owned())
                .spawn_scoped(scope, move || {
                    if let Ok(mut writer) = stream.try_clone() {
                        let _ =
                            serve_stream(keyspace, config, coordinator, StopAware { stream, stop }, &mut writer);
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
