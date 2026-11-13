use super::serve_failover::{NodeStatus, ReplicationNode};
use eggdb::{Database, DatabaseApply, Replica, ReplicaId, ReplicationBatch, ReplicationPrimary, ReplicationRefusal};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const CHALLENGE: u8 = 1;
const HELLO: u8 = 2;
const WELCOME: u8 = 3;
const BATCH: u8 = 4;
const ACK: u8 = 5;
const REFUSED: u8 = 6;
const HEARTBEAT: u8 = 7;
const SNAPSHOT_CHUNK: u8 = 8;
const SNAPSHOT_END: u8 = 9;
const STATUS_REQUEST: u8 = 10;
const STATUS: u8 = 11;
const PROMOTE: u8 = 12;
const FOLLOW: u8 = 13;
const DONE: u8 = 14;
pub(super) const SHARD_COPY: u8 = 15;
const SHARD_PAGE: u8 = 16;
const SHARD_COPIED: u8 = 17;
const CATCH_UP: u8 = 18;
const SHARD_CHANGES: u8 = 19;
const CAUGHT_UP: u8 = 20;
const KEYS_PER_SHARD_PAGE: usize = 4096;
const CATCH_UP_ROUNDS: usize = 1000;
const WANTS_SNAPSHOT: u64 = u64::MAX;
const SNAPSHOT_CHUNK_BYTES: usize = 1 << 20;
const SNAPSHOT_STAGING_SUFFIX: &str = ".replica-seeding";
const MOST_FRAME_BYTES: usize = 128 << 20;
const BATCHES_PER_ROUND: usize = 64;
const IDLE_WAIT: Duration = Duration::from_secs(1);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const RECONNECT_WAIT: Duration = Duration::from_secs(2);
const FAILED_LOGIN_PAUSE: Duration = Duration::from_millis(250);
const PROOF_LABEL: &[u8] = b"eggdb replication v1";
const RETAINED_ON_REPLICA: usize = 1024;

/// Who may follow the primary: the server's one user, proving the password by an HMAC of a
/// fresh challenge keyed with its SHA-256, so the password never crosses the wire.
#[derive(Clone)]
pub(super) struct ReplicationCredentials {
    pub(super) user: String,
    pub(super) password_sha256: [u8; 32],
}

pub(super) trait Stream: Read + Write + Send {}

impl<T: Read + Write + Send> Stream for T {}

pub(super) fn write_frame(stream: &mut dyn Stream, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    let length = u32::try_from(payload.len() + 1).map_err(|_| std::io::Error::other("a replication frame is too large"))?;
    let mut frame = Vec::with_capacity(payload.len() + 5);
    frame.extend_from_slice(&length.to_le_bytes());
    frame.push(kind);
    frame.extend_from_slice(payload);
    stream.write_all(&frame)?;
    stream.flush()
}

pub(super) fn read_frame(stream: &mut dyn Stream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MOST_FRAME_BYTES {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "a replication frame has a wrong length"));
    }
    let mut frame = vec![0u8; length];
    stream.read_exact(&mut frame)?;
    let kind = frame[0];
    frame.remove(0);
    Ok((kind, frame))
}

