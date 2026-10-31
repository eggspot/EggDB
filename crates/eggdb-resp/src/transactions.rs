use crate::commands::{Client, Context, execute};
use crate::keyspace::{KeysRead, KeysWrite, Keyspace};
use crate::protocol::Reply;
use std::cell::RefCell;

const NOT_IN_MULTI: &[&str] = &["multi", "exec", "discard", "watch", "unwatch"];

struct OpenTransaction<'t> {
    db: u32,
    keys: RefCell<&'t mut dyn KeysWrite>,
}

impl Keyspace for OpenTransaction<'_> {
    fn read(&self, db: u32, body: &mut dyn FnMut(&dyn KeysRead) -> Result<(), String>) -> Result<(), String> {
        if db != self.db {
            return Err("ERR a transaction uses one database: SELECT before MULTI".to_owned());
        }
        let keys = self.keys.try_borrow().map_err(|_| "ERR the transaction is busy".to_owned())?;
        let read: &dyn KeysRead = &**keys;
        body(read)
    }

    fn write(&self, db: u32, body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>) -> Result<(), String> {
        if db != self.db {
            return Err("ERR a transaction uses one database: SELECT before MULTI".to_owned());
        }
        let mut keys = self.keys.try_borrow_mut().map_err(|_| "ERR the transaction is busy".to_owned())?;
        body(&mut **keys)
    }
}

fn exec(context: &Context<'_>, client: &mut Client) -> Reply {
    let Some(queued) = client.queued.take() else {
        return Reply::Error("ERR EXEC without MULTI".to_owned());
    };
    let watched = std::mem::take(&mut client.watched);
    if std::mem::take(&mut client.queue_failed) {
        return Reply::Error("EXECABORT Transaction discarded because of previous errors.".to_owned());
    }
    let db = client.db;
    let mut replies = None;
    let outcome = context.keyspace.write(db, &mut |keys| {
        for (watched_db, key, seen) in &watched {
            if *watched_db == db && keys.get(key)? != *seen {
                replies = None;
                return Ok(());
            }
        }
        let open = OpenTransaction {
            db,
            keys: RefCell::new(keys),
        };
        let inner = Context {
            keyspace: &open,
            password_sha256: context.password_sha256,
            now_ms: context.now_ms,
        };
        let mut gathered = Vec::with_capacity(queued.len());
        for arguments in &queued {
            gathered.push(execute(&inner, client, arguments));
        }
        replies = Some(gathered);
        Ok(())
    });
    match outcome {
        Err(error) => Reply::Error(error),
        Ok(()) => Reply::Array(replies),
    }
}

/// Runs a command with `MULTI`, `EXEC`, `DISCARD`, `WATCH` and `UNWATCH` handled: inside
/// `MULTI` commands are queued (`+QUEUED`), and `EXEC` runs them all in one transaction of the
/// engine, or none when a watched key changed.
pub fn execute_with_transactions(context: &Context<'_>, client: &mut Client, arguments: &[Vec<u8>]) -> Reply {
    let name = arguments
        .first()
        .map(|first| String::from_utf8_lossy(first).to_ascii_lowercase())
        .unwrap_or_default();
    let args = arguments.get(1..).unwrap_or_default();
    if !client.authenticated && name != "auth" && name != "hello" {
        return execute(context, client, arguments);
    }
    match name.as_str() {
        "multi" if args.is_empty() => {
            if client.queued.is_some() {
                return Reply::Error("ERR MULTI calls can not be nested".to_owned());
            }
            client.queued = Some(Vec::new());
            client.queue_failed = false;
            Reply::ok()
        }
        "exec" if args.is_empty() => exec(context, client),
        "discard" if args.is_empty() => {
            if client.queued.take().is_none() {
                return Reply::Error("ERR DISCARD without MULTI".to_owned());
            }
            client.watched.clear();
            Reply::ok()
        }
        "watch" if !args.is_empty() => {
            if client.queued.is_some() {
                return Reply::Error("ERR WATCH inside MULTI is not allowed".to_owned());
            }
            let db = client.db;
            let mut seen = Vec::new();
            let outcome = context.keyspace.read(db, &mut |keys| {
                for key in args {
                    seen.push((db, key.clone(), keys.get(key)?));
                }
                Ok(())
            });
            match outcome {
                Ok(()) => {
                    client.watched.extend(seen);
                    Reply::ok()
                }
                Err(error) => Reply::Error(error),
            }
        }
        "unwatch" if args.is_empty() => {
            client.watched.clear();
            Reply::ok()
        }
        _ if client.queued.is_some() && !NOT_IN_MULTI.contains(&name.as_str()) => {
            if name.is_empty() {
                client.queue_failed = true;
                return Reply::Error("ERR empty command".to_owned());
            }
            if let Some(queue) = client.queued.as_mut() {
                queue.push(arguments.to_vec());
            }
            Reply::Status("QUEUED".to_owned())
        }
        _ if NOT_IN_MULTI.contains(&name.as_str()) => {
            Reply::Error(format!("ERR wrong number of arguments for '{name}' command"))
        }
        _ => execute(context, client, arguments),
    }
}
