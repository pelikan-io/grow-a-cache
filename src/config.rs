//! Configuration module for grow-a-cache server.
//!
//! Supports both command-line arguments and TOML configuration file.
//! CLI arguments take precedence over config file values.

use clap::{Parser, ValueEnum};
use serde::Deserialize;
use std::path::PathBuf;

/// Protocol type for the server
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolType {
    /// Memcached text protocol
    #[default]
    Memcached,
    /// Redis RESP protocol
    Resp,
    /// Ping protocol (responds PONG to PING, for health checks and latency testing)
    Ping,
    /// Echo protocol (echoes back input, for throughput testing with varied sizes)
    Echo,
}

/// Runtime backend for the server
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeType {
    /// Tokio async runtime (default, stable)
    #[default]
    Tokio,
    /// Custom io_uring/kqueue runtime (experimental)
    Native,
    /// mio/epoll runtime (for comparison with io_uring on Linux)
    Mio,
}

/// Command-line arguments for the cache server
#[derive(Parser, Debug)]
#[command(name = "grow-a-cache")]
#[command(author = "grow-a-cache authors")]
#[command(version = "0.1.0")]
#[command(about = "A memcached-compatible cache server", long_about = None)]
pub struct CliArgs {
    /// Path to TOML configuration file
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Address to bind to (e.g., 127.0.0.1:11211)
    #[arg(short = 'l', long)]
    pub listen: Option<String>,

    /// Maximum memory usage in bytes (e.g., 67108864 for 64MB)
    #[arg(short = 'm', long)]
    pub max_memory: Option<usize>,

    /// Default TTL for items in seconds (0 = no expiration)
    #[arg(short = 't', long)]
    pub default_ttl: Option<u64>,

    /// Number of worker threads (defaults to number of CPU cores)
    #[arg(short = 'w', long)]
    pub workers: Option<usize>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// Protocol to use (memcached or resp)
    #[arg(long, value_enum, default_value = "memcached")]
    pub protocol: ProtocolType,

    /// Runtime backend (tokio or native)
    #[arg(long, value_enum, default_value = "tokio")]
    pub runtime: RuntimeType,
}

/// TOML configuration file structure
#[derive(Debug, Deserialize, Default)]
pub struct TomlConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// Server-related configuration
#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    /// Address to bind to
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Number of worker threads
    pub workers: Option<usize>,
    /// Protocol to use
    #[serde(default)]
    pub protocol: ProtocolType,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            workers: None,
            protocol: ProtocolType::default(),
        }
    }
}

/// Storage-related configuration
#[derive(Debug, Deserialize)]
pub struct StorageConfig {
    /// Maximum memory usage in bytes
    #[serde(default = "default_max_memory")]
    pub max_memory: usize,
    /// Default TTL for items in seconds
    #[serde(default)]
    pub default_ttl: u64,
    /// Interval for running expiration cleanup in seconds
    #[serde(default = "default_cleanup_interval")]
    pub cleanup_interval: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            max_memory: default_max_memory(),
            default_ttl: 0,
            cleanup_interval: default_cleanup_interval(),
        }
    }
}

/// Logging configuration
#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    /// Log level
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

fn default_listen() -> String {
    "127.0.0.1:11211".to_string()
}

fn default_max_memory() -> usize {
    64 * 1024 * 1024 // 64 MB
}

fn default_cleanup_interval() -> u64 {
    60 // 60 seconds
}

fn default_log_level() -> String {
    "info".to_string()
}

/// Final resolved configuration
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub max_memory: usize,
    pub default_ttl: u64,
    pub cleanup_interval: u64,
    pub workers: usize,
    pub log_level: String,
    pub protocol: ProtocolType,
    pub runtime: RuntimeType,
    // Runtime configuration
    pub ring_size: usize,
    pub buffer_size: usize,
    pub max_connections: usize,
    pub batch_size: usize,
}

impl Config {
    /// Load configuration from CLI args and optional TOML file.
    /// CLI arguments take precedence over TOML file values.
    pub fn load() -> Result<Self, ConfigError> {
        let cli = CliArgs::parse();

        // Load TOML config if specified
        let toml_config = if let Some(ref config_path) = cli.config {
            let contents = std::fs::read_to_string(config_path)
                .map_err(|e| ConfigError::FileRead(config_path.clone(), e))?;
            toml::from_str(&contents).map_err(|e| ConfigError::TomlParse(config_path.clone(), e))?
        } else {
            TomlConfig::default()
        };

        // Merge CLI args with TOML config (CLI takes precedence)
        let listen = cli.listen.unwrap_or(toml_config.server.listen);
        let (host, port) = parse_listen_address(&listen)?;

        Ok(Config {
            host,
            port,
            max_memory: cli.max_memory.unwrap_or(toml_config.storage.max_memory),
            default_ttl: cli.default_ttl.unwrap_or(toml_config.storage.default_ttl),
            cleanup_interval: toml_config.storage.cleanup_interval,
            workers: cli.workers.or(toml_config.server.workers).unwrap_or(0),
            log_level: if cli.log_level != "info" {
                cli.log_level
            } else {
                toml_config.logging.level
            },
            protocol: if cli.protocol != ProtocolType::default() {
                cli.protocol
            } else {
                toml_config.server.protocol
            },
            runtime: cli.runtime,
            // Runtime defaults (TODO: make configurable)
            ring_size: 4096,
            buffer_size: 16384, // 16KB per connection
            max_connections: 10000,
            batch_size: 64,
        })
    }
}

fn parse_listen_address(addr: &str) -> Result<(String, u16), ConfigError> {
    if let Some((host, port_str)) = addr.rsplit_once(':') {
        let port = port_str
            .parse()
            .map_err(|_| ConfigError::InvalidAddress(addr.to_string()))?;
        Ok((host.to_string(), port))
    } else {
        Err(ConfigError::InvalidAddress(addr.to_string()))
    }
}

/// Configuration loading errors
#[derive(Debug)]
pub enum ConfigError {
    FileRead(PathBuf, std::io::Error),
    TomlParse(PathBuf, toml::de::Error),
    InvalidAddress(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::FileRead(path, e) => {
                write!(f, "Failed to read config file '{}': {}", path.display(), e)
            }
            ConfigError::TomlParse(path, e) => {
                write!(f, "Failed to parse config file '{}': {}", path.display(), e)
            }
            ConfigError::InvalidAddress(addr) => {
                write!(f, "Invalid listen address '{addr}': expected host:port")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = TomlConfig::default();
        assert_eq!(config.server.listen, "127.0.0.1:11211");
        assert_eq!(config.storage.max_memory, 64 * 1024 * 1024);
        assert_eq!(config.storage.default_ttl, 0);
    }

    #[test]
    fn test_toml_parsing() {
        let toml_str = r#"
            [server]
            listen = "0.0.0.0:11211"
            workers = 4

            [storage]
            max_memory = 134217728
            default_ttl = 3600

            [logging]
            level = "debug"
        "#;

        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.listen, "0.0.0.0:11211");
        assert_eq!(config.server.workers, Some(4));
        assert_eq!(config.storage.max_memory, 134217728);
        assert_eq!(config.storage.default_ttl, 3600);
        assert_eq!(config.logging.level, "debug");
    }
}
