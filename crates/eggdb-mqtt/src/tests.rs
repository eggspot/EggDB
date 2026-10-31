use crate::broker::{Broker, ServerConfig, serve_stream};
use crate::packet::{encode, filter_matches, put_string, take_packet, valid_filter};
use eggdb_keyspace::MemoryKeyspace;
use std::io::Write;
use std::sync::{Arc, Mutex};

fn config() -> ServerConfig {
    ServerConfig { user: "eggdb".to_owned(), password_sha256: eggdb_crypto::sha256(b"secret"), max_connections: 4 }
}

fn connect(version: u8, password: &str, will: Option<(&str, &str)>) -> Vec<u8> {
    let mut body = Vec::new();
    put_string(&mut body, b"MQTT");
    body.push(version);
    body.push(0xc2 | if will.is_some() { 0x04 } else { 0 });
    body.extend_from_slice(&30u16.to_be_bytes());
    if version >= 5 {
        body.push(0);
    }
    put_string(&mut body, b"client");
    if let Some((topic, payload)) = will {
        if version >= 5 {
            body.push(0);
        }
        put_string(&mut body, topic.as_bytes());
        put_string(&mut body, payload.as_bytes());
    }
    put_string(&mut body, b"eggdb");
    put_string(&mut body, password.as_bytes());
    encode(1, 0, &body)
}

fn publish(topic: &str, payload: &str, qos: u8, retain: bool) -> Vec<u8> {
    let mut body = Vec::new();
    put_string(&mut body, topic.as_bytes());
    if qos > 0 {
        body.extend_from_slice(&7u16.to_be_bytes());
    }
    body.extend_from_slice(payload.as_bytes());
    encode(3, (qos << 1) | u8::from(retain), &body)
}

fn subscribe(filter: &str, qos: u8) -> Vec<u8> {
    let mut body = 1u16.to_be_bytes().to_vec();
    put_string(&mut body, filter.as_bytes());
    body.push(qos);
    encode(8, 2, &body)
}

fn packets(output: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut buffer = output.to_vec();
    let mut out = Vec::new();
    while let Some(packet) = take_packet(&mut buffer).unwrap() {
        out.push((packet.kind, packet.body));
    }
    out
}

fn session(broker: &Broker<'_>, id: u64, input: Vec<u8>) -> (Arc<Mutex<Vec<u8>>>, Vec<(u8, Vec<u8>)>) {
    let output = Arc::new(Mutex::new(Vec::new()));
    let shared: Arc<Mutex<dyn Write + Send>> = output.clone();
    serve_stream(broker, &config(), id, input.as_slice(), shared, &|| false).unwrap();
    let bytes = output.lock().unwrap().clone();
    (output, packets(&bytes))
}

#[test]
fn clients_connect_publish_and_receive_retained_messages() {
    let keyspace = MemoryKeyspace::default();
    let broker = Broker::new(&keyspace);
    let mut input = connect(4, "secret", None);
    input.extend(publish("home/kitchen/temp", "21.5", 1, true));
    input.extend(publish("home/hall/temp", "19", 2, false));
    input.extend(encode(6, 2, &7u16.to_be_bytes()));
    input.extend(encode(12, 0, &[]));
    input.extend(encode(14, 0, &[]));
    let (_, answers) = session(&broker, 1, input);
    let kinds: Vec<u8> = answers.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(kinds, [2, 4, 5, 7, 13]);
    assert_eq!(answers[0].1, [0, 0]);
    let mut input = connect(5, "secret", None);
    input.extend(subscribe("home/+/temp", 1));
    input.extend(encode(14, 0, &[]));
    let (_, answers) = session(&broker, 2, input);
    let kinds: Vec<u8> = answers.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(kinds, [2, 9, 3]);
    assert_eq!(answers[1].1, [0, 1, 0, 1]);
    assert!(answers[2].1.ends_with(b"21.5"));
}

#[test]
fn wrong_passwords_are_refused_and_wills_are_published_on_abnormal_ends() {
    let keyspace = MemoryKeyspace::default();
    let broker = Broker::new(&keyspace);
    let (_, answers) = session(&broker, 1, connect(4, "wrong", None));
    assert_eq!(answers, [(2, vec![0, 5])]);
    let (_, answers) = session(&broker, 2, connect(5, "wrong", None));
    assert_eq!(answers, [(2, vec![0, 0x87, 0])]);
    let watcher_output = Arc::new(Mutex::new(Vec::new()));
    {
        let shared: Arc<Mutex<dyn Write + Send>> = watcher_output.clone();
        let mut input = connect(4, "secret", None);
        input.extend(subscribe("status/#", 0));
        serve_stream(&broker, &config(), 3, input.as_slice(), shared.clone(), &|| false).unwrap();
    }
    let mut retained_will = connect(4, "secret", Some(("status/device", "offline")));
    retained_will[9] |= 0x20;
    session(&broker, 4, retained_will);
    let (_, answers) = session(&broker, 5, {
        let mut input = connect(4, "secret", None);
        input.extend(subscribe("status/#", 0));
        input
    });
    assert!(answers.iter().any(|(kind, body)| *kind == 3 && body.ends_with(b"offline")));
}

#[test]
fn filters_match_as_the_specification_says() {
    assert!(filter_matches("sport/#", "sport"));
    assert!(filter_matches("sport/+/player1", "sport/tennis/player1"));
    assert!(!filter_matches("sport/+", "sport/tennis/player1"));
    assert!(!filter_matches("#", "$SYS/broker"));
    assert!(valid_filter("a/+/#"));
    assert!(!valid_filter("a/#/b"));
    assert!(!valid_filter("a+/b"));
}

#[test]
fn hostile_bytes_end_the_session_without_panicking() {
    let keyspace = MemoryKeyspace::default();
    let broker = Broker::new(&keyspace);
    let mut valid = connect(5, "secret", Some(("w", "x")));
    valid.extend(subscribe("a/#", 1));
    valid.extend(publish("a/b", "payload", 1, true));
    for cut in 0..valid.len() {
        session(&broker, 9, valid[..cut].to_vec());
    }
    for at in 0..valid.len() {
        let mut damaged = valid.clone();
        damaged[at] ^= 0x5a;
        session(&broker, 9, damaged);
    }
    session(&broker, 9, vec![0x10, 0xff, 0xff, 0xff, 0xff, 0x01]);
}
