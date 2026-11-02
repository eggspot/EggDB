use super::expiry::{not_swapped, with_time_to_live};
use super::{missing_collection, operation_failure, scan_entry};
use crate::cli::args::{CommandName, Format, Request, ScanOptions};
use crate::cli::byte_argument::ByteArgument;
use crate::cli::failure::{Exit, Failure};
use crate::cli::output::Output;
use crate::cli::render::{self, JsonObject, LineBreaks};
use eggdb::sql::SharedWriteStore;
use eggdb::{Error, ErrorKind, Scan, WriteTransaction};
use eggdb_sql::TransactionStatus;
use std::ops::Bound;
use std::time::Duration;

const READS_THE_LAST_COMMIT: [CommandName; 3] =
    [CommandName::Check, CommandName::Stats, CommandName::Backup];

const IN_BLOCK: [CommandName; 10] = [
    CommandName::Put,
    CommandName::Get,
    CommandName::Del,
    CommandName::Incr,
    CommandName::Decr,
    CommandName::Expire,
    CommandName::Persist,
    CommandName::Cas,
    CommandName::Scan,
    CommandName::Ttl,
];

const WRITES: [CommandName; 7] = [
    CommandName::Put,
    CommandName::Del,
    CommandName::Incr,
    CommandName::Decr,
    CommandName::Expire,
    CommandName::Persist,
    CommandName::Cas,
];

const DEFAULT_EXPIRE_TIME: Duration = Duration::from_secs(1);

pub(super) struct BlockState {
    pub(super) status: TransactionStatus,
    pub(super) has_savepoints: bool,
}

pub(super) enum InBlock {
    ReadsTheLastCommit,
    Ran {
        printed: Vec<u8>,
        message: Option<String>,
        note: Option<String>,
        exit: Exit,
    },
}

impl InBlock {
    fn printed(text: String) -> Self {
        Self::Ran {
            printed: text.into_bytes(),
            message: None,
            note: None,
            exit: Exit::Done,
        }
    }

    fn not_found(message: Option<String>) -> Self {
        Self::Ran {
            printed: Vec::new(),
            message,
            note: None,
            exit: Exit::NotFound,
        }
    }

    fn done_or_not_found(format: Format, found: bool, printed: String, message: String) -> Self {
        Self::Ran {
            printed: printed.into_bytes(),
            message: (!found && format != Format::Json).then_some(message),
            note: None,
            exit: if found { Exit::Done } else { Exit::NotFound },
        }
    }
}

struct Arguments {
    key: Vec<u8>,
    value: Option<Vec<u8>>,
    expected: Option<Vec<u8>>,
    prefix: Option<Vec<u8>>,
    from: Option<Vec<u8>>,
    to: Option<Vec<u8>>,
}

impl Arguments {
    fn resolved(request: &Request) -> Result<Self, Failure> {
        Ok(Self {
            key: bytes_of(request.key.clone())?.unwrap_or_default(),
            value: bytes_of(request.value.clone())?,
            expected: bytes_of(request.swap.expected.clone())?,
            prefix: bytes_of(request.scan.prefix.clone())?,
            from: bytes_of(request.scan.from.clone())?,
            to: bytes_of(request.scan.to.clone())?,
        })
    }

    fn range(&self) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
        (
            self.from.clone().map_or(Bound::Unbounded, Bound::Included),
            self.to.clone().map_or(Bound::Unbounded, Bound::Excluded),
        )
    }
}

