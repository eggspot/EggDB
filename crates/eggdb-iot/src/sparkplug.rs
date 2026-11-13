//! Sparkplug B ingest (D1072): a subscription to `spBv1.0/<group>/#` on any MQTT broker, EggDB's
//! own `--mqtt-listen` included. Every message's metrics become rows of `sparkplug_metrics`,
//! decoded in SQL by `sparkplug_decode`; birth certificates record each metric's alias in
//! `sparkplug_aliases`, so the data messages that carry only aliases are stored with names.

use crate::mqtt::{Broker, Subscriber};
use crate::sql::{bytea, literal, optional, pause, run, session};
use eggdb_sql::{KeyValueStore, Session};
use std::time::Duration;

const FIRST_PAUSE: Duration = Duration::from_secs(1);
const LONGEST_PAUSE: Duration = Duration::from_secs(30);

/// What a Sparkplug topic says: `spBv1.0/<group>/<message type>/<edge node>[/<device>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topic {
    /// The group id.
    pub group: String,
    /// `NBIRTH`, `NDATA`, `DDATA` and the rest.
    pub message_type: String,
    /// The edge node id.
    pub edge_node: String,
    /// The device id, for device messages.
    pub device: Option<String>,
}

impl Topic {
    /// Reads a Sparkplug B topic; `None` for other topics and for host `STATE` messages.
    pub fn parse(topic: &str) -> Option<Self> {
        let mut parts = topic.split('/');
        if parts.next()? != "spBv1.0" {
            return None;
        }
        let group = parts.next()?.to_owned();
        let message_type = parts.next()?.to_owned();
        let edge_node = parts.next()?.to_owned();
        let device = parts.next().map(str::to_owned);
        if parts.next().is_some() || !matches!(message_type.as_str(), "NBIRTH" | "NDEATH" | "DBIRTH" | "DDEATH" | "NDATA" | "DDATA" | "NCMD" | "DCMD") {
            return None;
        }
        Some(Self { group, message_type, edge_node, device })
    }
}

/// The statements that store one Sparkplug message: on a birth, the aliases first; then one row
/// per metric, its name taken from the aliases when the message carries only an alias.
pub fn statements(topic: &Topic, payload: &[u8]) -> Vec<String> {
    let device = topic.device.clone().unwrap_or_default();
    let keys = format!("{}, {}, {}", literal(&topic.group), literal(&topic.edge_node), literal(&device));
    let decoded = format!("sparkplug_decode({})", bytea(payload));
    let mut out = Vec::new();
    if topic.message_type.ends_with("BIRTH") {
        out.push(format!(
            "INSERT INTO sparkplug_aliases (group_id, edge_node, device, alias, metric) SELECT {keys}, (m ->> 'alias')::bigint, m ->> 'name' FROM jsonb_array_elements({decoded} -> 'metrics') AS m WHERE m ->> 'alias' IS NOT NULL AND m ->> 'name' IS NOT NULL ON CONFLICT (group_id, edge_node, device, alias) DO UPDATE SET metric = EXCLUDED.metric"
        ));
    }
    out.push(format!(
        "INSERT INTO sparkplug_metrics (received_at, group_id, message_type, edge_node, device, metric, alias, metric_time, datatype, value, seq) \
         SELECT now(), {}, {}, {}, {}, coalesce(m ->> 'name', a.metric), (m ->> 'alias')::bigint, to_timestamp((m ->> 'timestamp')::double precision / 1000), m ->> 'datatype', m -> 'value', (p.payload ->> 'seq')::bigint \
         FROM (SELECT {decoded} AS payload) AS p CROSS JOIN LATERAL jsonb_array_elements(p.payload -> 'metrics') AS m \
         LEFT JOIN sparkplug_aliases AS a ON a.group_id = {} AND a.edge_node = {} AND a.device = {} AND a.alias = (m ->> 'alias')::bigint",
        literal(&topic.group),
        literal(&topic.message_type),
        literal(&topic.edge_node),
        optional(topic.device.as_deref()),
        literal(&topic.group),
        literal(&topic.edge_node),
        literal(&device)
    ));
    out
}

