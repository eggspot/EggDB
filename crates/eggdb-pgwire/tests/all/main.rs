//! Integration tests of `eggdb-pgwire`, built as one test program (docs/testing.md, D335).

mod common;
mod column_privileges;
mod copy;
mod hostile_messages;
mod limits;
mod oracle_procedures;
mod protocol;
mod roles_and_databases;
mod tls;
mod transactions_and_cancel;
