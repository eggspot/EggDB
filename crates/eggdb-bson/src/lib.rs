//! BSON (D197), written from the public specification at bsonspec.org: [`Bson`] values and
//! ordered [`Document`]s, reading with every length, string and nesting depth checked (hostile
//! input is an error, never a panic), writing, the comparison order MongoDB documents for
//! sorting values of different types ([`compare`]), and relaxed Extended JSON output for people
//! and logs.

mod decode;
mod document;
mod encode;
mod extended_json;
mod object_id;
mod order;

pub use decode::{BsonError, MAX_DEPTH, read_document};
pub use document::{Bson, Document};
pub use extended_json::relaxed_json;
pub use object_id::new_object_id;
pub use order::{compare, type_rank};

#[cfg(test)]
mod tests;