fn number(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn proof(credentials: &ReplicationCredentials, nonce: &[u8]) -> [u8; 32] {
    eggdb_crypto::hmac_sha256(&credentials.password_sha256, &[nonce, PROOF_LABEL].concat())
}

fn refusal_text(refusal: &ReplicationRefusal) -> String {
    match refusal {
        ReplicationRefusal::StaleTerm { current } => format!("the batch is from a term older than {current}"),
        ReplicationRefusal::Gap { next } => format!("the replica needs batch {next} next"),
        ReplicationRefusal::Diverged { at } => {
            format!("the replica's history differs from the primary's from batch {at}; copy it again from a snapshot")
        }
        ReplicationRefusal::Failed(message) => format!("applying the batch failed: {message}"),
    }
}

fn ship(stream: &mut dyn Stream, db: &Database, primary: &ReplicationPrimary, replica: ReplicaId, mut applied: u64, stop: &(dyn Fn() -> bool + Sync)) -> std::io::Result<()> {
    let failed = |error: eggdb::Error| std::io::Error::other(error.to_string());
    while !stop() && !primary.is_retired() {
        let batches = match primary.batches_after(applied, BATCHES_PER_ROUND).map_err(failed)? {
            Ok(batches) => batches,
            Err(first_kept) if db.archived_batches(applied, first_kept).map_err(failed)?.is_some() => {
                db.archived_batches(applied, first_kept).map_err(failed)?.unwrap_or_default()
            }
            Err(first_kept) => {
                let message = format!(
                    "the primary keeps batches from {first_kept} on, and the replica needs {}; copy it again from a snapshot",
                    applied + 1
                );
                return write_frame(stream, REFUSED, message.as_bytes());
            }
        };
        if batches.is_empty() {
            primary.wait_for_more(applied, IDLE_WAIT).map_err(failed)?;
            write_frame(stream, HEARTBEAT, &primary.last().map_err(failed)?.to_le_bytes())?;
        } else {
            for batch in &batches {
                write_frame(stream, BATCH, &eggdb::encode_batch(batch))?;
            }
        }
        let sent = batches.len().max(1);
        for _ in 0..sent {
            match read_frame(stream)? {
                (ACK, payload) => {
                    applied = number(&payload, 0).unwrap_or(applied);
                    primary.acknowledge(replica, applied).map_err(failed)?;
                }
                (REFUSED, _) => return Ok(()),
                _ => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "the replica sent an unexpected frame")),
            }
        }
    }
    Ok(())
}

fn send_snapshot(stream: &mut dyn Stream, db: &Database) -> std::io::Result<()> {
    let directory = std::env::temp_dir();
    let mut nonce = [0u8; 8];
    eggdb_platform::secure_random::fill(&mut nonce)?;
    let path = directory.join(format!("eggdb-replica-snapshot-{:016x}.eggdb", u64::from_le_bytes(nonce)));
    let outcome = (|| -> std::io::Result<()> {
        db.backup_to(&path).map_err(|error| std::io::Error::other(error.to_string()))?;
        stream_file(stream, &path)
    })();
    let _ = std::fs::remove_file(&path);
    outcome
}

/// Streams the file at `path` as snapshot chunks and a final SHA-256, as
/// [`receive_snapshot`] reads them.
pub(super) fn stream_file(stream: &mut dyn Stream, path: &std::path::Path) -> std::io::Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = eggdb_crypto::Sha256::new();
    let mut chunk = vec![0u8; SNAPSHOT_CHUNK_BYTES];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
        write_frame(stream, SNAPSHOT_CHUNK, &chunk[..read])?;
    }
    write_frame(stream, SNAPSHOT_END, &hasher.finish())
}

pub(super) fn signed_greeting(credentials: &ReplicationCredentials, nonce: &[u8], tail: &[u8]) -> Result<Vec<u8>, String> {
    let mut greeting = vec![u8::try_from(credentials.user.len()).map_err(|_| "the user name is too long")?];
    greeting.extend_from_slice(credentials.user.as_bytes());
    greeting.extend_from_slice(&proof(credentials, nonce));
    greeting.extend_from_slice(tail);
    Ok(greeting)
}

pub(super) fn checked_greeting<'g>(greeting: &'g [u8], credentials: &ReplicationCredentials, nonce: &[u8]) -> Option<&'g [u8]> {
    let user_length = usize::from(*greeting.first()?);
    let user = greeting.get(1..1 + user_length)?;
    let rest = greeting.get(1 + user_length..)?;
    let offered = rest.get(..32)?;
    (user == credentials.user.as_bytes() && eggdb_crypto::constant_time_eq(offered, &proof(credentials, nonce))).then(|| &rest[32..])
}

fn status_bytes(status: &NodeStatus) -> Vec<u8> {
    let mut out = Vec::with_capacity(29);
    out.extend_from_slice(&status.id.to_le_bytes());
    out.extend_from_slice(&status.term.to_le_bytes());
    out.extend_from_slice(&status.applied.to_le_bytes());
    out.push(u8::from(status.is_primary));
    out.extend_from_slice(&status.contact_age_ms.to_le_bytes());
    out
}

