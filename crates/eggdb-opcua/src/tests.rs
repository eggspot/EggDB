use crate::binary::{NodeId, Reader, Writer};
use crate::{Client, Endpoint, Subscription};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

fn frame(kind: &[u8; 3], body: &[u8]) -> Vec<u8> {
    let mut out = kind.to_vec();
    out.push(b'F');
    out.extend_from_slice(&((body.len() + 8) as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

fn response_header(writer: &mut Writer, handle: u32) {
    writer.now().u32(handle).u32(0).u8(0).i32(-1).no_extension_object();
}

fn read_frame(stream: &mut TcpStream) -> Option<([u8; 3], Vec<u8>)> {
    let mut head = [0u8; 8];
    stream.read_exact(&mut head).ok()?;
    let size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
    let mut body = vec![0u8; size - 8];
    stream.read_exact(&mut body).ok()?;
    Some(([head[0], head[1], head[2]], body))
}

fn serve_one(listener: TcpListener, seen_policy: std::sync::mpsc::Sender<String>) {
    let (mut stream, _) = listener.accept().unwrap();
    while let Some((kind, body)) = read_frame(&mut stream) {
        match &kind {
            b"HEL" => {
                let mut ack = Writer::default();
                ack.u32(0).u32(65_536).u32(65_536).u32(0).u32(0);
                stream.write_all(&frame(b"ACK", &ack.bytes)).unwrap();
            }
            b"OPN" => {
                let mut reader = Reader::new(&body);
                reader.u32().unwrap();
                reader.string().unwrap();
                reader.byte_string().unwrap();
                reader.byte_string().unwrap();
                let sequence = reader.u32().unwrap();
                let request = reader.u32().unwrap();
                let mut reply = Writer::default();
                reply.u32(7).string(Some("http://opcfoundation.org/UA/SecurityPolicy#None")).byte_string(None).byte_string(None).u32(sequence).u32(request);
                reply.node_id(&NodeId::Numeric(0, 449));
                response_header(&mut reply, request);
                reply.u32(0).u32(7).u32(1).now().u32(600_000).byte_string(None);
                stream.write_all(&frame(b"OPN", &reply.bytes)).unwrap();
            }
            b"MSG" => {
                let mut reader = Reader::new(&body);
                reader.take(8).unwrap();
                let sequence = reader.u32().unwrap();
                let request = reader.u32().unwrap();
                let NodeId::Numeric(0, request_type) = reader.node_id().unwrap() else { panic!("a numeric request type") };
                let mut reply = Writer::default();
                reply.u32(7).u32(1).u32(sequence).u32(request);
                match request_type {
                    461 => {
                        reply.node_id(&NodeId::Numeric(0, 464));
                        response_header(&mut reply, request);
                        reply.node_id(&NodeId::Numeric(1, 5)).node_id(&NodeId::Numeric(1, 99)).f64(120_000.0).byte_string(None).byte_string(None);
                        reply.i32(1).string(Some("opc.tcp://test")).string(Some("urn:test")).string(None).u8(0).u32(0).string(None).string(None).i32(-1);
                        reply.byte_string(None).u32(1).string(Some("http://opcfoundation.org/UA/SecurityPolicy#None"));
                        reply.i32(1).string(Some("anon-policy")).u32(0).string(None).string(None).string(None);
                        reply.string(None).u8(0).i32(-1).string(None).byte_string(None).u32(0);
                    }
                    467 => {
                        reader.node_id().unwrap();
                        reader.date_time().unwrap();
                        reader.u32().unwrap();
                        reader.u32().unwrap();
                        reader.string().unwrap();
                        reader.u32().unwrap();
                        reader.extension_object().unwrap();
                        reader.string().unwrap();
                        reader.byte_string().unwrap();
                        reader.i32().unwrap();
                        reader.i32().unwrap();
                        let (_, token) = reader.extension_object().unwrap();
                        seen_policy.send(Reader::new(token).string().unwrap().unwrap_or_default()).unwrap();
                        reply.node_id(&NodeId::Numeric(0, 470));
                        response_header(&mut reply, request);
                        reply.byte_string(None).i32(-1).i32(-1);
                    }
                    787 => {
                        reply.node_id(&NodeId::Numeric(0, 790));
                        response_header(&mut reply, request);
                        reply.u32(42).f64(1_000.0).u32(600).u32(20);
                    }
                    751 => {
                        reply.node_id(&NodeId::Numeric(0, 754));
                        response_header(&mut reply, request);
                        reply.i32(2);
                        for (status, id) in [(0u32, 1u32), (0x8034_0000, 0)] {
                            reply.u32(status).u32(id).f64(1_000.0).u32(10).no_extension_object();
                        }
                        reply.i32(-1);
                    }
                    826 => {
                        reply.node_id(&NodeId::Numeric(0, 829));
                        response_header(&mut reply, request);
                        reply.u32(42).i32(-1).boolean(false).u32(1).now();
                        let mut change = Writer::default();
                        change.i32(1).u32(0).u8(0x01).u8(11).f64(21.5).i32(-1);
                        reply.i32(1).extension_object(811, &change.bytes).i32(-1).i32(-1);
                    }
                    _ => return,
                }
                stream.write_all(&frame(b"MSG", &reply.bytes)).unwrap();
            }
            _ => return,
        }
    }
}

#[test]
fn node_ids_and_endpoints_read_as_tools_write_them() {
    assert_eq!(NodeId::parse("ns=2;s=Line 1/Temperature").unwrap(), NodeId::Text(2, "Line 1/Temperature".to_owned()));
    assert_eq!(NodeId::parse("i=2258").unwrap(), NodeId::Numeric(0, 2258));
    assert!(NodeId::parse("ns=x;i=1").is_err());
    let endpoint = Endpoint::parse("opc.tcp://ops:secret@plc.local:4841/UA/Server").unwrap();
    assert_eq!(endpoint.url, "opc.tcp://plc.local:4841/UA/Server");
    assert_eq!(endpoint.address, "plc.local:4841");
    assert_eq!(endpoint.login, Some(("ops".to_owned(), "secret".to_owned())));
    let subscription = Subscription::parse("opc.tcp://plc|ns=2;s=A|i=2258").unwrap();
    assert_eq!(subscription.endpoint.address, "plc:4840");
    assert_eq!(subscription.nodes.len(), 2);
    assert!(Subscription::parse("opc.tcp://plc").is_err());
    let mut writer = Writer::default();
    writer.node_id(&NodeId::Numeric(3, 70_000)).node_id(&NodeId::Text(1, "x".to_owned()));
    let mut reader = Reader::new(&writer.bytes);
    assert_eq!(reader.node_id().unwrap(), NodeId::Numeric(3, 70_000));
    assert_eq!(reader.node_id().unwrap(), NodeId::Text(1, "x".to_owned()));
    assert!(reader.done());
    assert!(Reader::new(&[0x03, 0x00]).node_id().is_err());
}

#[test]
fn a_session_subscribes_and_receives_data_changes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || serve_one(listener, sender));
    let endpoint = Endpoint::parse(&format!("opc.tcp://{address}")).unwrap();
    let mut client = Client::connect(&endpoint).unwrap();
    assert_eq!(receiver.recv().unwrap(), "anon-policy", "the anonymous policy the server offered");
    let statuses = client.subscribe(&[NodeId::parse("ns=2;s=Temperature").unwrap(), NodeId::parse("ns=2;s=Missing").unwrap()], 1_000.0).unwrap();
    assert_eq!(statuses, vec![0, 0x8034_0000]);
    let changes = client.publish().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].handle, 0);
    assert!(changes[0].data_value.contains("21.5"), "{}", changes[0].data_value);
    client.close();
    server.join().unwrap();
}
