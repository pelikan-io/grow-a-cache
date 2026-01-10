//! Protocol implementations.
//!
//! Each protocol has a parser module used by the runtime event loops.
//!
//! ## Production Protocols
//! - `memcached`: Memcached text protocol for cache operations
//! - `resp`: Redis RESP protocol for cache operations
//!
//! ## Test Protocols
//! - `ping`: Minimal ping/pong (inline in runtime)
//! - `echo`: Echo service (inline in runtime)

pub mod memcached;
pub mod resp;
