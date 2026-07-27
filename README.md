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

Add `--profile monitoring` for Prometheus and Grafana.

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

Weir reads a TOML file (default `config.toml`, see `config.example.toml` for every option). The main sections are `[server]`, `[logging]`, `[ratelimit]`, `[protection]`, `[metrics]`, and `[redis]`.

A few values can also be set through environment variables or CLI flags:

| Variable | Purpose |
| --- | --- |
| `WEIR_CONFIG` | Path to the config file |
| `PORT` | Override the listen port |
| `LOG_LEVEL` | Override the log level (`trace`, `debug`, `info`, `warn`, `error`). Ignored if `RUST_LOG` is set |
| `METRICS_PORT` | Override the Prometheus metrics port |

Prometheus metrics are exposed on the metrics port at `/metrics`. Health checks live on the main port. `/health/live` and `/health/ready` report whether this instance is serving and are the ones to probe. `/health/upstream` returns 503 when weir is Cloudflare-blocked or the invalid request count passes 8000 of Discord's 10k per 10 minutes. On the Redis backend that state is shared by every instance, so alert on it rather than probing it.

## Backends

Weir ships two rate-limit backends, selected by `[ratelimit] backend` in the config.

### Memory (default)

In-process state, zero external dependencies. Fast, lock-free hot path. Use this when running a single instance.

```toml
[ratelimit]
backend = "memory"
```

### Redis (distributed)

Shared state across multiple weir instances. Pods cooperate on the same bucket counters, global limits, Cloudflare ban state, and token/webhook health.

Bucket and global limits are per token, so routing each bot to a fixed instance (an nginx `hash $http_authorization consistent` upstream, for example) enforces both without Redis. Redis buys two things that token routing cannot: interchangeable replicas, and a shared view of the limits Discord scopes to your IP rather than your token, namely the Cloudflare ban and the 10k per 10 minutes invalid request budget.

Sharing IP-scoped state is only correct if every instance egresses from one address. On Kubernetes without a NAT gateway, pods take the IP of the node they land on, so one node's Cloudflare ban would pause pods that were never banned.

```toml
[ratelimit]
backend = "redis"

[redis]
url = "redis://redis:6379"           # standalone Redis or managed Redis primary endpoint
cluster_nodes = []                   # set this for Redis Cluster, e.g. ["redis://n1:6379", "redis://n2:6379"]
key_prefix = "weir:v1:"
connect_timeout_ms = 5000
command_timeout_ms = 200
l1_cache_ttl_ms = 250
```

Requirements:

- Redis 5 or newer with Lua scripting enabled. Managed services work when pointed at their primary endpoint.
- Standalone or Cluster. For Cluster, set `cluster_nodes` to the seed list and leave `url` unused.
- `key_prefix` must not contain braces. Weir refuses to start if it does, since a token's keys are hash tagged (`{token}`) to share a slot.
- Use a `rediss://` URL for TLS.
- Sentinel is not supported directly. Front it with HAProxy or any proxy that exposes a plain Redis URL.

Expect three Redis round trips per proxied request. Cloudflare, health and route lookups are cached in process for `l1_cache_ttl_ms` (250ms), so raising it trades staleness for fewer round trips.

If Redis becomes unreachable, a pod keeps serving from in-process state and rejoins on its own once Redis is back. Limits are enforced per pod while it is in that state, so N pods can send up to N times the configured rate and Cloudflare bans stop propagating between them. Alert on `weir_redis_fallback_active` and `weir_redis_fail_open_total`.

The binary is built with the `redis` Cargo feature enabled by default. To produce a slimmer memory-only binary, build with `--no-default-features`.

## Running multiple instances

### Docker Compose

The repo ships a `cluster` compose profile that wires three weir replicas behind an nginx load balancer, with Redis backing rate-limit state:

```bash
docker compose --profile cluster up -d
```

This exposes the load balancer on `http://localhost:8081`, keeping port 8080 free for the single-instance service. Nginx round-robins to the replicas via Docker DNS. Scale with `docker compose --profile cluster up -d --scale weir-cluster=5`.

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
          livenessProbe:
            httpGet: { path: /health/live, port: 8080 }
          readinessProbe:
            httpGet: { path: /health/ready, port: 8080 }
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

Do not point a readiness probe at `/health/upstream`. On the Redis backend a single Cloudflare ban fails it on every pod at once, which would empty the Service while weir is still serving.

## Contributing

Pull requests are welcome. If you are planning a larger change, please open an issue first so we can talk through the approach. For smaller fixes, just send the PR. Please run `cargo fmt` and `cargo clippy` before you push.

## License

Weir is released under the MIT License. See `LICENSE` for the full text.
