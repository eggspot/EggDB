//! `eggdb-broker-compare`: EggDB's message broker protocols measured against the brokers they
//! are compatible with, on the same machine, through the same client of this crate's own (D7).
//!
//! Run with `cargo run --release --manifest-path crates/eggdb-broker-compare/Cargo.toml --
//! kafka [--results <file>]`, with `eggdb serve --kafka-listen` at `EGGDB_KAFKA_ADDRESS`
//! (`127.0.0.1:19092` by default) and Apache Kafka at `KAFKA_ADDRESS` (`127.0.0.1:9092`), or
//! `rabbitmq`, with `eggdb serve --amqp-listen` at `EGGDB_AMQP_ADDRESS` (`127.0.0.1:15672`, user
//! `eggdb` with `EGGDB_PASSWORD`) and RabbitMQ at `RABBITMQ_ADDRESS` (`127.0.0.1:5672`), `mqtt`
//! (Mosquitto and EMQX), `nats`, with `--nats-listen` at `EGGDB_NATS_ADDRESS`
//! (`127.0.0.1:14222`) and nats-server with JetStream at `NATS_ADDRESS` (`127.0.0.1:4222`), or
//! `sqs`, with `--sqs-listen` at `EGGDB_SQS_ADDRESS` (`127.0.0.1:19324`) and ElasticMQ at
//! `ELASTICMQ_ADDRESS` (`127.0.0.1:9324`). Both
//! sides get the same records over the same wire, so the ratio compares the servers, not the
//! clients. The ratios are reported, not gated. A side that is not running is reported and the
//! program exits successfully.

mod amqp;
mod kafka;
mod measure;
mod mqtt;
mod nats;
mod sqs;

use std::process::ExitCode;

const BATCHES: usize = 2_000;
const BATCH_SIZE: usize = 100;
const AMQP_BATCHES: usize = 200;
const MQTT_MESSAGES: u16 = 20_000;
const MQTT_WINDOW: u16 = 100;
const NATS_MESSAGES: u64 = 200_000;
const JETSTREAM_MESSAGES: u64 = 20_000;
const NATS_WINDOW: u64 = 100;
const SQS_MESSAGES: u64 = 5_000;

fn address(variable: &str, default: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| default.to_owned())
}

fn kafka(lines: &mut Vec<String>) -> Result<(), String> {
    let eggdb = address("EGGDB_KAFKA_ADDRESS", "127.0.0.1:19092");
    let other = address("KAFKA_ADDRESS", "127.0.0.1:9092");
    for side in [&eggdb, &other] {
        if std::net::TcpStream::connect(side).is_err() {
            println!("eggdb-broker-compare: nothing listens at {side}; Kafka not measured");
            return Ok(());
        }
    }
    let topic = format!("compare-{}", std::process::id());
    let produced_eggdb = kafka::produce(&eggdb, &topic, BATCHES, BATCH_SIZE)?;
    let produced_kafka = kafka::produce(&other, &topic, BATCHES, BATCH_SIZE)?;
    measure::report("kafka.produce (batches of 100 records of 100 bytes, acks=1)", &produced_eggdb, &produced_kafka, "Kafka", lines);
    let wanted = (BATCHES * BATCH_SIZE) as u64;
    let consumed_eggdb = kafka::consume(&eggdb, &topic, wanted)?;
    let consumed_kafka = kafka::consume(&other, &topic, wanted)?;
    measure::report("kafka.fetch (catching up from offset 0)", &consumed_eggdb, &consumed_kafka, "Kafka", lines);
    Ok(())
}

fn rabbitmq(lines: &mut Vec<String>) -> Result<(), String> {
    let eggdb = address("EGGDB_AMQP_ADDRESS", "127.0.0.1:15672");
    let other = address("RABBITMQ_ADDRESS", "127.0.0.1:5672");
    let eggdb_password = address("EGGDB_PASSWORD", "compare");
    for side in [&eggdb, &other] {
        if std::net::TcpStream::connect(side).is_err() {
            println!("eggdb-broker-compare: nothing listens at {side}; AMQP not measured");
            return Ok(());
        }
    }
    let queue = format!("compare-{}", std::process::id());
    let published_eggdb = amqp::publish(&eggdb, "eggdb", &eggdb_password, &queue, AMQP_BATCHES, BATCH_SIZE)?;
    let published_rabbit = amqp::publish(&other, "guest", "guest", &queue, AMQP_BATCHES, BATCH_SIZE)?;
    measure::report("amqp.publish (persistent, confirmed in batches of 100)", &published_eggdb, &published_rabbit, "RabbitMQ", lines);
    let wanted = (AMQP_BATCHES * BATCH_SIZE) as u64;
    let consumed_eggdb = amqp::consume(&eggdb, "eggdb", &eggdb_password, &queue, wanted)?;
    let consumed_rabbit = amqp::consume(&other, "guest", "guest", &queue, wanted)?;
    measure::report("amqp.consume (prefetch 1000, acknowledged every 100)", &consumed_eggdb, &consumed_rabbit, "RabbitMQ", lines);
    Ok(())
}

