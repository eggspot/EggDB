use super::{Access, Session};
use crate::cli::args::CommandName;
use crate::cli::failure::{Exit, Failure};
use eggdb::Database;
use eggdb_resp::{Client, Context, Reply};

const MOST_REPLY_BYTES: usize = 64 << 20;
const REFUSED_COMMANDS: &[&str] = &[
    "auth", "blmove", "blmpop", "blpop", "brpop", "brpoplpush", "bzmpop", "bzpopmax", "bzpopmin",
    "client", "debug", "discard", "exec", "hello", "monitor", "multi", "psubscribe", "punsubscribe",
    "quit", "reset", "select", "shutdown", "ssubscribe", "subscribe", "sunsubscribe", "swapdb",
    "unsubscribe", "unwatch", "wait", "waitaof", "watch",
];

impl Session<'_> {
    pub(super) fn resp_command(&mut self) -> Result<Exit, Failure> {
        let Some(command) = self.request.resp.clone() else {
            return Err(Failure::usage("resp needs get", Some(CommandName::Resp)));
        };
        let db = self.open(Access::ReadOnly, None)?;
        let outcome = run_read_only(&db, &command);
        self.close(db)?;
        match outcome {
            Ok(reply) => {
                self.out.text(&reply);
                self.out.text("\n");
                Ok(Exit::Done)
            }
            Err(message) => Err(Failure::coded("resp_error", message, Exit::DatabaseError)),
        }
    }
}

/// Runs one Redis-compatible command, written as `redis-cli` takes it (`HGETALL "user:1"`), on
/// the file's RESP keys of database 0 opened read-only, and returns `{"reply": ...}`; an error
/// reply, a command that blocks, subscribes or changes the connection, or text that is not one
/// command is the message.
pub(super) fn run_read_only(db: &Database, command: &str) -> Result<String, String> {
    let arguments = split_command(command)?;
    let Some(name) = arguments.first().map(|name| String::from_utf8_lossy(name).to_ascii_lowercase()) else {
        return Err("the command is empty".to_owned());
    };
    if REFUSED_COMMANDS.contains(&name.as_str()) {
        return Err(format!("{} blocks, subscribes or changes the connection, so it cannot run here", name.to_ascii_uppercase()));
    }
    let keyspace = super::serve_resp::DatabaseKeyspace {
        db,
        prefix: super::serve_resp::RESP_COLLECTIONS,
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    let context = Context {
        keyspace: &keyspace,
        password_sha256: None,
        now_ms,
        reply_room_bytes: MOST_REPLY_BYTES,
        blocked_clients: 0,
        scripts: None,
    };
    let mut client = Client {
        resp3: true,
        authenticated: true,
        ..Client::default()
    };
    match eggdb_resp::execute(&context, &mut client, &arguments) {
        Reply::Error(message) => Err(message),
        reply => {
            let mut out = String::from("{\"reply\":");
            push_reply(&mut out, &reply);
            out.push('}');
            Ok(out)
        }
    }
}

fn split_command(text: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut arguments = Vec::new();
    let mut characters = text.chars().peekable();
    loop {
        while characters.next_if(|character| character.is_whitespace()).is_some() {}
        let Some(&first) = characters.peek() else {
            return Ok(arguments);
        };
        let mut argument = String::new();
        if first == '"' || first == '\'' {
            characters.next();
            loop {
                match characters.next() {
                    None => return Err(format!("a {first} quote is not closed")),
                    Some(close) if close == first => break,
                    Some('\\') if first == '"' => match characters.next() {
                        Some('n') => argument.push('\n'),
                        Some('r') => argument.push('\r'),
                        Some('t') => argument.push('\t'),
                        Some(other) => argument.push(other),
                        None => return Err("a \\ ends the command".to_owned()),
                    },
                    Some(other) => argument.push(other),
                }
            }
            if characters.peek().is_some_and(|next| !next.is_whitespace()) {
                return Err("a closing quote must be followed by a space".to_owned());
            }
        } else {
            while let Some(character) = characters.next_if(|character| !character.is_whitespace()) {
                argument.push(character);
            }
        }
        arguments.push(argument.into_bytes());
    }
}

fn push_reply(out: &mut String, reply: &Reply) {
    match reply {
        Reply::Status(text) | Reply::Error(text) => push_text(out, text.as_bytes()),
        Reply::Integer(number) => out.push_str(&number.to_string()),
        Reply::Bulk(None) | Reply::Array(None) => out.push_str("null"),
        Reply::Bulk(Some(bytes)) => push_text(out, bytes),
        Reply::Double(number) | Reply::DoubleWithText(number, _) if number.is_finite() => {
            out.push_str(&eggdb_resp::format_double(*number));
        }
        Reply::Double(number) | Reply::DoubleWithText(number, _) => {
            push_text(out, eggdb_resp::format_double(*number).as_bytes());
        }
        Reply::Array(Some(items)) | Reply::Set(items) | Reply::Push(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_reply(out, item);
            }
            out.push(']');
        }
        Reply::Map(pairs) | Reply::PairArray(pairs) | Reply::Pairs(pairs) => {
            out.push('[');
            for (index, (key, value)) in pairs.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push('[');
                push_reply(out, key);
                out.push(',');
                push_reply(out, value);
                out.push(']');
            }
            out.push(']');
        }
    }
}

fn push_text(out: &mut String, bytes: &[u8]) {
    match std::str::from_utf8(bytes) {
        Ok(text) => out.push_str(&crate::cli::render::json_string(text)),
        Err(_) => {
            out.push_str("{\"hex\":\"");
            for byte in bytes {
                out.push_str(&format!("{byte:02x}"));
            }
            out.push_str("\"}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::split_command;

    #[test]
    fn commands_split_as_redis_cli_splits_them() {
        let split = split_command(r#"HSET "user:1" name 'Ada L' note "a\"b\n""#).unwrap();
        let expected: Vec<Vec<u8>> = ["HSET", "user:1", "name", "Ada L", "note", "a\"b\n"]
            .iter()
            .map(|word| word.as_bytes().to_vec())
            .collect();
        assert_eq!(split, expected);
        assert!(split_command("GET \"open").is_err());
        assert!(split_command("GET \"a\"b").is_err());
        assert_eq!(split_command("   ").unwrap(), Vec::<Vec<u8>>::new());
    }
}