fn status_of(bytes: &[u8]) -> Option<NodeStatus> {
    Some(NodeStatus {
        id: u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?),
        term: number(bytes, 4)?,
        applied: number(bytes, 12)?,
        is_primary: *bytes.get(20)? != 0,
        contact_age_ms: number(bytes, 21)?,
    })
}

fn serve_replica(stream: &mut dyn Stream, node: &ReplicationNode<'_>, stop: &(dyn Fn() -> bool + Sync)) -> std::io::Result<()> {
    let mut nonce = [0u8; 32];
    eggdb_platform::secure_random::fill(&mut nonce)?;
    write_frame(stream, CHALLENGE, &nonce)?;
    let (kind, greeting) = read_frame(stream)?;
    let Some(rest) = checked_greeting(&greeting, &node.credentials, &nonce) else {
        std::thread::sleep(FAILED_LOGIN_PAUSE);
        return write_frame(stream, REFUSED, b"the user or password is not this server's");
    };
    match kind {
        STATUS_REQUEST => return write_frame(stream, STATUS, &status_bytes(&node.status())),
        PROMOTE => {
            let term = number(rest, 0).unwrap_or(0);
            return match node.promote(term) {
                Ok(()) => write_frame(stream, DONE, &[]),
                Err(message) => write_frame(stream, REFUSED, message.as_bytes()),
            };
        }
        FOLLOW => {
            let address = String::from_utf8_lossy(rest).into_owned();
            node.follow(&address);
            return write_frame(stream, DONE, &[]);
        }
        SHARD_COPY => return serve_shard_copy(stream, node.db, node.current_primary(), rest),
        HELLO => {}
        _ => return write_frame(stream, REFUSED, b"an unknown greeting"),
    }
    let (Some(replica), Some(applied)) = (rest.get(..4).and_then(|bytes| bytes.try_into().ok()).map(u32::from_le_bytes), number(rest, 4)) else {
        return write_frame(stream, REFUSED, b"the replica's greeting is malformed");
    };
    let failed = |error: eggdb::Error| std::io::Error::other(error.to_string());
    if applied == WANTS_SNAPSHOT {
        return send_snapshot(stream, node.db);
    }
    let Some(primary) = node.current_primary() else {
        return write_frame(stream, REFUSED, b"this server is not the primary now");
    };
    primary.add_replica(replica, applied).map_err(failed)?;
    write_frame(stream, WELCOME, &primary.term().map_err(failed)?.to_le_bytes())?;
    node.connected_replicas.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let shipped = ship(stream, node.db, &primary, replica, applied, stop);
    node.connected_replicas.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    let _ = primary.remove_replica(replica);
    shipped
}

fn ask(peer: &PrimaryAddress, credentials: &ReplicationCredentials, kind: u8, tail: &[u8]) -> Result<(u8, Vec<u8>), String> {
    let mut stream = connect(peer)?;
    let io = |error: std::io::Error| format!("{}: {error}", peer.address);
    let (challenge, nonce) = read_frame(stream.as_mut()).map_err(io)?;
    if challenge != CHALLENGE {
        return Err(format!("{} did not send its challenge", peer.address));
    }
    write_frame(stream.as_mut(), kind, &signed_greeting(credentials, &nonce, tail)?).map_err(io)?;
    read_frame(stream.as_mut()).map_err(io)
}

/// Asks the server at `peer` where it stands in the group (D1026).
pub(super) fn query_status(peer: &PrimaryAddress, credentials: &ReplicationCredentials) -> Result<NodeStatus, String> {
    match ask(peer, credentials, STATUS_REQUEST, &[])? {
        (STATUS, bytes) => status_of(&bytes).ok_or_else(|| format!("{} sent a malformed status", peer.address)),
        (_, message) => Err(format!("{}: {}", peer.address, String::from_utf8_lossy(&message))),
    }
}

