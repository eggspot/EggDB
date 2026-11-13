use eggdb_pgwire::client::{ConnectOptions, PgClient, parse_postgres_url};
use eggdb_sql::multi_primary::{LoggedChange, apply_script, literal, log_query};
use eggdb_sql::{Dialect, KeyValueStore, Session};
use std::collections::BTreeMap;
use std::time::Duration;

const PULL_EVERY: Duration = Duration::from_secs(1);
const CHANGES_PER_PULL: u32 = 500;

/// Reads `--multi-primary b=postgresql://host:port/db,c=...`: each other server's name and URL.
pub(super) fn parse_peers(text: &str) -> Result<BTreeMap<String, ConnectOptions>, String> {
    let mut peers = BTreeMap::new();
    for part in text.split(',').map(str::trim).filter(|part| !part.is_empty()) {
        let (name, url) = part
            .split_once('=')
            .ok_or_else(|| format!("--multi-primary: '{part}' is not name=postgresql://host:port/database"))?;
        let name = name.trim();
        if name.is_empty() || !name.chars().all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_') {
            return Err(format!("--multi-primary: '{name}' is not a server name (letters, digits, - and _)"));
        }
        let options = parse_postgres_url(url.trim()).map_err(|message| format!("--multi-primary {name}: {message}"))?;
        if peers.insert(name.to_owned(), options).is_some() {
            return Err(format!("--multi-primary names {name} twice"));
        }
    }
    if peers.is_empty() {
        return Err("--multi-primary takes name=postgresql://host:port/database for every other server".to_owned());
    }
    Ok(peers)
}

fn run_local(store: &impl KeyValueStore, sql: &str) -> Result<Vec<Vec<Option<String>>>, String> {
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    let mut rows = Vec::new();
    session
        .execute(sql, &mut |result| {
            rows = result
                .rows
                .iter()
                .map(|row| row.iter().map(|value| value.to_text()).collect())
                .collect();
        })
        .map_err(|error| error.message().to_owned())?;
    let _ = session.close();
    Ok(rows)
}

fn changes_from(rows: &[Vec<Option<String>>]) -> Vec<LoggedChange> {
    rows.iter()
        .filter_map(|row| {
            let text = |at: usize| row.get(at).cloned().flatten();
            Some(LoggedChange {
                id: text(0)?.parse().ok()?,
                origin: text(1)?,
                stamp: text(2)?,
                table_schema: text(3)?,
                table_name: text(4)?,
                op: text(5)?,
                before: text(6),
                after: text(7),
            })
        })
        .collect()
}

/// Names this server in its multi-primary group (installing `eggdb_multi_primary` when it is
/// missing), then pulls every other server's new changes each second and applies them until
/// `stop` (D1098). A peer that cannot be reached is tried again on the next round.
pub(super) fn pull_peers(
    store: &impl KeyValueStore,
    node: &str,
    peers: &BTreeMap<String, ConnectOptions>,
    password: &str,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<(), String> {
    run_local(
        store,
        &format!(
            "CREATE EXTENSION IF NOT EXISTS eggdb_multi_primary; UPDATE eggdb_mp.settings SET value = {} WHERE name = 'node'",
            literal(Some(node))
        ),
    )?;
    let mut clients: BTreeMap<String, PgClient> = BTreeMap::new();
    while !stop() {
        std::thread::sleep(PULL_EVERY);
        for (name, options) in peers {
            if stop() {
                break;
            }
            if let Err(message) = pull_one(store, name, options, password, &mut clients) {
                clients.remove(name);
                eprintln!("eggdb serve: --multi-primary {name}: {message}");
            }
        }
    }
    Ok(())
}

fn pull_one(
    store: &impl KeyValueStore,
    name: &str,
    options: &ConnectOptions,
    password: &str,
    clients: &mut BTreeMap<String, PgClient>,
) -> Result<(), String> {
    if !clients.contains_key(name) {
        let mut options = options.clone();
        options.password.get_or_insert_with(|| password.to_owned());
        let client = PgClient::connect(&options).map_err(|error| error.to_string())?;
        clients.insert(name.to_owned(), client);
    }
    let client = clients.get_mut(name).ok_or_else(|| "the connection was lost".to_owned())?;
    loop {
        let cursor = run_local(store, &format!("SELECT last_id FROM eggdb_mp.cursors WHERE peer = {}", literal(Some(name))))?
            .first()
            .and_then(|row| row.first().cloned().flatten())
            .and_then(|text| text.parse::<i64>().ok())
            .unwrap_or(0);
        let rows = client
            .simple_query(&log_query(cursor, CHANGES_PER_PULL))
            .map_err(|error| error.to_string())?
            .pop()
            .map(|result| result.rows)
            .unwrap_or_default();
        let changes = changes_from(&rows);
        let Some(script) = apply_script(name, &changes) else {
            return Ok(());
        };
        run_local(store, &script)?;
        if changes.len() < CHANGES_PER_PULL as usize {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_are_named_urls() {
        let peers = parse_peers("b=postgresql://eggdb@127.0.0.1:5433/eggdb, c=postgresql://eggdb@127.0.0.1:5434/eggdb").unwrap();
        assert_eq!(peers.keys().cloned().collect::<Vec<_>>(), vec!["b", "c"]);
        assert_eq!(peers["b"].port, 5433);
        assert!(parse_peers("b=postgresql://h:1/d,b=postgresql://h:2/d").is_err());
        assert!(parse_peers("x y=postgresql://h:1/d").is_err());
        assert!(parse_peers("").is_err());
    }

    #[test]
    fn logged_rows_become_changes() {
        let row = vec![
            Some("7".to_owned()),
            Some("b".to_owned()),
            Some("000000000000001.000000.b".to_owned()),
            Some("public".to_owned()),
            Some("t".to_owned()),
            Some("d".to_owned()),
            Some("{}".to_owned()),
            None,
        ];
        let changes = changes_from(&[row]);
        assert_eq!(changes.len(), 1);
        assert_eq!((changes[0].id, changes[0].after.clone()), (7, None));
    }
}
