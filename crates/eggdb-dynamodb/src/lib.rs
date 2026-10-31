//! EggDB's DynamoDB-compatible endpoint (D241), written from the public DynamoDB API reference:
//! `CreateTable`, `DescribeTable`, `ListTables` and `DeleteTable`; `PutItem` (with
//! `attribute_not_exists` conditions), `GetItem`, `UpdateItem` (`SET` and `REMOVE`) and
//! `DeleteItem`; `Query` on a partition key with sort-key conditions, in either direction; and
//! `Scan`, both with `Limit` and paging keys. Requests use the JSON protocol
//! (`X-Amz-Target: DynamoDB_20120810.<Action>`) signed with Signature Version 4 for the
//! server's user and password. Items live in EggDB's key-value transactions, ordered by
//! partition key and then sort key. Amazon DynamoDB is a trademark of Amazon; EggDB is
//! compatible with its API.

mod api;
mod keys;

pub use api::Endpoint;

#[cfg(test)]
mod tests;
