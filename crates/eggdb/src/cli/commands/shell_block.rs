use super::{missing_collection, operation_failure};
use crate::cli::args::{CommandName, Format, Request};
use crate::cli::byte_argument::ByteArgument;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject, LineBreaks};
use eggdb::sql::SharedWriteStore;
use eggdb::{Error, ErrorKind, WriteTransaction};
use eggdb_sql::TransactionStatus;

const READS_THE_LAST_COMMIT: [CommandName; 5] = [
    CommandName::Scan,
    CommandName::Ttl,
    CommandName::Check,
    CommandName::Stats,
    CommandName::Backup,
];
const IN_BLOCK: [CommandName; 5] = [
    CommandName::Put,
    CommandName::Get,
    CommandName::Del,
    CommandName::Incr,
    CommandName::Decr,
];

pub(super) enum InBlock {
    ReadsTheLastCommit,
    Ran {
        printed: Vec<u8>,
        message: Option<String>,
        exit: Exit,
    },
}

impl InBlock {
    fn printed(text: String) -> Self {
        Self::Ran {
            printed: text.into_bytes(),
            message: None,
            exit: Exit::Done,
        }
    }

    fn not_found(message: Option<String>) -> Self {
        Self::Ran {
            printed: Vec::new(),
            message,
            exit: Exit::NotFound,
        }
    }
}

pub(super) fn key_value_in_block(
    store: &SharedWriteStore<'_>,
    status: TransactionStatus,
    request: &Request,
    file: &str,
) -> Result<InBlock, Failure> {
    let command = request.command;
    if READS_THE_LAST_COMMIT.contains(&command) {
        return Ok(InBlock::ReadsTheLastCommit);
    }
    if status == TransactionStatus::Failed {
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
    let key = bytes_of(request.key.clone())?;
    let value = bytes_of(request.value.clone())?;
    let ran = store.with_open_transaction(|transaction| {
        run(transaction, request, &key, &value, file).map_err(|error| {
            let context = format!(
                "cannot {} {} in {file}",
                command.name(),
                render::text(&key)
            );
            match (&request.collection, error.kind()) {
                (Some(name), ErrorKind::CollectionNotFound) => {
                    missing_collection(file, name, &context, command)
                }
                _ => operation_failure(&error, &context),
            }
        })
    });
    match ran {
        Some(outcome) => outcome,
        None if command == CommandName::Get => Ok(InBlock::ReadsTheLastCommit),
        None => Err(Failure::database(
            format!(
                "cannot run {} in {file}: the transaction block is read-only",
                command.name()
            ),
            Exit::DatabaseError,
        )),
    }
}

fn bytes_of(argument: Option<ByteArgument>) -> Result<Vec<u8>, Failure> {
    let Some(argument) = argument else {
        return Ok(Vec::new());
    };
    let mut stdin_taken = true;
    argument.resolve(&mut std::io::empty(), &mut stdin_taken)
}

fn run(
    transaction: &mut WriteTransaction<'_>,
    request: &Request,
    key: &[u8],
    value: &[u8],
    file: &str,
) -> Result<InBlock, Error> {
    let format = request.format.unwrap_or(Format::Text);
    let collection = request.collection.as_deref();
    match request.command {
        CommandName::Put => {
            put(transaction, request, key, value)?;
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
                None => InBlock::not_found(Some(format!(
                    "key not found: {}",
                    render::text(key)
                ))),
            })
        }
        CommandName::Del => {
            let deleted = match collection {
                Some(name) => transaction.collection(name)?.delete(key)?,
                None => transaction.delete(key)?,
            };
            if format == Format::Json {
                let line = JsonObject::new()
                    .bytes("key", key)
                    .boolean("deleted", deleted)
                    .line();
                return Ok(InBlock::Ran {
                    printed: line.into_bytes(),
                    message: None,
                    exit: if deleted { Exit::Done } else { Exit::NotFound },
                });
            }
            Ok(if deleted {
                InBlock::printed(String::new())
            } else {
                InBlock::not_found(Some(format!(
                    "nothing to delete: {} is not in {file}",
                    render::text(key)
                )))
            })
        }
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
            exit: Exit::Done,
        },
    }
}
