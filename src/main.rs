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
        protocol = ?config.protocol,
        max_memory_mb = config.max_memory / 1024 / 1024,
        default_ttl = config.default_ttl,
        "Starting grow-a-cache server"
    );

    // Create and run the server
    let server = Server::new(config);
    server.run().await?;

    Ok(())
}
