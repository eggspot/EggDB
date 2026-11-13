use eggdb::Database;
use eggdb::cluster::NodeId;
use eggdb::cluster::crdt::CrdtKind;
use eggdb::cluster::quorum::Resolution;
use eggdb::cluster::quorum_net::{QuorumConfig, QuorumNode, Stream, dial_tcp};
use eggdb::quorum_store::DatabaseReplicaStore;
use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// A server's place among the nodes of a leaderless cluster (D1097).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QuorumSettings {
    pub(super) node: NodeId,
    pub(super) members: BTreeMap<NodeId, String>,
    pub(super) replicas: (usize, usize, usize),
    pub(super) resolution: Resolution,
    pub(super) ca_file: Option<std::path::PathBuf>,
}

pub(super) fn resolution_named(name: Option<&str>) -> Resolution {
    match name {
        Some("siblings") => Resolution::KeepSiblings,
        Some("counter") => Resolution::Crdt(CrdtKind::Counter),
        Some("set") => Resolution::Crdt(CrdtKind::Set),
        _ => Resolution::LastWriterWins,
    }
}

pub(super) fn default_replicas(members: usize) -> (usize, usize, usize) {
    let n = members.clamp(1, 3);
    (n, n / 2 + 1, n / 2 + 1)
}

impl QuorumSettings {
    pub(super) fn config(&self) -> QuorumConfig {
        let (n, r, w) = self.replicas;
        QuorumConfig {
            node: self.node,
            peers: self
                .members
                .iter()
                .filter(|(id, _)| **id != self.node)
                .map(|(id, address)| (*id, address.clone()))
                .collect(),
            n,
            r,
            w,
            resolution: self.resolution,
            timeout: CALL_TIMEOUT,
        }
    }
}

fn dialer(ca_file: Option<std::path::PathBuf>) -> Box<eggdb::cluster::quorum_net::Dialer> {
    Box::new(move |_, address: &str, timeout| -> std::io::Result<Box<dyn Stream>> {
        let stream = dial_tcp(address, timeout)?;
        match super::serve_replication::client_tls(address, ca_file.as_deref()).map_err(std::io::Error::other)? {
            Some(config) => Ok(Box::new(eggdb_tls::TlsStream::connect(stream, &config).map_err(|error| std::io::Error::other(error.to_string()))?)),
            None => Ok(Box::new(stream)),
        }
    })
}

/// Runs this server's quorum node until `stop`: it answers clients and peers on `listener`
/// (over TLS when `tls` is given), asks every connection for `password`, keeps its replica in
/// `db`, and hands hints off and runs anti-entropy in the background.
pub(super) fn run_quorum(
    db: &Database,
    settings: &QuorumSettings,
    listener: &TcpListener,
    tls: Option<Arc<eggdb_tls::ServerConfig>>,
    password: &str,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<(), String> {
    let store = DatabaseReplicaStore::new(db).map_err(|error| error.to_string())?;
    let node = QuorumNode::new(settings.config(), store)
        .map_err(|error| format!("--quorum-replicas does not fit the nodes in --quorum: {error:?}"))?
        .with_dialer(dialer(settings.ca_file.clone()))
        .with_password(password.as_bytes().to_vec());
    let accept = move |stream: TcpStream| -> std::io::Result<Box<dyn Stream>> {
        match &tls {
            Some(config) => Ok(Box::new(eggdb_tls::TlsStream::accept(stream, config).map_err(|error| std::io::Error::other(error.to_string()))?)),
            None => Ok(Box::new(stream)),
        }
    };
    std::thread::scope(|scope| {
        scope.spawn(|| node.maintain(stop));
        node.serve(listener, &accept, stop).map_err(|error| error.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replicas_default_to_three_with_majorities() {
        assert_eq!(default_replicas(1), (1, 1, 1));
        assert_eq!(default_replicas(2), (2, 2, 2));
        assert_eq!(default_replicas(5), (3, 2, 2));
    }

    #[test]
    fn resolutions_are_named_on_the_command_line() {
        assert_eq!(resolution_named(None), Resolution::LastWriterWins);
        assert_eq!(resolution_named(Some("siblings")), Resolution::KeepSiblings);
        assert_eq!(resolution_named(Some("counter")), Resolution::Crdt(CrdtKind::Counter));
    }
}