/// Tells the replica at `peer` to become the primary of `term`.
pub(super) fn send_promote(peer: &PrimaryAddress, credentials: &ReplicationCredentials, term: u64) -> Result<(), String> {
    match ask(peer, credentials, PROMOTE, &term.to_le_bytes())? {
        (DONE, _) => Ok(()),
        (_, message) => Err(format!("{} did not take the primary's role: {}", peer.address, String::from_utf8_lossy(&message))),
    }
}

/// Tells the server at `peer` to follow the primary at `address`.
pub(super) fn send_follow(peer: &PrimaryAddress, credentials: &ReplicationCredentials, address: &str) -> Result<(), String> {
    match ask(peer, credentials, FOLLOW, address.as_bytes())? {
        (DONE, _) => Ok(()),
        (_, message) => Err(format!("{}: {}", peer.address, String::from_utf8_lossy(&message))),
    }
}

/// Serves replicas on `listener` from `primary` until `stop` turns true, a thread per replica,
/// over TLS when `tls` is given.
pub(super) fn serve_primary(
    listener: &TcpListener,
    node: &ReplicationNode<'_>,
    tls: Option<Arc<eggdb_tls::ServerConfig>>,
    stop: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    std::thread::scope(|scope| {
        while !stop() {
            let stream = match eggdb_platform::accept_tcp(listener) {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err() || stream.set_read_timeout(Some(READ_TIMEOUT)).is_err() {
                continue;
            }
            let tls = tls.clone();
            let _ = std::thread::Builder::new().name("eggdb-replication".to_owned()).spawn_scoped(scope, move || {
                let _ = match tls {
                    Some(config) => match eggdb_tls::TlsStream::accept(stream, &config) {
                        Ok(mut secured) => serve_replica(&mut secured, node, stop),
                        Err(_) => Ok(()),
                    },
                    None => {
                        let mut plain = stream;
                        serve_replica(&mut plain, node, stop)
                    }
                };
            });
        }
    });
    Ok(())
}

/// How a replica reaches its primary, or a server one of its peers.
#[derive(Debug, Clone)]
pub(super) struct PrimaryAddress {
    pub(super) address: String,
    pub(super) tls: Option<eggdb_tls::ClientConfig>,
}

pub(super) fn connect(primary: &PrimaryAddress) -> Result<Box<dyn Stream>, String> {
    let stream = TcpStream::connect(&primary.address).map_err(|error| format!("cannot reach the primary {}: {error}", primary.address))?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).map_err(|error| error.to_string())?;
    Ok(match &primary.tls {
        Some(config) => Box::new(eggdb_tls::TlsStream::connect(stream, config).map_err(|error| format!("TLS with the primary failed: {error}"))?),
        None => Box::new(stream),
    })
}

fn greet(stream: &mut dyn Stream, credentials: &ReplicationCredentials, replica_id: ReplicaId, applied: u64) -> Result<(), String> {
    let io = |error: std::io::Error| format!("the primary connection failed: {error}");
    let (kind, nonce) = read_frame(stream).map_err(io)?;
    if kind != CHALLENGE {
        return Err("the primary did not send its challenge".into());
    }
    let tail = [replica_id.to_le_bytes().as_slice(), applied.to_le_bytes().as_slice()].concat();
    write_frame(stream, HELLO, &signed_greeting(credentials, &nonce, &tail)?).map_err(io)
}

/// Copies the primary's database to the new file `path` before a replica first opens it
/// (D1025): the primary streams a fresh online backup, which carries its replication position,
/// so the replica follows on from exactly that commit.
pub(super) fn seed_from_primary(primary: &PrimaryAddress, path: &std::path::Path, credentials: &ReplicationCredentials) -> Result<(), String> {
    let mut stream = connect(primary)?;
    greet(stream.as_mut(), credentials, replica_id_of(path), WANTS_SNAPSHOT)?;
    receive_snapshot(stream.as_mut(), path)
}

