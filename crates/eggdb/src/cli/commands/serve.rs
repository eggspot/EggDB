use super::serve_databases::{ServedDirectory, ServedDirectorySetup};
use super::serve_tls::{CertificateSource, TlsSetup, fips_requested, tls_setup};
use super::{Access, Session};
use crate::cli::args::{CommandName, ExportFile, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject, TimeStyle};
use eggdb::sql::Store as DatabaseStore;
use eggdb_pgwire::{
    DEFAULT_AUTHENTICATION_TIMEOUT, DEFAULT_IDLE_TIMEOUT, DEFAULT_MAX_CONNECTIONS, ListenError,
    MIN_SCRAM_ITERATIONS, MemoryBudget, SCRAM_SALT_BYTES, ScramVerifier, ServerConfig,
};
use eggdb_platform::{
    DatagramSocket, Network, OsNetwork, TcpKeepAlive, secure_random, stop_signals,
};
use std::io::BufRead;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

const DEFAULT_USER: &str = "eggdb";
const DEFAULT_PORT: u16 = 5432;
const PASSWORD_VARIABLE: &str = "EGGDB_PASSWORD";
const DEFAULT_TCP_KEEPALIVE_SECONDS: u64 = 300;

fn stamped(line: &str) -> String {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_micros() as u64);
    let stamp = render::utc_time(micros - micros % 1_000_000, TimeStyle::Readable);
    let one_line: String = line
        .chars()
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect();
    format!("{}  {one_line}\n", stamp.replace(".000000", ""))
}

fn database_name_of(file: &Path) -> String {
    file.file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("eggdb")
        .to_owned()
}

