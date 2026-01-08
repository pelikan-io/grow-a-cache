//! Custom runtime for high-performance networking.
//!
//! Platform-specific implementations:
//! - Linux: io_uring for completion-based I/O, or mio/epoll for comparison
//! - macOS: mio/kqueue for readiness-based I/O
//!
//! Shared abstractions:
//! - `BufferPool`: Per-worker buffer management
//! - `Connection`: Connection state machine with control/data plane separation
//! - `ConnPhase`: Control plane state (Accepting, Handshaking, Established, Closing)
//! - `DataState`: Data plane state (Reading, Writing)

mod buffer;
mod connection;
pub mod request;

// Re-export shared types for use by platform-specific implementations
pub(crate) use buffer::BufferPool;
pub(crate) use connection::{ConnPhase, Connection, ConnectionRegistry, DataState};
pub(crate) use request::{ProcessResult, Protocol};

// io_uring backend (Linux only)
#[cfg(target_os = "linux")]
mod uring;

// Re-export io_uring-specific types
#[cfg(target_os = "linux")]
pub(crate) use uring::{OpType, TokenAllocator};

// mio-based implementation for both Linux and macOS
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod mio;

use crate::config::{Config, ProtocolType};
use crate::storage::Storage;

/// Map config protocol to runtime protocol.
fn map_protocol(config_protocol: ProtocolType) -> Protocol {
    match config_protocol {
        ProtocolType::Memcached => Protocol::Memcached,
        ProtocolType::Resp => Protocol::Resp,
        ProtocolType::Ping => Protocol::Ping,
        ProtocolType::Echo => Protocol::Echo,
    }
}

/// Run the server with native io_uring (Linux) or mio/kqueue (macOS) backend.
pub fn run(config: Config) -> std::io::Result<()> {
    let storage = Storage::new(config.max_memory, config.default_ttl);
    let protocol = map_protocol(config.protocol);

    #[cfg(target_os = "linux")]
    {
        uring::run(config, storage, protocol)
    }

    #[cfg(target_os = "macos")]
    {
        // macOS uses mio/kqueue for native runtime
        mio::run(config, storage, protocol)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (storage, protocol);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Unsupported platform: only Linux and macOS are supported",
        ))
    }
}

/// Run the server with mio backend (epoll on Linux, kqueue on macOS).
/// This allows comparison with io_uring on Linux.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn run_mio(config: Config) -> std::io::Result<()> {
    let storage = Storage::new(config.max_memory, config.default_ttl);
    let protocol = map_protocol(config.protocol);
    mio::run(config, storage, protocol)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn run_mio(_config: Config) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Unsupported platform: only Linux and macOS are supported",
    ))
}
