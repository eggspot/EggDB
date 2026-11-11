//! EggDB's server for the Kafka protocol (D213), written from Apache Kafka's public protocol
//! guide: `ApiVersions`, `Metadata`, `Produce` (record batches, magic 2), `Fetch` and
//! `ListOffsets`, in their non-flexible versions. Each topic partition is a log of record batches
//! on EggDB's key-value transactions, stored as producers sent them with the broker's offsets
//! written in; batches are checked (length, magic and CRC-32C) before they are kept. Topics are
//! created on first use with one partition. Kafka is a trademark of the Apache Software
//! Foundation; EggDB is compatible with its protocol.

mod groups;
mod log;
mod server;
mod wire;

pub use server::{ServerConfig, serve};

const PUBLISHED_PARTITION: i32 = 0;

/// Whether `name` is a valid Kafka topic name: 1 to 249 letters, digits, `.`, `_` and `-`.
pub fn valid_topic_name(name: &str) -> bool {
    log::valid_topic(name)
}

/// Appends `value` as one record (no key, timestamp now) to partition 0 of `topic` in
/// `keyspace`, creating the topic with one partition when it is missing, so the server's own
/// producers (change data capture, D527) write where Kafka consumers read.
pub fn publish(keyspace: &dyn eggdb_keyspace::Keyspace, topic: &str, value: &[u8]) -> Result<(), String> {
    let created = log::create_topic(keyspace, topic, 1, log::Retention::UNLIMITED, false)?;
    if created != log::error::NONE && created != log::error::TOPIC_ALREADY_EXISTS {
        return Err(format!("cannot create topic {topic}: Kafka error {created}"));
    }
    match log::append(keyspace, topic, PUBLISHED_PARTITION, &log::single_record_batch(value))? {
        Ok(_) => Ok(()),
        Err(code) => Err(format!("cannot append to topic {topic}: Kafka error {code}")),
    }
}

#[cfg(test)]
mod tests;
