//! Custom runtime for high-performance networking.
//!
//! Platform-specific implementations:
//! - Linux: io_uring for completion-based I/O
//! - macOS: mio/kqueue for readiness-based I/O
//!
//! Both share common abstractions:
//! - `BufferPool`: Per-worker buffer management
//! - `Connection`: Connection state machine
//! - `Token`: Operation tracking for completion correlation

mod buffer;
mod connection;
pub mod protocol;
mod token;

// Re-export for use by platform-specific implementations
pub(crate) use buffer::BufferPool;
pub(crate) use connection::ConnState;
pub(crate) use protocol::{ProcessResult, Protocol};

// These will be used when io_uring is wired in
#[allow(unused_imports)]
pub(crate) use connection::{Connection, ConnectionRegistry};
#[allow(unused_imports)]
pub(crate) use token::{OpType, TokenAllocator};

#[cfg(target_os = "linux")]
mod uring;

#[cfg(target_os = "macos")]
mod kqueue;

use crate::config::{Config, ProtocolType};
use crate::storage::Storage;

/// Run the server with platform-appropriate backend.
pub fn run(config: Config) -> std::io::Result<()> {
    // Create shared storage (Storage::new returns Arc<Storage>)
    let storage = Storage::new(config.max_memory, config.default_ttl);

    // Map config protocol to runtime protocol
    let protocol = match config.protocol {
        ProtocolType::Memcached => Protocol::Memcached,
        ProtocolType::Resp => Protocol::Resp,
    };

    #[cfg(target_os = "linux")]
    {
        uring::run(config, storage, protocol)
    }

    #[cfg(target_os = "macos")]
    {
        kqueue::run(config, storage, protocol)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (storage, protocol); // suppress unused warnings
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Unsupported platform: only Linux and macOS are supported",
        ))
    }
}
