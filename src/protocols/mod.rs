//! Protocol implementations.
//!
//! Each protocol has a parser module used by the runtime event loops.
//!
//! ## Production Protocols
//! - `memcached`: Memcached text protocol for cache operations
//! - `resp`: Redis RESP protocol for cache operations
//!
//! ## Test Protocols
//! - `ping`: Minimal ping/pong for latency testing
//! - `echo`: Length-prefixed echo for throughput testing

pub mod echo;
pub mod memcached;
pub mod ping;
pub mod resp;