pub(super) fn key_value_in_block(
    store: &SharedWriteStore<'_>,
    block: BlockState,
    request: &Request,
    out: &mut Output<'_>,
    file: &str,
) -> Result<InBlock, Failure> {
    let command = request.command;
    if READS_THE_LAST_COMMIT.contains(&command) {
        return Ok(InBlock::ReadsTheLastCommit);
    }
    if block.status == TransactionStatus::Failed {
        return Err(Failure::database(
            format!(
                "cannot run {} in {file}: the transaction block failed; ROLLBACK ends it",
                command.name()
            ),
            Exit::DatabaseError,
        ));
    }
    if !IN_BLOCK.contains(&command) {
        return Err(Failure::usage(
            format!(
                "{} cannot run inside a transaction block yet; COMMIT or ROLLBACK first",
                command.name()
            ),
            Some(command),
        ));
    }
    if block.has_savepoints && WRITES.contains(&command) {
        return Err(Failure::usage(
            format!(
                "{} cannot run while a savepoint is set, since ROLLBACK TO SAVEPOINT would not \
                 undo it; RELEASE SAVEPOINT, COMMIT or ROLLBACK first",
                command.name()
            ),
            Some(command),
        ));
    }
    let arguments = Arguments::resolved(request)?;
    let ran = store.with_open_transaction(|transaction| {
        if command == CommandName::Scan {
            return scan(transaction, request, &arguments, out, file);
        }
        run(transaction, request, &arguments, file)
            .map_err(|error| key_value_failure(&error, request, &arguments.key, file))
    });
    match ran {
        Some(outcome) => outcome,
        None if !WRITES.contains(&command) => Ok(InBlock::ReadsTheLastCommit),
        None => Err(Failure::database(
            format!(
                "cannot run {} in {file}: the transaction block is read-only",
                command.name()
            ),
            Exit::DatabaseError,
        )),
    }
}

fn key_value_failure(error: &Error, request: &Request, key: &[u8], file: &str) -> Failure {
    let command = request.command;
    let context = match command {
        CommandName::Scan => format!("cannot scan {file}"),
        _ => format!("cannot {} {} in {file}", command.name(), render::text(key)),
    };
    match (&request.collection, error.kind()) {
        (Some(name), ErrorKind::CollectionNotFound) => {
            missing_collection(file, name, &context, command)
        }
        _ => operation_failure(error, &context),
    }
}

fn bytes_of(argument: Option<ByteArgument>) -> Result<Option<Vec<u8>>, Failure> {
    argument
        .map(|argument| {
            let mut stdin_taken = true;
            argument.resolve(&mut std::io::empty(), &mut stdin_taken)
        })
        .transpose()
}

