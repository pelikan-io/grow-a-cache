//! macOS mio/kqueue event loop implementation.
//!
//! Readiness-based I/O using mio's cross-platform abstractions over kqueue.
//! Same thread-per-core model as io_uring, but with different event semantics.

mod event_loop;

use crate::config::Config;
use crate::runtime::Protocol;
use crate::storage::Storage;
use std::sync::Arc;

/// Run the server using mio/kqueue backend.
pub fn run(config: Config, storage: Arc<Storage>, protocol: Protocol) -> std::io::Result<()> {
    event_loop::run(config, storage, protocol)
}
