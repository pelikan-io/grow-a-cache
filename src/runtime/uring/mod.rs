//! Linux io_uring event loop implementation.
//!
//! Completion-based I/O with batched submissions for high throughput.
//! Uses provided buffer rings for kernel-managed buffer selection.

mod buf_ring;
mod event_loop;
mod token;

pub(crate) use token::{OpType, TokenAllocator};

use crate::config::Config;
use crate::runtime::Protocol;
use crate::storage::Storage;
use std::sync::Arc;

/// Run the server using io_uring backend.
pub fn run(config: Config, storage: Arc<Storage>, protocol: Protocol) -> std::io::Result<()> {
    event_loop::run(config, storage, protocol)
}
