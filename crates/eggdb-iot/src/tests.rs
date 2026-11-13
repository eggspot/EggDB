use crate::{Broker, Device, SparkplugSource, Table, Topic, modbus_request, modbus_values, sparkplug_statements};

#[test]
fn sparkplug_topics_and_sources_read_as_the_specification_writes_them() {
    let topic = Topic::parse("spBv1.0/Plant1/DDATA/Edge7/Press3").unwrap();
    assert_eq!((topic.group.as_str(), topic.message_type.as_str(), topic.edge_node.as_str()), ("Plant1", "DDATA", "Edge7"));
    assert_eq!(topic.device.as_deref(), Some("Press3"));
    assert!(Topic::parse("spBv1.0/STATE/host").is_none());
    assert!(Topic::parse("spBv1.0/G/NOPE/E").is_none());
    let source = SparkplugSource::parse("mqtt://ops:secret@broker.local|Plant1").unwrap();
    assert_eq!(source.broker, Broker { address: "broker.local:1883".to_owned(), login: Some(("ops".to_owned(), "secret".to_owned())) });
    assert_eq!(source.group, "Plant1");
    assert!(SparkplugSource::parse("mqtt://broker|a/b").is_err());
}

fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn metric(name: Option<&str>, alias: u64, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(name) = name {
        out.push(0x0a);
        varint(name.len() as u64, &mut out);
        out.extend_from_slice(name.as_bytes());
    }
    out.push(0x10);
    varint(alias, &mut out);
    out.push(0x20);
    varint(3, &mut out);
    out.push(0x50);
    varint(value, &mut out);
    out
}

fn payload(metrics: &[Vec<u8>], seq: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for metric in metrics {
        out.push(0x12);
        varint(metric.len() as u64, &mut out);
        out.extend_from_slice(metric);
    }
    out.push(0x18);
    varint(seq, &mut out);
    out
}

#[test]
fn births_record_aliases_and_data_messages_resolve_them() {
    let birth = Topic::parse("spBv1.0/Plant1/DBIRTH/Edge7/Press3").unwrap();
    let statements = sparkplug_statements(&birth, &payload(&[metric(Some("Pressure"), 4, 10)], 0));
    assert_eq!(statements.len(), 2);
    assert!(statements[0].starts_with("INSERT INTO sparkplug_aliases"), "{}", statements[0]);
    assert!(statements[0].contains("ON CONFLICT (group_id, edge_node, device, alias) DO UPDATE"), "{}", statements[0]);
    let data = Topic::parse("spBv1.0/Plant1/NDATA/Edge7").unwrap();
    let statements = sparkplug_statements(&data, &payload(&[metric(None, 4, 12)], 1));
    assert_eq!(statements.len(), 1);
    assert!(statements[0].contains("coalesce(m ->> 'name', a.metric)"), "{}", statements[0]);
    assert!(statements[0].contains("a.device = ''"), "node messages match aliases recorded without a device");
    assert!(statements[0].contains("'\\x12"), "the payload travels as a bytea literal: {}", statements[0]);
}

#[test]
fn modbus_requests_and_responses_follow_the_specification() {
    assert_eq!(modbus_request(7, 1, Table::HoldingRegisters, 40, 2), [0, 7, 0, 0, 0, 6, 1, 3, 0, 40, 0, 2]);
    assert_eq!(modbus_values(&[3, 4, 0x01, 0x02, 0xff, 0xfe], Table::HoldingRegisters, 2).unwrap(), vec![0x0102, 0xfffe]);
    assert_eq!(modbus_values(&[1, 1, 0b0000_0101], Table::Coils, 3).unwrap(), vec![1, 0, 1]);
    assert!(modbus_values(&[0x83, 2], Table::HoldingRegisters, 1).unwrap_err().contains("exception 2"));
    assert!(modbus_values(&[3, 4, 0x01], Table::HoldingRegisters, 2).is_err());
    let device = Device::parse("tcp://plc.local|unit=3|holding=0+10|coils=8+16|every=250").unwrap();
    assert_eq!(device.address, "plc.local:502");
    assert_eq!(device.unit, 3);
    assert_eq!(device.ranges, vec![(Table::HoldingRegisters, 0, 10), (Table::Coils, 8, 16)]);
    assert!(Device::parse("tcp://plc|holding=0+200").is_err());
    assert!(Device::parse("tcp://plc").is_err());
}
