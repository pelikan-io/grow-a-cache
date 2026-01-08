//! mio-based event loop implementation.
//!
//! Readiness-based I/O using mio (epoll on Linux, kqueue on macOS).
//! This module can be used on both Linux and macOS for comparison.

mod event_loop;

use crate::config::Config;
use crate::runtime::Protocol;
use crate::storage::Storage;
use std::sync::Arc;

/// Run the server using mio backend.
pub fn run(config: Config, storage: Arc<Storage>, protocol: Protocol) -> std::io::Result<()> {
    event_loop::run(config, storage, protocol)
}
