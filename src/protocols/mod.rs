//! Protocol implementations.
//!
//! Each protocol is a self-contained vertical slice with its own parser and handler.
//!
//! ## Production Protocols
//! - `memcached`: Memcached text protocol for cache operations
//! - `resp`: Redis RESP protocol for cache operations
//!
//! ## Test Protocols
//! - `ping`: Minimal ping/pong for health checks and latency measurement
//! - `echo`: Echo service for throughput testing with varied payload sizes

pub mod echo;
pub mod memcached;
pub mod ping;
pub mod resp;
