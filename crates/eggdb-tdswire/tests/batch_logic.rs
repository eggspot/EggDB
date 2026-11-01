//! Transact-SQL batch logic over a real loopback socket (D137): `PRINT` and low-severity
//! `RAISERROR` as `INFO` tokens, `RAISERROR` and `THROW` as `ERROR` tokens with their numbers,
//! a `TRY ... CATCH` that keeps its error from the client, and loops with variables.

mod common;

use common::{Client, TestServer, Token, errors, rows};

fn infos(tokens: &[Token]) -> Vec<(u32, String)> {
    tokens
        .iter()
        .filter_map(|token| match token {
            Token::Info { number, message } => Some((*number, message.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn print_and_low_severity_raiserror_arrive_as_info_messages() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let tokens = client.batch(
        "PRINT N'starting'\nRAISERROR(N'step %d of %d', 10, 1, 1, 2) WITH NOWAIT\nSELECT 1 AS one",
    );
    assert!(errors(&tokens).is_empty(), "{tokens:?}");
    assert_eq!(
        infos(&tokens),
        vec![
            (0, "starting".to_owned()),
            (50_000, "step 1 of 2".to_owned())
        ]
    );
    assert_eq!(rows(&tokens), vec![vec![Some("1".to_owned())]]);
}

#[test]
fn raiserror_continues_the_batch_and_throw_ends_it() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let raised = client.batch("SELECT 1 AS a\nRAISERROR(N'bad', 16, 4)\nSELECT 2 AS b");
    assert_eq!(errors(&raised), vec![(50_000, 16, "bad".to_owned())]);
    assert_eq!(
        rows(&raised),
        vec![vec![Some("1".to_owned())], vec![Some("2".to_owned())]]
    );
    let line = raised.iter().find_map(|token| match token {
        Token::Error { line, .. } => Some(*line),
        _ => None,
    });
    assert_eq!(line, Some(2));
    let thrown = client.batch("THROW 51000, N'stop', 1\nSELECT 3 AS c");
    assert_eq!(errors(&thrown), vec![(51_000, 16, "stop".to_owned())]);
    assert!(rows(&thrown).is_empty());
}

#[test]
fn try_catch_and_loops_run_over_tds() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let tokens = client.batch(
        "DECLARE @i int = 0, @total int = 0
WHILE @i < 4 BEGIN SET @i += 1 SET @total += @i END
BEGIN TRY
    SELECT 1 / 0 AS impossible
END TRY
BEGIN CATCH
    SELECT ERROR_NUMBER() AS n, @total AS total
END CATCH",
    );
    assert!(errors(&tokens).is_empty(), "{tokens:?}");
    assert_eq!(
        rows(&tokens),
        vec![vec![Some("8134".to_owned()), Some("10".to_owned())]]
    );
}
