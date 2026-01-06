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
mod server;
mod storage;

use config::{Config, RuntimeType};
use server::Server;
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
        RuntimeType::Tokio => run_tokio(config),
        RuntimeType::Native => run_native(config),
    }
}

/// Run with Tokio async runtime (stable)
fn run_tokio(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let server = Server::new(config);
        server.run().await
    })
}

/// Run with native io_uring/kqueue runtime (experimental)
fn run_native(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    info!("Using native runtime (experimental)");
    runtime::run(config)?;
    Ok(())
}
