//! Data changes stored as they arrive (D1071): one connection per subscribed endpoint, each
//! change a row of the `opcua_values` table, reconnecting with a growing pause after a failure.

use crate::binary::NodeId;
use crate::client::{Change, Client, Endpoint};
use eggdb_sql::json::{Json, parse_document};
use eggdb_sql::{Dialect, KeyValueStore, Session};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TABLE: &str = "opcua_values";
const PUBLISHING_INTERVAL_MS: f64 = 1_000.0;
const FIRST_PAUSE: Duration = Duration::from_secs(1);
const LONGEST_PAUSE: Duration = Duration::from_secs(30);
const STOP_POLL: Duration = Duration::from_millis(100);

/// One endpoint and the nodes whose values it delivers, written
/// `opc.tcp://[user:password@]host:port[/path]|<node>|<node>...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    /// Where to connect.
    pub endpoint: Endpoint,
    /// Each node as written and as read.
    pub nodes: Vec<(String, NodeId)>,
}

impl Subscription {
    /// Reads `endpoint|node|node...`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut parts = text.split('|');
        let endpoint = Endpoint::parse(parts.next().unwrap_or_default())?;
        let nodes = parts.map(|node| Ok((node.trim().to_owned(), NodeId::parse(node)?))).collect::<Result<Vec<_>, String>>()?;
        if nodes.is_empty() {
            return Err(format!("{text}: name at least one node after the endpoint, such as |ns=2;s=Temperature"));
        }
        Ok(Self { endpoint, nodes })
    }
}

/// Keeps every subscription's data changes in the `opcua_values` table: `received_at`,
/// `endpoint`, `node_id`, `value` (the Variant as JSON), `status` (the StatusCode) and the
/// `source_time` and `server_time` the server gave.
pub struct OpcUaCollector<'s, S: KeyValueStore> {
    store: &'s S,
    user: String,
    database: String,
    subscriptions: Vec<Subscription>,
}

impl<S: KeyValueStore> std::fmt::Debug for OpcUaCollector<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpcUaCollector").field("subscriptions", &self.subscriptions).finish_non_exhaustive()
    }
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn run(session: &mut Session<'_>, sql: &str) -> Result<(), String> {
    session.execute(sql, &mut |_| {}).map_err(|error| error.message().to_owned())
}

fn now_micros() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| i64::try_from(since.as_micros()).unwrap_or(i64::MAX))
}

fn row(endpoint: &str, node: &str, change: &Change) -> String {
    let document = parse_document(&change.data_value).ok();
    let member = |name: &str| {
        document.as_ref().and_then(|document| match &document.value {
            Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, node)| node),
            _ => None,
        })
    };
    let value = member("value").and_then(|node| change.data_value.get(node.start..node.end)).map_or_else(|| "NULL".to_owned(), |text| format!("{}::jsonb", literal(text)));
    let status = member("status").and_then(|node| match &node.value {
        Json::Number(number) => number.parse::<i64>().ok(),
        _ => None,
    });
    let time = |name: &str| {
        member(name)
            .and_then(|node| match &node.value {
                Json::String(text) => Some(format!("{}::timestamptz", literal(text))),
                _ => None,
            })
            .unwrap_or_else(|| "NULL".to_owned())
    };
    format!(
        "(to_timestamp({}), {}, {}, {value}, {}, {}, {})",
        now_micros() as f64 / 1_000_000.0,
        literal(endpoint),
        literal(node),
        status.unwrap_or(0),
        time("source_timestamp"),
        time("server_timestamp")
    )
}

impl<'s, S: KeyValueStore> OpcUaCollector<'s, S> {
    /// A collector writing into `database` as `user`.
    pub fn new(store: &'s S, user: &str, database: &str, subscriptions: Vec<Subscription>) -> Self {
        Self { store, user: user.to_owned(), database: database.to_owned(), subscriptions }
    }

    fn session(&self) -> Session<'s> {
        let mut session = Session::with_dialect(self.store, Dialect::PostgreSql);
        session.identify(&self.user, &self.database);
        session.set_protocol("opcua");
        session
    }

    /// Creates the `opcua_values` table when it is missing.
    pub fn prepare(&self) -> Result<(), String> {
        run(
            &mut self.session(),
            &format!("CREATE TABLE IF NOT EXISTS {TABLE} (received_at timestamptz NOT NULL, endpoint text NOT NULL, node_id text NOT NULL, value jsonb, status bigint NOT NULL, source_time timestamptz, server_time timestamptz)"),
        )
    }

    fn store_changes(&self, session: &mut Session<'_>, subscription: &Subscription, changes: &[Change]) -> Result<(), String> {
        let rows: Vec<String> = changes
            .iter()
            .filter_map(|change| {
                let (written, _) = subscription.nodes.get(change.handle as usize)?;
                Some(row(&subscription.endpoint.url, written, change))
            })
            .collect();
        if rows.is_empty() {
            return Ok(());
        }
        run(session, &format!("INSERT INTO {TABLE} (received_at, endpoint, node_id, value, status, source_time, server_time) VALUES {}", rows.join(", ")))
    }

    fn follow_once(&self, subscription: &Subscription, stop: &(dyn Fn() -> bool + Sync), log: &(dyn Fn(&str) + Sync)) -> Result<(), String> {
        let mut client = Client::connect(&subscription.endpoint)?;
        let nodes: Vec<NodeId> = subscription.nodes.iter().map(|(_, node)| node.clone()).collect();
        let statuses = client.subscribe(&nodes, PUBLISHING_INTERVAL_MS)?;
        for ((written, _), status) in subscription.nodes.iter().zip(statuses) {
            if status & 0x8000_0000 != 0 {
                log(&format!("{}: the server refused to monitor {written}: 0x{status:08x}", subscription.endpoint.url));
            }
        }
        let mut session = self.session();
        while !stop() {
            let changes = client.publish()?;
            if let Err(error) = self.store_changes(&mut session, subscription, &changes) {
                log(&format!("{}: {} changes were not stored: {error}", subscription.endpoint.url, changes.len()));
            }
        }
        client.close();
        Ok(())
    }

    fn follow(&self, subscription: &Subscription, stop: &(dyn Fn() -> bool + Sync), log: &(dyn Fn(&str) + Sync)) {
        let mut pause = FIRST_PAUSE;
        while !stop() {
            let started = Instant::now();
            match self.follow_once(subscription, stop, log) {
                Ok(()) => return,
                Err(error) => log(&format!("{}: {error}; trying again in {} s", subscription.endpoint.url, pause.as_secs())),
            }
            if started.elapsed() > LONGEST_PAUSE {
                pause = FIRST_PAUSE;
            }
            let waiting = Instant::now();
            while !stop() && waiting.elapsed() < pause {
                std::thread::sleep(STOP_POLL);
            }
            pause = (pause * 2).min(LONGEST_PAUSE);
        }
    }

    /// Follows every subscription, each on its own thread, until `stop` answers true; failures
    /// and refused nodes are reported to `log`.
    pub fn serve(&self, stop: &(dyn Fn() -> bool + Sync), log: &(dyn Fn(&str) + Sync))
    where
        S: Sync,
    {
        std::thread::scope(|scope| {
            for subscription in &self.subscriptions {
                scope.spawn(move || self.follow(subscription, stop, log));
            }
        });
    }
}
