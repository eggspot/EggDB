//! Bulk loads over real sockets, as `SqlBulkCopy` and `bcp in` run them ([MS-TDS] 2.2.6.1):
//! `SqlBulkCopy`'s first batch (`@@trancount`, `SET FMTONLY ON` and `sp_tablecollations_100`),
//! an `INSERT BULK` statement, then a bulk load message of column metadata, rows and DONE that
//! spans many packets; a load that fails and loads nothing; and malformed bulk data, bulk data
//! of the wrong shape and bulk data without an `INSERT BULK`, after each of which the
//! connection keeps serving.

mod common;

use common::{Client, TestServer, Token, dones, errors, parse_tokens, rows, utf16};

const DONE_COUNT: u16 = 0x0010;

fn column(name: &str, type_info: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 0, 0x09, 0x00];
    out.extend_from_slice(type_info);
    out.push(name.encode_utf16().count() as u8);
    out.extend(utf16(name));
    out
}

fn metadata() -> Vec<u8> {
    let mut body = vec![0x81, 2, 0];
    body.extend(column("id", &[0x26, 4]));
    body.extend(column(
        "name",
        &[0xE7, 0xFF, 0xFF, 0x09, 0x04, 0x00, 0x22, 0x00],
    ));
    body
}

fn row(id: i32, name: &str) -> Vec<u8> {
    let mut out = vec![0xD1, 4];
    out.extend_from_slice(&id.to_le_bytes());
    let text = utf16(name);
    out.extend_from_slice(&(text.len() as u64).to_le_bytes());
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(&text);
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

fn done() -> Vec<u8> {
    vec![0xFD, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

fn bulk_body(rows: &[Vec<u8>]) -> Vec<u8> {
    let mut body = metadata();
    for row in rows {
        body.extend_from_slice(row);
    }
    body.extend(done());
    body
}

fn send_bulk(client: &mut Client, body: &[u8]) -> Vec<Token> {
    client.send(0x07, body);
    let (_, reply) = client.read_message().expect("a bulk load reply");
    parse_tokens(&reply)
}

fn count(client: &mut Client) -> String {
    rows(&client.batch("SELECT count(*) AS n FROM items"))[0][0]
        .clone()
        .unwrap_or_default()
}

#[test]
fn sqlbulkcopys_requests_describe_the_table_and_load_rows_that_span_packets() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let created = client.batch("CREATE TABLE items (id INT PRIMARY KEY, name NVARCHAR(MAX))");
    assert!(errors(&created).is_empty());
    let initial = client.batch(
        "select @@trancount; SET FMTONLY ON select * from [dbo].[items] SET FMTONLY OFF exec ..sp_tablecollations_100 N'[dbo].[items]'",
    );
    assert!(errors(&initial).is_empty(), "{initial:?}");
    let described: Vec<&Vec<(String, u8)>> = initial
        .iter()
        .filter_map(|token| match token {
            Token::Columns(columns) => Some(columns),
            _ => None,
        })
        .collect();
    assert_eq!(described.len(), 3);
    assert_eq!(
        described[1],
        &vec![("id".to_owned(), 0x26), ("name".to_owned(), 0xE7)]
    );
    let found = rows(&initial);
    assert_eq!(found[0], vec![Some("0".to_owned())]);
    assert_eq!(found.len(), 3);
    assert_eq!(found[1][3], None);
    assert_eq!(found[2][3].as_deref(), Some("Latin1_General_100_BIN2"));
    let started = client.batch(
        "insert bulk [dbo].[items] ([id] Int, [name] NVarChar(max) COLLATE Latin1_General_100_BIN2) with (TABLOCK)",
    );
    assert!(errors(&started).is_empty(), "{started:?}");
    let long = "x".repeat(10_000);
    let sent: Vec<Vec<u8>> = (1..=300)
        .map(|id| {
            if id == 7 {
                row(id, &long)
            } else {
                row(id, &format!("item {id}"))
            }
        })
        .collect();
    let reply = send_bulk(&mut client, &bulk_body(&sent));
    assert!(errors(&reply).is_empty(), "{reply:?}");
    let (status, loaded) = *dones(&reply).last().unwrap();
    assert_eq!(loaded, 300);
    assert_ne!(status & DONE_COUNT, 0);
    assert_eq!(count(&mut client), "300");
    assert_eq!(
        rows(&client.batch("SELECT name FROM items WHERE id = 7")),
        vec![vec![Some(long)]]
    );
}

#[test]
fn failing_and_malformed_bulk_loads_load_nothing_and_the_connection_keeps_serving() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    client.batch("CREATE TABLE items (id INT PRIMARY KEY, name NVARCHAR(MAX))");
    client.batch("INSERT BULK items (id Int, name NVarChar(max))");
    let reply = send_bulk(
        &mut client,
        &bulk_body(&[row(1, "a"), row(2, "b"), row(1, "again")]),
    );
    assert_eq!(errors(&reply)[0].0, 2627);
    assert_eq!(count(&mut client), "0");

    let reply = send_bulk(&mut client, &bulk_body(&[row(3, "c")]));
    assert_eq!(errors(&reply)[0].0, 4002);

    client.batch("INSERT BULK items (id Int, name NVarChar(max))");
    let mut garbage = metadata();
    garbage.extend_from_slice(&[0xD1, 9, 9, 9]);
    garbage.extend(vec![0x55; 9000]);
    let reply = send_bulk(&mut client, &garbage);
    assert_eq!(errors(&reply)[0].0, 4804);

    client.batch("INSERT BULK items (id Int, name NVarChar(max))");
    let mut variant = vec![0x81, 1, 0];
    variant.extend(column("v", &[0x62, 0, 0, 0, 0]));
    variant.extend(done());
    let reply = send_bulk(&mut client, &variant);
    assert_eq!(errors(&reply)[0].0, 4816);

    client.batch("INSERT BULK items (id Int)");
    let reply = send_bulk(&mut client, &bulk_body(&[row(4, "d")]));
    assert_eq!(errors(&reply)[0].0, 4815);

    client.batch("INSERT BULK items (id Int, name NVarChar(max))");
    client.batch("SELECT 1 AS one");
    let reply = send_bulk(&mut client, &bulk_body(&[row(5, "e")]));
    assert_eq!(errors(&reply)[0].0, 4002);

    assert_eq!(count(&mut client), "0");
    assert_eq!(
        rows(&client.batch("SELECT 1 AS one")),
        vec![vec![Some("1".to_owned())]]
    );
}
