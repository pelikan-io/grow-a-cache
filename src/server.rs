//! TCP server for handling cache connections.
//!
//! Handles incoming connections and dispatches them to the appropriate
//! protocol handler based on configuration.

use crate::config::{Config, ProtocolType};
use crate::protocols::{echo, memcached, ping, resp};
use crate::storage::Storage;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tracing::{debug, error, info};

/// Maximum number of concurrent connections
const MAX_CONNECTIONS: usize = 10000;

/// Server instance
pub struct Server {
    config: Config,
    storage: Arc<Storage>,
    connection_limit: Arc<Semaphore>,
}

impl Server {
    /// Create a new server instance
    pub fn new(config: Config) -> Self {
        let storage = Storage::new(config.max_memory, config.default_ttl);

        Server {
            config,
            storage,
            connection_limit: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        }
    }

    /// Start the server and begin accepting connections
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let listener = TcpListener::bind(&addr).await?;
        info!(
            address = %addr,
            protocol = ?self.config.protocol,
            "Server listening"
        );

        // Start the expiration cleanup task
        let storage_clone = Arc::clone(&self.storage);
        let cleanup_interval = self.config.cleanup_interval;
        tokio::spawn(async move {
            cleanup_task(storage_clone, cleanup_interval).await;
        });

        loop {
            // Wait for a connection slot
            let permit = self.connection_limit.clone().acquire_owned().await?;

            match listener.accept().await {
                Ok((stream, addr)) => {
                    debug!(peer = %addr, "New connection");

                    let storage = Arc::clone(&self.storage);
                    let protocol = self.config.protocol;

                    tokio::spawn(async move {
                        let result = match protocol {
                            ProtocolType::Memcached => {
                                memcached::handle_connection(stream, storage).await
                            }
                            ProtocolType::Resp => resp::handle_connection(stream, storage).await,
                            ProtocolType::Ping => ping::handle_connection(stream, storage).await,
                            ProtocolType::Echo => echo::handle_connection(stream, storage).await,
                        };

                        if let Err(e) = result {
                            debug!(error = %e, "Connection error");
                        }
                        drop(permit);
                    });
                }
                Err(e) => {
                    error!(error = %e, "Failed to accept connection");
                }
            }
        }
    }

    /// Get a reference to the storage for testing
    #[cfg(test)]
    pub fn storage(&self) -> &Arc<Storage> {
        &self.storage
    }
}

/// Background task to clean up expired items
async fn cleanup_task(storage: Arc<Storage>, interval_secs: u64) {
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));

    loop {
        interval.tick().await;
        let count = storage.cleanup_expired();
        if count > 0 {
            debug!(count, "Cleaned up expired items");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeType;

    #[tokio::test]
    async fn test_server_creation() {
        let config = Config {
            host: "127.0.0.1".to_string(),
            port: 0,
            max_memory: 1024 * 1024,
            default_ttl: 0,
            cleanup_interval: 60,
            workers: 0,
            log_level: "info".to_string(),
            protocol: ProtocolType::Memcached,
            runtime: RuntimeType::Tokio,
            ring_size: 4096,
            buffer_size: 16384,
            max_connections: 10000,
            batch_size: 64,
            max_value_size: 8 * 1024 * 1024, // 8MB
        };

        let server = Server::new(config);
        assert!(server.storage().stats().item_count == 0);
    }
}
