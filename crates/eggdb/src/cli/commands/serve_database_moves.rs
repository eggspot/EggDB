use super::serve_databases::ServedDirectory;
use super::serve_replication::{
    PrimaryAddress, ReplicationCredentials, Stream, checked_greeting, client_tls, connect,
    read_frame, receive_snapshot, signed_greeting, stream_file, write_frame,
};
use eggdb::Database;
use eggdb_sql::ServedDatabaseError;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

const CHALLENGE: u8 = 1;
const REFUSED: u8 = 6;
const DATABASE_COPY: u8 = 30;
const FREEZE: u8 = 31;
const MOVED: u8 = 32;
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_TIMEOUT: Duration = Duration::from_secs(300);
const FAILED_LOGIN_PAUSE: Duration = Duration::from_millis(250);
const MOVE_REPLY: Duration = Duration::from_secs(24 * 60 * 60);
const STAGING_SUFFIX: &str = ".arriving";
const DELTA_SUFFIX: &str = ".arriving-delta";

type DatabaseMoveRequest = (String, String, Sender<Result<String, String>>);

static DATABASE_MOVES: std::sync::Mutex<Option<Sender<DatabaseMoveRequest>>> =
    std::sync::Mutex::new(None);

fn served_error(error: ServedDatabaseError) -> String {
    match error {
        ServedDatabaseError::Exists => "a database of that name is already here".to_owned(),
        ServedDatabaseError::Missing => "no database of that name is served there".to_owned(),
        ServedDatabaseError::InUse => "the database is in use".to_owned(),
        ServedDatabaseError::Unsupported(reason) | ServedDatabaseError::Failed(reason) => reason,
    }
}

fn temporary_path(label: &str) -> std::io::Result<std::path::PathBuf> {
    let mut nonce = [0u8; 8];
    eggdb_platform::secure_random::fill(&mut nonce)?;
    Ok(std::env::temp_dir().join(format!(
        "eggdb-move-{label}-{:016x}.eggdb",
        u64::from_le_bytes(nonce)
    )))
}

fn copy_out(stream: &mut dyn Stream, db: &Database, name: &str) -> std::io::Result<()> {
    let failed = |error: eggdb::Error| std::io::Error::other(error.to_string());
    let full = temporary_path("full")?;
    let delta = temporary_path("delta")?;
    let outcome = (|| -> std::io::Result<()> {
        db.backup_to(&full).map_err(failed)?;
        stream_file(stream, &full)?;
        let (kind, destination) = read_frame(stream)?;
        if kind != FREEZE {
            return Ok(());
        }
        let destination = String::from_utf8_lossy(&destination).into_owned();
        db.fence_and_drain(format!(
            "database {name} is moving to {destination}; connect there"
        ))
        .map_err(failed)?;
        let base = Database::open(&full).map_err(failed)?;
        let differential = db.backup_differential_to(&base, &delta);
        drop(base);
        if let Err(error) = differential {
            db.unfence();
            return Err(failed(error));
        }
        stream_file(stream, &delta)?;
        match read_frame(stream) {
            Ok((MOVED, _)) => {
                db.fence(format!("database {name} moved to {destination}; connect there"));
                Ok(())
            }
            _ => {
                db.unfence();
                Ok(())
            }
        }
    })();
    let _ = std::fs::remove_file(&full);
    let _ = std::fs::remove_file(&delta);
    outcome
}

fn serve_mover(
    stream: &mut dyn Stream,
    directory: &ServedDirectory<'_>,
    credentials: &ReplicationCredentials,
) -> std::io::Result<()> {
    let mut nonce = [0u8; 32];
    eggdb_platform::secure_random::fill(&mut nonce)?;
    write_frame(stream, CHALLENGE, &nonce)?;
    let (kind, greeting) = read_frame(stream)?;
    let Some(rest) = checked_greeting(&greeting, credentials, &nonce) else {
        std::thread::sleep(FAILED_LOGIN_PAUSE);
        return write_frame(stream, REFUSED, b"the user or password is not this server's");
    };
    if kind != DATABASE_COPY {
        return write_frame(stream, REFUSED, b"an unknown greeting");
    }
    let name = String::from_utf8_lossy(rest).into_owned();
    let Some(store) = directory.database_handle(&name) else {
        return write_frame(stream, REFUSED, b"no database of that name is served here, or it is the default one");
    };
    copy_out(stream, &store.0, &name)
}

