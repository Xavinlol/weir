# Weir

A high performance Discord REST API proxy written in Rust. Weir sits between your bots and Discord, handling global and per-route rate limits. It supports multiple bots, bearer tokens, and webhooks out of the box, with Cloudflare ban detection and Prometheus metrics. It exists because running many bots or sharded services against a single token means you need one authoritative place to track buckets and avoid 429s.

## Installation

Build from source with Cargo:

```bash
git clone https://github.com/xavinlol/weir.git
cd weir
cargo build --release
```

Or use Docker:

```bash
# Pull the pre-built image
docker pull ghcr.io/xavinlol/weir:latest

# Or use Docker Compose
docker compose up -d
```

## Quick Start

Copy the example config and start the proxy:

```bash
cp config.example.toml config.toml
./target/release/weir --config config.toml
```

Point your Discord library at the proxy instead of `https://discord.com`:

```bash
curl http://localhost:8080/api/v10/users/@me \
  -H "Authorization: Bot YOUR_BOT_TOKEN"
```

## Configuration

Weir reads a TOML file (default `config.toml`, see `config.example.toml` for every option). The main sections are `[server]`, `[logging]`, `[ratelimit]`, `[protection]`, and `[metrics]`.

A few values can also be set through environment variables or CLI flags:

| Variable | Purpose |
| --- | --- |
| `WEIR_CONFIG` | Path to the config file |
| `PORT` | Override the listen port |
| `LOG_LEVEL` | Override the log level (`trace`, `debug`, `info`, `warn`, `error`) |
| `METRICS_PORT` | Override the Prometheus metrics port |

Prometheus metrics are exposed on the metrics port at `/metrics`. Health checks live at `/health/live` and `/health/ready` on the main port.

## Contributing

Pull requests are welcome. If you are planning a larger change, please open an issue first so we can talk through the approach. For smaller fixes, just send the PR. Please run `cargo fmt` and `cargo clippy` before you push.

## License

Weir is released under the MIT License. See `LICENSE` for the full text.