/// Writes the snapshot `send_snapshot` streams to the new file `path`, through a staging file
/// renamed into place once its digest checks out.
pub(super) fn receive_snapshot(stream: &mut dyn Stream, path: &std::path::Path) -> Result<(), String> {
    let mut staging = path.as_os_str().to_owned();
    staging.push(SNAPSHOT_STAGING_SUFFIX);
    let staging = std::path::PathBuf::from(staging);
    let outcome = (|| -> Result<(), String> {
        let io = |error: std::io::Error| format!("copying the primary's snapshot failed: {error}");
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&staging).map_err(io)?;
        let mut hasher = eggdb_crypto::Sha256::new();
        loop {
            match read_frame(stream).map_err(io)? {
                (SNAPSHOT_CHUNK, bytes) => {
                    hasher.update(&bytes);
                    file.write_all(&bytes).map_err(io)?;
                }
                (SNAPSHOT_END, digest) => {
                    if digest.as_slice() != hasher.finish().as_slice() {
                        return Err("the primary's snapshot arrived damaged".into());
                    }
                    file.sync_all().map_err(io)?;
                    return std::fs::rename(&staging, path).map_err(io);
                }
                (REFUSED, message) => return Err(format!("the primary refused the replica: {}", String::from_utf8_lossy(&message))),
                _ => return Err("the primary sent an unexpected frame while copying".into()),
            }
        }
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    outcome
}

fn follow_once(primary: &PrimaryAddress, node: &ReplicationNode<'_>, stop: &(dyn Fn() -> bool + Sync)) -> Result<(), String> {
    let mut stream = connect(primary)?;
    let io = |error: std::io::Error| format!("the primary connection failed: {error}");
    let db = node.db;
    let (term, applied) = db.replication_position().map_err(|error| error.to_string())?.unwrap_or((0, 0));
    greet(stream.as_mut(), &node.credentials, node.replica_id, applied)?;
    let (kind, welcome) = read_frame(stream.as_mut()).map_err(io)?;
    match kind {
        WELCOME => {}
        REFUSED => return Err(format!("the primary refused the replica: {}", String::from_utf8_lossy(&welcome))),
        _ => return Err("the primary sent an unexpected greeting".into()),
    }
    node.touch();
    let mut replica = Replica::new(node.replica_id, term, applied, term, DatabaseApply(db), RETAINED_ON_REPLICA);
    replica.follow_term(number(&welcome, 0).unwrap_or(term));
    while !stop() && node.following().is_some_and(|following| following.address == primary.address) {
        let (kind, payload) = read_frame(stream.as_mut()).map_err(io)?;
        node.touch();
        match kind {
            BATCH => {
                let batch: ReplicationBatch = eggdb::decode_batch(&payload)?;
                match replica.receive(&batch) {
                    Ok(applied) => {
                        super::serve_failover::note_applied(batch.term, applied, batch.sequence);
                        write_frame(stream.as_mut(), ACK, &applied.to_le_bytes()).map_err(io)?;
                    }
                    Err(ReplicationRefusal::Gap { next }) => {
                        write_frame(stream.as_mut(), ACK, &next.saturating_sub(1).to_le_bytes()).map_err(io)?;
                    }
                    Err(refusal) => {
                        let text = refusal_text(&refusal);
                        let _ = write_frame(stream.as_mut(), REFUSED, text.as_bytes());
                        return Err(text);
                    }
                }
            }
            HEARTBEAT => {
                super::serve_failover::note_applied(replica.term(), replica.applied(), number(&payload, 0).unwrap_or(0));
                write_frame(stream.as_mut(), ACK, &replica.applied().to_le_bytes()).map_err(io)?;
            }
            REFUSED => return Err(format!("the primary stopped the replica: {}", String::from_utf8_lossy(&payload))),
            _ => return Err("the primary sent an unexpected frame".into()),
        }
    }
    Ok(())
}

/// Follows whichever primary the node is told to, until `stop` turns true: applies every batch
/// to the database, reconnects after a lost connection, waits while the node is the primary or
/// follows nobody, and gives up only when the primary refuses the replica for good (a diverged
/// history or a batch kept nowhere), returning why.
pub(super) fn follow_primary(node: &ReplicationNode<'_>, stop: &(dyn Fn() -> bool + Sync)) -> Result<(), String> {
    while !stop() {
        let Some(primary) = node.following() else {
            std::thread::sleep(RECONNECT_WAIT);
            continue;
        };
        match follow_once(&primary, node, stop) {
            Ok(()) => {}
            Err(message) if message.contains("copy it again") || message.contains("refused the replica") => return Err(message),
            Err(_) => std::thread::sleep(RECONNECT_WAIT),
        }
    }
    Ok(())
}

/// A replica's id: the first bytes of the SHA-256 of its file's path, the same at every start.
pub(super) fn replica_id_of(path: &std::path::Path) -> ReplicaId {
    let digest = eggdb_crypto::sha256(path.to_string_lossy().as_bytes());
    u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]])
}

