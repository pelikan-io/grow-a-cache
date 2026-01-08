//! Echo protocol implementation.
//!
//! A simple echo service for throughput and I/O testing:
//! - Client sends: `<length>\r\n<data>`
//! - Server echoes: `<length>\r\n<data>`
//!
//! ## Use Cases
//!
//! 1. **Throughput testing**: Measure raw I/O throughput without storage
//!    overhead. Useful for benchmarking the runtime's ability to move bytes.
//!
//! 2. **Variable payload sizes**: Test server behavior with different data
//!    sizes, from tiny (few bytes) to large (megabytes), to identify:
//!    - Buffer handling issues
//!    - Memory allocation patterns
//!    - Partial read/write handling
//!
//! 3. **Stress testing**: Generate high I/O load to test stability and
//!    resource limits (connections, memory, file descriptors).
//!
//! 4. **Correctness validation**: Verify data integrity by comparing
//!    echoed data against sent data.
//!
//! ## Protocol Format
//!
//! Length-prefixed binary protocol for predictable framing:
//!
//! ```text
//! Request:  <length>\r\n<data of exactly length bytes>
//! Response: <length>\r\n<data of exactly length bytes>
//!
//! Example:
//! Request:  5\r\nhello
//! Response: 5\r\nhello
//! ```
//!
//! Special commands (line-based):
//! - `QUIT\r\n` - Close connection gracefully

pub mod handler;

pub use handler::handle_connection;