impl Session<'_> {
    fn check_mcp_tables(
        &self,
        db: &eggdb::Database,
        tables: &eggdb_sql::VisibleTables,
    ) -> Result<(), Failure> {
        let file = render::text(self.request.file.display().to_string().as_bytes());
        let (unknown, unservable, _) = super::mcp::check_tables(&DatabaseStore(db), tables, false)
            .map_err(|message| {
                Failure::database(
                    format!("cannot read {file}: {message}"),
                    Exit::DatabaseError,
                )
            })?;
        if !unknown.is_empty() {
            return Err(super::mcp::unknown_tables_failure(
                &file,
                &unknown,
                "--mcp-tables",
            ));
        }
        if !unservable.is_empty() {
            return Err(super::mcp::unservable_columns_failure(
                &file,
                &unservable,
                "--mcp-tables",
                "eggdb serve",
            ));
        }
        Ok(())
    }

    pub(super) fn serve(&mut self) -> Result<Exit, Failure> {
        super::foreign::allow_from_environment();
        super::model_endpoints::allow_from_environment();
        super::sparql::allow_load_from_environment();
        super::sparql::allow_service_from_environment();
        audit_server_events_from_environment(&self.request.file)
            .map_err(|message| Failure::usage(message, Some(CommandName::Serve)))?;
        let usage = |message: String| Failure::usage(message, Some(CommandName::Serve));
        let options = self.request.serve.clone();
        if let Some(url) = &options.restore_from
            && std::fs::symlink_metadata(&self.request.file).is_err()
        {
            let (file, _) = super::archive::restore_from_bucket_into(url, None, &self.request.file)
                .map_err(|message| {
                    Failure::database(
                        format!("--restore-from {url}: {message}"),
                        Exit::DatabaseError,
                    )
                })?;
            self.out
                .text(&format!("restored {file} from {url} into {}\n", self.file));
        }
        let user = options
            .user
            .clone()
            .unwrap_or_else(|| DEFAULT_USER.to_owned());
        let password = self.serve_password(&user)?;
        let max_connections = options.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS);
        let memory_budget = Arc::new(
            options
                .max_connection_memory
                .map_or_else(MemoryBudget::default, MemoryBudget::new),
        );
        if !password.is_ascii() {
            return Err(usage(
                "the password must be ASCII in this release: SASLprep, which normalizes other characters for SCRAM, is not implemented yet".to_owned(),
            ));
        }
        let address = options.listen.unwrap_or(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            DEFAULT_PORT,
        ));
        let listen_hosts: Vec<IpAddr> = [Some(address), options.mysql_listen, options.tds_listen]
            .into_iter()
            .flatten()
            .map(|listen| listen.ip())
            .collect();
        let tls = tls_setup(&options, &listen_hosts)?;
        let with_tls = tls.is_some();
        missing_socket_directory(&options).map_or(Ok(()), |message| Err(usage(message)))?;
        let listener = self.listener(
            "--listen",
            "PostgreSQL",
            address,
            eggdb_pgwire::listen(address, with_tls).map_err(|error| match error {
                ListenError::NotLoopback(_) => None,
                ListenError::Io(error) => Some(error),
            }),
        )?;
        let tds_listener = match options.tds_listen {
            Some(address) => Some(self.listener(
                "--tds-listen",
                "SQL Server",
                address,
                eggdb_tdswire::listen(address, with_tls).map_err(|error| match error {
                    eggdb_tdswire::ListenError::NotLoopback(_) => None,
                    eggdb_tdswire::ListenError::Io(error) => Some(error),
                }),
            )?),
            None => None,
        };
        let metrics_listener = match options.metrics_listen {
            Some(address) => Some(std::net::TcpListener::bind(address).map_err(|error| {
                Failure::database(
                    format!("cannot listen for metrics on {address}: {error}"),
                    Exit::DatabaseError,
                )
            })?),
            None => None,
        };
        let mcp_listener = match (options.mcp_listen, &options.mcp_token_file) {
            (None, None) if !options.mcp_tables.is_empty() => {
                return Err(usage(
                    "--mcp-tables limits the MCP endpoint: give it with --mcp-listen and --mcp-token-file".to_owned(),
                ));
            }
            (None, None) => None,
            (Some(_), None) | (None, Some(_)) => {
                return Err(usage(
                    "--mcp-listen and --mcp-token-file go together: agents must send the token as a Bearer token".to_owned(),
                ));
            }
            (Some(address), Some(token_file)) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--mcp-listen {address}: the MCP endpoint has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                let tokens = super::serve_mcp::read_tokens(token_file).map_err(usage)?;
                let listener = std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for MCP on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?;
                Some((listener, tokens))
            }
        };
        let resp_listener = match options.resp_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--resp-listen {address}: the RESP listener has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for RESP on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let memcached_listener = match options.memcached_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--memcached-listen {address}: memcached's protocol has no authentication, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for memcached on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let zookeeper_listener = match options.zookeeper_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--zookeeper-listen {address}: EggDB does not enforce ZooKeeper's ACLs yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for ZooKeeper on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let es_listener = match options.es_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--es-listen {address}: the REST API has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for the REST API on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let es_password = eggdb_crypto::sha256(password.as_bytes());
        let mongo_listener = match options.mongo_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--mongo-listen {address}: the MongoDB-compatible listener has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for MongoDB clients on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let bolt_listener = match options.bolt_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--bolt-listen {address}: the Bolt listener has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for Bolt on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let gremlin_listener = match options.gremlin_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--gremlin-listen {address}: the Gremlin listener has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for Gremlin clients on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let replication_listener = match options.replication_listen {
            Some(address) => {
                if !address.ip().is_loopback() && tls.is_none() {
                    return Err(usage(format!(
                        "--replication-listen {address}: replicas outside this machine connect over TLS; give --tls-certificate and --tls-key, or --tls-self-signed"
                    )));
                }
                if !self.request.file.is_file() && self.request.file.exists() {
                    return Err(usage("--replication-listen replicates one database file, not a directory of databases".to_owned()));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(format!("cannot listen for replicas on {address}: {error}"), Exit::DatabaseError)
                })?)
            }
            None => None,
        };
        let moves_listener = match options.moves_listen {
            Some(address) => {
                if !address.ip().is_loopback() && tls.is_none() {
                    return Err(usage(format!(
                        "--moves-listen {address}: servers outside this machine connect over TLS; give --tls-certificate and --tls-key, or --tls-self-signed"
                    )));
                }
                if !self.request.file.is_dir() {
                    return Err(usage("--moves-listen moves databases of a data directory; serve a directory (eggdb serve <directory>)".to_owned()));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(format!("cannot listen for database moves on {address}: {error}"), Exit::DatabaseError)
                })?)
            }
            None => None,
        };
        let cluster = match &options.cluster {
            Some(text) => {
                let members = super::serve_cluster::parse_members(text).map_err(usage)?;
                let node = options
                    .cluster_node
                    .ok_or_else(|| usage("--cluster needs --cluster-node <id>, this server's id in it".to_owned()))?;
                if options.replication_listen.is_some() || options.replica_of.is_some() || options.standby_of.is_some() {
                    return Err(usage(
                        "--cluster replicates through its own consensus; it goes without --replication-listen, --replica-of and --standby-of".to_owned(),
                    ));
                }
                if !self.request.file.is_file() && self.request.file.exists() {
                    return Err(usage("--cluster replicates one database file, not a directory of databases".to_owned()));
                }
                let settings = super::serve_cluster::ClusterSettings {
                    node,
                    members,
                    ca_file: options.cluster_ca.clone(),
                };
                let address = settings
                    .own_address()
                    .ok_or_else(|| usage(format!("--cluster-node {node} is not one of the ids in --cluster")))?
                    .to_owned();
                if !super::serve_cluster::is_loopback(&address) && tls.is_none() {
                    return Err(usage(format!(
                        "--cluster: node {node} listens on {address}, and peers outside this machine connect over TLS; give --tls-certificate and --tls-key, or --tls-self-signed"
                    )));
                }
                let listener = std::net::TcpListener::bind(&address).map_err(|error| {
                    Failure::database(format!("cannot listen for the cluster's peers on {address}: {error}"), Exit::DatabaseError)
                })?;
                Some((settings, listener))
            }
            None => {
                if options.cluster_node.is_some() || options.cluster_seed || options.cluster_ca.is_some() {
                    return Err(usage("--cluster-node, --cluster-seed and --cluster-ca go with --cluster".to_owned()));
                }
                None
            }
        };
        let kafka_listener = match options.kafka_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--kafka-listen {address}: the Kafka listener has no authentication or TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for Kafka clients on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let kafka_config = eggdb_kafka::ServerConfig {
            advertised: kafka_listener
                .as_ref()
                .and_then(|listener| listener.local_addr().ok())
                .unwrap_or(address),
            max_connections,
            memory_budget: Arc::clone(&memory_budget),
            credentials: Some((user.clone(), eggdb_crypto::sha256(password.as_bytes()))),
        };
        let amqp_listener = match options.amqp_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--amqp-listen {address}: the AMQP listener has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for AMQP clients on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let mqtt_listener = match options.mqtt_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--mqtt-listen {address}: the MQTT listener has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for MQTT clients on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let sqs_listener = match options.sqs_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--sqs-listen {address}: the SQS-compatible endpoint has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for SQS clients on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let sqs_secret = sqs_listener.as_ref().map(|_| password.clone());
        let s3_listener = match options.s3_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--s3-listen {address}: the S3-compatible endpoint has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for S3 clients on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let s3_secret = s3_listener.as_ref().map(|_| password.clone());
        let dynamodb_listener = match options.dynamodb_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--dynamodb-listen {address}: the DynamoDB-compatible endpoint has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for DynamoDB clients on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let dynamodb_secret = dynamodb_listener.as_ref().map(|_| password.clone());
        let observability_listener = match options.observability_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--observability-listen {address}: the metrics endpoint has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for metrics clients on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let server_password_sha256 = eggdb_crypto::sha256(password.as_bytes());
        let trino_listener = match options.trino_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--trino-listen {address}: the Trino protocol has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for the Trino protocol on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let flight_listener = match options.flight_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--flight-listen {address}: Arrow Flight SQL has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for Arrow Flight SQL on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let smtp_listener = match options.smtp_listen {
            Some(address) => {
                if self.request.read_only {
                    return Err(usage(format!(
                        "--smtp-listen {address}: a read-only server cannot store the mail it receives"
                    )));
                }
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--smtp-listen {address}: the SMTP receiver has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for SMTP on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let imap_listener = match options.imap_listen {
            Some(address) => {
                if self.request.read_only {
                    return Err(usage(format!(
                        "--imap-listen {address}: IMAP changes flags and mailboxes, which a read-only server cannot"
                    )));
                }
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--imap-listen {address}: the IMAP server has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for IMAP on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let jmap_listener = match options.jmap_listen {
            Some(address) => {
                if self.request.read_only {
                    return Err(usage(format!(
                        "--jmap-listen {address}: JMAP changes mailboxes and keywords, which a read-only server cannot"
                    )));
                }
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--jmap-listen {address}: the JMAP endpoint has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for JMAP on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let dav_listener = match options.dav_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--dav-listen {address}: the WebDAV server has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for WebDAV on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let sparql_listener = match options.sparql_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--sparql-listen {address}: the SPARQL endpoint has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for SPARQL on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let clickhouse_listener = match options.clickhouse_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--clickhouse-listen {address}: the ClickHouse HTTP interface has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for the ClickHouse HTTP interface on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let rest_listener = match options.rest_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--rest-listen {address}: the REST API has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for the REST API on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let cql_listener = match options.cql_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--cql-listen {address}: the CQL listener has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for CQL clients on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let mut cql_config = eggdb_cql::ServerConfig {
            user: user.clone(),
            password_sha256: eggdb_crypto::sha256(password.as_bytes()),
            database: String::new(),
            max_connections,
            memory_budget: Arc::clone(&memory_budget),
        };
        let stomp_listener = match options.stomp_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--stomp-listen {address}: the STOMP listener has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for STOMP clients on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let stomp_config = eggdb_stomp::ServerConfig {
            user: user.clone(),
            password_sha256: eggdb_crypto::sha256(password.as_bytes()),
            max_connections,
            memory_budget: Arc::clone(&memory_budget),
        };
        let nats_listener = match options.nats_listen {
            Some(address) => {
                if !address.ip().is_loopback() {
                    return Err(usage(format!(
                        "--nats-listen {address}: the NATS listener has no TLS yet, so it listens only on a loopback address"
                    )));
                }
                Some(std::net::TcpListener::bind(address).map_err(|error| {
                    Failure::database(
                        format!("cannot listen for NATS clients on {address}: {error}"),
                        Exit::DatabaseError,
                    )
                })?)
            }
            None => None,
        };
        let syslog_sockets = match options.syslog_listen {
            Some(address) => Some(bind_syslog(address, tls.is_some())?),
            None => None,
        };
        let nats_config = eggdb_nats::ServerConfig {
            user: user.clone(),
            password_sha256: eggdb_crypto::sha256(password.as_bytes()),
            max_connections,
            memory_budget: Arc::clone(&memory_budget),
            nkeys: options.nats_nkeys.clone(),
        };
        let mqtt_config = eggdb_mqtt::ServerConfig {
            user: user.clone(),
            password_sha256: eggdb_crypto::sha256(password.as_bytes()),
            max_connections,
            memory_budget: Arc::clone(&memory_budget),
        };
        let amqp_config = eggdb_amqp::ServerConfig {
            user: user.clone(),
            password_sha256: eggdb_crypto::sha256(password.as_bytes()),
            max_connections,
            memory_budget: Arc::clone(&memory_budget),
        };
        let bolt_config = eggdb_bolt::ServerConfig {
            user: user.clone(),
            password_sha256: eggdb_crypto::sha256(password.as_bytes()),
            max_connections,
            memory_budget: Arc::clone(&memory_budget),
        };
        let replication_credentials = super::serve_replication::ReplicationCredentials {
            user: user.clone(),
            password_sha256: eggdb_crypto::sha256(password.as_bytes()),
        };
        let gremlin_config = eggdb_gremlin::ServerConfig {
            user: user.clone(),
            password_sha256: eggdb_crypto::sha256(password.as_bytes()),
            max_connections,
        };
        let mongo_config = match &mongo_listener {
            Some(_) => Some(
                super::serve_mongo::config(&user, &password, max_connections, &memory_budget)
                    .map(|config| eggdb_mongowire::ServerConfig {
                        replica_set: options.mongo_replica_set.clone(),
                        ..config
                    })
                    .map_err(|error| {
                        Failure::database(
                            format!("cannot prepare the MongoDB-compatible listener: {error}"),
                            Exit::DatabaseError,
                        )
                    })?,
            ),
            None => None,
        };
        let resp_password = resp_listener
            .as_ref()
            .map(|_| eggdb_crypto::sha256(password.as_bytes()));
        let mysql_listener = match options.mysql_listen {
            Some(address) => Some(self.listener(
                "--mysql-listen",
                "MySQL",
                address,
                eggdb_mysqlwire::listen(address, with_tls).map_err(|error| match error {
                    eggdb_mysqlwire::ListenError::NotLoopback(_) => None,
                    eggdb_mysqlwire::ListenError::Io(error) => Some(error),
                }),
            )?),
            None => None,
        };
        let bound = listener.local_addr().map_err(|error| {
            Failure::database(format!("cannot listen: {error}"), Exit::DatabaseError)
        })?;
        if options.mysql_socket.is_some() && options.mysql_listen.is_none() {
            return Err(usage(
                "--mysql-socket needs --mysql-listen: the socket serves the same MySQL server"
                    .to_owned(),
            ));
        }
        if options.mysql_pipe.is_some() && options.mysql_listen.is_none() {
            return Err(usage(
                "--mysql-pipe needs --mysql-listen: the pipe serves the same MySQL server"
                    .to_owned(),
            ));
        }
        if options.tds_pipe.is_some() && options.tds_listen.is_none() {
            return Err(usage(
                "--tds-pipe needs --tds-listen: the pipe serves the same SQL Server server"
                    .to_owned(),
            ));
        }
        if options.mysql_pipe.is_some() && options.mysql_socket.is_some() {
            return Err(usage(
                "--mysql-pipe and --mysql-socket cannot be used together; pick one".to_owned(),
            ));
        }
        let socket_path = options.socket_dir.as_ref().map(|directory| {
            eggdb_platform::local_socket::postgresql_socket_path(directory, bound.port())
        });
        let local_listener = match &socket_path {
            Some(path) => Some(self.local_listener(path, "--socket-dir")?),
            None => None,
        };
        let mysql_local_listener = match (&options.mysql_socket, &options.mysql_pipe) {
            (Some(path), _) => Some(self.local_listener(path, "--mysql-socket")?),
            (None, Some(name)) => Some(self.named_pipe_listener(name, "--mysql-pipe", "MySQL")?),
            (None, None) => None,
        };
        let tds_local_listener = match &options.tds_pipe {
            Some(name) => Some(self.named_pipe_listener(name, "--tds-pipe", "SQL Server")?),
            None => None,
        };
        let mysql_bound = match &mysql_listener {
            Some(listener) => Some(listener.local_addr().map_err(|error| {
                Failure::database(format!("cannot listen: {error}"), Exit::DatabaseError)
            })?),
            None => None,
        };
        let tds_bound = match &tds_listener {
            Some(listener) => Some(listener.local_addr().map_err(|error| {
                Failure::database(format!("cannot listen: {error}"), Exit::DatabaseError)
            })?),
            None => None,
        };
        let mut salt = [0u8; SCRAM_SALT_BYTES];
        secure_random::fill(&mut salt).map_err(|error| {
            Failure::database(
                format!("cannot get random bytes from the operating system: {error}"),
                Exit::DatabaseError,
            )
        })?;
        let verifier = ScramVerifier::derive(password.as_bytes(), &salt, MIN_SCRAM_ITERATIONS);
        let mysql_credentials = mysql_listener
            .as_ref()
            .map(|_| eggdb_mysqlwire::MySqlCredentials::derive(password.as_bytes()));
        let tds_credentials = match &tds_listener {
            Some(_) => Some(
                eggdb_tdswire::TdsCredentials::derive(
                    password.as_bytes(),
                    eggdb_tdswire::DEFAULT_ITERATIONS,
                )
                .map_err(|error| {
                    Failure::database(
                        format!("cannot get random bytes from the operating system: {error}"),
                        Exit::DatabaseError,
                    )
                })?,
            ),
            None => None,
        };
        if let Some(address) = &options.replica_of
            && std::fs::symlink_metadata(&self.request.file).is_err()
        {
            let primary = super::serve_replication::PrimaryAddress {
                address: address.clone(),
                tls: super::serve_replication::client_tls(address, options.replica_of_ca.as_deref()).map_err(usage)?,
            };
            let credentials = super::serve_replication::ReplicationCredentials {
                user: user.clone(),
                password_sha256: eggdb_crypto::sha256(password.as_bytes()),
            };
            super::serve_replication::seed_from_primary(&primary, &self.request.file, &credentials)
                .map_err(|message| Failure::database(message, Exit::DatabaseError))?;
        }
        if let Some((settings, _)) = &cluster
            && options.cluster_seed
        {
            super::serve_cluster::seed_from_cluster(settings, &self.request.file, &replication_credentials)
                .map_err(|message| Failure::database(message, Exit::DatabaseError))?;
        }
        drop(password);
        let read_only = self.request.read_only;
        let access = if read_only {
            Access::ReadOnly
        } else {
            Access::Write {
                create: self.request.create,
            }
        };
        let mut directory = self.directory_databases(access)?;
        let (db, served_name) = if directory.is_empty() {
            (
                self.open(access, None)?,
                database_name_of(&self.request.file),
            )
        } else {
            let (name, first) = directory.remove(0);
            (super::Opened::Owned(Box::new(first)), name)
        };
        let mcp_tables = (!options.mcp_tables.is_empty())
            .then(|| eggdb_sql::VisibleTables::new(options.mcp_tables.iter().map(String::as_str)));
        if let Some(tables) = &mcp_tables {
            self.check_mcp_tables(&db, tables)?;
        }
        let idle_timeout = match options.idle_timeout_seconds {
            None => Some(DEFAULT_IDLE_TIMEOUT),
            Some(0) => None,
            Some(seconds) => Some(std::time::Duration::from_secs(seconds)),
        };
        let config = ServerConfig {
            user,
            verifier,
            database: options.database.unwrap_or(served_name),
            max_connections,
            authentication_timeout: DEFAULT_AUTHENTICATION_TIMEOUT,
            idle_timeout,
            memory_budget: Arc::clone(&memory_budget),
            auth_rules: self.auth_rules()?,
            tls: tls.as_ref().map(|setup| Arc::clone(&setup.postgresql)),
            logical_replication: options.pg_replication,
        };
        cql_config.database = config.database.clone();
        let mysql_config = mysql_credentials.map(|credentials| eggdb_mysqlwire::ServerConfig {
            user: config.user.clone(),
            credentials,
            database: config.database.clone(),
            max_connections: config.max_connections,
            authentication_timeout: eggdb_mysqlwire::DEFAULT_AUTHENTICATION_TIMEOUT,
            idle_timeout,
            memory_budget: Arc::clone(&memory_budget),
            tls: tls.as_ref().map(|setup| Arc::clone(&setup.mysql)),
            auth_rules: config.auth_rules.clone(),
            fips_only: fips_requested(&options),
        });
        let tds_config = tds_credentials.map(|credentials| eggdb_tdswire::ServerConfig {
            user: config.user.clone(),
            credentials,
            database: config.database.clone(),
            max_connections: config.max_connections,
            authentication_timeout: eggdb_tdswire::DEFAULT_AUTHENTICATION_TIMEOUT,
            idle_timeout,
            memory_budget: Arc::clone(&memory_budget),
            log_batches: options.log_batches,
            tls: tls.as_ref().map(|setup| Arc::clone(&setup.tds)),
            auth_rules: config.auth_rules.clone(),
        });
        let sockets = Sockets {
            postgresql: socket_path.clone(),
            mysql: options.mysql_socket.clone(),
            mysql_pipe: options.mysql_pipe.clone(),
            tds_pipe: options.tds_pipe.clone(),
        };
        let move_here = bound.to_string();
        let bound = Bound {
            postgresql: bound,
            mysql: mysql_bound,
            tds: tds_bound,
        };
        self.turn_on_tcp_keep_alives(
            options.tcp_keepalive_seconds,
            [
                Some(&listener),
                mysql_listener.as_ref(),
                tds_listener.as_ref(),
                metrics_listener.as_ref(),
                mcp_listener.as_ref().map(|(listener, _)| listener),
                resp_listener.as_ref(),
                memcached_listener.as_ref(),
                zookeeper_listener.as_ref(),
                es_listener.as_ref(),
                mongo_listener.as_ref(),
                bolt_listener.as_ref(),
                gremlin_listener.as_ref(),
                replication_listener.as_ref(),
                kafka_listener.as_ref(),
                amqp_listener.as_ref(),
                mqtt_listener.as_ref(),
                sqs_listener.as_ref(),
                s3_listener.as_ref(),
                dynamodb_listener.as_ref(),
                observability_listener.as_ref(),
                rest_listener.as_ref(),
                clickhouse_listener.as_ref(),
                trino_listener.as_ref(),
                sparql_listener.as_ref(),
                smtp_listener.as_ref(),
                imap_listener.as_ref(),
                jmap_listener.as_ref(),
                dav_listener.as_ref(),
                cql_listener.as_ref(),
                stomp_listener.as_ref(),
                nats_listener.as_ref(),
                syslog_sockets.as_ref().map(|(_, listener)| listener),
            ],
        );
        self.print_listening(bound, &config, read_only, tls.as_ref(), &sockets);
        self.out.finish()?;
        let _ = stop_signals::watch();
        let store = DatabaseStore(&db);
        let served_databases = ServedDirectory::new(ServedDirectorySetup {
            default_name: &config.database,
            default: &store,
            directory: self
                .request
                .file
                .is_dir()
                .then(|| self.request.file.clone()),
            options: self.database_options(Access::Write { create: true }, None),
            read_only,
            others: directory,
        });
        self.fire_instance_triggers(&store, InstanceEvent::Startup);
        let (events, received) = std::sync::mpsc::channel::<String>();
        let mysql_events = events.clone();
        let tds_events = events.clone();
        let syslog_events = events.clone();
        eggdb_sql::statement_statistics::collect(
            options
                .statement_statistics
                .unwrap_or(eggdb_sql::statement_statistics::DEFAULT_CAPACITY),
        );
        let held_telemetry = std::sync::Arc::new(super::serve_otlp::HeldTelemetry::default());
        let exports_telemetry = options.otlp_endpoint.is_some();
        if let Some(milliseconds) = options.log_slow_statements_ms {
            let slow_events = events.clone();
            let slow_spans = std::sync::Arc::clone(&held_telemetry);
            eggdb_sql::statement_statistics::log_slow_statements(
                std::time::Duration::from_millis(milliseconds),
                Box::new(move |statement| {
                    if exports_telemetry {
                        slow_spans.slow_statement(statement);
                    }
                    let _ = slow_events.send(stamped(&statement.to_string()));
                }),
            );
        }
        let syslog_collector = eggdb_observability::SyslogCollector::new(
            &store,
            &config.user,
            &config.database,
            max_connections,
            idle_timeout,
        );
        let syslog_collector = match &tls {
            Some(setup) => syslog_collector.with_tls(Arc::clone(&setup.postgresql)),
            None => syslog_collector,
        };
        if syslog_sockets.is_some() {
            syslog_collector.prepare().map_err(|error| {
                Failure::database(
                    format!("cannot create the syslog table: {error}"),
                    Exit::DatabaseError,
                )
            })?;
        }
        let observability_endpoint = eggdb_observability::Endpoint::new(
            &store,
            &config.user,
            server_password_sha256,
            &config.database,
        )
        .with_memory_budget(Arc::clone(&memory_budget));
        let rate_limiter = options
            .rate_limit
            .map(|requests| eggdb_http::RateLimiter::new(requests, requests.saturating_mul(2)));
        let mut rest_endpoint = eggdb_api::Endpoint::new(
            &store,
            &config.user,
            server_password_sha256,
            &config.database,
            "public",
        );
        if let Some(rules) = &config.auth_rules {
            rest_endpoint = rest_endpoint.with_auth_rules(Arc::clone(rules));
        }
        let clickhouse_endpoint = eggdb_clickhouse::Endpoint::new(
            &store,
            &config.user,
            server_password_sha256,
            &config.database,
        );
        let trino_endpoint = eggdb_trino::Endpoint::new(
            &store,
            &config.user,
            server_password_sha256,
            &config.database,
        );
        let flight_endpoint = eggdb_flight::Endpoint::new(
            &store,
            &config.user,
            server_password_sha256,
            &config.database,
        );
        let mcp_opener = super::serve_mcp::SharedOpener { db: &db };
        let mcp_oauth = super::serve_mcp::oauth_from_environment().map_err(|message| Failure::usage(message, Some(CommandName::Serve)))?;
        let mcp_endpoint = match mcp_listener.as_ref() {
            None => None,
            Some((_, tokens)) => {
                let endpoint = super::serve_mcp::McpEndpoint::new(
                    &mcp_opener,
                    &config.database,
                    tokens.clone(),
                    mcp_tables.clone(),
                    eggdb_mcp::audit::audit_path_for(&self.request.file),
                );
                Some(match mcp_oauth {
                    Some((issuer, jwks, resource)) => endpoint
                        .with_oauth(issuer, &jwks, resource)
                        .map_err(|message| Failure::usage(message, Some(CommandName::Serve)))?,
                    None => endpoint,
                })
            }
        };
        let resp_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::RESP_COLLECTIONS,
        };
        let memcached_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::MEMCACHED_COLLECTIONS,
        };
        let memcached_shared = eggdb_memcached::Shared {
            keyspace: &memcached_database,
            next_cas: std::sync::atomic::AtomicU64::new(1),
            counters: Default::default(),
            lru: eggdb_memcached::Lru::with_limit(options.memcached_max_memory.unwrap_or(0)),
        };
        let es_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::ES_COLLECTIONS,
        };
        let es_readers = match &options.es_users {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|error| usage(format!("cannot read --es-users {}: {error}", path.display())))?;
                eggdb_esrest::parse_search_users(&text)
                    .map_err(|reason| usage(format!("--es-users {}: {reason}", path.display())))?
            }
            None => Vec::new(),
        };
        let es_api = eggdb_esrest::Api::new(&es_database, &config.user, es_password)
            .with_memory_budget(Arc::clone(&memory_budget))
            .with_search_users(es_readers, options.es_permission_field.as_deref().unwrap_or("readers"));
        let replication_mode = match options.synchronous_replicas {
            Some(acknowledgements) => eggdb::ReplicationMode::Synchronous { acknowledgements },
            None => eggdb::ReplicationMode::Asynchronous,
        };
        let replication_tls = tls.as_ref().map(|setup| Arc::clone(&setup.postgresql));
        let peer_address = |address: &String| -> Result<super::serve_replication::PrimaryAddress, Failure> {
            Ok(super::serve_replication::PrimaryAddress {
                address: address.clone(),
                tls: super::serve_replication::client_tls(address, options.replica_of_ca.as_deref()).map_err(usage)?,
            })
        };
        if let Some((settings, _)) = &cluster {
            db.fence(format!("this server is joining its cluster as node {}; retry the write shortly", settings.node));
        }
        let replicates = replication_listener.is_some() || options.replica_of.is_some();
        let following = options.replica_of.as_ref().map(&peer_address).transpose()?;
        if let Some(primary) = &following {
            db.fence(format!("this server is a replica of {}; send writes to the primary", primary.address));
        }
        let starting_primary = match (&replication_listener, &following) {
            (Some(_), None) => Some(db.start_primary(replication_mode).map_err(|error| {
                Failure::database(format!("cannot start replicating {}: {error}", self.file), Exit::DatabaseError)
            })?),
            _ => None,
        };
        let peers = options.replication_peers.iter().map(&peer_address).collect::<Result<Vec<_>, Failure>>()?;
        let advertise = options
            .replication_advertise
            .clone()
            .or_else(|| options.replication_listen.filter(|address| !address.ip().is_unspecified()).map(|address| address.to_string()));
        let replication_node = super::serve_failover::ReplicationNode::new(
            &db,
            replication_credentials.clone(),
            replication_mode,
            starting_primary,
            following,
            peers,
            advertise,
            options.failover_after_seconds.map(std::time::Duration::from_secs),
            super::serve_replication::replica_id_of(&self.request.file),
            options.replica_of_ca.clone(),
        );
        if replication_listener.is_some() {
            eggdb_sql::set_switchover_hook(super::serve_failover::request_switchover);
        }
        let moves_shards = replicates || cluster.is_some();
        if moves_shards {
            eggdb_sql::set_shard_move_hook(super::serve_replication::request_shard_move);
            eggdb_sql::set_tenant_move_hook(super::serve_replication::request_tenant_move);
        }
        if moves_listener.is_some() {
            eggdb_sql::set_database_move_hook(super::serve_database_moves::request_database_move);
        }
        let shard_owner = match &cluster {
            Some((settings, _)) => settings.node,
            None => u64::from(super::serve_replication::replica_id_of(&self.request.file)),
        };
        let shard_ca = options.cluster_ca.clone().or_else(|| options.replica_of_ca.clone());
        if options.cdc_capture_engine && !options.pg_replication && options.cdc_webhook.is_none() && options.cdc_topic.is_none() && options.cdc_queue.is_none() {
            return Err(usage("--cdc-capture engine captures every change for a sink: give --cdc-webhook, --cdc-topic or --cdc-queue too".to_owned()));
        }
        if options.cdc_capture_engine || options.pg_replication {
            db.capture_rows(eggdb_sql::cdc::is_row_key).map_err(|error| {
                Failure::database(format!("cannot capture changes in {}: {error}", self.file), Exit::DatabaseError)
            })?;
        }
        if replicates {
            replication_node.publish_role();
            eggdb_sql::set_replication_hooks(super::serve_failover::freshness_hook, super::serve_failover::token_hook);
        }
        if let Some(directory) = &options.standby_of {
            if replicates {
                return Err(usage("--standby-of follows a change archive; it goes without --replica-of and --replication-listen".to_owned()));
            }
            db.fence(format!("this server is a standby fed by the change archive in {}; send writes to the primary", directory.display()));
        }
        if options.apply_delay_seconds.is_some() && options.standby_of.is_none() {
            return Err(usage("--apply-delay delays a standby; give --standby-of <archive directory>".to_owned()));
        }
        let graph_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::GRAPH_COLLECTIONS,
        };
        let rdf_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::RDF_COLLECTIONS,
        };
        let mail_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::MAIL_COLLECTIONS,
        };
        let mail_config = eggdb_mail::ServerConfig {
            user: config.user.clone(),
            password_sha256: server_password_sha256,
            max_connections,
            max_message_bytes: 25 << 20,
        };
        let jmap_endpoint = eggdb_mail::JmapEndpoint::new(&mail_database, mail_config.clone());
        let dav_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::DAV_COLLECTIONS,
        };
        let dav_endpoint = eggdb_dav::Endpoint::new(&dav_database, &config.user, server_password_sha256, read_only);
        let sparql_endpoint = eggdb_rdf::Endpoint::new(
            &rdf_database,
            0,
            &config.user,
            server_password_sha256,
            read_only,
        );
        let kafka_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::KAFKA_COLLECTIONS,
        };
        let queue_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::QUEUE_COLLECTIONS,
        };
        let mqtt_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::MQTT_COLLECTIONS,
        };
        let zookeeper_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::ZOOKEEPER_COLLECTIONS,
        };
        let nats_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::NATS_COLLECTIONS,
        };
        let sqs_endpoint = match (&sqs_listener, &sqs_secret) {
            (Some(listener), Some(secret)) => {
                let base_url = listener.local_addr().map_or_else(
                    |_| "http://127.0.0.1".to_owned(),
                    |address| format!("http://{address}"),
                );
                Some(eggdb_sqs::Endpoint::new(
                    &queue_database,
                    &base_url,
                    &config.user,
                    secret,
                ))
            }
            _ => None,
        };
        let object_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::OBJECT_COLLECTIONS,
        };
        let s3_endpoint = s3_secret
            .as_ref()
            .map(|secret| eggdb_s3::Endpoint::new(&object_database, &config.user, secret));
        let dynamodb_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::DYNAMODB_COLLECTIONS,
        };
        let dynamodb_endpoint = dynamodb_secret
            .as_ref()
            .map(|secret| eggdb_dynamodb::Endpoint::new(&dynamodb_database, &config.user, secret));
        let mongo_database = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::MONGO_COLLECTIONS,
        };
        let mongo_store =
            eggdb_documents::Store::new(&mongo_database, super::serve_mongo::store_seed());
        let resp_limited = options.resp_max_memory.and_then(|limit| {
            let policy = options
                .resp_eviction
                .unwrap_or(eggdb_resp::EvictionPolicy::NoEviction);
            eggdb_resp::EvictingKeyspace::new(&resp_database, limit, policy).ok()
        });
        let resp_keyspace: &(dyn eggdb_resp::Keyspace + Sync) = match &resp_limited {
            Some(limited) => limited,
            None => &resp_database,
        };
        let resp_config = eggdb_resp::ServerConfig {
            password_sha256: resp_password,
            max_connections: config.max_connections,
            memory_budget: Arc::clone(&memory_budget),
            cluster: options.resp_cluster,
        };
        let served = std::thread::scope(|scope| {
            let server = scope.spawn(|| {
                let log = move |line: &str| {
                    let _ = events.send(stamped(line));
                };
                let served = eggdb_pgwire::serve_directory(
                    &served_databases,
                    &listener,
                    local_listener.as_ref(),
                    &config,
                    &stop_signals::requested,
                    &log,
                );
                eggdb_sql::statement_statistics::stop_logging_slow_statements();
                served
            });
            let mysql_server = scope.spawn(|| match (&mysql_listener, &mysql_config) {
                (Some(listener), Some(config)) => {
                    let log = move |line: &str| {
                        let _ = mysql_events.send(stamped(&format!("mysql {line}")));
                    };
                    eggdb_mysqlwire::serve_directory(
                        &served_databases,
                        listener,
                        mysql_local_listener.as_ref(),
                        config,
                        &stop_signals::requested,
                        &log,
                    )
                }
                _ => {
                    drop(mysql_events);
                    Ok(0)
                }
            });
            let observability_server = scope.spawn(|| match &observability_listener {
                Some(listener) => eggdb_http::serve_replies_with_limits(
                    listener,
                    &|request| observability_endpoint.reply(request),
                    &stop_signals::requested,
                    &eggdb_http::Limits::new(
                        eggdb_observability::MAX_REQUEST_BYTES,
                        &memory_budget,
                    ).with_rate_limit(rate_limiter.as_ref())
                    .decoding_content_encoding_of(eggdb_observability::wants_content_decoded),
                ),
                None => Ok(()),
            });
            let rest_server = scope.spawn(|| match &rest_listener {
                Some(listener) => eggdb_http::serve_replies_with_limits(
                    listener,
                    &|request| rest_endpoint.reply(request),
                    &stop_signals::requested,
                    &eggdb_http::Limits::new(eggdb_api::MAX_REQUEST_BYTES, &memory_budget).with_rate_limit(rate_limiter.as_ref())
                        .decoding_content_encoding(),
                ),
                None => Ok(()),
            });
            let clickhouse_server = scope.spawn(|| match &clickhouse_listener {
                Some(listener) => eggdb_http::serve_with_limits(
                    listener,
                    &|request| clickhouse_endpoint.answer(request),
                    &stop_signals::requested,
                    &eggdb_http::Limits::new(eggdb_clickhouse::MAX_REQUEST_BYTES, &memory_budget)
                        .with_rate_limit(rate_limiter.as_ref())
                        .decoding_content_encoding(),
                ),
                None => Ok(()),
            });
            let smtp_server = scope.spawn(|| match &smtp_listener {
                Some(listener) => eggdb_mail::serve_smtp(&mail_database, listener, &mail_config, &stop_signals::requested),
                None => Ok(()),
            });
            let imap_server = scope.spawn(|| match &imap_listener {
                Some(listener) => eggdb_mail::serve_imap(&mail_database, listener, &mail_config, &stop_signals::requested),
                None => Ok(()),
            });
            let jmap_server = scope.spawn(|| match &jmap_listener {
                Some(listener) => eggdb_http::serve_with_limits(
                    listener,
                    &|request| jmap_endpoint.answer(request),
                    &stop_signals::requested,
                    &eggdb_http::Limits::new(eggdb_mail::MAX_JMAP_REQUEST_BYTES, &memory_budget)
                        .with_rate_limit(rate_limiter.as_ref())
                        .decoding_content_encoding(),
                ),
                None => Ok(()),
            });
            let dav_server = scope.spawn(|| match &dav_listener {
                Some(listener) => eggdb_http::serve_with_limits(
                    listener,
                    &|request| dav_endpoint.answer(request),
                    &stop_signals::requested,
                    &eggdb_http::Limits::new(eggdb_dav::MAX_REQUEST_BYTES, &memory_budget)
                        .with_rate_limit(rate_limiter.as_ref())
                        .decoding_content_encoding(),
                ),
                None => Ok(()),
            });
            let sparql_server = scope.spawn(|| match &sparql_listener {
                Some(listener) => eggdb_http::serve_with_limits(
                    listener,
                    &|request| sparql_endpoint.answer(request),
                    &stop_signals::requested,
                    &eggdb_http::Limits::new(eggdb_rdf::MAX_REQUEST_BYTES, &memory_budget)
                        .with_rate_limit(rate_limiter.as_ref())
                        .decoding_content_encoding(),
                ),
                None => Ok(()),
            });
            let trino_server = scope.spawn(|| match &trino_listener {
                Some(listener) => eggdb_http::serve_with_limits(
                    listener,
                    &|request| trino_endpoint.answer(request),
                    &stop_signals::requested,
                    &eggdb_http::Limits::new(eggdb_trino::MAX_REQUEST_BYTES, &memory_budget)
                        .with_rate_limit(rate_limiter.as_ref())
                        .decoding_content_encoding(),
                ),
                None => Ok(()),
            });
            let flight_server = scope.spawn(|| match &flight_listener {
                Some(listener) => {
                    eggdb_flight::serve(&flight_endpoint, listener, max_connections, &stop_signals::requested)
                }
                None => Ok(()),
            });
            let cql_server = scope.spawn(|| match &cql_listener {
                Some(listener) => {
                    eggdb_cql::serve(&store, listener, &cql_config, &stop_signals::requested)
                }
                None => Ok(()),
            });
            let job_scheduler = scope
                .spawn(|| super::serve_jobs::run_scheduled_jobs(&store, &stop_signals::requested));
            let report_delivery = scope
                .spawn(|| super::serve_jobs::deliver_reports(&store, &stop_signals::requested));
            let mail_delivery = scope
                .spawn(|| super::serve_jobs::deliver_database_mail(&store, &stop_signals::requested));
            let cdc_delivery = scope.spawn(|| {
                if let Some(url) = &options.cdc_webhook {
                    super::serve_jobs::deliver_changes(
                        &store,
                        &config.database,
                        url,
                        options.cdc_capture_engine,
                        &stop_signals::requested,
                    )
                }
            });
            scope.spawn(|| {
                if let Some(endpoint) = &options.otlp_endpoint {
                    super::serve_otlp::export_telemetry(
                        endpoint,
                        &|| served_databases.with_databases(super::serve_metrics::families),
                        &held_telemetry,
                        &stop_signals::requested,
                    );
                }
            });
            let metrics_server = scope.spawn(|| match &metrics_listener {
                Some(listener) => eggdb_http::serve_with_limits(
                    listener,
                    &|request| {
                        served_databases.with_databases(|databases| {
                            super::serve_metrics::answer(request, databases)
                        })
                    },
                    &stop_signals::requested,
                    &eggdb_http::Limits::new(eggdb_http::MAX_BODY_BYTES, &memory_budget).with_rate_limit(rate_limiter.as_ref()),
                ),
                None => Ok(()),
            });
            let mcp_server = scope.spawn(|| match (&mcp_listener, &mcp_endpoint) {
                (Some((listener, _)), Some(endpoint)) => eggdb_http::serve_with_limits(
                    listener,
                    &|request| endpoint.answer(request),
                    &stop_signals::requested,
                    &eggdb_http::Limits::new(eggdb_http::MAX_BODY_BYTES, &memory_budget).with_rate_limit(rate_limiter.as_ref()),
                ),
                _ => Ok(()),
            });
            let memcached_server = scope.spawn(|| match &memcached_listener {
                Some(listener) => eggdb_memcached::serve(
                    &memcached_shared,
                    listener,
                    eggdb_memcached::ServerConfig {
                        max_connections: config.max_connections,
                        memory_budget: Arc::clone(&memory_budget),
                    },
                    &stop_signals::requested,
                ),
                None => Ok(()),
            });
            let zookeeper_server = scope.spawn(|| match &zookeeper_listener {
                Some(listener) => eggdb_zookeeper::serve(
                    &zookeeper_database,
                    listener,
                    &eggdb_zookeeper::ServerConfig {
                        max_connections: config.max_connections,
                        ..eggdb_zookeeper::ServerConfig::default()
                    },
                    &stop_signals::requested,
                ),
                None => Ok(()),
            });
            let es_server = scope.spawn(|| match &es_listener {
                Some(listener) => eggdb_http::serve_replies_with_limits(
                    listener,
                    &|request| es_api.reply(request),
                    &stop_signals::requested,
                    &eggdb_http::Limits::new(eggdb_http::MAX_BODY_BYTES, &memory_budget).with_rate_limit(rate_limiter.as_ref())
                        .answering_over_budget_with(eggdb_esrest::over_memory_budget)
                        .decoding_content_encoding(),
                ),
                None => Ok(()),
            });
            let mqtt_server = scope.spawn(|| match &mqtt_listener {
                Some(listener) => eggdb_mqtt::serve(
                    &mqtt_database,
                    listener,
                    &mqtt_config,
                    &stop_signals::requested,
                ),
                None => Ok(()),
            });
            let stomp_server = scope.spawn(|| match &stomp_listener {
                Some(listener) => eggdb_stomp::serve(
                    &queue_database,
                    listener,
                    &stomp_config,
                    &stop_signals::requested,
                ),
                None => Ok(()),
            });
            let cdc_topic_delivery = scope.spawn(|| {
                if let Some(topic) = &options.cdc_topic {
                    super::serve_jobs::deliver_changes_to_topic(
                        &store,
                        &config.database,
                        &kafka_database,
                        topic,
                        options.cdc_capture_engine,
                        &stop_signals::requested,
                    );
                }
            });
            let cdc_queue_delivery = scope.spawn(|| {
                if let Some(queue) = &options.cdc_queue {
                    super::serve_jobs::deliver_changes_to_queue(
                        &store,
                        &config.database,
                        &queue_database,
                        queue,
                        options.cdc_capture_engine,
                        &stop_signals::requested,
                    );
                }
            });
            let syslog_server = scope.spawn(|| match &syslog_sockets {
                Some((datagrams, listener)) => {
                    let log = move |line: &str| {
                        let _ = syslog_events.send(stamped(&format!("syslog {line}")));
                    };
                    syslog_collector.serve(
                        datagrams.as_ref(),
                        Some(listener),
                        &stop_signals::requested,
                        &log,
                    )
                }
                None => {
                    drop(syslog_events);
                    Ok(())
                }
            });
            let nats_server = scope.spawn(|| match &nats_listener {
                Some(listener) => {
                    eggdb_nats::serve_with_jetstream(&nats_database, listener, &nats_config, &stop_signals::requested)
                }
                None => Ok(()),
            });
            let dynamodb_server = scope.spawn(|| match (&dynamodb_listener, &dynamodb_endpoint) {
                (Some(listener), Some(endpoint)) => eggdb_http::serve_with_limits(
                    listener,
                    &|request| endpoint.answer(request),
                    &stop_signals::requested,
                    &eggdb_http::Limits::new(16 << 20, &memory_budget).with_rate_limit(rate_limiter.as_ref())
                        .answering_over_budget_with(eggdb_dynamodb::over_memory_budget),
                ),
                _ => Ok(()),
            });
            let s3_server = scope.spawn(|| match (&s3_listener, &s3_endpoint) {
                (Some(listener), Some(endpoint)) => eggdb_http::serve_replies_with_limits(
                    listener,
                    &|request| endpoint.reply(request),
                    &stop_signals::requested,
                    &eggdb_http::Limits::new(eggdb_s3::MAX_REQUEST_BYTES, &memory_budget).with_rate_limit(rate_limiter.as_ref())
                        .answering_over_budget_with(eggdb_s3::over_memory_budget),
                ),
                _ => Ok(()),
            });
            let sqs_server = scope.spawn(|| match (&sqs_listener, &sqs_endpoint) {
                (Some(listener), Some(endpoint)) => eggdb_http::serve_with_limits(
                    listener,
                    &|request| endpoint.answer(request),
                    &stop_signals::requested,
                    &eggdb_http::Limits::new(eggdb_http::MAX_BODY_BYTES, &memory_budget).with_rate_limit(rate_limiter.as_ref()),
                ),
                _ => Ok(()),
            });
            let amqp_server = scope.spawn(|| match &amqp_listener {
                Some(listener) => eggdb_amqp::serve(
                    &queue_database,
                    listener,
                    &amqp_config,
                    &stop_signals::requested,
                ),
                None => Ok(()),
            });
            let kafka_server = scope.spawn(|| match &kafka_listener {
                Some(listener) => eggdb_kafka::serve(
                    &kafka_database,
                    listener,
                    &kafka_config,
                    &stop_signals::requested,
                ),
                None => Ok(()),
            });
            let bolt_server = scope.spawn(|| match &bolt_listener {
                Some(listener) => eggdb_bolt::serve(
                    &graph_database,
                    listener,
                    &bolt_config,
                    &stop_signals::requested,
                ),
                None => Ok(()),
            });
            let replication_server = scope.spawn(|| match &replication_listener {
                Some(listener) => super::serve_replication::serve_primary(
                    listener,
                    &replication_node,
                    replication_tls.clone(),
                    &stop_signals::requested,
                ),
                None => Ok(()),
            });
            let replica_follower = scope.spawn(|| {
                if !replicates {
                    return Ok(());
                }
                let outcome = super::serve_replication::follow_primary(&replication_node, &stop_signals::requested);
                if let Err(message) = &outcome {
                    eprintln!("eggdb serve: the replica stopped following its primary: {message}");
                }
                outcome
            });
            let database_lender = scope.spawn(|| match &moves_listener {
                Some(listener) => super::serve_database_moves::serve_database_moves(
                    listener,
                    &served_databases,
                    &replication_credentials,
                    replication_tls.clone(),
                    &stop_signals::requested,
                ),
                None => Ok(()),
            });
            let database_arrivals = scope.spawn(|| {
                if moves_listener.is_some() {
                    super::serve_database_moves::serve_database_arrivals(
                        &served_databases,
                        &replication_credentials,
                        shard_ca.as_deref(),
                        &move_here,
                        &stop_signals::requested,
                    );
                }
            });
            let shard_mover = scope.spawn(|| {
                if moves_shards {
                    super::serve_replication::serve_shard_moves(
                        &db,
                        &replication_credentials,
                        shard_ca.as_deref(),
                        shard_owner,
                        &stop_signals::requested,
                    );
                }
            });
            let cluster_node = scope.spawn(|| match &cluster {
                Some((settings, listener)) => {
                    let outcome = super::serve_cluster::run_cluster(
                        &db,
                        settings,
                        listener,
                        replication_tls.clone(),
                        &replication_credentials,
                        &stop_signals::requested,
                    );
                    if let Err(message) = &outcome {
                        eprintln!("eggdb serve: the cluster node stopped: {message}");
                    }
                    outcome
                }
                None => Ok(()),
            });
            let standby_follower = scope.spawn(|| match &options.standby_of {
                Some(directory) => {
                    let delay = std::time::Duration::from_secs(options.apply_delay_seconds.unwrap_or(0));
                    let outcome = super::serve_standby::follow_archive(&db, directory, delay, &stop_signals::requested);
                    if let Err(message) = &outcome {
                        eprintln!("eggdb serve: the standby stopped following its change archive: {message}");
                    }
                    outcome
                }
                None => Ok(()),
            });
            let replication_monitor = scope.spawn(|| {
                if replicates {
                    replication_node.monitor(&stop_signals::requested);
                }
            });
            let gremlin_server = scope.spawn(|| match &gremlin_listener {
                Some(listener) => eggdb_gremlin::serve(
                    &graph_database,
                    listener,
                    &gremlin_config,
                    &stop_signals::requested,
                ),
                None => Ok(()),
            });
            let mongo_server = scope.spawn(|| match (&mongo_listener, &mongo_config) {
                (Some(listener), Some(mongo_config)) => eggdb_mongowire::serve(
                    &mongo_store,
                    listener,
                    mongo_config,
                    &stop_signals::requested,
                ),
                _ => Ok(()),
            });
            let resp_server = scope.spawn(|| match &resp_listener {
                Some(listener) => eggdb_resp::serve(
                    resp_keyspace,
                    listener,
                    &resp_config,
                    &stop_signals::requested,
                    &|_| {},
                )
                .map(|_| ()),
                None => Ok(()),
            });
            let tds_server = scope.spawn(|| match (&tds_listener, &tds_config) {
                (Some(listener), Some(config)) => {
                    let log = move |line: &str| {
                        let _ = tds_events.send(stamped(&format!("tds {line}")));
                    };
                    eggdb_tdswire::serve_directory_with_local(
                        &served_databases,
                        listener,
                        tds_local_listener.as_ref(),
                        config,
                        &stop_signals::requested,
                        &log,
                    )
                }
                _ => {
                    drop(tds_events);
                    Ok(0)
                }
            });
            for line in received {
                if exports_telemetry {
                    held_telemetry.log(&line);
                }
                self.messages.text(&line);
                let _ = self.messages.finish();
            }
            let unexpected = || {
                Err(std::io::Error::other(
                    "the server thread stopped unexpectedly",
                ))
            };
            let postgresql = server.join().unwrap_or_else(|_| unexpected());
            let mysql = mysql_server.join().unwrap_or_else(|_| unexpected());
            let tds = tds_server.join().unwrap_or_else(|_| unexpected());
            let _ = metrics_server.join();
            let _ = mcp_server.join();
            let _ = resp_server.join();
            let _ = memcached_server.join();
            let _ = zookeeper_server.join();
            let _ = es_server.join();
            let _ = mongo_server.join();
            let _ = bolt_server.join();
            let _ = gremlin_server.join();
            let _ = replication_server.join();
            let _ = replica_follower.join();
            let _ = replication_monitor.join();
            let _ = standby_follower.join();
            let _ = cluster_node.join();
            let _ = shard_mover.join();
            let _ = database_lender.join();
            let _ = database_arrivals.join();
            let _ = kafka_server.join();
            let _ = amqp_server.join();
            let _ = mqtt_server.join();
            let _ = nats_server.join();
            let _ = syslog_server.join();
            let _ = stomp_server.join();
            let _ = cql_server.join();
            let _ = observability_server.join();
            let _ = rest_server.join();
            let _ = clickhouse_server.join();
            let _ = trino_server.join();
            let _ = flight_server.join();
            let _ = sparql_server.join();
            let _ = smtp_server.join();
            let _ = imap_server.join();
            let _ = jmap_server.join();
            let _ = dav_server.join();
            let _ = cdc_queue_delivery.join();
            let _ = cdc_topic_delivery.join();
            let _ = sqs_server.join();
            let _ = s3_server.join();
            let _ = dynamodb_server.join();
            let _ = job_scheduler.join();
            let _ = report_delivery.join();
            let _ = mail_delivery.join();
            let _ = cdc_delivery.join();
            postgresql
                .and_then(|open| mysql.map(|mysql_open| open + mysql_open))
                .and_then(|open| tds.map(|tds_open| open + tds_open))
        });
        drop(listener);
        drop(mysql_listener);
        drop(tds_listener);
        for path in [socket_path.as_ref(), options.mysql_socket.as_ref()]
            .into_iter()
            .flatten()
        {
            let _ = std::fs::remove_file(path);
        }
        let open = served.map_err(|error| {
            Failure::database(format!("the server failed: {error}"), Exit::DatabaseError)
        })?;
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .boolean("stopped", true)
                    .number("connections_closed", open as u64)
                    .line(),
            ),
            _ => {
                if open > 0 {
                    self.out.text(&format!(
                        "stopping: {open} connection{} open; finished their statements and rolled back open transactions\n",
                        if open == 1 { " was" } else { "s were" }
                    ));
                }
                self.out.text("stopped\n");
            }
        }
        self.fire_instance_triggers(&store, InstanceEvent::Shutdown);
        for other in served_databases.into_databases() {
            self.close_database(other)?;
        }
        self.close(db)?;
        Ok(Exit::Done)
    }

    fn fire_instance_triggers(&mut self, store: &DatabaseStore<'_>, event: InstanceEvent) {
        let mut session = eggdb_sql::Session::with_dialect(store, eggdb_sql::Dialect::Oracle);
        let (fired, name) = match event {
            InstanceEvent::Startup => (session.fire_startup_triggers(), "STARTUP"),
            InstanceEvent::Shutdown => (session.fire_shutdown_triggers(), "SHUTDOWN"),
        };
        if let Err(error) = fired {
            self.out.text(&format!(
                "warning: an Oracle {name} trigger failed and was rolled back: {error}\n"
            ));
        }
        let _ = session.close();
    }

    fn turn_on_tcp_keep_alives<'l>(
        &mut self,
        seconds: Option<u64>,
        listeners: impl IntoIterator<Item = Option<&'l std::net::TcpListener>>,
    ) {
        let seconds = seconds.unwrap_or(DEFAULT_TCP_KEEPALIVE_SECONDS);
        if seconds == 0 {
            return;
        }
        let Ok(keep_alive) = TcpKeepAlive::after_idle(std::time::Duration::from_secs(seconds))
        else {
            return;
        };
        for listener in listeners.into_iter().flatten() {
            let Err(error) = eggdb_platform::set_tcp_keep_alive(listener, keep_alive) else {
                continue;
            };
            let address = listener
                .local_addr()
                .map_or_else(|_| "a listener".to_owned(), |address| address.to_string());
            self.messages.text(&stamped(&format!(
                "TCP keep-alives are off for connections to {address}: {error}"
            )));
            let _ = self.messages.finish();
            if error.kind() == eggdb_platform::ErrorKind::Unsupported {
                return;
            }
        }
    }

    fn auth_rules(&self) -> Result<Option<Arc<eggdb_auth::AuthRules>>, Failure> {
        let Some(path) = &self.request.serve.auth_rules else {
            return Ok(None);
        };
        eggdb_auth::AuthRules::load(path)
            .map(|rules| Some(Arc::new(rules)))
            .map_err(|error| {
                Failure::usage(
                    format!("--auth-rules {}: {error}", path.display()),
                    Some(CommandName::Serve),
                )
            })
    }

    fn directory_databases(
        &self,
        access: Access,
    ) -> Result<Vec<(String, eggdb::Database)>, Failure> {
        if !self.request.file.is_dir() {
            return Ok(Vec::new());
        }
        let entries = std::fs::read_dir(&self.request.file).map_err(|error| {
            Failure::database(
                format!(
                    "cannot list the data directory {}: {error}",
                    self.file_display
                ),
                Exit::DatabaseError,
            )
        })?;
        let mut paths: Vec<std::path::PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|extension| extension == "eggdb")
            })
            .collect();
        paths.sort();
        if paths.is_empty() {
            return Err(Failure::usage(
                format!(
                    "the data directory {} holds no .eggdb files; create one with eggdb sql {}/<name>.eggdb --create",
                    self.file_display, self.file_display
                ),
                Some(CommandName::Serve),
            ));
        }
        paths
            .into_iter()
            .map(|path| {
                eggdb::Database::open_with(&path, self.database_options(access, None))
                    .map(|db| (database_name_of(&path), db))
                    .map_err(|error| {
                        Failure::database(
                            format!("cannot open {}: {error}", path.display()),
                            Exit::DatabaseError,
                        )
                    })
            })
            .collect()
    }

    fn listener(
        &self,
        option: &str,
        protocol: &str,
        address: SocketAddr,
        opened: Result<std::net::TcpListener, Option<std::io::Error>>,
    ) -> Result<std::net::TcpListener, Failure> {
        match opened {
            Ok(listener) => Ok(listener),
            Err(None) => Err(Failure::Database {
                message: format!(
                    "cannot listen on {address}: it is not a loopback address. Without TLS, serve listens only on this machine (127.0.0.1, ::1 or another 127.x address), so no password crosses a network in the clear. Pass --tls-certificate <file> --tls-key <file>, or --tls-self-signed"
                ),
                try_command: Some(format!(
                    "eggdb serve {} {option} 127.0.0.1:{}",
                    self.file,
                    address.port()
                )),
                exit: Exit::Usage,
                code: crate::cli::failure::USAGE_CODE,
            }),
            Err(Some(error)) => {
                let in_use = error.kind() == std::io::ErrorKind::AddrInUse;
                Err(Failure::Database {
                    message: if in_use {
                        format!(
                            "cannot listen on {address}: address already in use. Another server is listening there (often {protocol} itself); pick another port, or port 0 for any free one."
                        )
                    } else {
                        format!("cannot listen on {address}: {error}")
                    },
                    try_command: in_use.then(|| {
                        format!(
                            "eggdb serve {} {option} {}",
                            self.file,
                            SocketAddr::new(address.ip(), address.port().wrapping_add(1))
                        )
                    }),
                    exit: Exit::DatabaseError,
                    code: if in_use {
                        "database_busy"
                    } else {
                        "database_failed"
                    },
                })
            }
        }
    }

    fn print_listening(
        &mut self,
        Bound {
            postgresql: bound,
            mysql: mysql_bound,
            tds: tds_bound,
        }: Bound,
        config: &ServerConfig,
        read_only: bool,
        tls: Option<&TlsSetup>,
        sockets: &Sockets,
    ) {
        let host = bound.ip().to_string();
        let url_host = match bound.ip() {
            IpAddr::V6(_) => format!("[{host}]"),
            IpAddr::V4(_) => host.clone(),
        };
        let mysql_line = mysql_bound.map_or_else(String::new, |mysql| {
            format!(
                "  mysql        listening on {mysql} (MySQL protocol): mysql -h {} -P {} -u {} -p {}{}\n",
                client_host(mysql.ip(), tls),
                mysql.port(),
                config.user,
                mysql_ssl_mode(tls),
                config.database
            )
        });
        let tds_line = tds_bound.map_or_else(String::new, |tds| {
            match tls {
                None => format!(
                    "  sqlcmd       listening on {tds} (SQL Server's TDS protocol, no encryption): sqlcmd -S {},{} -U {} -d {} (password in SQLCMDPASSWORD or -P; go-sqlcmd's default encryption or -N disable)\n",
                    tds.ip(),
                    tds.port(),
                    config.user,
                    config.database
                ),
                Some(setup) => format!(
                    "  sqlcmd       listening on {tds} (SQL Server's TDS protocol, encrypted): sqlcmd -S {},{} -U {} -d {}{} (password in SQLCMDPASSWORD or -P{})\n",
                    client_host(tds.ip(), tls),
                    tds.port(),
                    config.user,
                    config.database,
                    if matches!(setup.source, CertificateSource::SelfSigned) { " -C" } else { "" },
                    if matches!(setup.source, CertificateSource::SelfSigned) { "; -C trusts this certificate" } else { "" },
                ),
            }
        });
        match self.format {
            Format::Json => {
                let mut object = JsonObject::new()
                    .string("listening", &bound.to_string())
                    .string("host", &host)
                    .number("port", u64::from(bound.port()));
                if let Some(mysql) = mysql_bound {
                    object = object
                        .string("mysql_listening", &mysql.to_string())
                        .number("mysql_port", u64::from(mysql.port()));
                }
                if let Some(tds) = tds_bound {
                    object = object
                        .string("tds_listening", &tds.to_string())
                        .number("tds_port", u64::from(tds.port()));
                }
                object = object
                    .string("file", &self.file_display)
                    .string("database", &config.database)
                    .string("user", &config.user)
                    .number("max_connections", config.max_connections as u64)
                    .boolean("read_only", read_only)
                    .boolean("tls", tls.is_some())
                    .string_if_any(
                        "socket",
                        sockets
                            .postgresql
                            .as_ref()
                            .map(|path| path.display().to_string())
                            .as_deref(),
                    )
                    .string_if_any(
                        "mysql_socket",
                        sockets
                            .mysql
                            .as_ref()
                            .map(|path| path.display().to_string())
                            .as_deref(),
                    )
                    .string_if_any("mysql_pipe", sockets.mysql_pipe.as_deref())
                    .string_if_any("tds_pipe", sockets.tds_pipe.as_deref())
                    .boolean("loopback_only", tls.is_none());
                if let Some(setup) = tls {
                    let certificate = match &setup.source {
                        CertificateSource::Files { certificate, .. } => certificate.as_str(),
                        CertificateSource::SelfSigned => "self-signed",
                    };
                    object = object
                        .string("tls_certificate", certificate)
                        .string("tls_fingerprint_sha256", &setup.fingerprint_hex())
                        .raw("tls_names", &json_list(&setup.names))
                        .string("tls_not_after", &setup.not_after_rfc3339())
                        .string("tls_min_version", setup.minimum_version_number());
                    if let CertificateSource::Files { key, .. } = &setup.source {
                        object = object.string("tls_key", key);
                    }
                }
                self.out.text(&object.line());
            }
            _ => {
                let (transport, tls_lines, connect) = match tls {
                    None => (
                        "loopback only without TLS: see --tls-certificate".to_owned(),
                        String::new(),
                        format!(
                            "psql postgresql://{}@{url_host}:{}/{}",
                            config.user,
                            bound.port(),
                            config.database
                        ),
                    ),
                    Some(setup) => {
                        let required = if bound.ip().is_loopback() {
                            "optional on loopback"
                        } else {
                            "required from other machines"
                        };
                        let (certificate_line, fingerprint_note, ssl_mode) = match &setup.source {
                            CertificateSource::Files { certificate, key } => (
                                format!(
                                    "{certificate} (chain of {}, {}, expires {}) with {key}, for {}",
                                    setup.chain_length,
                                    setup.key_kind,
                                    setup.not_after_date(),
                                    setup.names.join(", ")
                                ),
                                "the server certificate",
                                "verify-full",
                            ),
                            CertificateSource::SelfSigned => (
                                format!(
                                    "self-signed {} for {}, made at this start (new one each start)",
                                    setup.key_kind,
                                    setup.names.join(" and ")
                                ),
                                "pin it: clients cannot verify a self-signed certificate by name",
                                "require",
                            ),
                        };
                        (
                            format!("{}, {required}", setup.version_text()),
                            format!(
                                "  tls          {certificate_line}\n  fingerprint  SHA256 {} ({fingerprint_note})\n",
                                setup.fingerprint_colons()
                            ),
                            format!(
                                "psql \"postgresql://{}@{}:{}/{}?sslmode={ssl_mode}\"",
                                config.user,
                                client_host(bound.ip(), tls),
                                bound.port(),
                                config.database
                            ),
                        )
                    }
                };
                let mut socket_lines = String::new();
                let mut connect = connect;
                if let Some(path) = &sockets.postgresql {
                    socket_lines.push_str(&format!(
                        "  socket       {} (owner only, PostgreSQL protocol)\n",
                        path.display()
                    ));
                    if let Some(directory) = path.parent() {
                        connect.push_str(&format!(
                            "   or   psql -h {} -p {} -d {}",
                            directory.display(),
                            bound.port(),
                            config.database
                        ));
                    }
                }
                if let Some(path) = &sockets.mysql {
                    socket_lines
                        .push_str(&format!("  mysql socket {} (owner only)\n", path.display()));
                }
                if let Some(name) = &sockets.mysql_pipe {
                    socket_lines.push_str(&format!(
                        "  mysql pipe   \\\\.\\pipe\\{name} (owner only, this machine only)\n"
                    ));
                }
                if let Some(name) = &sockets.tds_pipe {
                    socket_lines.push_str(&format!(
                        "  tds pipe     \\\\.\\pipe\\{name} (owner only, this machine only)\n"
                    ));
                }
                self.out.text(&format!(
                    "listening on {bound} (PostgreSQL protocol; {transport})\n  database     {} ({}, open {})\n  user         {} (SCRAM-SHA-256), up to {} connections\n{socket_lines}{tls_lines}  connect      {connect}\n{mysql_line}{tds_line}  stop         Ctrl-C\n",
                    config.database,
                    self.file,
                    if read_only { "read-only" } else { "for writing" },
                    config.user,
                    config.max_connections,
                ));
            }
        }
    }

    fn serve_password(&mut self, user: &str) -> Result<String, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Serve));
        match self.request.serve.password_file.clone() {
            Some(source) => {
                let (shown, first_line) = match &source {
                    ExportFile::Standard => {
                        self.stdin_taken = true;
                        let mut line = String::new();
                        std::io::BufReader::new(&mut *self.stdin)
                            .read_line(&mut line)
                            .map_err(|error| {
                                usage(format!("cannot read the password from stdin: {error}"))
                            })?;
                        ("-".to_owned(), line)
                    }
                    ExportFile::Path(path) => {
                        let shown = render::text(path.display().to_string().as_bytes());
                        let text = std::fs::read(path).map_err(|error| {
                            usage(format!("cannot read --password-file {shown}: {error}"))
                        })?;
                        let text = String::from_utf8(text).map_err(|_| {
                            usage(format!("--password-file {shown} is not UTF-8 text"))
                        })?;
                        let line = text.lines().next().unwrap_or("").to_owned();
                        (shown, line)
                    }
                };
                let password = first_line.trim_end_matches(['\r', '\n']).to_owned();
                if password.is_empty() {
                    return Err(usage(format!(
                        "--password-file {shown} holds no password on its first line"
                    )));
                }
                Ok(password)
            }
            None => match std::env::var(PASSWORD_VARIABLE) {
                Ok(password) if !password.is_empty() => Ok(password),
                Ok(_) => Err(usage(format!("{PASSWORD_VARIABLE} is set but empty"))),
                Err(std::env::VarError::NotUnicode(_)) => {
                    Err(usage(format!("{PASSWORD_VARIABLE} is not UTF-8 text")))
                }
                Err(std::env::VarError::NotPresent) => Err(usage(format!(
                    "serve needs a password for user {user}: set {PASSWORD_VARIABLE}, or pass --password-file <file> (its first line, or - for stdin). It is never taken on the command line, where every user of this machine can read it."
                ))),
            },
        }
    }
}