/// The TLS setup for reaching the replication listener at `address`: none on this machine
/// without a CA file, and otherwise a full check of its certificate against `ca_file`.
pub(super) fn client_tls(address: &str, ca_file: Option<&std::path::Path>) -> Result<Option<eggdb_tls::ClientConfig>, String> {
    let host = address.rsplit_once(':').map_or(address, |(host, _)| host).trim_start_matches('[').trim_end_matches(']');
    let loopback = host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    let Some(ca_file) = ca_file else {
        if loopback {
            return Ok(None);
        }
        return Err(format!(
            "--replica-of {address}: a primary on another machine is reached over TLS; give the CA that signed its certificate with --replica-of-ca <file>"
        ));
    };
    let text = std::fs::read_to_string(ca_file).map_err(|error| format!("cannot read --replica-of-ca {}: {error}", ca_file.display()))?;
    let trust = eggdb_tls::verify::TrustStore::from_pem(&text).map_err(|error| format!("--replica-of-ca {}: {error}", ca_file.display()))?;
    let server_name = eggdb_tls::verify::ServerName::parse(host).map_err(|error| format!("--replica-of {address}: {error}"))?;
    Ok(Some(eggdb_tls::ClientConfig::new(eggdb_tls::ServerVerification::Full { trust, server_name }, host)))
}

fn shard_request(rest: &[u8]) -> Option<(eggdb::cluster::ShardId, u64)> {
    Some((u32::from_le_bytes(rest.get(..4)?.try_into().ok()?), number(rest, 4)?))
}

pub(super) fn serve_shard_copy(
    stream: &mut dyn Stream,
    db: &Database,
    primary: Option<Arc<ReplicationPrimary>>,
    rest: &[u8],
) -> std::io::Result<()> {
    let failed = |error: eggdb::Error| std::io::Error::other(error.to_string());
    let Some((shard, epoch)) = shard_request(rest) else {
        return write_frame(stream, REFUSED, b"the shard copy request is malformed");
    };
    let Some(primary) = primary else {
        return write_frame(stream, REFUSED, b"this server is not the primary of the shard's owner now");
    };
    let Some(map) = db.shard_map().map_err(failed)? else {
        return write_frame(stream, REFUSED, b"this server has no shard map");
    };
    if map.epoch != epoch || !map.shards.contains_key(&shard) {
        return write_frame(
            stream,
            REFUSED,
            format!("the shard map here is at epoch {}, not {epoch}, or has no shard {shard}", map.epoch).as_bytes(),
        );
    }
    let start = primary.last().map_err(failed)?;
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = db.shard_page(&map, shard, after.as_deref(), KEYS_PER_SHARD_PAGE).map_err(failed)?;
        let changes: Vec<eggdb::ShardChange> = page.entries.into_iter().map(|(key, value)| (key, Some(value))).collect();
        write_frame(stream, SHARD_PAGE, &eggdb::encode_shard_changes(&changes))?;
        match page.next_after {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    write_frame(stream, SHARD_COPIED, &start.to_le_bytes())?;
    loop {
        let (kind, payload) = read_frame(stream)?;
        if kind == DONE {
            return Ok(());
        }
        if kind != CATCH_UP {
            return write_frame(stream, REFUSED, b"an unexpected frame during the shard copy");
        }
        let mut applied = number(&payload, 0).unwrap_or(start);
        let batches = match primary.batches_after(applied, BATCHES_PER_ROUND).map_err(failed)? {
            Ok(batches) => batches,
            Err(first_kept) => match db.archived_batches(applied, first_kept).map_err(failed)? {
                Some(batches) => batches,
                None => {
                    return write_frame(stream, REFUSED, b"the changes since the copy started are no longer kept; move the shard again");
                }
            },
        };
        let mut changes = Vec::new();
        for batch in &batches {
            changes.extend(eggdb::shard_changes(batch, &map, shard));
            applied = batch.sequence;
        }
        write_frame(stream, SHARD_CHANGES, &eggdb::encode_shard_changes(&changes))?;
        let caught_up = batches.len() < BATCHES_PER_ROUND;
        let mut status = applied.to_le_bytes().to_vec();
        status.push(u8::from(caught_up));
        write_frame(stream, CAUGHT_UP, &status)?;
    }
}

/// What a shard move copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct ShardCopyReport {
    pub(super) keys: u64,
    pub(super) changes: u64,
    pub(super) epoch: u64,
}

