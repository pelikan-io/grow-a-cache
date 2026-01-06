//! Ping protocol implementation.
//!
//! A minimal protocol for health checks and latency measurement:
//! - Client sends: `PING\r\n` or `PING <message>\r\n`
//! - Server responds: `PONG\r\n` or `PONG <message>\r\n`
//!
//! ## Use Cases
//!
//! 1. **Health checks**: Load balancers and monitoring systems can verify
//!    the server is responsive without touching the cache.
//!
//! 2. **Latency measurement**: Measures pure network + runtime overhead
//!    without storage operations, useful for:
//!    - Benchmarking the runtime (Tokio vs native io_uring/kqueue)
//!    - Establishing baseline latency for comparison
//!    - Network path validation
//!
//! 3. **Connection testing**: Verify connectivity before issuing real commands.
//!
//! ## Protocol Format
//!
//! ```text
//! Request:  PING\r\n
//! Response: PONG\r\n
//!
//! Request:  PING hello\r\n
//! Response: PONG hello\r\n
//! ```

pub mod handler;

pub use handler::handle_connection;
