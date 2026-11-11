use crate::protocol::Reply;
use std::collections::BTreeMap;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::Instant;

const CONTAINER_COMMANDS: [&str; 12] = [
    "client", "config", "cluster", "object", "xinfo", "xgroup", "debug", "command", "memory", "script",
    "function", "acl",
];

/// What one connection last reported about itself, for `CLIENT LIST` and `CLIENT INFO`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Activity {
    pub(crate) name: Option<Vec<u8>>,
    pub(crate) db: u32,
    pub(crate) resp3: bool,
    pub(crate) channels: usize,
    pub(crate) patterns: usize,
    pub(crate) shard_channels: usize,
    pub(crate) queued: Option<usize>,
    pub(crate) watched: usize,
    pub(crate) command: String,
}

struct Record {
    address: SocketAddr,
    local_address: SocketAddr,
    stream: Option<TcpStream>,
    created: Instant,
    last_active: Instant,
    activity: Activity,
    killed: bool,
}

impl Record {
    fn is_pubsub(&self) -> bool {
        self.activity.channels + self.activity.patterns + self.activity.shard_channels > 0
    }

    fn line(&self, id: u64) -> String {
        let activity = &self.activity;
        let mut flags = String::new();
        if self.is_pubsub() {
            flags.push('P');
        }
        if activity.queued.is_some() {
            flags.push('x');
        }
        if flags.is_empty() {
            flags.push('N');
        }
        let name = activity.name.as_deref().map(String::from_utf8_lossy).unwrap_or_default();
        format!(
            "id={id} addr={} laddr={} fd=-1 name={name} age={} idle={} flags={flags} db={} sub={} psub={} ssub={} multi={} watch={} qbuf=0 qbuf-free=0 argv-mem=0 multi-mem=0 rbs=0 rbp=0 obl=0 oll=0 omem=0 tot-mem=0 events=r cmd={} user=default redir=-1 resp={} lib-name= lib-ver=\n",
            self.address,
            self.local_address,
            self.created.elapsed().as_secs(),
            self.last_active.elapsed().as_secs(),
            activity.db,
            activity.channels,
            activity.patterns,
            activity.shard_channels,
            activity.queued.map_or(-1, |queued| queued as i64),
            activity.watched,
            if activity.command.is_empty() { "NULL" } else { activity.command.as_str() },
            if activity.resp3 { 3 } else { 2 },
        )
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct KillFilter {
    ids: Vec<u64>,
    address: Option<String>,
    local_address: Option<String>,
    pubsub: Option<bool>,
    user: Option<String>,
    skip_me: bool,
    max_age_secs: Option<u64>,
}

fn lower(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_ascii_lowercase()
}

fn client_type(word: &[u8]) -> Result<Option<bool>, String> {
    match lower(word).as_str() {
        "normal" => Ok(Some(false)),
        "pubsub" => Ok(Some(true)),
        "master" | "replica" | "slave" => Ok(None),
        other => Err(format!("ERR Unknown client type '{other}'")),
    }
}

fn kill_filter(args: &[Vec<u8>]) -> Result<KillFilter, String> {
    let mut filter = KillFilter {
        skip_me: true,
        ..KillFilter::default()
    };
    let mut rest = args;
    while let [option, value, tail @ ..] = rest {
        let text = String::from_utf8_lossy(value).into_owned();
        match lower(option).as_str() {
            "id" => filter
                .ids
                .push(text.parse().map_err(|_| "ERR client-id should be greater than 0".to_owned())?),
            "addr" => filter.address = Some(text),
            "laddr" => filter.local_address = Some(text),
            "type" => match client_type(value)? {
                Some(pubsub) => filter.pubsub = Some(pubsub),
                None => filter.ids.push(0),
            },
            "user" => filter.user = Some(text),
            "skipme" => {
                filter.skip_me = match lower(value).as_str() {
                    "yes" => true,
                    "no" => false,
                    _ => return Err("ERR syntax error".to_owned()),
                }
            }
            "maxage" => {
                filter.max_age_secs = Some(text.parse().map_err(|_| "ERR syntax error".to_owned())?)
            }
            _ => return Err("ERR syntax error".to_owned()),
        }
        rest = tail;
    }
    if rest.is_empty() {
        Ok(filter)
    } else {
        Err("ERR syntax error".to_owned())
    }
}

/// The server's open connections, for `CLIENT LIST`, `CLIENT INFO` and `CLIENT KILL`.
#[derive(Default)]
pub(crate) struct Clients {
    records: Mutex<BTreeMap<u64, Record>>,
}

impl Clients {
    fn locked(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Record>> {
        self.records.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn register(&self, id: u64, address: SocketAddr, local_address: SocketAddr, stream: Option<TcpStream>) {
        let now = Instant::now();
        self.locked().insert(
            id,
            Record {
                address,
                local_address,
                stream,
                created: now,
                last_active: now,
                activity: Activity::default(),
                killed: false,
            },
        );
    }

    pub(crate) fn seen(&self, id: u64, mut activity: Activity, arguments: &[Vec<u8>]) {
        let name = arguments.first().map(|word| lower(word)).unwrap_or_default();
        activity.command = match arguments.get(1) {
            Some(sub) if CONTAINER_COMMANDS.contains(&name.as_str()) => format!("{name}|{}", lower(sub)),
            _ => name,
        };
        if let Some(record) = self.locked().get_mut(&id) {
            record.last_active = Instant::now();
            record.activity = activity;
        }
    }

    pub(crate) fn remove(&self, id: u64) {
        self.locked().remove(&id);
    }

    pub(crate) fn is_killed(&self, id: u64) -> bool {
        self.locked().get(&id).is_some_and(|record| record.killed)
    }

    fn kill(record: &mut Record, own: bool) {
        record.killed = true;
        if !own {
            if let Some(stream) = record.stream.take() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }

    /// Answers the `CLIENT` subcommands that need every connection: `LIST`, `INFO`, `KILL` and
    /// `ID`; `None` for the ones the connection answers alone.
    pub(crate) fn command(&self, id: u64, args: &[Vec<u8>]) -> Option<Reply> {
        let (sub, rest) = args.split_first()?;
        Some(match lower(sub).as_str() {
            "id" if rest.is_empty() => Reply::Integer(i64::try_from(id).unwrap_or(i64::MAX)),
            "info" if rest.is_empty() => {
                Reply::Bulk(self.locked().get(&id).map(|record| record.line(id).into_bytes()))
            }
            "list" => self.list(rest),
            "kill" => self.kill_matching(id, rest),
            "no-evict" | "no-touch" if rest.len() == 1 => Reply::ok(),
            _ => return None,
        })
    }

    fn list(&self, args: &[Vec<u8>]) -> Reply {
        let (pubsub, ids) = match args {
            [] => (None, None),
            [option, kind] if lower(option) == "type" => match client_type(kind) {
                Ok(Some(pubsub)) => (Some(pubsub), None),
                Ok(None) => (None, Some(Vec::new())),
                Err(error) => return Reply::Error(error),
            },
            [option, ids @ ..] if lower(option) == "id" && !ids.is_empty() => {
                let parsed: Result<Vec<u64>, _> =
                    ids.iter().map(|id| String::from_utf8_lossy(id).parse::<u64>()).collect();
                match parsed {
                    Ok(parsed) => (None, Some(parsed)),
                    Err(_) => return Reply::Error("ERR Invalid client ID".to_owned()),
                }
            }
            _ => return Reply::Error("ERR syntax error".to_owned()),
        };
        let records = self.locked();
        let mut text = String::new();
        for (record_id, record) in records.iter() {
            if pubsub.is_some_and(|pubsub| pubsub != record.is_pubsub()) {
                continue;
            }
            if ids.as_ref().is_some_and(|ids| !ids.contains(record_id)) {
                continue;
            }
            text.push_str(&record.line(*record_id));
        }
        Reply::Bulk(Some(text.into_bytes()))
    }

    fn kill_matching(&self, own_id: u64, args: &[Vec<u8>]) -> Reply {
        if let [address] = args {
            let wanted = String::from_utf8_lossy(address).into_owned();
            let mut records = self.locked();
            return match records
                .iter_mut()
                .find(|(_, record)| !record.killed && record.address.to_string() == wanted)
            {
                Some((id, record)) => {
                    Self::kill(record, *id == own_id);
                    Reply::ok()
                }
                None => Reply::Error("ERR No such client".to_owned()),
            };
        }
        let filter = match kill_filter(args) {
            Ok(filter) if !args.is_empty() => filter,
            Ok(_) => return Reply::Error("ERR syntax error".to_owned()),
            Err(error) => return Reply::Error(error),
        };
        let mut records = self.locked();
        let mut killed = 0_i64;
        for (id, record) in records.iter_mut() {
            let matches = !record.killed
                && !(filter.skip_me && *id == own_id)
                && (filter.ids.is_empty() || filter.ids.contains(id))
                && filter.address.as_ref().is_none_or(|address| record.address.to_string() == *address)
                && filter
                    .local_address
                    .as_ref()
                    .is_none_or(|address| record.local_address.to_string() == *address)
                && filter.pubsub.is_none_or(|pubsub| pubsub == record.is_pubsub())
                && filter.user.as_ref().is_none_or(|user| user == "default")
                && filter
                    .max_age_secs
                    .is_none_or(|age| record.created.elapsed().as_secs() >= age);
            if matches {
                Self::kill(record, *id == own_id);
                killed += 1;
            }
        }
        Reply::Integer(killed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<Vec<u8>> {
        line.split(' ').map(|word| word.as_bytes().to_vec()).collect()
    }

    #[test]
    fn kill_filters_parse_every_option_and_refuse_odd_ones() {
        let filter = kill_filter(&words("ID 7 TYPE pubsub SKIPME no MAXAGE 30 ADDR 1.2.3.4:5")).unwrap();
        assert_eq!(filter.ids, vec![7]);
        assert_eq!(filter.pubsub, Some(true));
        assert!(!filter.skip_me);
        assert_eq!(filter.max_age_secs, Some(30));
        assert_eq!(filter.address.as_deref(), Some("1.2.3.4:5"));
        assert!(kill_filter(&words("ID")).is_err());
        assert!(kill_filter(&words("TYPE nobody")).is_err());
        assert!(kill_filter(&words("SKIPME maybe")).is_err());
    }
}