fn bind_syslog(
    address: SocketAddr,
    secured: bool,
) -> Result<(Option<eggdb_platform::OsDatagramSocket>, std::net::TcpListener), Failure> {
    if !address.ip().is_loopback() && !secured {
        return Err(Failure::usage(
            format!(
                "--syslog-listen {address}: syslog without TLS has no authentication, so it listens only on a loopback address; give --tls-certificate and --tls-key for syslog over TLS"
            ),
            Some(CommandName::Serve),
        ));
    }
    if address.port() == 0 {
        return Err(Failure::usage(
            format!(
                "--syslog-listen {address}: give a port, such as 127.0.0.1:5514, since senders are configured with it"
            ),
            Some(CommandName::Serve),
        ));
    }
    let cannot_listen = |error: String| {
        Failure::database(
            format!("cannot listen for syslog on {address}: {error}"),
            Exit::DatabaseError,
        )
    };
    if !address.ip().is_loopback() {
        let listener =
            std::net::TcpListener::bind(address).map_err(|error| cannot_listen(error.to_string()))?;
        return Ok((None, listener));
    }
    let datagrams = OsNetwork::new()
        .bind_datagram(address)
        .map_err(|error| cannot_listen(error.to_string()))?;
    let bound = datagrams
        .local_address()
        .map_err(|error| cannot_listen(error.to_string()))?;
    let listener =
        std::net::TcpListener::bind(bound).map_err(|error| cannot_listen(error.to_string()))?;
    Ok((Some(datagrams), listener))
}

