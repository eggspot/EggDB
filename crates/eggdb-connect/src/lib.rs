//! EggDB's Kafka Connect (D46, D1055): the Connect REST API on its own listener, and a worker in
//! the server that runs EggDB's own Rust connectors (a file source and sink and a topic copy,
//! answering to the Apache file connectors' class names too) against the server's Kafka topics,
//! each connector's configuration, state and position kept in the database so it resumes where
//! it stopped. The worker group protocol's metadata and assignments, and the leader's round
//! robin, are in [`protocol`]. Kafka is a trademark of the Apache Software Foundation; EggDB is
//! compatible with Kafka Connect's REST API.

mod connectors;
mod json;
pub mod protocol;
mod rest;
mod store;

use eggdb_keyspace::Keyspace;
use std::net::TcpListener;
use std::time::Duration;
use store::{Connectors, State};

const STEP_PAUSE: Duration = Duration::from_millis(500);

/// Runs one step of every running connector, keeping each one's new position, and marks a
/// connector that fails as failed with the reason.
pub fn run_connectors_once(connect: &(dyn Keyspace + Sync), kafka: &dyn Keyspace) -> Result<usize, String> {
    let connectors = Connectors::new(connect);
    let mut moved = 0;
    for name in connectors.names()? {
        let Some(stored) = connectors.get(&name)? else {
            continue;
        };
        if stored.state != State::Running {
            continue;
        }
        match connectors::step(kafka, &stored.config, stored.position) {
            Ok((position, count)) => {
                if position != stored.position {
                    connectors.set_position(&name, position)?;
                }
                moved += count;
            }
            Err(message) => connectors.set_state(&name, &State::Failed(message))?,
        }
    }
    Ok(moved)
}

/// Serves the Connect REST API on `listener` and runs the connectors until `stop` turns true.
pub fn serve(
    listener: &TcpListener,
    connect: &(dyn Keyspace + Sync),
    kafka: &(dyn Keyspace + Sync),
    stop: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<()> {
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop() {
                let _ = run_connectors_once(connect, kafka);
                std::thread::sleep(STEP_PAUSE);
            }
        });
        let connectors = Connectors::new(connect);
        let handler = |request: &eggdb_http::Request| rest::handle(&connectors, request);
        eggdb_http::serve(listener, &handler, stop)
    })
}

#[cfg(test)]
mod tests;
