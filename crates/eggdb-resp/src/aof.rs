use crate::commands::{Client, Context};
use crate::keyspace::Keyspace;
use crate::protocol::{ProtocolError, Reply, RequestReader};
use crate::transactions::execute_with_transactions;
use std::io::Read;

const RDB_MAGIC: &[u8] = b"REDIS";

/// What replaying an append-only file did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AofReport {
    /// Commands replayed.
    pub commands: u64,
    /// Commands that answered with an error.
    pub failed: u64,
    /// The first of those errors, with the command's number.
    pub first_error: Option<String>,
}

/// Replays an append-only file, the RESP commands a server logged (Redis's AOF format, which
/// is RESP requests one after another), into `keyspace`; `MULTI`/`EXEC` blocks run as one
/// transaction each, and `SELECT` switches databases as it did when logged.
pub fn replay_aof(
    keyspace: &dyn Keyspace,
    input: impl Read,
    now_ms: u64,
) -> Result<AofReport, String> {
    let mut replay = AofReplay::new(keyspace, now_ms);
    replay.replay(input)?;
    Ok(replay.finish())
}

/// Replays the files of a multi-part append-only file one after another, as one client: the
/// database `SELECT` chose in one file stays chosen in the next, and the report counts every
/// file's commands.
pub struct AofReplay<'k> {
    context: Context<'k>,
    client: Client,
    report: AofReport,
}

impl std::fmt::Debug for AofReplay<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AofReplay")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

impl<'k> AofReplay<'k> {
    /// Starts a replay into `keyspace`, with `now_ms` as the time expiries are measured from.
    #[must_use]
    pub fn new(keyspace: &'k dyn Keyspace, now_ms: u64) -> Self {
        Self {
            context: Context {
                keyspace,
                password_sha256: None,
                now_ms,
            },
            client: Client {
                authenticated: true,
                ..Client::default()
            },
            report: AofReport::default(),
        }
    }

    /// Replays one file of RESP commands. A file that starts with an RDB snapshot (Redis's
    /// `aof-use-rdb-preamble`) is refused, since the RDB format has no public specification.
    pub fn replay(&mut self, mut input: impl Read) -> Result<(), String> {
        let mut start = Vec::with_capacity(RDB_MAGIC.len());
        (&mut input)
            .take(RDB_MAGIC.len() as u64)
            .read_to_end(&mut start)
            .map_err(|error| format!("cannot read the file: {error}"))?;
        if start == RDB_MAGIC {
            return Err("the file starts with an RDB snapshot (Redis's aof-use-rdb-preamble), and RDB is not read: Redis does not publish a specification of its format; set aof-use-rdb-preamble no and run BGREWRITEAOF in the old server, then restore again".to_owned());
        }
        let mut reader = RequestReader::new(std::io::Cursor::new(start).chain(input));
        loop {
            let arguments = match reader.next_request() {
                Ok(arguments) => arguments,
                Err(ProtocolError::Closed) => return Ok(()),
                Err(ProtocolError::Invalid(message)) => {
                    return Err(format!(
                        "the file is not an append-only file of RESP commands after command {}: {message}",
                        self.report.commands
                    ));
                }
                Err(ProtocolError::OverBudget(bytes)) => {
                    return Err(format!(
                        "command {} of the append-only file declares {bytes} bytes, more than it may hold",
                        self.report.commands + 1
                    ));
                }
            };
            self.report.commands += 1;
            if let Reply::Error(error) =
                execute_with_transactions(&self.context, &mut self.client, &arguments)
            {
                self.report.failed += 1;
                if self.report.first_error.is_none() {
                    self.report.first_error =
                        Some(format!("command {}: {error}", self.report.commands));
                }
            }
        }
    }

    /// Ends the replay with what it did.
    #[must_use]
    pub fn finish(self) -> AofReport {
        self.report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyspace::MemoryKeyspace;

    #[test]
    fn an_append_only_file_replays_into_the_keyspace() {
        let aof = b"*2\r\n$6\r\nSELECT\r\n$1\r\n0\r\n*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n*1\r\n$5\r\nMULTI\r\n*4\r\n$5\r\nRPUSH\r\n$1\r\nl\r\n$1\r\na\r\n$1\r\nb\r\n*1\r\n$4\r\nEXEC\r\n*2\r\n$4\r\nINCR\r\n$1\r\nk\r\n".to_vec();
        let keyspace = MemoryKeyspace::default();
        let report = replay_aof(&keyspace, &aof[..], 0).unwrap();
        assert_eq!(report.commands, 6);
        assert_eq!(report.failed, 1);
        assert!(report.first_error.unwrap().starts_with("command 6:"));
        let mut client = Client {
            authenticated: true,
            ..Client::default()
        };
        let context = Context {
            keyspace: &keyspace,
            password_sha256: None,
            now_ms: 0,
        };
        let reply =
            crate::commands::execute(&context, &mut client, &[b"LLEN".to_vec(), b"l".to_vec()]);
        assert_eq!(reply, Reply::Integer(2));
        assert!(replay_aof(&keyspace, &b"*1\r\n:x\r\n"[..], 0).is_err());
    }

    #[test]
    fn the_parts_of_a_multi_part_file_replay_as_one_client() {
        let keyspace = MemoryKeyspace::default();
        let mut replay = AofReplay::new(&keyspace, 0);
        replay
            .replay(
                &b"*2\r\n$6\r\nSELECT\r\n$1\r\n3\r\n*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\n1\r\n"[..],
            )
            .unwrap();
        replay.replay(&b""[..]).unwrap();
        replay
            .replay(&b"*2\r\n$4\r\nINCR\r\n$1\r\nk\r\n"[..])
            .unwrap();
        let report = replay.finish();
        assert_eq!(report.commands, 3);
        assert_eq!(report.failed, 0);
        let mut client = Client {
            authenticated: true,
            db: 3,
            ..Client::default()
        };
        let context = Context {
            keyspace: &keyspace,
            password_sha256: None,
            now_ms: 0,
        };
        let reply =
            crate::commands::execute(&context, &mut client, &[b"GET".to_vec(), b"k".to_vec()]);
        assert_eq!(reply, Reply::Bulk(Some(b"2".to_vec())));
    }

    #[test]
    fn a_file_starting_with_an_rdb_snapshot_is_refused() {
        let keyspace = MemoryKeyspace::default();
        let error = replay_aof(&keyspace, &b"REDIS0011\xfa\x09redis-ver"[..], 0).unwrap_err();
        assert!(error.contains("RDB snapshot"), "{error}");
    }
}