fn client_host(address: IpAddr, tls: Option<&TlsSetup>) -> String {
    let host = if address.is_unspecified() {
        tls.and_then(TlsSetup::first_dns_name)
            .map_or_else(|| "localhost".to_owned(), str::to_owned)
    } else {
        address.to_string()
    };
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => format!("[{host}]"),
        _ => host,
    }
}

fn json_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| render::json_string(item)).collect();
    format!("[{}]", quoted.join(","))
}

fn mysql_ssl_mode(tls: Option<&TlsSetup>) -> &'static str {
    match tls.map(|setup| &setup.source) {
        None => "",
        Some(CertificateSource::Files { .. }) => " --ssl-mode=VERIFY_IDENTITY",
        Some(CertificateSource::SelfSigned) => " --ssl-mode=REQUIRED",
    }
}

struct Bound {
    postgresql: SocketAddr,
    mysql: Option<SocketAddr>,
    tds: Option<SocketAddr>,
}

fn missing_socket_directory(options: &crate::cli::args::ServeOptions) -> Option<String> {
    let directories = [
        (options.socket_dir.as_deref(), "--socket-dir"),
        (
            options.mysql_socket.as_deref().and_then(Path::parent),
            "--mysql-socket",
        ),
    ];
    directories.into_iter().find_map(|(directory, option)| {
        let directory = directory.filter(|directory| !directory.as_os_str().is_empty())?;
        (!directory.is_dir()).then(|| {
            format!(
                "cannot listen in {}: the directory does not exist. Create it (mkdir -p {}), or pass another {option}",
                directory.display(),
                directory.display()
            )
        })
    })
}