fn run(
    transaction: &mut WriteTransaction<'_>,
    request: &Request,
    arguments: &Arguments,
    file: &str,
) -> Result<InBlock, Error> {
    let format = request.format.unwrap_or(Format::Text);
    let collection = request.collection.as_deref();
    let key = arguments.key.as_slice();
    match request.command {
        CommandName::Put => {
            put(
                transaction,
                request,
                key,
                arguments.value.as_deref().unwrap_or_default(),
            )?;
            Ok(InBlock::printed(match format {
                Format::Json => JsonObject::new()
                    .bytes("key", key)
                    .boolean("stored", true)
                    .line(),
                _ => String::new(),
            }))
        }
        CommandName::Get => {
            let found = match collection {
                Some(name) => transaction.collection(name)?.get(key)?,
                None => transaction.get(key)?,
            };
            Ok(match found {
                Some(found) => shown(format, key, &found),
                None if format == Format::Json => InBlock::not_found(None),
                None => InBlock::not_found(Some(format!("key not found: {}", render::text(key)))),
            })
        }
        CommandName::Del => {
            let deleted = match collection {
                Some(name) => transaction.collection(name)?.delete(key)?,
                None => transaction.delete(key)?,
            };
            let printed = match format {
                Format::Json => JsonObject::new()
                    .bytes("key", key)
                    .boolean("deleted", deleted)
                    .line(),
                _ => String::new(),
            };
            Ok(InBlock::done_or_not_found(
                format,
                deleted,
                printed,
                format!("nothing to delete: {} is not in {file}", render::text(key)),
            ))
        }
        CommandName::Ttl => time_to_live(transaction, collection, format, key),
        CommandName::Expire => {
            let ttl = request.ttl.unwrap_or(DEFAULT_EXPIRE_TIME);
            let expires = match collection {
                Some(name) => transaction.collection(name)?.set_expiry(key, ttl)?,
                None => transaction.set_expiry(key, ttl)?,
            };
            let printed = match format {
                Format::Json => {
                    let object = JsonObject::new()
                        .bytes("key", key)
                        .boolean("expires", expires);
                    let object = if expires {
                        with_time_to_live(object, Some(ttl))
                    } else {
                        object
                    };
                    object.line()
                }
                _ => String::new(),
            };
            Ok(InBlock::done_or_not_found(
                format,
                expires,
                printed,
                format!("nothing to expire: {} is not in {file}", render::text(key)),
            ))
        }
        CommandName::Persist => {
            let present = match collection {
                Some(name) => {
                    let mut named = transaction.collection(name)?;
                    let present = named.get_with(key, |_| ())?.is_some();
                    named.clear_expiry(key)?;
                    present
                }
                None => {
                    let present = transaction.get_with(key, |_| ())?.is_some();
                    transaction.clear_expiry(key)?;
                    present
                }
            };
            let printed = match format {
                Format::Json => JsonObject::new()
                    .bytes("key", key)
                    .boolean("persisted", present)
                    .line(),
                _ => String::new(),
            };
            Ok(InBlock::done_or_not_found(
                format,
                present,
                printed,
                format!("nothing to persist: {} is not in {file}", render::text(key)),
            ))
        }
        CommandName::Cas => compare_and_set(transaction, collection, format, arguments, file),
        _ => {
            let magnitude = i64::try_from(request.step.unwrap_or(1)).unwrap_or(i64::MAX);
            let by = if request.command == CommandName::Decr {
                -magnitude
            } else {
                magnitude
            };
            let count = match collection {
                Some(name) => transaction.collection(name)?.increment(key, by)?,
                None => transaction.increment(key, by)?,
            };
            let count = count.to_string();
            Ok(InBlock::printed(match format {
                Format::Json => JsonObject::new()
                    .bytes("key", key)
                    .string("value", &count)
                    .line(),
                _ => format!("{count}\n"),
            }))
        }
    }
}

fn time_to_live(
    transaction: &mut WriteTransaction<'_>,
    collection: Option<&str>,
    format: Format,
    key: &[u8],
) -> Result<InBlock, Error> {
    let (present, left) = match collection {
        Some(name) => {
            let named = transaction.collection(name)?;
            let present = named.get_with(key, |_| ())?.is_some();
            (present, named.time_to_live(key)?)
        }
        None => {
            let present = transaction.get_with(key, |_| ())?.is_some();
            (present, transaction.time_to_live(key)?)
        }
    };
    if !present {
        let message = (format != Format::Json)
            .then(|| format!("key not found: {}", render::text(key)));
        return Ok(InBlock::not_found(message));
    }
    Ok(InBlock::printed(match format {
        Format::Json => with_time_to_live(JsonObject::new().bytes("key", key), left).line(),
        _ => format!(
            "{}\n",
            left.map_or_else(|| "never".to_owned(), eggdb_ui::format_time_left)
        ),
    }))
}

fn compare_and_set(
    transaction: &mut WriteTransaction<'_>,
    collection: Option<&str>,
    format: Format,
    arguments: &Arguments,
    file: &str,
) -> Result<InBlock, Error> {
    let key = arguments.key.as_slice();
    let expected = arguments.expected.as_deref();
    let new = arguments.value.as_deref();
    let (found, swapped) = match collection {
        Some(name) => {
            let mut named = transaction.collection(name)?;
            let found = named.get(key)?;
            (found, named.compare_and_set(key, expected, new)?)
        }
        None => {
            let found = transaction.get(key)?;
            (found, transaction.compare_and_set(key, expected, new)?)
        }
    };
    let printed = match format {
        Format::Json => {
            let object = JsonObject::new()
                .bytes("key", key)
                .boolean("swapped", swapped);
            match (&found, swapped) {
                (Some(found), false) => object.bytes("value", found),
                _ => object,
            }
            .line()
        }
        _ => String::new(),
    };
    Ok(InBlock::done_or_not_found(
        format,
        swapped,
        printed,
        not_swapped(key, found.as_deref(), expected, file),
    ))
}