fn mqtt_brokers(lines: &mut Vec<String>) -> Result<(), String> {
    let eggdb = address("EGGDB_MQTT_ADDRESS", "127.0.0.1:11883");
    let eggdb_password = address("EGGDB_PASSWORD", "compare");
    if std::net::TcpStream::connect(&eggdb).is_err() {
        println!("eggdb-broker-compare: nothing listens at {eggdb}; MQTT not measured");
        return Ok(());
    }
    let topic = format!("compare/{}", std::process::id());
    let (published_eggdb, received_eggdb) = mqtt::publish_and_receive(&eggdb, Some(("eggdb", &eggdb_password)), &topic, MQTT_MESSAGES, MQTT_WINDOW)?;
    for (name, variable, default) in [("Mosquitto", "MOSQUITTO_ADDRESS", "127.0.0.1:1883"), ("EMQX", "EMQX_ADDRESS", "127.0.0.1:2883")] {
        let other = address(variable, default);
        if std::net::TcpStream::connect(&other).is_err() {
            println!("eggdb-broker-compare: nothing listens at {other}; {name} not measured");
            continue;
        }
        let (published, received) = mqtt::publish_and_receive(&other, None, &topic, MQTT_MESSAGES, MQTT_WINDOW)?;
        measure::report(&format!("mqtt.publish against {name} (QoS 1, 100 in flight)"), &published_eggdb, &published, name, lines);
        measure::report(&format!("mqtt.receive against {name} (a QoS 0 subscriber)"), &received_eggdb, &received, name, lines);
    }
    Ok(())
}

fn nats_servers(lines: &mut Vec<String>) -> Result<(), String> {
    let eggdb = address("EGGDB_NATS_ADDRESS", "127.0.0.1:14222");
    let other = address("NATS_ADDRESS", "127.0.0.1:4222");
    let eggdb_password = address("EGGDB_PASSWORD", "compare");
    for side in [&eggdb, &other] {
        if std::net::TcpStream::connect(side).is_err() {
            println!("eggdb-broker-compare: nothing listens at {side}; NATS not measured");
            return Ok(());
        }
    }
    let login = Some(("eggdb", eggdb_password.as_str()));
    let subject = format!("compare.{}", std::process::id());
    let (published_eggdb, received_eggdb) = nats::publish_and_receive(&eggdb, login, &subject, NATS_MESSAGES, NATS_WINDOW)?;
    let (published_nats, received_nats) = nats::publish_and_receive(&other, None, &subject, NATS_MESSAGES, NATS_WINDOW)?;
    measure::report("nats.publish (core, 100 bytes, a PING after every 100)", &published_eggdb, &published_nats, "nats-server", lines);
    measure::report("nats.receive (core, one subscriber)", &received_eggdb, &received_nats, "nats-server", lines);
    let stream = format!("COMPARE_{}", std::process::id());
    let stored_subject = format!("stored.{}", std::process::id());
    let stored_eggdb = nats::jetstream_publish(&eggdb, login, &stream, &stored_subject, JETSTREAM_MESSAGES, NATS_WINDOW)?;
    let stored_nats = nats::jetstream_publish(&other, None, &stream, &stored_subject, JETSTREAM_MESSAGES, NATS_WINDOW)?;
    measure::report("nats.jetstream.publish (file storage, 100 awaiting acknowledgement)", &stored_eggdb, &stored_nats, "nats-server", lines);
    Ok(())
}

fn sqs_queues(lines: &mut Vec<String>) -> Result<(), String> {
    let eggdb = address("EGGDB_SQS_ADDRESS", "127.0.0.1:19324");
    let other = address("ELASTICMQ_ADDRESS", "127.0.0.1:9324");
    let eggdb_password = address("EGGDB_PASSWORD", "compare");
    for side in [&eggdb, &other] {
        if std::net::TcpStream::connect(side).is_err() {
            println!("eggdb-broker-compare: nothing listens at {side}; SQS not measured");
            return Ok(());
        }
    }
    let queue = format!("compare-{}", std::process::id());
    let (sent_eggdb, received_eggdb) = sqs::send_and_receive(&eggdb, "eggdb", &eggdb_password, &queue, SQS_MESSAGES)?;
    let (sent_elasticmq, received_elasticmq) = sqs::send_and_receive(&other, "x", "x", &queue, SQS_MESSAGES)?;
    measure::report("sqs.send (one message per request)", &sent_eggdb, &sent_elasticmq, "ElasticMQ", lines);
    measure::report("sqs.receive and delete (ten per receive)", &received_eggdb, &received_elasticmq, "ElasticMQ", lines);
    Ok(())
}

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let broker = arguments.next().unwrap_or_default();
    let mut results = None;
    while let Some(argument) = arguments.next() {
        match (argument.as_str(), arguments.next()) {
            ("--results", Some(path)) => results = Some(path),
            _ => {
                eprintln!("usage: eggdb-broker-compare kafka|rabbitmq|mqtt|nats|sqs [--results <file>]");
                return ExitCode::from(2);
            }
        }
    }
    let mut lines = Vec::new();
    let outcome = match broker.as_str() {
        "kafka" => kafka(&mut lines),
        "rabbitmq" => rabbitmq(&mut lines),
        "mqtt" => mqtt_brokers(&mut lines),
        "nats" => nats_servers(&mut lines),
        "sqs" => sqs_queues(&mut lines),
        _ => {
            eprintln!("usage: eggdb-broker-compare kafka|rabbitmq|mqtt|nats|sqs [--results <file>]");
            return ExitCode::from(2);
        }
    };
    if let Err(message) = outcome {
        eprintln!("eggdb-broker-compare: {message}");
        return ExitCode::from(1);
    }
    if let Some(path) = results
        && let Err(error) = std::fs::write(&path, lines.join("\n") + "\n")
    {
        eprintln!("eggdb-broker-compare: cannot write {path}: {error}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