pub(super) struct Sockets {
    postgresql: Option<std::path::PathBuf>,
    mysql: Option<std::path::PathBuf>,
    mysql_pipe: Option<String>,
    tds_pipe: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum InstanceEvent {
    Startup,
    Shutdown,
}

impl Session<'_> {
    fn named_pipe_listener(
        &self,
        name: &str,
        option: &str,
        product: &str,
    ) -> Result<eggdb_platform::local_socket::LocalListener, Failure> {
        use eggdb_platform::local_socket::{LocalBindError, bind_named_pipe};
        bind_named_pipe(name).map_err(|error| match error {
            LocalBindError::InUse => Failure::database(
                format!(
                    "cannot listen on pipe {name}: another server already has it (often {product} itself). Stop it, or pick another {option} name"
                ),
                Exit::DatabaseError,
            ),
            LocalBindError::Unsupported => Failure::usage(
                format!("{option} is only available on Windows"),
                Some(CommandName::Serve),
            ),
            LocalBindError::MissingDirectory(_) | LocalBindError::NotASocket => Failure::database(
                format!("cannot listen on pipe {name}"),
                Exit::DatabaseError,
            ),
            LocalBindError::Io(error) => Failure::database(
                format!("cannot listen on pipe {name}: {error}"),
                Exit::DatabaseError,
            ),
        })
    }

