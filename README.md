# Mojang API Proxy

Mojang API Proxy provides cached Minecraft username and skin lookups. It batches username requests, rotates optional outbound proxies, and exposes an OpenAPI document with Swagger UI.

## Run locally

Install Rust through `rustup`, then start the service. The repository pins the Rust version in `rust-toolchain.toml`.

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

The service reads environment variables and loads the first `.env` file it finds in the working directory or its parents. Variables that are already set take precedence over `.env`. A variable set to an empty value counts as unset. When a value is invalid, the service stops at startup and names the variable. An invalid `RUST_LOG` is the exception: the service logs a warning and uses `info`.

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `SERVER_PORT` | No | `PORT` or `3000` | Compatibility override for the TCP port |
| `PORT` | No | `3000` | TCP port used by the HTTP server and hosting platforms |
| `PROXY_LIST_FILE` | No | Direct connection | File containing outbound HTTP proxies |
| `DISCORD_WEBHOOK` | No | Disabled | `https://` Discord webhook that receives five-minute status reports |
| `TRUSTED_PROXIES` | No | Loopback, private ranges, and `100.64.0.0/10` | Comma-separated IP addresses or CIDR ranges whose `CF-Connecting-IP` header is trusted, or `none`. A list with no entries is an error. The server logs the effective list at startup. |
| `RUST_LOG` | No | `info` | Logging filter used by `tracing` |

Each non-empty line in the proxy file must use one of these formats:

```text
host:port
host:port:username:password
```

An entry may start with `http://`. Write IPv6 hosts in brackets, for example `[::1]:8080` or `[::1]:8080:username:password`. Passwords may contain additional colons. Port 0 is invalid. Invalid entries are logged and ignored. If the configured file cannot be read, the service does not start. If the file has no valid entries, the service logs a warning and uses direct connections.

### Client addresses and rate limits

Each client may send 1,000 requests per minute to the Mojang and documentation endpoints. `/health` has no limit. The limiter identifies a client by the TCP peer address. When the peer is in `TRUSTED_PROXIES`, it uses the `CF-Connecting-IP` header instead. IPv6 clients share one limit per /64 prefix. Responses from these endpoints include `RateLimit-Limit`, `RateLimit-Remaining`, `RateLimit-Reset`, and `RateLimit-Policy` headers, and CORS exposes them to browser scripts.

The default trusts loopback and private peers, which fits a reverse proxy such as Traefik on the same host or network. That proxy passes `CF-Connecting-IP` through unchanged, so make sure the origin only accepts traffic from Cloudflare. Otherwise, a client that reaches the origin directly can set the header to any address.

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

Usernames must have 1 to 16 characters from `A-Z`, `a-z`, `0-9`, `_`, and `-`.

### Look up a skin property

```http
GET /mojang/skin/{uuid}
```

The UUID may use the dashed form or 32 hex digits without dashes.

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

Missing profiles or texture properties return `200 OK` with `exists` set to `false` and `skinProperty` set to `null`.

### Other endpoints

