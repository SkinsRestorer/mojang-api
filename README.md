# Mojang API Proxy

Mojang API Proxy provides cached Minecraft username and skin lookups. It batches username requests, rotates optional outbound proxies, and exposes an OpenAPI document with Swagger UI.

## Run locally

Install Rust through `rustup`, then start the service. The repository pins Rust 1.97.1 in `rust-toolchain.toml`.

```bash
cargo run
```

The server listens on `http://localhost:3000` by default. Open `http://localhost:3000/swagger` for the interactive API reference.

Check the service from a terminal:

```bash
curl http://localhost:3000/health
curl http://localhost:3000/mojang/uuid/Pistonmaster
```

## Configuration

The service reads `.env` during local development and uses normal environment variables in production.

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `SERVER_PORT` | No | `PORT` or `3000` | Compatibility override for the TCP port |
| `PORT` | No | `3000` | TCP port used by the HTTP server and hosting platforms |
| `PROXY_LIST_FILE` | No | Direct connection | File containing outbound HTTP proxies |
| `DISCORD_WEBHOOK` | No | Disabled | Discord webhook that receives five-minute status reports |
| `RUST_LOG` | No | `info` | Logging filter used by `tracing` |

Each non-empty line in the proxy file must use one of these formats:

```text
host:port
host:port:username:password
```

Passwords may contain additional colons. Invalid entries are logged and ignored. If the configured file does not exist, the service starts with direct connections.

## API

### Look up a UUID

```http
GET /mojang/uuid/{name}
```

A successful response returns a dashed UUID:

```json
{
  "exists": true,
  "uuid": "b1ae0778-4817-436c-96a3-a72c67cda060"
}
```

Unknown usernames return `200 OK` with `exists` set to `false` and `uuid` set to `null`.

### Look up a skin property

```http
GET /mojang/skin/{uuid}
```

A successful response contains the signed Mojang texture property:

```json
{
  "exists": true,
  "skinProperty": {
    "value": "base64-encoded-texture-data",
    "signature": "texture-signature"
  }
}
```

Missing profiles or texture properties return `200 OK` with `exists` set to `false`.

### Other endpoints

- `GET /health` returns the liveness status.
- `GET /openapi` returns the generated OpenAPI document.
- `GET /swagger` opens Swagger UI.

Successful Mojang lookup responses include `Cache-Control: public, max-age=900`. Invalid input returns `400`, upstream timeouts return `503`, and other upstream failures return `500`.

## Deploy with Railpack

Railpack detects the Rust project from `Cargo.toml`, installs its system dependencies, uses the version pinned in `rust-toolchain.toml`, builds the release binary, and starts `./bin/mojang-api`. No custom Railpack configuration or start command is required.

Set any production configuration variables in the deployment environment. The application reads the platform-provided `PORT` variable directly while retaining `SERVER_PORT` as a compatibility override.

## Development checks

Run the same checks used by CI:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

## Architecture

Axum serves the HTTP API on Tokio. A bounded channel collects username lookups and flushes batches when ten distinct requests are queued or after three seconds. Concurrent requests for the same username or UUID share one in-flight lookup instead of creating duplicate Mojang requests.

Moka stores up to 10,000 username entries and uses a 32 MiB weighted budget for skin entries. Positive results expire after six hours. Negative results expire after 15 minutes so newly created profiles and skins become visible sooner.

Reqwest handles Mojang and Discord traffic with Rustls. Mojang requests use a 15-second timeout, reject response bodies larger than 1 MiB, and select a random configured proxy for each request. The rate limiter uses the trusted `CF-Connecting-IP` address and shards client state to reduce lock contention.

Metrics remain cumulative in memory. A Discord report advances its reporting window only after Discord accepts it, so transport errors and non-successful HTTP responses do not discard counters.

On `SIGINT` or `SIGTERM`, the server stops accepting traffic and gives HTTP requests and queued batches up to 30 seconds each to drain. Work that exceeds either deadline is cancelled so shutdown cannot hang indefinitely.