/// Moves `shard` from the server at `from` into `db` over the replication protocol (D1040):
/// copies its keys page by page while that server keeps serving, applies the changes made
/// meanwhile from its replication batches until a round finds nothing new, then publishes the
/// shard map with this server as the owner and the epoch raised, and applies the last changes
/// written before the cut-over. `owner` is this server's id in the map.
pub(super) fn copy_shard(
    from: &PrimaryAddress,
    credentials: &ReplicationCredentials,
    db: &Database,
    shard: eggdb::cluster::ShardId,
    owner: u64,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<ShardCopyReport, String> {
    let map = db
        .shard_map()
        .map_err(|error| error.to_string())?
        .ok_or("this server has no shard map; publish one before moving shards")?;
    if !map.shards.contains_key(&shard) {
        return Err(format!("the shard map has no shard {shard}"));
    }
    let mut stream = connect(from)?;
    let io = |error: std::io::Error| format!("the shard copy from {} failed: {error}", from.address);
    let (kind, nonce) = read_frame(stream.as_mut()).map_err(io)?;
    if kind != CHALLENGE {
        return Err(format!("{} did not send its challenge", from.address));
    }
    let mut tail = shard.to_le_bytes().to_vec();
    tail.extend_from_slice(&map.epoch.to_le_bytes());
    write_frame(stream.as_mut(), SHARD_COPY, &signed_greeting(credentials, &nonce, &tail)?).map_err(io)?;
    let mut report = ShardCopyReport::default();
    let start = loop {
        match read_frame(stream.as_mut()).map_err(io)? {
            (SHARD_PAGE, bytes) => {
                let changes = eggdb::decode_shard_changes(&bytes).ok_or("a shard page arrived damaged")?;
                report.keys += changes.len() as u64;
                db.apply_shard_changes(&changes).map_err(|error| error.to_string())?;
            }
            (SHARD_COPIED, bytes) => break number(&bytes, 0).ok_or("the copy's end arrived damaged")?,
            (REFUSED, message) => return Err(format!("{} refused the shard copy: {}", from.address, String::from_utf8_lossy(&message))),
            _ => return Err(format!("{} sent an unexpected frame during the shard copy", from.address)),
        }
        if stop() {
            return Err("the shard copy was stopped".into());
        }
    };
    let mut applied = start;
    let mut catch_up = |applied: &mut u64, report: &mut ShardCopyReport| -> Result<bool, String> {
        write_frame(stream.as_mut(), CATCH_UP, &applied.to_le_bytes()).map_err(io)?;
        let changes = match read_frame(stream.as_mut()).map_err(io)? {
            (SHARD_CHANGES, bytes) => eggdb::decode_shard_changes(&bytes).ok_or("catch-up changes arrived damaged")?,
            (REFUSED, message) => return Err(format!("{} stopped the shard copy: {}", from.address, String::from_utf8_lossy(&message))),
            _ => return Err(format!("{} sent an unexpected frame during the catch-up", from.address)),
        };
        report.changes += changes.len() as u64;
        db.apply_shard_changes(&changes).map_err(|error| error.to_string())?;
        match read_frame(stream.as_mut()).map_err(io)? {
            (CAUGHT_UP, status) => {
                *applied = number(&status, 0).ok_or("the catch-up status arrived damaged")?;
                Ok(status.get(8) == Some(&1))
            }
            _ => Err(format!("{} sent an unexpected frame during the catch-up", from.address)),
        }
    };
    let mut caught_up = false;
    for _ in 0..CATCH_UP_ROUNDS {
        if stop() {
            return Err("the shard copy was stopped".into());
        }
        if catch_up(&mut applied, &mut report)? {
            caught_up = true;
            break;
        }
    }
    if !caught_up {
        return Err(format!("shard {shard} kept changing faster than it could be copied; try the move again when it is quieter"));
    }
    let mut moved = map.clone();
    if let Some(entry) = moved.shards.get_mut(&shard) {
        entry.owner = owner;
    }
    moved.epoch = map.epoch + 1;
    if !db.publish_shard_map(&moved, Some(map.epoch)).map_err(|error| error.to_string())? {
        return Err("the shard map changed while the shard was copied; the move was not cut over".into());
    }
    report.epoch = moved.epoch;
    catch_up(&mut applied, &mut report)?;
    write_frame(stream.as_mut(), DONE, &[]).map_err(io)?;
    Ok(report)
}

type ShardMoveRequest = (eggdb::cluster::ShardId, String, std::sync::mpsc::Sender<Result<String, String>>);

static SHARD_MOVES: std::sync::Mutex<Option<std::sync::mpsc::Sender<ShardMoveRequest>>> = std::sync::Mutex::new(None);

const SHARD_MOVE_REPLY: Duration = Duration::from_secs(24 * 60 * 60);

/// The hook `MOVE SHARD` calls in this process: it hands the move to the server's shard mover
/// and waits for its answer.
pub(super) fn request_shard_move(shard: eggdb::cluster::ShardId, from: &str) -> Result<String, String> {
    let sender = SHARD_MOVES
        .lock()
        .map_err(|_| "the shard mover failed".to_owned())?
        .clone()
        .ok_or("this server does not move shards")?;
    let (reply, answer) = std::sync::mpsc::channel();
    sender.send((shard, from.to_owned(), reply)).map_err(|_| "the shard mover stopped".to_owned())?;
    answer.recv_timeout(SHARD_MOVE_REPLY).map_err(|_| "the shard move did not finish in time".to_owned())?
}

/// Runs the shard moves `MOVE SHARD` asks for, one at a time, until `stop` turns true:
/// each copies the shard into `db` from the named server and cuts it over to `owner`.
pub(super) fn serve_shard_moves(
    db: &Database,
    credentials: &ReplicationCredentials,
    ca_file: Option<&std::path::Path>,
    owner: u64,
    stop: &(dyn Fn() -> bool + Sync),
) {
    let (sender, requests) = std::sync::mpsc::channel::<ShardMoveRequest>();
    if let Ok(mut slot) = SHARD_MOVES.lock() {
        *slot = Some(sender);
    }
    while !stop() {
        let Ok((shard, from, reply)) = requests.recv_timeout(ACCEPT_POLL * 5) else {
            continue;
        };
        let outcome = client_tls(&from, ca_file)
            .map(|tls| PrimaryAddress { address: from.clone(), tls })
            .and_then(|address| copy_shard(&address, credentials, db, shard, owner, stop))
            .map(|report| {
                format!(
                    "shard {shard} moved from {from}: {} keys copied, {} changes caught up, shard map epoch {}",
                    report.keys, report.changes, report.epoch
                )
            });
        let _ = reply.send(outcome);
    }
    if let Ok(mut slot) = SHARD_MOVES.lock() {
        *slot = None;
    }
}
