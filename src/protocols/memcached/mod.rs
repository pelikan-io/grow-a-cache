//! Memcached text protocol implementation.

pub mod handler;
pub mod parser;

pub use handler::handle_connection;