- `GET /health` returns `200` with `"status":"UP"`, or `503` with `"status":"DOWN"` when username lookups have stopped. See [Health checks](#health-checks).
- `GET /openapi` returns the generated OpenAPI document.
- `GET /swagger` opens Swagger UI. `GET /` redirects to it.

Lookup responses for existing profiles include `Cache-Control: public, max-age=900`. Responses with `exists` set to `false` include `Cache-Control: public, max-age=60`. These lifetimes are fixed and shorter than the server cache TTLs. A client copy can outlive the server entry it came from, so a result can be up to the server TTL plus `max-age` old.

### Errors

Every error response has a JSON body such as `{"error":"INVALID_NAME"}`. A request that runs longer than 8 seconds gets `503` with `INTERNAL_TIMEOUT`.

| Status | `error` | Meaning |
| --- | --- | --- |
| `400` | `INVALID_NAME`, `INVALID_UUID` | The username or UUID is not valid or is empty |
| `404` | `NOT_FOUND` | The path does not exist |
| `405` | `METHOD_NOT_ALLOWED` | The path does not support the method |
| `429` | `RATE_LIMITED` | The client exceeded its rate limit. Retry after `Retry-After` seconds |
| `500` | `INTERNAL_ERROR` | The service failed internally, for example because username batching stopped |
| `502` | `UPSTREAM_ERROR` | The request to Mojang failed, Mojang returned an unexpected HTTP status, or Mojang sent an invalid response |
| `503` | `INTERNAL_TIMEOUT` | Mojang did not respond in time |
| `503` | `SERVICE_UNAVAILABLE` | Mojang rate-limited the service, or the service is at capacity. Retry after `Retry-After` seconds |

## Build the image

The `Dockerfile` builds the release binary with the official Rust image and copies it into the `gcr.io/distroless/cc-debian13:nonroot` runtime image. The runtime image has no shell or package manager. It runs the service as the `nonroot` user (UID 65532) and has the CA certificates that the service needs to verify Mojang's TLS certificates. The binary is the entrypoint, so it runs as PID 1 and receives `SIGTERM` directly.

Build the image locally:

```bash
docker build -t mojang-api .
```

Set production configuration variables in the deployment environment.

## Run the published image

Pushes to `main` build the image from the `Dockerfile` and publish it to
`ghcr.io/skinsrestorer/mojang-api` with these tags:

- `latest`: the latest published build from `main`
- `main`: the latest published build from `main`
- `sha-<12-character-commit-sha>`: the build for a specific commit

The workflow builds images only after the formatting, Clippy, test, and minimum Rust version
checks pass for the same commit. Each tag is a multi-architecture image for `amd64` and `arm64`. After it publishes
the image, the workflow calls the Dokploy webhook in the `DOKPLOY_WEBHOOK_URL` secret to deploy it.

Run the published image:

```bash
docker run -d --name mojang-api --restart unless-stopped \
  --stop-timeout 60 -p 3000:3000 ghcr.io/skinsrestorer/mojang-api:latest
```

Docker sends `SIGKILL` 10 seconds after `SIGTERM` by default. `--stop-timeout 60` gives the service time to drain requests and batches during shutdown. Give other orchestrators, such as Dokploy, the same stop grace period.

The service is available at `http://localhost:3000`. Pass configuration with `-e NAME=value` or `--env-file`. To use a proxy list, mount the file into the container and set `PROXY_LIST_FILE` to its path inside the container.

### Health checks

`GET /health` returns a body like this:

```json
{
  "status": "UP",
  "uptimeSeconds": 3600,
  "mojangCoolingDown": { "usernameLookups": false, "skinLookups": false }
}
```

- `status` is `DOWN`, with status code `503`, only when username lookups have stopped. A restart fixes that state.
- `uptimeSeconds` is the time since the server started. A small value after a long run shows a restart.
- `mojangCoolingDown` shows which lookups Mojang has rate-limited on every outbound route. While a value is `true`, lookups that miss the cache get `SERVICE_UNAVAILABLE`. These values do not change `status` or the status code, because a restart does not end a Mojang rate limit.

Container images without a shell or `curl` can run the binary itself as the health check command:

```bash
mojang-api --health-check
```

The command requests `/health` on `127.0.0.1` at the configured port. It exits with `0` when the response is `200`, and with `1` otherwise. It reads the same `PORT`, `SERVER_PORT`, and `.env` settings as the server. The published image already runs it as its Docker `HEALTHCHECK` (`/usr/local/bin/mojang-api --health-check`), so Docker and Dokploy mark the container unhealthy when `/health` fails.

## Architecture

Axum serves the HTTP API on Tokio. A bounded channel collects username lookups and sends a batch when ten requests are queued, and sends any smaller batch every second. Concurrent requests for the same username or UUID share one in-flight lookup instead of creating duplicate Mojang requests.

A username lookup fails with `503` when the batch queue is full, when the client already has 32 username lookups queued or in a running batch, or when its batch does not finish within 7 seconds. The client is the same address that the rate limit uses. A batch caches every result it gets from Mojang, even when the request that queued the name has already timed out or disconnected.

Moka stores up to 100,000 username entries and uses a 32 MiB weighted budget for skin entries. Usernames expire after six hours and skins after one hour, because players can change skins at any time. Negative results expire after 15 minutes so newly created profiles and skins become visible sooner. A request counts as a cache hit only when the value was already cached.

Reqwest handles Mojang and Discord traffic with Rustls. Mojang requests use a 4-second timeout and reject response bodies larger than 1 MiB. Each configured proxy has its own HTTP client, and each request picks a random proxy. When Mojang answers `429`, the service stops using that proxy for that Mojang host until the `Retry-After` delay passes (30 seconds by default), and retries the request once through another proxy if at least 2 seconds of a 5-second budget remain. The retry gets only the rest of that budget. Mojang limits the two username batch hosts separately, so a username lookup prefers a batch host that has not rate-limited the chosen proxy. When the service cannot connect through a proxy, it retries the request the same way through another available proxy and sends no traffic through the failed proxy for 30 seconds, unless no other proxy is available. Logs identify a route by its `route` index, which is the position of the proxy among the valid entries, starting at 0, or 0 for the direct connection. Logs never show proxy addresses or credentials. At most 64 skin lookups run at the same time.

The timeouts fit the SkinsRestorer plugin, which stops waiting after 5 seconds. A skin lookup finishes or fails within the 5-second Mojang budget. A username lookup can also wait up to one second for its batch. The 7-second lookup deadline and the 8-second request deadline only catch work that is stuck, such as batches that wait for a free batch slot.

The rate limiter uses the generic cell rate algorithm, so a client cannot send two bursts across a window boundary. It shards client state to reduce lock contention and caps the number of tracked clients. When a shard is full, it samples a few clients and forgets the one closest to a full quota, so a rate-limited client rarely gets a fresh quota.

The HTTP server listens on `[::]` for both IPv4 and IPv6 clients, and falls back to IPv4 only with a warning when the host has no IPv6. It closes connections that do not send complete request headers within 10 seconds, accepts at most 4,096 connections at a time, and ends requests that run longer than 8 seconds. It closes a connection when a response write makes no progress for 30 seconds, so a client that stops reading cannot hold a connection slot. It has no per-IP connection cap, because behind a reverse proxy every connection comes from the proxy.

Logs are plain text on stdout, with colors only when stdout is a terminal. Warnings that can repeat for every request log at most once per minute: a full batch queue, too many skin lookups, every proxy cooling down for a Mojang API, and `5xx` responses. Each of these lines has a `suppressed` field with the number of identical events since the previous line.

Metrics remain cumulative in memory. A Discord report advances its reporting window only after Discord accepts it, so transport errors and non-successful HTTP responses do not discard counters.

Each report shows the Mojang error rate of its window: Mojang errors plus lookups skipped because every route was rate-limited, divided by Mojang requests plus those skips. The report is green below 1%, orange from 1% to below 10%, and red at 10% or more. The report also counts rejected work: username lookups rejected because the batch queue was full, skin lookups rejected because 64 were already in progress, and client requests rejected by the rate limiter.

On `SIGINT` or `SIGTERM`, the server stops accepting traffic and gives HTTP requests and queued batches up to 30 seconds each to drain. Work that exceeds either deadline is cancelled so shutdown cannot hang indefinitely. A second signal stops the service at once with a non-zero exit code.

## Contributing and support

Read [the contribution guide](CONTRIBUTING.md) for development and review.
Use [the support guide](SUPPORT.md) for questions and issue routing.
