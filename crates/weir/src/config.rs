use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

use crate::Cli;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub logging: LoggingConfig,
    pub ratelimit: RatelimitConfig,
    pub protection: ProtectionConfig,
    pub metrics: MetricsConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: IpAddr,
    pub port: u16,
    pub request_timeout_ms: u64,
    pub graceful_shutdown_timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
    pub access_log: bool,
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Json,
    Pretty,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct RatelimitConfig {
    pub backend: RatelimitBackend,
    pub global_limit_default: u32,
    pub disable_global_detection: bool,
    pub bucket_ttl_ms: u64,
    pub cleanup_interval_ms: u64,
    pub queue_timeout_ms: u64,
    pub overrides: HashMap<String, BotOverride>,
}

#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RatelimitBackend {
    #[default]
    Memory,
    Redis,
}

#[derive(Debug, Deserialize)]
pub struct BotOverride {
    pub global_limit: u32,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ProtectionConfig {
    pub consecutive_error_threshold: u32,
    pub consecutive_404_threshold: u32,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 8080,
            request_timeout_ms: 10_000,
            graceful_shutdown_timeout_ms: 30_000,
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            format: LogFormat::default(),
            access_log: true,
        }
    }
}

impl Default for RatelimitConfig {
    fn default() -> Self {
        Self {
            backend: RatelimitBackend::default(),
            global_limit_default: 50,
            disable_global_detection: false,
            bucket_ttl_ms: 86_400_000,
            cleanup_interval_ms: 300_000,
            queue_timeout_ms: 10_000,
            overrides: HashMap::new(),
        }
    }
}

impl Default for ProtectionConfig {
    fn default() -> Self {
        Self {
            consecutive_error_threshold: 5,
            consecutive_404_threshold: 10,
        }
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 9000,
        }
    }
}

impl Config {
    pub fn load(path: &str, cli: &Cli) -> Result<Self> {
        let mut config = if Path::new(path).exists() {
            let contents = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read config file: {path}"))?;
            toml::from_str(&contents)
                .with_context(|| format!("failed to parse config file: {path}"))?
        } else {
            tracing::warn!(path, "config file not found, using defaults");
            Self::default()
        };

        if let Some(port) = cli.port {
            config.server.port = port;
        }
        if let Some(ref level) = cli.log_level {
            config.logging.level.clone_from(level);
        }
        if let Some(metrics_port) = cli.metrics_port {
            config.metrics.port = metrics_port;
        }

        Ok(config)
    }
}

pub fn init_logging(config: &LoggingConfig) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false);

    match config.format {
        LogFormat::Json => subscriber.json().init(),
        LogFormat::Pretty => subscriber.pretty().init(),
    }
}
