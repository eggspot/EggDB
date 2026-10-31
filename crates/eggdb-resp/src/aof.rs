use crate::commands::{Client, Context};
use crate::keyspace::Keyspace;
use crate::protocol::{ProtocolError, Reply, RequestReader};
use crate::transactions::execute_with_transactions;
use std::io::Read;

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
pub fn replay_aof(keyspace: &dyn Keyspace, input: impl Read, now_ms: u64) -> Result<AofReport, String> {
    let mut reader = RequestReader::new(input);
    let mut client = Client {
        authenticated: true,
        ..Client::default()
    };
    let context = Context {
        keyspace,
        password_sha256: None,
        now_ms,
    };
    let mut report = AofReport::default();
    loop {
        let arguments = match reader.next() {
            Ok(arguments) => arguments,
            Err(ProtocolError::Closed) => return Ok(report),
            Err(ProtocolError::Invalid(message)) => {
                return Err(format!(
                    "the file is not an append-only file of RESP commands after command {}: {message}",
                    report.commands
                ));
            }
        };
        report.commands += 1;
        if let Reply::Error(error) = execute_with_transactions(&context, &mut client, &arguments) {
            report.failed += 1;
            if report.first_error.is_none() {
                report.first_error = Some(format!("command {}: {error}", report.commands));
            }
        }
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
        let mut client = Client { authenticated: true, ..Client::default() };
        let context = Context { keyspace: &keyspace, password_sha256: None, now_ms: 0 };
        let reply = crate::commands::execute(&context, &mut client, &[b"LLEN".to_vec(), b"l".to_vec()]);
        assert_eq!(reply, Reply::Integer(2));
        assert!(replay_aof(&keyspace, &b"*1\r\n:x\r\n"[..], 0).is_err());
    }
}
