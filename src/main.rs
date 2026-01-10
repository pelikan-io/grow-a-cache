//! grow-a-cache: A multi-protocol cache server
//!
//! This server supports multiple cache protocols:
//! - Memcached text protocol
//! - Redis RESP protocol
//!
//! Features:
//! - Key-value storage with get, set, delete, cas
//! - Automatic key expiration
//! - Memory usage capping with LRU eviction
//! - Configuration via CLI arguments or TOML file

mod config;
mod protocols;
mod runtime;
mod storage;

use config::{Config, RuntimeType};
use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load configuration
    let config = Config::load()?;

    // Initialize logging
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    info!(
        host = %config.host,
        port = config.port,
        protocol = ?config.protocol,
        runtime = ?config.runtime,
        max_memory_mb = config.max_memory / 1024 / 1024,
        default_ttl = config.default_ttl,
        "Starting grow-a-cache server"
    );

    match config.runtime {
        RuntimeType::Mio => run_mio(config),
        RuntimeType::IoUring => run_uring(config),
    }
}

/// Run with mio runtime (epoll on Linux, kqueue on macOS)
fn run_mio(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    info!("Using mio runtime (epoll/kqueue)");
    runtime::run_mio(config)?;
    Ok(())
}

/// Run with io_uring runtime (Linux only)
fn run_uring(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    info!("Using io_uring runtime (Linux only)");
    runtime::run_uring(config)?;
    Ok(())
}
