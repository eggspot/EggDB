mod common;

use common::{Client, TestServer, Token, errors, rows};

fn described(tokens: &[Token]) -> Vec<(String, u8)> {
    tokens
        .iter()
        .find_map(|token| match token {
            Token::Columns(columns) => Some(columns.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[test]
fn dates_times_offsets_and_guids_go_in_their_own_tds_types() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let tokens = client.batch(
        "SELECT CAST('0001-01-01' AS date) AS first, CAST('9999-12-31' AS date) AS last, CAST('13:45:10.123' AS time(3)) AS t, CAST('2020-01-01T10:00:00+02:00' AS datetimeoffset(0)) AS o, CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS uniqueidentifier) AS g",
    );
    assert!(errors(&tokens).is_empty(), "{tokens:?}");
    assert_eq!(
        described(&tokens),
        vec![
            ("first".into(), 0x28),
            ("last".into(), 0x28),
            ("t".into(), 0x29),
            ("o".into(), 0x2B),
            ("g".into(), 0x24),
        ]
    );
    assert_eq!(
        rows(&tokens)[0],
        vec![
            Some("days=0".to_owned()),
            Some("days=3652058".to_owned()),
            Some("units=49510123".to_owned()),
            Some("days=737424 units=28800 offset=0".to_owned()),
            Some("ff19966f868b11d0b42d00c04fc964ff".to_owned()),
        ]
    );
}

#[test]
fn nulls_of_the_new_types_have_a_zero_length() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    client.batch(
        "CREATE TABLE n (id INT PRIMARY KEY, d DATE, t TIME, o DATETIMEOFFSET, g UNIQUEIDENTIFIER) INSERT INTO n (id) VALUES (1)",
    );
    let tokens = client.batch("SELECT d, t, o, g FROM n");
    assert_eq!(
        described(&tokens)
            .into_iter()
            .map(|(_, kind)| kind)
            .collect::<Vec<_>>(),
        vec![0x28, 0x29, 0x2B, 0x24]
    );
    assert_eq!(rows(&tokens)[0], vec![None, None, None, None]);
}

#[test]
fn unconstrained_decimals_take_the_precision_their_values_need() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let tokens =
        client.batch("SELECT 1.50 AS small, 123456789012345678901234567890123456789.5 AS wide");
    assert_eq!(
        described(&tokens),
        vec![("small".into(), 0x6A), ("wide".into(), 0xE7)]
    );
    assert_eq!(
        rows(&tokens)[0],
        vec![
            Some("1.50".to_owned()),
            Some("123456789012345678901234567890123456789.5".to_owned())
        ]
    );
}

#[test]
fn output_rows_carry_their_statements_command_and_count() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    client.batch("CREATE TABLE o (id INT IDENTITY(1,1) PRIMARY KEY, qty INT)");
    let tokens = client.batch("INSERT INTO o (qty) OUTPUT inserted.id VALUES (5), (6)");
    assert_eq!(
        rows(&tokens),
        vec![vec![Some("1".to_owned())], vec![Some("2".to_owned())]]
    );
    let done = tokens
        .iter()
        .find_map(|token| match token {
            Token::Done { command, rows, .. } => Some((*command, *rows)),
            _ => None,
        })
        .unwrap();
    assert_eq!(done, (0xC3, 2));
    let count = client.batch("SELECT COUNT(*) AS n FROM o");
    assert_eq!(described(&count), vec![("n".into(), 0x26)]);
    assert_eq!(rows(&count)[0], vec![Some("2".to_owned())]);
}
