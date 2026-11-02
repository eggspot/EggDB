//! SQL Server statements and client metadata calls over a real loopback socket (D291):
//! SqlClient's `GetSchema("Databases")` RPC, `MERGE` with `WHEN NOT MATCHED BY SOURCE`,
//! `OPENJSON` without `WITH`, and `sp_getapplock`'s deadlock victim status `-3`.

mod common;

use common::{Client, TestServer, call_by_id, errors, rows, text_parameter};

const SP_EXECUTESQL: u16 = 10;

const SQLCLIENT_DATABASES: &str = "IF OBJECT_ID('master..sysdatabases') IS NULL EXEC sp_executesql N'select name as database_name, dbid, crdate as create_date from sysdatabases where (name = @Name or (@Name is null))',N'@Name NVARCHAR(128)',@Name=@Name ELSE EXEC sp_executesql N'select name as database_name, dbid, crdate as create_date from master..sysdatabases where (name = @Name or (@Name is null))',N'@Name NVARCHAR(128)',@Name=@Name";

fn databases_named(client: &mut Client, name: Option<&str>) -> Vec<Vec<Option<String>>> {
    let tokens = client.rpc(&[call_by_id(
        SP_EXECUTESQL,
        &[
            text_parameter("", Some(SQLCLIENT_DATABASES), false),
            text_parameter("", Some("@Name nvarchar(4000)"), false),
            text_parameter("@Name", name, false),
        ],
    )]);
    assert!(errors(&tokens).is_empty(), "{tokens:?}");
    rows(&tokens)
}

#[test]
fn sqlclient_get_schema_databases_lists_the_served_database() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let current = rows(&client.batch("SELECT DB_NAME() AS name"))[0][0].clone();
    let every = databases_named(&mut client, None);
    assert!(every.iter().any(|row| row[0] == current), "{every:?}");
    assert!(every.iter().all(|row| row.len() == 3));
    let one = databases_named(&mut client, current.as_deref());
    assert_eq!(one.len(), 1);
    assert_eq!(one[0][0], current);
    assert!(databases_named(&mut client, Some("nowhere")).is_empty());
}

#[test]
fn merge_by_source_and_openjson_without_with_run_over_tds() {
    let server = TestServer::start(8);
    let mut client = Client::login(server.address);
    let created = client.batch(
        "CREATE TABLE stock (id INT PRIMARY KEY, qty INT)\nINSERT INTO stock VALUES (1, 1), (2, 2), (3, 3)",
    );
    assert!(errors(&created).is_empty(), "{created:?}");
    let merged = client.batch(
        "MERGE stock AS t USING (VALUES (1, 10)) AS s (id, qty) ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET qty = s.qty \
         WHEN NOT MATCHED BY SOURCE AND t.id = 3 THEN DELETE \
         WHEN NOT MATCHED BY SOURCE THEN UPDATE SET qty = 0;\
         SELECT id, qty FROM stock ORDER BY id",
    );
    assert!(errors(&merged).is_empty(), "{merged:?}");
    assert_eq!(
        rows(&merged),
        vec![
            vec![Some("1".to_owned()), Some("10".to_owned())],
            vec![Some("2".to_owned()), Some("0".to_owned())],
        ]
    );
    let pairs = client.batch("SELECT [key], [value], [type] FROM OPENJSON(N'[\"a\",2]')");
    assert!(errors(&pairs).is_empty(), "{pairs:?}");
    assert_eq!(
        rows(&pairs),
        vec![
            vec![
                Some("0".to_owned()),
                Some("a".to_owned()),
                Some("1".to_owned())
            ],
            vec![
                Some("1".to_owned()),
                Some("2".to_owned()),
                Some("2".to_owned())
            ],
        ]
    );
}

fn wait_for(client: &mut Client, wanted: &str, held: &str) -> Option<String> {
    let tokens = client.batch(&format!(
        "DECLARE @r int\nEXEC @r = sp_getapplock N'{wanted}', 'Exclusive', 'Session', 30000\nIF @r < 0 EXEC sp_releaseapplock N'{held}', 'Session'\nSELECT @r AS r"
    ));
    assert!(errors(&tokens).is_empty(), "{tokens:?}");
    rows(&tokens).into_iter().next()?.into_iter().next()?
}

#[test]
fn two_connections_waiting_for_each_others_application_locks_pick_a_deadlock_victim() {
    let server = TestServer::start(8);
    let mut first = Client::login(server.address);
    let mut second = Client::login(server.address);
    for (client, resource) in [(&mut first, "a"), (&mut second, "b")] {
        let taken = client.batch(&format!(
            "DECLARE @r int\nEXEC @r = sp_getapplock N'{resource}', 'Exclusive', 'Session'\nSELECT @r AS r"
        ));
        assert_eq!(rows(&taken), vec![vec![Some("0".to_owned())]], "{taken:?}");
    }
    let waiting = std::thread::spawn(move || wait_for(&mut first, "b", "a"));
    std::thread::sleep(std::time::Duration::from_millis(100));
    let second_status = wait_for(&mut second, "a", "b");
    let first_status = waiting.join().unwrap();
    let mut statuses = vec![first_status, second_status];
    statuses.sort();
    assert_eq!(
        statuses,
        vec![Some("-3".to_owned()), Some("1".to_owned())],
        "one waiter is the victim and the other gets the lock after waiting"
    );
}