/// Lends the databases of `directory` to servers that move them away (D1048), on `listener`,
/// until `stop` turns true, a thread per request, over TLS when `tls` is given.
pub(super) fn serve_database_moves(
    listener: &TcpListener,
    directory: &ServedDirectory<'_>,
    credentials: &ReplicationCredentials,
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
            let _ = std::thread::Builder::new().name("eggdb-database-move".to_owned()).spawn_scoped(scope, move || {
                let _ = match tls {
                    Some(config) => match eggdb_tls::TlsStream::accept(stream, &config) {
                        Ok(mut secured) => serve_mover(&mut secured, directory, credentials),
                        Err(_) => Ok(()),
                    },
                    None => {
                        let mut plain = stream;
                        serve_mover(&mut plain, directory, credentials)
                    }
                };
            });
        }
    });
    Ok(())
}

fn staged(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut staged = path.as_os_str().to_owned();
    staged.push(suffix);
    std::path::PathBuf::from(staged)
}

/// Moves the database `name` from the server whose move listener is at `from` into
/// `directory` (D1048): a full online copy while it keeps taking writes, then a freeze of its
/// writes there, the differential since the copy applied here, and the database served here.
pub(super) fn move_database_in(
    from: &PrimaryAddress,
    credentials: &ReplicationCredentials,
    directory: &ServedDirectory<'_>,
    name: &str,
    here: &str,
) -> Result<String, String> {
    let path = directory.arrival_path(name).map_err(served_error)?;
    let staging = staged(&path, STAGING_SUFFIX);
    let delta = staged(&path, DELTA_SUFFIX);
    let mut stream = connect(from)?;
    let io = |error: std::io::Error| format!("moving database {name} from {} failed: {error}", from.address);
    let (kind, nonce) = read_frame(stream.as_mut()).map_err(io)?;
    if kind != CHALLENGE {
        return Err(format!("{} did not send its challenge", from.address));
    }
    write_frame(stream.as_mut(), DATABASE_COPY, &signed_greeting(credentials, &nonce, name.as_bytes())?).map_err(io)?;
    let outcome = (|| -> Result<String, String> {
        receive_snapshot(stream.as_mut(), &staging)?;
        write_frame(stream.as_mut(), FREEZE, here.as_bytes()).map_err(io)?;
        receive_snapshot(stream.as_mut(), &delta)?;
        let events = eggdb::read_change_file(&delta).map_err(|error| error.to_string())?;
        let base = events
            .iter()
            .find_map(|event| match event {
                eggdb::ArchivedEvent::Base(stamp) => Some(*stamp),
                _ => None,
            })
            .ok_or("the differential arrived without its base")?;
        {
            let db = Database::open(&staging).map_err(|error| error.to_string())?;
            eggdb::replay_events(&db, events, &delta, base, None).map_err(|error| error.to_string())?;
        }
        std::fs::rename(&staging, &path).map_err(|error| format!("cannot put {} in place: {error}", path.display()))?;
        directory.adopt(name, &path).map_err(served_error)?;
        let _ = write_frame(stream.as_mut(), MOVED, &[]);
        Ok(format!("database {name} moved here from {}", from.address))
    })();
    let _ = std::fs::remove_file(&delta);
    if outcome.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    outcome
}

/// The hook `MOVE DATABASE` calls in this process: it hands the move to the server's database
/// mover and waits for its answer.
pub(super) fn request_database_move(name: &str, from: &str) -> Result<String, String> {
    let sender = DATABASE_MOVES
        .lock()
        .map_err(|_| "the database mover failed".to_owned())?
        .clone()
        .ok_or("this server does not move databases: it needs a data directory and --moves-listen")?;
    let (reply, answer) = channel();
    sender
        .send((name.to_owned(), from.to_owned(), reply))
        .map_err(|_| "the database mover stopped".to_owned())?;
    answer
        .recv_timeout(MOVE_REPLY)
        .map_err(|_| "the database move did not finish in time".to_owned())?
}

/// Runs the moves `MOVE DATABASE` asks for, one at a time, until `stop` turns true; `here` is
/// the address clients of a moved database are sent to.
pub(super) fn serve_database_arrivals(
    directory: &ServedDirectory<'_>,
    credentials: &ReplicationCredentials,
    ca_file: Option<&std::path::Path>,
    here: &str,
    stop: &(dyn Fn() -> bool + Sync),
) {
    let (sender, requests) = channel::<DatabaseMoveRequest>();
    if let Ok(mut slot) = DATABASE_MOVES.lock() {
        *slot = Some(sender);
    }
    while !stop() {
        let Ok((name, from, reply)) = requests.recv_timeout(ACCEPT_POLL * 5) else {
            continue;
        };
        let outcome = client_tls(&from, ca_file)
            .map(|tls| PrimaryAddress { address: from.clone(), tls })
            .and_then(|address| move_database_in(&address, credentials, directory, &name, here));
        let _ = reply.send(outcome);
    }
    if let Ok(mut slot) = DATABASE_MOVES.lock() {
        *slot = None;
    }
}
