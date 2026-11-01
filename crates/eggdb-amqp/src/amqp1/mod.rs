mod connection;
pub(crate) mod value;

pub(crate) use connection::{AMQP_HEADER, SASL_HEADER, serve_stream};

#[cfg(test)]
mod tests;
