//! EggDB's SQS-compatible endpoint (D216): the queue actions of the AWS JSON 1.0 protocol (the
//! `X-Amz-Target: AmazonSQS.<Action>` form current AWS SDKs send) — create, get URL, list,
//! delete and purge queues, send, receive with a visibility timeout, delete and change the
//! visibility of messages, and queue attributes — on EggDB's durable queues, with each request's
//! AWS Signature Version 4 checked against the server's user and password. Amazon SQS is a
//! trademark of Amazon; EggDB is compatible with its API.

mod api;
mod md5;
mod sigv4;

pub use api::Endpoint;

#[cfg(test)]
mod tests;
