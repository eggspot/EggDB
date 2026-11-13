use crate::{Database, Error, ErrorKind, Result};
use eggdb_cluster::ShardMap;
use eggdb_cluster::transaction::{Decision, DecisionLog, TransactionId, Vote, decision_from};

const MAP_KEY: &[u8] = b"\xffCOL\x00\x7fcluster\x00map";
const NEXT_TRANSACTION_KEY: &[u8] = b"\xffCOL\x00\x7fcluster\x00next-transaction";
const DECISION_PREFIX: &[u8] = b"\xffCOL\x00\x7fcluster\x00decision\x00";
const FEATURE_LEVEL_KEY: &[u8] = b"\xffCOL\x00\x7fcluster\x00feature-level";
const FIRST_FEATURE_LEVEL: u32 = 1;
const COMMIT: u8 = 1;
const ABORT: u8 = 2;

fn decision_key(transaction: TransactionId) -> Vec<u8> {
    let mut key = DECISION_PREFIX.to_vec();
    key.extend_from_slice(&transaction.to_be_bytes());
    key
}

fn decision_of(bytes: &[u8]) -> Result<Decision> {
    match bytes {
        [COMMIT] => Ok(Decision::Commit),
        [ABORT] => Ok(Decision::Abort),
        _ => Err(broken("decision")),
    }
}

fn decision_byte(decision: Decision) -> u8 {
    match decision {
        Decision::Commit => COMMIT,
        Decision::Abort => ABORT,
    }
}

fn broken(what: &str) -> Error {
    Error::new(
        ErrorKind::Corrupt,
        format!("the cluster's stored {what} does not have its layout"),
    )
}

impl Database {
    /// The cluster's shard map (D212, D1039), kept as a key of the database itself, so in a
    /// cluster it is replicated through Raft like any write and every server reads the same
    /// map; `None` before one is published.
    pub fn shard_map(&self) -> Result<Option<ShardMap>> {
        let read = self.begin_read()?;
        read.get_raw(MAP_KEY)?
            .map(|bytes| ShardMap::decode(&bytes).ok_or_else(|| broken("shard map")))
            .transpose()
    }

    /// Publishes `map` when the stored map's epoch is still `expected_epoch` (`None` when no
    /// map is stored yet) and `map` raises it, in one write transaction; returns whether it
    /// was published. Two servers moving shards at once therefore cannot both win, and a
    /// follower's fenced database refuses the write, so only the leader publishes.
    pub fn publish_shard_map(&self, map: &ShardMap, expected_epoch: Option<u64>) -> Result<bool> {
        let mut transaction = self.begin_write()?;
        let current = transaction
            .get_raw(MAP_KEY)?
            .map(|bytes| ShardMap::decode(&bytes).ok_or_else(|| broken("shard map")))
            .transpose()?
            .map(|stored| stored.epoch);
        if current != expected_epoch || current.is_some_and(|epoch| map.epoch <= epoch) {
            return Ok(false);
        }
        transaction.put_raw(MAP_KEY, &map.encode())?;
        transaction.commit()?;
        Ok(true)
    }

    /// The feature level the cluster runs at (D1049): 1 until a rolling upgrade raised it.
    pub fn cluster_feature_level(&self) -> Result<u32> {
        let read = self.begin_read()?;
        match read.get_raw(FEATURE_LEVEL_KEY)? {
            Some(bytes) => Ok(u32::from_be_bytes(
                bytes.as_slice().try_into().map_err(|_| broken("feature level"))?,
            )),
            None => Ok(FIRST_FEATURE_LEVEL),
        }
    }

    /// Raises the cluster's feature level to `level` once every member speaks it; a level at
    /// or below the stored one changes nothing. Returns whether it rose. In a cluster the write
    /// goes through the log like any other, so every member learns it in order.
    pub fn raise_cluster_feature_level(&self, level: u32) -> Result<bool> {
        let mut transaction = self.begin_write()?;
        let current = match transaction.get_raw(FEATURE_LEVEL_KEY)? {
            Some(bytes) => u32::from_be_bytes(bytes.as_slice().try_into().map_err(|_| broken("feature level"))?),
            None => FIRST_FEATURE_LEVEL,
        };
        if level <= current {
            return Ok(false);
        }
        transaction.put_raw(FEATURE_LEVEL_KEY, &level.to_be_bytes())?;
        transaction.commit()?;
        Ok(true)
    }

    /// The decision log of cross-shard transactions kept in this database (D1039).
    pub fn decision_log(&self) -> DatabaseDecisions<'_> {
        DatabaseDecisions(self)
    }
}

/// The coordinator's decisions for cross-shard transactions, kept as keys of the cluster's
/// database: each decision is a write that commits through Raft before the coordinator tells
/// any participant, so a new leader finds every decision the old one sent.
#[derive(Debug, Clone, Copy)]
pub struct DatabaseDecisions<'d>(&'d Database);

impl DatabaseDecisions<'_> {
    /// The recorded decision of `transaction`, if any.
    pub fn recorded(&self, transaction: TransactionId) -> Result<Option<Decision>> {
        let read = self.0.begin_read()?;
        read.get_raw(&decision_key(transaction))?
            .map(|bytes| decision_of(&bytes))
            .transpose()
    }

    /// Drops the record of `transaction` once every participant has applied it.
    pub fn forget(&self, transaction: TransactionId) -> Result<()> {
        let mut write = self.0.begin_write()?;
        write.delete_raw(&decision_key(transaction))?;
        write.commit()?;
        Ok(())
    }

    fn record_unless_known(&self, transaction: TransactionId, decision: Decision) -> Result<Decision> {
        let mut write = self.0.begin_write()?;
        let key = decision_key(transaction);
        if let Some(bytes) = write.get_raw(&key)? {
            return decision_of(&bytes);
        }
        write.put_raw(&key, &[decision_byte(decision)])?;
        write.commit()?;
        Ok(decision)
    }
}

impl DecisionLog for DatabaseDecisions<'_> {
    fn begin(&mut self) -> std::result::Result<TransactionId, String> {
        let outcome = (|| -> Result<TransactionId> {
            let mut write = self.0.begin_write()?;
            let next = match write.get_raw(NEXT_TRANSACTION_KEY)? {
                Some(bytes) => u64::from_be_bytes(
                    bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| broken("transaction counter"))?,
                ),
                None => 0,
            }
            .checked_add(1)
            .ok_or_else(|| broken("transaction counter"))?;
            write.put_raw(NEXT_TRANSACTION_KEY, &next.to_be_bytes())?;
            write.commit()?;
            Ok(next)
        })();
        outcome.map_err(|error| error.to_string())
    }

    fn decide(
        &mut self,
        transaction: TransactionId,
        votes: &[Vote],
    ) -> std::result::Result<Decision, String> {
        self.record_unless_known(transaction, decision_from(votes))
            .map_err(|error| error.to_string())
    }

    fn outcome(&mut self, transaction: TransactionId) -> std::result::Result<Decision, String> {
        self.record_unless_known(transaction, Decision::Abort)
            .map_err(|error| error.to_string())
    }
}
