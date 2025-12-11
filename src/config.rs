//! Configuration module for grow-a-cache server.
//!
//! Supports both command-line arguments and TOML configuration file.
//! CLI arguments take precedence over config file values.

use clap::Parser;
use serde::Deserialize;
use std::path::PathBuf;

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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            workers: None,
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
    pub listen: String,
    pub max_memory: usize,
    pub default_ttl: u64,
    pub cleanup_interval: u64,
    pub workers: Option<usize>,
    pub log_level: String,
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
            toml::from_str(&contents)
                .map_err(|e| ConfigError::TomlParse(config_path.clone(), e))?
        } else {
            TomlConfig::default()
        };

        // Merge CLI args with TOML config (CLI takes precedence)
        Ok(Config {
            listen: cli
                .listen
                .unwrap_or(toml_config.server.listen),
            max_memory: cli
                .max_memory
                .unwrap_or(toml_config.storage.max_memory),
            default_ttl: cli
                .default_ttl
                .unwrap_or(toml_config.storage.default_ttl),
            cleanup_interval: toml_config.storage.cleanup_interval,
            workers: cli.workers.or(toml_config.server.workers),
            log_level: if cli.log_level != "info" {
                cli.log_level
            } else {
                toml_config.logging.level
            },
        })
    }
}

/// Configuration loading errors
#[derive(Debug)]
pub enum ConfigError {
    FileRead(PathBuf, std::io::Error),
    TomlParse(PathBuf, toml::de::Error),
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