    fn local_listener(
        &self,
        path: &Path,
        option: &str,
    ) -> Result<eggdb_platform::local_socket::LocalListener, Failure> {
        use eggdb_platform::local_socket::{LocalBindError, bind_local};
        let usage = |message: String| Failure::usage(message, Some(CommandName::Serve));
        bind_local(path).map_err(|error| match error {
            LocalBindError::MissingDirectory(directory) => usage(format!(
                "cannot listen on {}: directory {} does not exist. Create it (mkdir -p {}), or pass another {option}",
                path.display(),
                directory.display(),
                directory.display()
            )),
            LocalBindError::NotASocket => usage(format!(
                "cannot listen on {}: it is not a socket. Remove it or pass another path",
                path.display()
            )),
            LocalBindError::InUse => Failure::database(
                format!(
                    "cannot listen on {}: a server is already accepting connections there (often {} itself). Stop it, or pick another --listen port or {option}",
                    path.display(),
                    if option == "--mysql-socket" { "MySQL" } else { "PostgreSQL" }
                ),
                Exit::DatabaseError,
            ),
            LocalBindError::Unsupported => usage(format!("{option} needs Windows 10 version 1803 or later, whose Unix sockets it uses; connect over --listen instead")),
            LocalBindError::Io(error) => Failure::database(format!("cannot listen on {}: {error}", path.display()), Exit::DatabaseError),
        })
    }
}

