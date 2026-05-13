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

## Backends

Weir ships two rate-limit backends, selected by `[ratelimit] backend` in the config.

### Memory (default)

In-process state, zero external dependencies. Fast, lock-free hot path. Use this when running a single instance.

```toml
[ratelimit]
backend = "memory"
```

### Redis (distributed)

Shared state across multiple weir instances. Pods cooperate on the same bucket counters, global limits, Cloudflare ban state, and token/webhook health. Required when running behind a load balancer with more than one replica.

```toml
[ratelimit]
backend = "redis"

[redis]
url = "redis://redis:6379"
key_prefix = "weir:v1:"
connect_timeout_ms = 5000
command_timeout_ms = 200
l1_cache_ttl_ms = 250
```

State for each token uses Redis Cluster hash tags (`{token}`) so all keys for that token land in the same slot. The shipped client supports standalone Redis and Sentinel. Cluster support is on the roadmap.

On Redis outage, each pod degrades to a fresh in-process limiter and continues serving traffic. When Redis returns, a background task reconnects, replays `SCRIPT LOAD`, and resumes shared-state mode. The `weir_redis_fallback_active` gauge tracks this state.

The binary is built with the `redis` Cargo feature enabled by default. To produce a slimmer memory-only binary, build with `--no-default-features`.

Known limitations of the Redis backend (planned for future work):

* The `weir_active_buckets` and `weir_invalid_request_count` gauges are not aggregated across pods; they report 0 on the Redis backend.
* Cluster mode (`redis://` URLs pointing at a Redis Cluster) is on the roadmap. Standalone and Sentinel topologies work today.

## Running multiple instances

### Docker Compose

The repo ships a `cluster` compose profile that wires three weir replicas behind an nginx load balancer, with Redis backing rate-limit state:

```bash
docker compose --profile cluster up -d
```

This exposes the proxy on `http://localhost:8080`. Nginx round-robins to the replicas via Docker DNS; scaling is `docker compose --profile cluster up -d --scale weir-cluster=5`.

### Kubernetes

A `Deployment` with `replicas: N` plus a `Service` is enough. Kube-proxy handles load balancing for free. Use a Redis Operator (or a managed Redis) and point every weir pod at the same URL:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: weir
spec:
  replicas: 3
  selector:
    matchLabels: { app: weir }
  template:
    metadata:
      labels: { app: weir }
    spec:
      containers:
        - name: weir
          image: ghcr.io/xavinlol/weir:latest
          ports: [{ containerPort: 8080 }, { containerPort: 9000 }]
          env:
            - { name: WEIR_CONFIG, value: /etc/weir/config.toml }
          volumeMounts:
            - { name: config, mountPath: /etc/weir, readOnly: true }
      volumes:
        - name: config
          configMap: { name: weir-config }
---
apiVersion: v1
kind: Service
metadata:
  name: weir
spec:
  selector: { app: weir }
  ports:
    - { port: 8080, targetPort: 8080 }
```

The ConfigMap should contain a `config.toml` with `backend = "redis"` and a `[redis]` section pointing at the shared Redis.

## Contributing

Pull requests are welcome. If you are planning a larger change, please open an issue first so we can talk through the approach. For smaller fixes, just send the PR. Please run `cargo fmt` and `cargo clippy` before you push.

## License

Weir is released under the MIT License. See `LICENSE` for the full text.
