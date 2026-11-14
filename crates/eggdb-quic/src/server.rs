//! A QUIC server on one UDP socket: datagrams are routed to their connections by destination
//! connection ID, new connections start from a client's Initial, a client asking for another
//! version is told the one this server speaks, and each connection's timers run between
//! receives.

use crate::connection::{LOCAL_CONNECTION_ID_BYTES, ServerConnection};
use crate::http3::Http3Session;
use crate::packet::{Header, version_negotiation};
use eggdb_http::{Request, Response};
use eggdb_platform::DatagramSocket;
use eggdb_tls::ServerConfig;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

const RECEIVE_WAIT: Duration = Duration::from_millis(20);
const RECEIVE_BUFFER_BYTES: usize = 65_536;
const MOST_CONNECTIONS: usize = 4096;

struct Served<'c> {
    connection: ServerConnection<'c>,
    http3: Http3Session,
    peer: SocketAddr,
    original_destination: Vec<u8>,
}

fn new_connection_id() -> Option<Vec<u8>> {
    let mut id = vec![0u8; LOCAL_CONNECTION_ID_BYTES];
    eggdb_platform::secure_random::fill(&mut id).ok()?;
    Some(id)
}

fn destination_of(datagram: &[u8]) -> Option<(Vec<u8>, bool, u32)> {
    match Header::parse(datagram, LOCAL_CONNECTION_ID_BYTES).ok()? {
        Header::Long { destination, version, .. } => Some((destination, true, version)),
        Header::Short { destination, .. } => Some((destination, false, crate::QUIC_VERSION_1)),
        Header::VersionNegotiation { .. } => None,
    }
}

/// Serves HTTP/3 on `socket` with `config`'s certificate until `stop` says so, answering each
/// request with `handler`, the same handlers EggDB's HTTP/1.1 listeners use. `config` must offer
/// the `h3` ALPN protocol.
pub fn serve_http3(
    socket: &dyn DatagramSocket,
    config: &ServerConfig,
    handler: &(dyn Fn(&Request) -> Response + Sync),
    stop: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<()> {
    let mut connections: HashMap<Vec<u8>, Served<'_>> = HashMap::new();
    let mut aliases: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut buffer = vec![0u8; RECEIVE_BUFFER_BYTES];
    while !stop() {
        let now = Instant::now();
        match socket.receive_from(&mut buffer, RECEIVE_WAIT) {
            Ok((length, peer)) => {
                let datagram = &buffer[..length];
                receive(datagram, peer, now, config, socket, &mut connections, &mut aliases);
            }
            Err(error) if error.kind() == eggdb_platform::ErrorKind::TimedOut => {}
            Err(_) => std::thread::sleep(RECEIVE_WAIT),
        }
        let now = Instant::now();
        let mut finished = Vec::new();
        for (id, served) in &mut connections {
            if now >= served.connection.timeout() {
                served.connection.on_timeout(now);
            }
            served.http3.drive(&mut served.connection, handler);
            while let Some(out) = served.connection.poll_transmit(now) {
                if socket.send_to(&out, served.peer).is_err() {
                    break;
                }
            }
            if served.connection.closed().is_some() && served.connection.poll_transmit(now).is_none() {
                finished.push(id.clone());
            }
        }
        for id in finished {
            if let Some(served) = connections.remove(&id) {
                aliases.remove(&served.original_destination);
            }
        }
    }
    Ok(())
}

fn receive<'c>(
    datagram: &[u8],
    peer: SocketAddr,
    now: Instant,
    config: &'c ServerConfig,
    socket: &dyn DatagramSocket,
    connections: &mut HashMap<Vec<u8>, Served<'c>>,
    aliases: &mut HashMap<Vec<u8>, Vec<u8>>,
) {
    let Some((destination, long, version)) = destination_of(datagram) else {
        return;
    };
    if long && version != crate::QUIC_VERSION_1 {
        if datagram.len() >= crate::recovery::MAX_DATAGRAM_BYTES
            && let Ok(Header::Long { source, .. }) = Header::parse(datagram, LOCAL_CONNECTION_ID_BYTES)
        {
            let answer = version_negotiation(&destination, &source, &[crate::QUIC_VERSION_1], datagram[0]);
            let _ = socket.send_to(&answer, peer);
        }
        return;
    }
    let key = aliases.get(&destination).cloned().unwrap_or(destination.clone());
    if let Some(served) = connections.get_mut(&key) {
        served.connection.receive(datagram, now);
        return;
    }
    if !long || connections.len() >= MOST_CONNECTIONS {
        return;
    }
    let Some(local) = new_connection_id() else {
        return;
    };
    let Ok(connection) = ServerConnection::accept(config, datagram, local.clone(), now) else {
        return;
    };
    aliases.insert(destination.clone(), local.clone());
    connections.insert(
        local,
        Served { connection, http3: Http3Session::default(), peer, original_destination: destination },
    );
}