fn scan(
    transaction: &mut WriteTransaction<'_>,
    request: &Request,
    arguments: &Arguments,
    out: &mut Output<'_>,
    file: &str,
) -> Result<InBlock, Failure> {
    let failure = |error: Error| key_value_failure(&error, request, &[], file);
    let format = request.format.unwrap_or(Format::Text);
    let entries = match &request.collection {
        Some(name) => {
            let named = transaction.collection(name).map_err(&failure)?;
            let entries = match &arguments.prefix {
                Some(prefix) => named.scan_prefix(prefix),
                None => named.scan(arguments.range()),
            };
            listed(entries, &request.scan, format, out, &failure)?
        }
        None => {
            let entries = match &arguments.prefix {
                Some(prefix) => transaction.scan_prefix(prefix),
                None => transaction.scan(arguments.range()),
            };
            listed(entries, &request.scan, format, out, &failure)?
        }
    };
    Ok(InBlock::Ran {
        printed: Vec::new(),
        message: None,
        note: (format == Format::Text).then(|| render::entries(entries)),
        exit: Exit::Done,
    })
}

fn listed(
    mut entries: Scan<'_>,
    options: &ScanOptions,
    format: Format,
    out: &mut Output<'_>,
    failure: impl Fn(Error) -> Failure,
) -> Result<u64, Failure> {
    let limit = options.limit.unwrap_or(u64::MAX);
    let mut count = 0u64;
    while count < limit {
        let next = if options.reverse {
            entries.next_back_entry()
        } else {
            entries.next_entry()
        };
        let Some((key, value)) = next.map_err(&failure)? else {
            break;
        };
        scan_entry(out, format, options.keys_only, key, value);
        count += 1;
        out.line_done()?;
    }
    Ok(count)
}

fn put(
    transaction: &mut WriteTransaction<'_>,
    request: &Request,
    key: &[u8],
    value: &[u8],
) -> Result<(), Error> {
    match (&request.collection, request.ttl) {
        (Some(name), ttl) => {
            if request.create {
                transaction.create_collection(name)?;
            }
            let mut collection = transaction.collection(name)?;
            match ttl {
                Some(ttl) => collection.put_with_expiry(key, value, ttl),
                None => collection.put(key, value),
            }
        }
        (None, Some(ttl)) => transaction.put_with_expiry(key, value, ttl),
        (None, None) => transaction.put(key, value),
    }
}

fn shown(format: Format, key: &[u8], value: &[u8]) -> InBlock {
    match format {
        Format::Text => InBlock::printed(format!(
            "{}\n",
            render::text_with(value, LineBreaks::Allowed)
        )),
        Format::Json => InBlock::printed(
            JsonObject::new()
                .bytes("key", key)
                .bytes("value", value)
                .line(),
        ),
        Format::Hex => InBlock::printed(format!("{}\n", render::hex(value))),
        Format::Raw => InBlock::Ran {
            printed: value.to_vec(),
            message: None,
            note: None,
            exit: Exit::Done,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{IN_BLOCK, READS_THE_LAST_COMMIT, WRITES};

    #[test]
    fn every_write_runs_in_the_block_and_no_command_is_on_two_lists() {
        for command in WRITES {
            assert!(IN_BLOCK.contains(&command), "{command:?}");
        }
        for command in READS_THE_LAST_COMMIT {
            assert!(!IN_BLOCK.contains(&command), "{command:?}");
        }
    }
}
