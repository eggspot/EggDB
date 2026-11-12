//! EggDB's Arrow Flight SQL service (D29): Flight's gRPC methods (`Handshake`, `GetFlightInfo`,
//! `DoGet`, `DoPut`, `DoAction`, `ListActions`) over EggDB's own HTTP/2 and gRPC
//! ([`eggdb_http2`]) and protocol buffers codec, with Flight SQL's statement, prepared
//! statement, update and catalog commands, and results as Arrow record batches
//! ([`eggdb_arrow`]). Written from the Arrow Flight and Flight SQL protocol definitions, with no
//! third-party code. Apache Arrow is a trademark of the Apache Software Foundation; EggDB is
//! compatible with its Flight SQL protocol.

mod columns;
mod proto;
mod service;

pub use columns::columns_of;
pub use service::Endpoint;

use eggdb_sql::KeyValueStore;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// Serves Flight SQL on `listener` until `stop` says so, one thread per connection, at most
/// `max_connections` at once.
pub fn serve<S: KeyValueStore + Sync>(
    endpoint: &Endpoint<'_, S>,
    listener: &TcpListener,
    max_connections: usize,
    stop: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    let active = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        while !stop() {
            let mut stream = match eggdb_platform::accept_tcp(listener) {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err() || stream.set_nodelay(true).is_err() {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let active = &active;
            let spawned = std::thread::Builder::new()
                .name("eggdb-flight".to_owned())
                .spawn_scoped(scope, move || {
                    let _ = eggdb_http2::serve_connection(&mut stream, endpoint);
                    active.fetch_sub(1, Ordering::AcqRel);
                });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(())
}