/// One broker and the Sparkplug group to follow (`+` for every group), written
/// `mqtt://[user:password@]host[:port][|group]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparkplugSource {
    /// The broker.
    pub broker: Broker,
    /// The group id, or `+`.
    pub group: String,
}

impl SparkplugSource {
    /// Reads `mqtt://...[|group]`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (broker, group) = text.split_once('|').unwrap_or((text, "+"));
        if group.is_empty() || group.contains(['/', '#']) {
            return Err(format!("{text}: the group is one topic level, or + for all"));
        }
        Ok(Self { broker: Broker::parse(broker)?, group: group.to_owned() })
    }
}

/// Stores the Sparkplug messages of each source as they arrive.
pub struct SparkplugCollector<'s, S: KeyValueStore> {
    store: &'s S,
    user: String,
    database: String,
    sources: Vec<SparkplugSource>,
}

impl<S: KeyValueStore> std::fmt::Debug for SparkplugCollector<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SparkplugCollector").field("sources", &self.sources).finish_non_exhaustive()
    }
}

impl<'s, S: KeyValueStore> SparkplugCollector<'s, S> {
    /// A collector writing into `database` as `user`.
    pub fn new(store: &'s S, user: &str, database: &str, sources: Vec<SparkplugSource>) -> Self {
        Self { store, user: user.to_owned(), database: database.to_owned(), sources }
    }

    fn session(&self) -> Session<'s> {
        session(self.store, &self.user, &self.database, "sparkplug")
    }

    /// Creates `sparkplug_metrics` and `sparkplug_aliases` when they are missing.
    pub fn prepare(&self) -> Result<(), String> {
        let mut session = self.session();
        run(&mut session, "CREATE TABLE IF NOT EXISTS sparkplug_metrics (received_at timestamptz NOT NULL, group_id text NOT NULL, message_type text NOT NULL, edge_node text NOT NULL, device text, metric text, alias bigint, metric_time timestamptz, datatype text, value jsonb, seq bigint)")?;
        run(&mut session, "CREATE TABLE IF NOT EXISTS sparkplug_aliases (group_id text NOT NULL, edge_node text NOT NULL, device text NOT NULL, alias bigint NOT NULL, metric text NOT NULL, PRIMARY KEY (group_id, edge_node, device, alias))")
    }

    fn follow_once(&self, source: &SparkplugSource, stop: &(dyn Fn() -> bool + Sync), log: &(dyn Fn(&str) + Sync)) -> Result<(), String> {
        let client_id = format!("eggdb-sparkplug-{}", std::process::id());
        let mut subscriber = Subscriber::connect(&source.broker, &client_id, &format!("spBv1.0/{}/#", source.group))?;
        let mut session = self.session();
        while !stop() {
            let Some(delivered) = subscriber.receive()? else { continue };
            let Some(topic) = Topic::parse(&delivered.topic) else { continue };
            for statement in statements(&topic, &delivered.payload) {
                if let Err(error) = run(&mut session, &statement) {
                    log(&format!("{}: a {} message from {} was not stored: {error}", source.broker.address, topic.message_type, topic.edge_node));
                    break;
                }
            }
        }
        Ok(())
    }

    fn follow(&self, source: &SparkplugSource, stop: &(dyn Fn() -> bool + Sync), log: &(dyn Fn(&str) + Sync)) {
        let mut wait = FIRST_PAUSE;
        while !stop() {
            match self.follow_once(source, stop, log) {
                Ok(()) => return,
                Err(error) => log(&format!("{}: {error}; trying again in {} s", source.broker.address, wait.as_secs())),
            }
            pause(wait, stop);
            wait = (wait * 2).min(LONGEST_PAUSE);
        }
    }

    /// Follows every source on its own thread until `stop` answers true.
    pub fn serve(&self, stop: &(dyn Fn() -> bool + Sync), log: &(dyn Fn(&str) + Sync))
    where
        S: Sync,
    {
        std::thread::scope(|scope| {
            for source in &self.sources {
                scope.spawn(move || self.follow(source, stop, log));
            }
        });
    }
}
