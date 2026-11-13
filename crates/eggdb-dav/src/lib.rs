//! EggDB's WebDAV, CalDAV and CardDAV server (D56, D895): files, calendars and address books
//! kept on EggDB's key-value transactions through [`eggdb_keyspace::Keyspace`] (database 0 of
//! the keyspace the caller gives, which the `eggdb` binary keeps in the collection `dav0`),
//! served over HTTP by [`Endpoint`]. Everything is written from the IETF RFCs (RFC 4918,
//! RFC 4791, RFC 5545, RFC 5689, RFC 6352, RFC 6578); no other DAV server's code is used or read.

mod acl;
pub mod ical;
mod scheduling;
mod server;
pub mod store;
pub mod xml;

pub use server::{Endpoint, MAX_REQUEST_BYTES};
