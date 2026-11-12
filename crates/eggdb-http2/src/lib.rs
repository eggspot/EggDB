//! EggDB's own HTTP/2 (RFC 9113), written from the RFCs with no third-party code: frames, HPACK
//! header compression (RFC 7541, Huffman coding included), a server connection that answers
//! one request at a time per connection with flow control, and gRPC's length-prefixed message
//! framing. It is the transport of EggDB's gRPC APIs and Arrow Flight SQL. Every length is
//! checked: a hostile peer gets a connection error, never a panic.

mod frame;
mod grpc;
mod hpack;
mod huffman;
mod server;

pub use frame::{Frame, FrameKind, Http2Error};
pub use grpc::{GrpcStatus, grpc_message, grpc_messages};
pub use hpack::{HeaderDecoder, encode_headers};
pub use huffman::{huffman_decode, huffman_encode};
pub use server::{Handler, Http2Request, Http2Response, serve_connection};
