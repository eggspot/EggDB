//! EggDB's S3-compatible endpoint (D219), written from the public Amazon S3 API reference:
//! buckets (create, head, list, delete), objects (put, get with ranges, head, copy, delete,
//! delete many), `ListObjects` and `ListObjectsV2` with prefixes, delimiters and continuation,
//! multipart uploads, and presigned URLs, every request signed with AWS Signature Version 4
//! for the server's user and password. Objects are kept in EggDB's key-value transactions in
//! 256 KiB chunks. [`client`] is the other side: a small client for any S3-compatible service,
//! which EggDB's archives use (D226). Amazon S3 is a trademark of Amazon; EggDB is compatible
//! with its API.

mod api;
pub mod client;
mod store;
mod xml;

pub use api::{Endpoint, MAX_REQUEST_BYTES};

#[cfg(test)]
mod tests;
