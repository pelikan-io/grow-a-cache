//! Custom runtime for high-performance networking.
//!
//! Platform-specific implementations:
//! - Linux: io_uring for completion-based I/O, or mio/epoll for comparison
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
#[allow(unused_imports)]
pub(crate) use connection::ConnState;
pub(crate) use protocol::{ProcessResult, Protocol};

// Used by io_uring implementation
#[allow(unused_imports)]
pub(crate) use connection::{Connection, ConnectionRegistry};
#[allow(unused_imports)]
pub(crate) use token::{OpType, TokenAllocator};

#[cfg(target_os = "linux")]
mod uring;

#[cfg(target_os = "macos")]
mod kqueue;

// mio-based implementation available on both Linux and macOS
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod mio_impl;

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

/// Run the server with native io_uring (Linux) or kqueue (macOS) backend.
pub fn run(config: Config) -> std::io::Result<()> {
    let storage = Storage::new(config.max_memory, config.default_ttl);
    let protocol = map_protocol(config.protocol);

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
    mio_impl::run(config, storage, protocol)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn run_mio(_config: Config) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Unsupported platform: only Linux and macOS are supported",
    ))
}