const AUDIT_VARIABLE: &str = "EGGDB_AUDIT";
const AUDIT_TABLES_VARIABLE: &str = "EGGDB_AUDIT_TABLES";
const AUDIT_DIGEST_EVERY: u64 = 1000;
const AUDIT_ROTATE_VARIABLE: &str = "EGGDB_AUDIT_ROTATE_MB";
const AUDIT_KEEP_DAYS_VARIABLE: &str = "EGGDB_AUDIT_KEEP_DAYS";
const AUDIT_SIGNING_KEY_VARIABLE: &str = "EGGDB_AUDIT_SIGNING_KEY";

fn audit_signing_key() -> Result<Option<[u8; eggdb_crypto::ED25519_SEED_BYTES]>, String> {
    let Some(path) = std::env::var_os(AUDIT_SIGNING_KEY_VARIABLE).filter(|path| !path.is_empty()) else {
        return Ok(None);
    };
    let shown = std::path::Path::new(&path).display().to_string();
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {AUDIT_SIGNING_KEY_VARIABLE} {shown}: {error}"))?;
    let digits = text.trim();
    if digits.len() != eggdb_crypto::ED25519_SEED_BYTES * 2 {
        return Err(format!(
            "{AUDIT_SIGNING_KEY_VARIABLE} {shown} must hold an Ed25519 private key as 64 hexadecimal digits"
        ));
    }
    let mut seed = [0u8; eggdb_crypto::ED25519_SEED_BYTES];
    for (slot, pair) in seed.iter_mut().zip(digits.as_bytes().chunks_exact(2)) {
        *slot = std::str::from_utf8(pair)
            .ok()
            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
            .ok_or_else(|| format!("{AUDIT_SIGNING_KEY_VARIABLE} {shown} holds a digit that is not hexadecimal"))?;
    }
    Ok(Some(seed))
}
const DEFAULT_AUDIT_ROTATE_BYTES: u64 = 512 << 20;
static AUDIT_RECORDS_WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn audit_server_events_from_environment(database: &std::path::Path) -> Result<(), String> {
    let wanted = std::env::var(AUDIT_VARIABLE).is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "on"));
    if !wanted {
        return Ok(());
    }
    let signing_key = audit_signing_key()?;
    if let Some(seed) = &signing_key {
        let public: String = eggdb_crypto::ed25519_public_key(seed)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        eprintln!("eggdb serve: audit chain signed every {AUDIT_DIGEST_EVERY} records by key {public}");
    }
    if let Ok(tables) = std::env::var(AUDIT_TABLES_VARIABLE) {
        eggdb_sql::audit_events::set_audited_tables(tables.split(',').map(|name| name.trim().to_owned()).collect());
    }
    let log = eggdb_mcp::audit::AuditLog::new(eggdb_mcp::audit::audit_path_for(database));
    let rotate_bytes = std::env::var(AUDIT_ROTATE_VARIABLE)
        .ok()
        .and_then(|megabytes| megabytes.parse::<u64>().ok())
        .map_or(DEFAULT_AUDIT_ROTATE_BYTES, |megabytes| megabytes.saturating_mul(1 << 20).max(1 << 20));
    let keep_days = std::env::var(AUDIT_KEEP_DAYS_VARIABLE).ok().and_then(|days| days.parse::<u64>().ok());
    eggdb_sql::audit_events::set_audit_sink(Box::new(move |event| {
        let record = eggdb_mcp::audit::server_event_record(
            event.kind.name(),
            event.user,
            event.database,
            event.protocol,
            event.detail,
            event.succeeded,
        );
        if log.size() >= rotate_bytes {
            match log.rotate() {
                Ok(Some(archived)) => eprintln!("eggdb serve: audit file moved aside to {}", archived.display()),
                Ok(None) => {}
                Err(failure) => eprintln!("eggdb serve: the audit file could not be moved aside: {failure}"),
            }
            if let Some(days) = keep_days
                && let Err(failure) = log.remove_rotated_older_than(days)
            {
                eprintln!("eggdb serve: old audit files could not be removed: {failure}");
            }
        }
        match log.append_with_digest(&record) {
            Ok(digest) => {
                let written = AUDIT_RECORDS_WRITTEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if written % AUDIT_DIGEST_EVERY == 0 {
                    eprintln!("eggdb serve: audit digest after {written} records: {digest}");
                    if let Some(seed) = &signing_key
                        && let Err(failure) = log.append_signed(seed)
                    {
                        eprintln!("eggdb serve: the audit chain could not be signed: {failure}");
                    }
                }
            }
            Err(failure) => eprintln!("eggdb serve: the audit record was not written: {failure}"),
        }
    }));
    Ok(())
}

