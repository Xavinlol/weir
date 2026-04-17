mod config;
mod error;
mod health;
mod proxy;
mod request;
mod response;
mod server;

use anyhow::Result;
use clap::Parser;
use tracing::info;

/// Weir - A high-performance Discord REST API proxy.
#[derive(Parser, Debug)]
#[command(name = "weir", version, about, long_about = None)]
struct Cli {
    /// Path to the configuration file.
    #[arg(short, long, default_value = "config.toml", env = "WEIR_CONFIG")]
    config: String,

    /// Override the listen port.
    #[arg(short, long, env = "PORT")]
    port: Option<u16>,

    /// Override the log level.
    #[arg(short, long, env = "LOG_LEVEL")]
    log_level: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = config::Config::load(&cli.config, &cli)?;

    config::init_logging(&config.logging);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        port = config.server.port,
        "starting weir"
    );

    server::run(config).await
}
