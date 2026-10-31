//! EggDB's server for Cassandra's CQL native protocol v4 (D218), written from the public
//! protocol specification and CQL documentation: Cassandra's password authenticator, `QUERY`,
//! `PREPARE`, `EXECUTE` and `BATCH`, and the `system` and `system_schema` tables drivers read
//! when they connect. Keyspaces are EggDB SQL schemas and CQL tables are SQL tables, so the same
//! rows answer CQL and SQL: CQL statements are translated into the PostgreSQL dialect, with
//! `INSERT` and `UPDATE` as the upserts CQL defines. Apache Cassandra is a trademark of the
//! Apache Software Foundation; EggDB is compatible with its protocol.

mod cql;
mod frame;
mod server;
mod translate;
mod types;

pub use server::{ServerConfig, serve};

#[cfg(test)]
mod tests;
