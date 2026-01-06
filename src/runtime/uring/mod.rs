//! Linux io_uring event loop implementation.
//!
//! Completion-based I/O with batched submissions for high throughput.
//! Uses registered buffers for reduced kernel validation overhead.

mod event_loop;

use crate::config::Config;
use crate::runtime::Protocol;
use crate::storage::Storage;
use std::sync::Arc;

/// Run the server using io_uring backend.
pub fn run(config: Config, storage: Arc<Storage>, protocol: Protocol) -> std::io::Result<()> {
    event_loop::run(config, storage, protocol)
}
