//! EggDB's own connectors, written in Rust (D46): a file source that publishes each new line of
//! a file to a topic, a file sink that appends each record of a topic to a file, and a topic
//! copy. Each `step` moves what is there now from a stored position and returns the new one,
//! so a connector resumes exactly where it stopped. The Apache file connectors' class names are
//! taken too, so existing configurations run unchanged.

use eggdb_keyspace::Keyspace;
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};

const MOST_BYTES_PER_STEP: u64 = 1 << 20;
const SINK_PARTITION: i32 = 0;

/// A connector plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Plugin {
    pub(crate) class: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) required: &'static [&'static str],
    pub(crate) aliases: &'static [&'static str],
}

pub(crate) const PLUGINS: [Plugin; 3] = [
    Plugin {
        class: "org.eggdb.connect.file.FileStreamSourceConnector",
        kind: "source",
        required: &["file", "topic"],
        aliases: &["org.apache.kafka.connect.file.FileStreamSourceConnector", "FileStreamSource", "FileStreamSourceConnector"],
    },
    Plugin {
        class: "org.eggdb.connect.file.FileStreamSinkConnector",
        kind: "sink",
        required: &["file", "topics"],
        aliases: &["org.apache.kafka.connect.file.FileStreamSinkConnector", "FileStreamSink", "FileStreamSinkConnector"],
    },
    Plugin {
        class: "org.eggdb.connect.TopicCopyConnector",
        kind: "source",
        required: &["source.topic", "target.topic"],
        aliases: &["TopicCopy", "TopicCopyConnector"],
    },
];

/// The plugin a `connector.class` names, by class or alias.
pub(crate) fn plugin_for(class: &str) -> Option<&'static Plugin> {
    PLUGINS.iter().find(|plugin| plugin.class == class || plugin.aliases.contains(&class))
}

/// The problems of a connector's configuration, by field; empty when it can run.
pub(crate) fn validate(config: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let Some(class) = config.get("connector.class") else {
        return vec![("connector.class".to_owned(), "Missing required configuration \"connector.class\"".to_owned())];
    };
    let Some(plugin) = plugin_for(class) else {
        return vec![("connector.class".to_owned(), format!("Failed to find any class that implements Connector and which name matches {class}"))];
    };
    let mut problems: Vec<(String, String)> = plugin
        .required
        .iter()
        .filter(|field| config.get(**field).is_none_or(|value| value.trim().is_empty()))
        .map(|field| ((*field).to_owned(), format!("Missing required configuration \"{field}\" which has no default value.")))
        .collect();
    if plugin.kind == "sink" && config.get("topics").is_some_and(|topics| topics.contains(',')) {
        problems.push(("topics".to_owned(), "EggDB's file sink reads one topic; run one connector per topic".to_owned()));
    }
    if let Some(topic) = config.get("topic").or_else(|| config.get("target.topic"))
        && !eggdb_kafka::valid_topic_name(topic)
    {
        problems.push(("topic".to_owned(), format!("\"{topic}\" is not a valid topic name")));
    }
    problems
}

fn required<'c>(config: &'c BTreeMap<String, String>, field: &str) -> Result<&'c str, String> {
    config.get(field).map(String::as_str).filter(|value| !value.is_empty()).ok_or_else(|| format!("the configuration has no {field}"))
}

/// Runs one step of the connector configured by `config` from `position`; returns the new
/// position and how many records it moved.
pub(crate) fn step(kafka: &dyn Keyspace, config: &BTreeMap<String, String>, position: i64) -> Result<(i64, usize), String> {
    let class = required(config, "connector.class")?;
    let plugin = plugin_for(class).ok_or_else(|| format!("no connector plugin is called {class}"))?;
    match plugin.class {
        "org.eggdb.connect.file.FileStreamSourceConnector" => file_source(kafka, required(config, "file")?, required(config, "topic")?, position),
        "org.eggdb.connect.file.FileStreamSinkConnector" => file_sink(kafka, required(config, "topics")?, required(config, "file")?, position),
        _ => topic_copy(kafka, required(config, "source.topic")?, required(config, "target.topic")?, position),
    }
}

fn file_source(kafka: &dyn Keyspace, path: &str, topic: &str, position: i64) -> Result<(i64, usize), String> {
    let failed = |error: std::io::Error| format!("cannot read {path}: {error}");
    let mut file = std::fs::File::open(path).map_err(failed)?;
    let length = file.metadata().map_err(failed)?.len();
    let start = u64::try_from(position).unwrap_or(0).min(length);
    file.seek(SeekFrom::Start(start)).map_err(failed)?;
    let mut chunk = Vec::new();
    file.take(MOST_BYTES_PER_STEP).read_to_end(&mut chunk).map_err(failed)?;
    let Some(last_newline) = chunk.iter().rposition(|byte| *byte == b'\n') else {
        return Ok((start as i64, 0));
    };
    let mut moved = 0;
    for line in chunk[..last_newline].split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        eggdb_kafka::publish(kafka, topic, line)?;
        moved += 1;
    }
    Ok((start as i64 + last_newline as i64 + 1, moved))
}

fn file_sink(kafka: &dyn Keyspace, topic: &str, path: &str, position: i64) -> Result<(i64, usize), String> {
    let (records, next) = eggdb_kafka::read_records(kafka, topic.trim(), SINK_PARTITION, position.max(0))?;
    if records.is_empty() {
        return Ok((next.max(position), 0));
    }
    let failed = |error: std::io::Error| format!("cannot write {path}: {error}");
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).map_err(failed)?;
    let mut out = Vec::new();
    for record in &records {
        if let Some(value) = &record.value {
            out.extend_from_slice(value);
            out.push(b'\n');
        }
    }
    file.write_all(&out).map_err(failed)?;
    file.sync_data().map_err(failed)?;
    let after = records.last().map_or(position, |record| record.offset + 1);
    Ok((after, records.len()))
}

fn topic_copy(kafka: &dyn Keyspace, source: &str, target: &str, position: i64) -> Result<(i64, usize), String> {
    let (records, next) = eggdb_kafka::read_records(kafka, source, SINK_PARTITION, position.max(0))?;
    for record in &records {
        if let Some(value) = &record.value {
            eggdb_kafka::publish(kafka, target, value)?;
        }
    }
    let after = records.last().map_or(next.max(position), |record| record.offset + 1);
    Ok((after, records.len()))
}
