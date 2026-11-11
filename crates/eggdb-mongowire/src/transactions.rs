use crate::commands::{Shared, document, run, text};
use crate::cursors::Cursors;
use crate::server::{Session, without_generic_fields};
use eggdb_bson::{Bson, Document};
use eggdb_keyspace::{Overlay, StagedChanges};
use std::sync::Mutex;

const ILLEGAL_OPERATION: i32 = 20;
const NO_SUCH_TRANSACTION: i32 = 251;
const TRANSACTION_CURSOR_SEED: u64 = 0x7472_616e_7361_6374;
const TRANSIENT_TRANSACTION_ERROR: &str = "TransientTransactionError";

/// A multi-document transaction a connection holds open: its session id and number, and the
/// writes staged for it, which reach the store only at `commitTransaction`.
#[derive(Debug)]
pub(crate) struct Transaction {
    session: Option<Bson>,
    number: i64,
    staged: Mutex<StagedChanges>,
}

fn number_of(value: Option<&Bson>) -> Option<i64> {
    match value? {
        Bson::Int64(number) => Some(*number),
        Bson::Int32(number) => Some(i64::from(*number)),
        Bson::Double(number) if number.fract() == 0.0 => Some(*number as i64),
        _ => None,
    }
}

fn refusal(code: i32, code_name: &str, message: &str, transient: bool) -> Document {
    let mut reply = document(vec![
        ("ok", Bson::Double(0.0)),
        ("errmsg", text(message)),
        ("code", Bson::Int32(code)),
        ("codeName", text(code_name)),
    ]);
    if transient {
        reply.insert(
            "errorLabels",
            Bson::Array(vec![text(TRANSIENT_TRANSACTION_ERROR)]),
        );
    }
    reply
}

fn no_such_transaction(number: i64) -> Document {
    refusal(
        NO_SUCH_TRANSACTION,
        "NoSuchTransaction",
        &format!("Transaction with {{ txnNumber: {number} }} has been aborted."),
        true,
    )
}

fn acknowledged() -> Document {
    document(vec![("ok", Bson::Double(1.0))])
}

/// Runs a command with MongoDB's transaction fields applied: `startTransaction` opens one
/// (on a replica set only, as MongoDB requires), a command with `autocommit: false` and the
/// open transaction's `txnNumber` runs against its staged writes, and `commitTransaction` and
/// `abortTransaction` end it. Every other command runs as before.
pub(crate) fn run_command(
    shared: &Shared<'_>,
    session: &mut Session,
    database: &str,
    body: &Document,
) -> Document {
    if session.authenticated_user.is_none() {
        return run(shared, session, database, &without_generic_fields(body));
    }
    let command = body.first().map(|(name, _)| name.clone()).unwrap_or_default();
    let number = number_of(body.get("txnNumber"));
    let in_transaction = body.get("autocommit") == Some(&Bson::Boolean(false));
    match (command.as_str(), number) {
        ("commitTransaction", Some(number)) => commit(shared, session, number),
        ("abortTransaction", Some(number)) => abort(session, number),
        (_, Some(number)) if in_transaction => {
            if body.get("startTransaction") == Some(&Bson::Boolean(true)) {
                if shared.config.replica_set.is_none() {
                    return refusal(
                        ILLEGAL_OPERATION,
                        "IllegalOperation",
                        "Transaction numbers are only allowed on a replica set member or mongos",
                        false,
                    );
                }
                session.transaction = Some(Transaction {
                    session: body.get("lsid").cloned(),
                    number,
                    staged: Mutex::new(StagedChanges::new()),
                });
            }
            let Some(transaction) = session.transaction.take() else {
                return no_such_transaction(number);
            };
            if transaction.number != number
                || transaction.session.as_ref() != body.get("lsid")
            {
                session.transaction = Some(transaction);
                return no_such_transaction(number);
            }
            let reply = {
                let overlay = Overlay::new(shared.store.keyspace(), &transaction.staged);
                let store = shared.store.over(&overlay);
                let inside = Shared {
                    store: &store,
                    config: shared.config,
                    cursors: Mutex::new(Cursors::new(
                        TRANSACTION_CURSOR_SEED ^ number as u64,
                        &shared.config.memory_budget,
                    )),
                    started: shared.started,
                };
                run(&inside, session, database, &without_generic_fields(body))
            };
            session.transaction = Some(transaction);
            reply
        }
        _ => run(shared, session, database, &without_generic_fields(body)),
    }
}

fn commit(shared: &Shared<'_>, session: &mut Session, number: i64) -> Document {
    match session.transaction.take() {
        Some(transaction) if transaction.number == number => {
            let overlay = Overlay::new(shared.store.keyspace(), &transaction.staged);
            match overlay.commit() {
                Ok(()) => {
                    session.committed_transaction = Some(number);
                    acknowledged()
                }
                Err(message) => refusal(
                    NO_SUCH_TRANSACTION,
                    "NoSuchTransaction",
                    &format!("the transaction could not be committed: {message}"),
                    true,
                ),
            }
        }
        held => {
            session.transaction = held;
            if session.committed_transaction == Some(number) {
                acknowledged()
            } else {
                no_such_transaction(number)
            }
        }
    }
}

fn abort(session: &mut Session, number: i64) -> Document {
    match session.transaction.take() {
        Some(transaction) if transaction.number == number => acknowledged(),
        held => {
            session.transaction = held;
            no_such_transaction(number)
        }
    }
}
