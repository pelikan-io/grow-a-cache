//! grow-a-cache: A memcached-compatible cache server
//!
//! This server implements the memcached text protocol and provides:
//! - Key-value storage with get, set, add, replace, delete, gets, cas
//! - Automatic key expiration
//! - Memory usage capping with LRU eviction
//! - Configuration via CLI arguments or TOML file

mod config;
mod protocol;
mod server;
mod storage;

use config::Config;
use server::Server;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load configuration
    let config = Config::load()?;

    // Initialize logging
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.log_level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    info!(
        listen = %config.listen,
        max_memory_mb = config.max_memory / 1024 / 1024,
        default_ttl = config.default_ttl,
        "Starting grow-a-cache server"
    );

    // Create and run the server
    let server = Server::new(config);
    server.run().await?;

    Ok(())
}
