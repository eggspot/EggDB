/// A reply of the server a script calls into, in RESP's shapes.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// A simple string (`+OK`).
    Status(Vec<u8>),
    /// An error (`-ERR ...`).
    Error(Vec<u8>),
    /// An integer.
    Integer(i64),
    /// A bulk string, or the nil bulk string.
    Bulk(Option<Vec<u8>>),
    /// An array, or the nil array.
    Array(Option<Vec<Reply>>),
    /// A double (RESP3).
    Double(f64),
    /// A boolean (RESP3).
    Boolean(bool),
}

/// The server a script runs against: `redis.call` and `redis.pcall` send it commands.
pub trait Host {
    /// Runs one command, its name first, and returns its reply.
    fn command(&mut self, arguments: &[Vec<u8>]) -> Reply;

    /// A line the script logged with `redis.log`, at a level from 0 (debug) to 3 (warning).
    fn log(&mut self, level: u8, message: &[u8]) {
        let _ = (level, message);
    }
}

/// A host with no server behind it, for scripts that call no commands.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHost;

impl Host for NoHost {
    fn command(&mut self, _arguments: &[Vec<u8>]) -> Reply {
        Reply::Error(b"ERR this script has no server to call".to_vec())
    }
}
